// Minimal scrcpy 4.1 client: starts scrcpy-server on the device over adb, opens the video and
// control sockets through an adb forward tunnel, and demuxes the video stream.
// Protocol reference: https://github.com/Genymobile/scrcpy/blob/v4.1/doc/develop.md
import { EventEmitter } from 'node:events';
import { randomInt } from 'node:crypto';
import net from 'node:net';
import { SCRCPY_VERSION } from './scrcpy-version.js';

const REMOTE_SERVER_PATH = '/data/local/tmp/scrcpy-server.jar';
const CODEC_ID_H264 = 0x68323634;
const DEVICE_NAME_LENGTH = 64;
const PTS_MASK = (1n << 61n) - 1n;
const CONNECT_TIMEOUT_MS = 10000;

export const ControlType = Object.freeze({
  INJECT_KEYCODE: 0,
  INJECT_TEXT: 1,
  INJECT_TOUCH_EVENT: 2,
  INJECT_SCROLL_EVENT: 3,
  BACK_OR_SCREEN_ON: 4,
  EXPAND_NOTIFICATION_PANEL: 5,
  EXPAND_SETTINGS_PANEL: 6,
  COLLAPSE_PANELS: 7,
  GET_CLIPBOARD: 8,
  SET_CLIPBOARD: 9,
  SET_DISPLAY_POWER: 10,
  ROTATE_DEVICE: 11,
  START_APP: 16,
  RESET_VIDEO: 17,
});

const INJECT_TEXT_MAX_LENGTH = 300;
const CLIPBOARD_TEXT_MAX_LENGTH = (1 << 18) - 14;

/**
 * Checks that a buffer holds exactly one well-formed control message of an allowed type, so a
 * browser client can never desynchronize the control stream or reach UHID/file messages.
 */
export function isValidControlMessage(buf) {
  if (buf.length < 1) return false;
  switch (buf[0]) {
    case ControlType.INJECT_KEYCODE:
      return buf.length === 14;
    case ControlType.INJECT_TEXT: {
      if (buf.length < 5) return false;
      const len = buf.readUInt32BE(1);
      return len <= INJECT_TEXT_MAX_LENGTH && buf.length === 5 + len;
    }
    case ControlType.INJECT_TOUCH_EVENT:
      return buf.length === 32;
    case ControlType.INJECT_SCROLL_EVENT:
      return buf.length === 21;
    case ControlType.BACK_OR_SCREEN_ON:
    case ControlType.GET_CLIPBOARD:
    case ControlType.SET_DISPLAY_POWER:
      return buf.length === 2;
    case ControlType.EXPAND_NOTIFICATION_PANEL:
    case ControlType.EXPAND_SETTINGS_PANEL:
    case ControlType.COLLAPSE_PANELS:
    case ControlType.ROTATE_DEVICE:
    case ControlType.RESET_VIDEO:
      return buf.length === 1;
    case ControlType.SET_CLIPBOARD: {
      if (buf.length < 14) return false;
      const len = buf.readUInt32BE(10);
      return len <= CLIPBOARD_TEXT_MAX_LENGTH && buf.length === 14 + len;
    }
    case ControlType.START_APP:
      return buf.length >= 2 && buf.length === 2 + buf[1];
    default:
      return false;
  }
}

/** Pull-style reader over a socket: `await read(n)` resolves with exactly n bytes. */
class SocketReader {
  constructor(socket) {
    this.chunks = [];
    this.length = 0;
    this.ended = false;
    this.error = null;
    this.wake = null;
    socket.on('data', (chunk) => {
      this.chunks.push(chunk);
      this.length += chunk.length;
      this.notify();
    });
    socket.on('error', (err) => {
      this.error = err;
      this.ended = true;
      this.notify();
    });
    socket.on('close', () => {
      this.ended = true;
      this.notify();
    });
  }

  notify() {
    const wake = this.wake;
    this.wake = null;
    wake?.();
  }

  async read(n) {
    while (this.length < n) {
      if (this.ended) throw this.error ?? new Error('Socket closed');
      await new Promise((resolve) => (this.wake = resolve));
    }
    return this.take(n);
  }

  take(n) {
    this.length -= n;
    const first = this.chunks[0];
    if (first.length === n) return this.chunks.shift();
    if (first.length > n) {
      this.chunks[0] = first.subarray(n);
      return first.subarray(0, n);
    }
    const out = Buffer.allocUnsafe(n);
    let offset = 0;
    while (offset < n) {
      const chunk = this.chunks[0];
      const count = Math.min(chunk.length, n - offset);
      chunk.copy(out, offset, 0, count);
      offset += count;
      if (count === chunk.length) this.chunks.shift();
      else this.chunks[0] = chunk.subarray(count);
    }
    return out;
  }
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function tcpConnect(port) {
  return new Promise((resolve, reject) => {
    const socket = net.connect({ host: '127.0.0.1', port });
    socket.once('connect', () => {
      socket.removeListener('error', reject);
      socket.setNoDelay(true);
      resolve(socket);
    });
    socket.once('error', reject);
  });
}

function getFreePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

/** Server arguments end up in a remote `sh` command line, so keep them to a safe charset. */
function safeArg(value, name) {
  const str = String(value);
  if (!/^[\w.:=,-]*$/.test(str)) throw new Error(`Invalid characters in ${name}: ${str}`);
  return str;
}

export function buildServerArgs(extra) {
  return [
    `CLASSPATH=${REMOTE_SERVER_PATH}`,
    'app_process',
    '/',
    'com.genymobile.scrcpy.Server',
    SCRCPY_VERSION,
    ...Object.entries(extra)
      .filter(([, v]) => v !== undefined && v !== null && v !== '')
      .map(([k, v]) => `${k}=${safeArg(v, k)}`),
  ];
}

export async function pushServer(adb, serial, localPath) {
  await adb.push(serial, localPath, REMOTE_SERVER_PATH);
}

/**
 * Events:
 *  - 'ready'   { deviceName }
 *  - 'session' { width, height }       new capture session (start, rotation, reset)
 *  - 'packet'  { config, key, pts, data }  H.264 Annex B payload
 *  - 'clipboard' text                  device clipboard (after GET_CLIPBOARD)
 *  - 'log'     line                    scrcpy-server output
 *  - 'closed'  Error|null
 */
export class ScrcpySession extends EventEmitter {
  constructor({ adb, serial, serverPath, video }) {
    super();
    this.adb = adb;
    this.serial = serial;
    this.serverPath = serverPath;
    this.video = video;
    this.proc = null;
    this.port = null;
    this.videoSocket = null;
    this.controlSocket = null;
    this.closed = false;
    this.procExited = false;
  }

  async start() {
    try {
      await this.startInternal();
    } catch (err) {
      this.close(err);
      throw err;
    }
  }

  async startInternal() {
    const { adb, serial } = this;
    await pushServer(adb, serial, this.serverPath);

    const scid = randomInt(0, 0x7fffffff);
    const scidHex = scid.toString(16).padStart(8, '0');
    this.port = await getFreePort();
    await adb.forward(serial, this.port, `scrcpy_${scidHex}`);

    const args = buildServerArgs({
      scid: scidHex,
      log_level: 'info',
      tunnel_forward: true,
      audio: false,
      control: true,
      video_codec: 'h264',
      max_size: this.video.maxSize,
      video_bit_rate: this.video.bitRate,
      max_fps: this.video.maxFps,
      video_encoder: this.video.encoder,
      video_codec_options: this.video.codecOptions,
      clipboard_autosync: false,
      cleanup: true,
    });
    this.proc = adb.spawnShell(serial, args);
    const logLines = (data) => {
      for (const line of data.toString().split(/\r?\n/)) if (line.trim()) this.emit('log', line);
    };
    this.proc.stdout.on('data', logLines);
    this.proc.stderr.on('data', logLines);
    this.proc.on('exit', (code) => {
      this.procExited = true;
      if (!this.closed) this.close(new Error(`scrcpy-server exited (code ${code})`));
    });

    // Nothing listens on the device until the server has started, but adb still accepts the local
    // connection; the dummy byte is how we know the tunnel really reached the server.
    const deadline = Date.now() + CONNECT_TIMEOUT_MS;
    let videoReader;
    for (;;) {
      if (this.closed || this.procExited) throw new Error('scrcpy-server stopped before accepting a connection');
      let socket;
      try {
        socket = await tcpConnect(this.port);
        videoReader = new SocketReader(socket);
        await videoReader.read(1);
        this.videoSocket = socket;
        break;
      } catch {
        socket?.destroy();
        if (Date.now() > deadline) throw new Error('Timed out connecting to scrcpy-server');
        await sleep(100);
      }
    }

    this.controlSocket = await tcpConnect(this.port);
    const controlReader = new SocketReader(this.controlSocket);

    const nameBuf = await videoReader.read(DEVICE_NAME_LENGTH);
    const nul = nameBuf.indexOf(0);
    const deviceName = nameBuf.subarray(0, nul === -1 ? nameBuf.length : nul).toString('utf8');

    const codecId = (await videoReader.read(4)).readUInt32BE(0);
    if (codecId === 0) throw new Error('Device disabled the video stream');
    if (codecId === 1) throw new Error('scrcpy-server configuration error (see server log)');
    if (codecId !== CODEC_ID_H264) throw new Error(`Unexpected codec id 0x${codecId.toString(16)}`);

    this.emit('ready', { deviceName });
    this.videoLoop(videoReader).catch((err) => this.close(err));
    this.controlLoop(controlReader).catch((err) => this.close(err));
  }

  async videoLoop(reader) {
    for (;;) {
      const header = await reader.read(12);
      const hi = header.readUInt32BE(0);
      if (hi & 0x80000000) {
        this.emit('session', { width: header.readUInt32BE(4), height: header.readUInt32BE(8) });
        continue;
      }
      const config = (hi & 0x40000000) !== 0;
      const key = (hi & 0x20000000) !== 0;
      const pts = Number(header.readBigUInt64BE(0) & PTS_MASK);
      const size = header.readUInt32BE(8);
      const data = await reader.read(size);
      this.emit('packet', { config, key, pts, data });
    }
  }

  async controlLoop(reader) {
    for (;;) {
      const type = (await reader.read(1))[0];
      if (type === 0) {
        const len = (await reader.read(4)).readUInt32BE(0);
        this.emit('clipboard', (await reader.read(len)).toString('utf8'));
      } else if (type === 1) {
        await reader.read(8); // clipboard ack sequence
      } else if (type === 2) {
        await reader.read(2);
        const len = (await reader.read(2)).readUInt16BE(0);
        await reader.read(len);
      } else {
        throw new Error(`Unknown device message type ${type}`);
      }
    }
  }

  sendControl(buf) {
    if (!this.closed && this.controlSocket) this.controlSocket.write(buf);
  }

  close(err = null) {
    if (this.closed) return;
    this.closed = true;
    this.videoSocket?.destroy();
    this.controlSocket?.destroy();
    if (this.proc && !this.procExited) this.proc.kill();
    if (this.port) this.adb.removeForward(this.serial, this.port).catch(() => {});
    this.emit('closed', err);
  }
}
