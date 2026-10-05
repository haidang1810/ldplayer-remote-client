// One DeviceStream per adb serial: a single scrcpy session fanned out to every connected viewer.
import { performance } from 'node:perf_hooks';
import { ScrcpySession, ControlType, isValidControlMessage } from './scrcpy.js';
import { TouchPacer } from './touch-pacer.js';

export const now = () => performance.timeOrigin + performance.now();

const MSG_VIDEO = 1;
// Viewer → agent: [0xF0][f64 capture time ms][32-byte scrcpy touch message], see touch-pacer.js.
const MSG_TIMED_TOUCH = 0xf0;
const TIMED_TOUCH_SIZE = 41;
const VIDEO_HEADER_SIZE = 22;
const IDLE_STOP_MS = 10000;
const KEYFRAME_REQUEST_COOLDOWN_MS = 500;
// Viewers ack every video message. A frame unacked for longer than the path's base RTT plus this
// budget means data is queueing somewhere (relay, proxy, kernel buffers), so we start dropping.
const QUEUE_BUDGET_MS = 250;
const MAX_INFLIGHT_FRAMES = 240;
const RTT_SAMPLES = 120;

/** [u8 type][u8 flags][u32 seq][f64 pts µs][f64 server receive time ms] + Annex B payload. */
function videoMessage(key, seq, pts, receivedAt, payload) {
  const msg = Buffer.allocUnsafe(VIDEO_HEADER_SIZE + payload.length);
  msg.writeUInt8(MSG_VIDEO, 0);
  msg.writeUInt8(key ? 1 : 0, 1);
  msg.writeUInt32LE(seq, 2);
  msg.writeDoubleLE(pts, 6);
  msg.writeDoubleLE(receivedAt, 14);
  payload.copy(msg, VIDEO_HEADER_SIZE);
  return msg;
}

class Viewer {
  constructor(ws) {
    this.ws = ws;
    this.needKey = true;
    this.inflight = []; // { seq, sentAt } in send order
    this.rttSamples = [];
    this.baseRtt = null;
  }

  isOpen() {
    return this.ws.readyState === this.ws.OPEN;
  }

  sendJson(msg) {
    if (this.isOpen()) this.ws.send(JSON.stringify(msg));
  }

  sendFrame(seq, msg) {
    this.inflight.push({ seq, sentAt: now() });
    this.ws.send(msg, { binary: true });
  }

  onAck(seq) {
    const t = now();
    while (this.inflight.length && this.inflight[0].seq <= seq) {
      const frame = this.inflight.shift();
      if (frame.seq === seq) {
        this.rttSamples.push(t - frame.sentAt);
        if (this.rttSamples.length > RTT_SAMPLES) this.rttSamples.shift();
        this.baseRtt = Math.min(...this.rttSamples);
      }
    }
  }

  isCongested(maxBufferedBytes) {
    if (this.ws.bufferedAmount > maxBufferedBytes || this.inflight.length > MAX_INFLIGHT_FRAMES) return true;
    if (!this.inflight.length) return false;
    return now() - this.inflight[0].sentAt > (this.baseRtt ?? 100) + QUEUE_BUDGET_MS;
  }

  close(code, reason) {
    this.ws.close(code, reason);
  }
}

class DeviceStream {
  constructor(hub, serial) {
    this.hub = hub;
    this.serial = serial;
    this.viewers = new Set();
    this.session = null;
    this.state = 'idle'; // idle | starting | running
    this.deviceName = serial;
    this.size = null;
    this.config = null; // latest SPS/PPS, prepended to every key frame we forward
    this.seq = 0;
    this.lastKeyframeRequest = 0;
    this.stopTimer = null;
  }

  log(...args) {
    console.log(`[${this.serial}]`, ...args);
  }

  addViewer(viewer) {
    clearTimeout(this.stopTimer);
    this.stopTimer = null;
    this.viewers.add(viewer);
    if (this.state === 'running') {
      this.sendHello(viewer);
      this.requestKeyframe();
    } else {
      viewer.sendJson({ type: 'status', state: 'starting' });
      if (this.state === 'idle') this.start();
    }
  }

  removeViewer(viewer) {
    viewer.pacer.releaseAll();
    this.viewers.delete(viewer);
    if (this.viewers.size === 0 && !this.stopTimer) {
      this.stopTimer = setTimeout(() => this.stop(), IDLE_STOP_MS);
    }
  }

  sendHello(viewer) {
    viewer.sendJson({
      type: 'hello',
      serial: this.serial,
      deviceName: this.deviceName,
      width: this.size?.width,
      height: this.size?.height,
    });
  }

  async start() {
    this.state = 'starting';
    this.size = null;
    this.config = null;
    const session = new ScrcpySession({
      adb: this.hub.adb,
      serial: this.serial,
      serverPath: this.hub.serverPath,
      video: this.hub.video,
    });
    this.session = session;
    session.on('log', (line) => this.log('scrcpy:', line));
    session.on('ready', ({ deviceName }) => {
      this.deviceName = deviceName;
      this.state = 'running';
      this.log(`streaming "${deviceName}"`);
    });
    session.on('session', (size) => {
      const changed = !this.size || this.size.width !== size.width || this.size.height !== size.height;
      this.size = size;
      if (changed) {
        this.log(`video size ${size.width}x${size.height}`);
        // The first session packet is when viewers learn the size, so greet them then.
        for (const v of this.viewers) this.sendHello(v);
      }
    });
    session.on('packet', (packet) => this.onPacket(packet));
    session.on('clipboard', (text) => {
      for (const v of this.viewers) v.sendJson({ type: 'clipboard', text });
    });
    session.on('closed', (err) => {
      if (this.session !== session) return;
      this.session = null;
      this.state = 'idle';
      if (err) this.log('session closed:', err.message);
      for (const v of this.viewers) {
        v.sendJson({ type: 'status', state: 'stopped', message: err?.message });
        v.close(1011, 'stream stopped');
      }
    });
    try {
      await session.start();
    } catch (err) {
      this.log('failed to start:', err.message);
    }
  }

  stop() {
    this.stopTimer = null;
    if (this.viewers.size > 0) return;
    this.log('no viewers left, stopping');
    this.session?.close();
    this.hub.streams.delete(this.serial);
  }

  requestKeyframe() {
    const t = Date.now();
    if (!this.session || this.state !== 'running' || t - this.lastKeyframeRequest < KEYFRAME_REQUEST_COOLDOWN_MS) return;
    this.lastKeyframeRequest = t;
    this.session.sendControl(Buffer.from([ControlType.RESET_VIDEO]));
  }

  onPacket({ config, key, pts, data }) {
    if (config) {
      this.config = Buffer.from(data);
      return;
    }
    const seq = (this.seq = (this.seq + 1) >>> 0);
    const receivedAt = now();
    let msg = null;
    for (const v of this.viewers) {
      if (!v.isOpen()) continue;
      if (v.needKey && !key) continue;
      if (v.isCongested(this.hub.maxBufferedBytes)) {
        // Viewer cannot keep up: drop until the next key frame instead of queueing latency.
        v.needKey = true;
        this.requestKeyframe();
        continue;
      }
      msg ??= videoMessage(key, seq, pts, receivedAt, key && this.config ? Buffer.concat([this.config, data]) : data);
      if (key) v.needKey = false;
      v.sendFrame(seq, msg);
    }
  }

  handleMessage(viewer, data, isBinary) {
    if (isBinary) {
      if (this.state !== 'running') return;
      if (data[0] === MSG_TIMED_TOUCH) {
        const touch = data.subarray(9);
        if (data.length === TIMED_TOUCH_SIZE && touch[0] === ControlType.INJECT_TOUCH_EVENT) {
          viewer.pacer.push(data.readDoubleBE(1), touch);
        }
      } else if (isValidControlMessage(data)) {
        this.session.sendControl(data);
      }
      return;
    }
    let msg;
    try {
      msg = JSON.parse(data.toString());
    } catch {
      return;
    }
    if (msg.type === 'ack') {
      viewer.onAck(msg.seq >>> 0);
    } else if (msg.type === 'ping') {
      viewer.sendJson({ type: 'pong', t: msg.t, serverTime: now() });
    } else if (msg.type === 'keyframe') {
      viewer.needKey = true;
      this.requestKeyframe();
    }
  }
}

export class Hub {
  constructor({ adb, serverPath, video, maxBufferedBytes }) {
    this.adb = adb;
    this.serverPath = serverPath;
    this.video = video;
    this.maxBufferedBytes = maxBufferedBytes;
    this.streams = new Map();
  }

  /** Attaches a viewer WebSocket (a direct browser socket, or a relay channel) to a device stream. */
  attach(serial, ws) {
    let stream = this.streams.get(serial);
    if (!stream) {
      stream = new DeviceStream(this, serial);
      this.streams.set(serial, stream);
    }
    const viewer = new Viewer(ws);
    viewer.pacer = new TouchPacer((msg) => stream.session?.sendControl(msg));
    ws.on('message', (data, isBinary) => stream.handleMessage(viewer, data, isBinary));
    ws.on('close', () => stream.removeViewer(viewer));
    ws.on('error', (err) => stream.log('websocket error:', err.message));
    stream.addViewer(viewer);
  }

  shutdown() {
    for (const stream of this.streams.values()) stream.session?.close();
    this.streams.clear();
  }
}
