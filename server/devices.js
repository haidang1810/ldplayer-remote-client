// Device discovery and stream options shared by the local server and the relay agent.
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Adb } from './adb.js';
import { Hub } from './hub.js';
import { LdConsole, findLdplayerDir, serialsForIndex } from './ldplayer.js';
import { SCRCPY_VERSION } from './scrcpy-version.js';

export const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
export const SERVER_PATH = join(ROOT, 'vendor', `scrcpy-server-v${SCRCPY_VERSION}`);

/** CLI options (with env fallbacks) for anything that runs scrcpy sessions. */
export const STREAM_CLI_OPTIONS = {
  'ldplayer-dir': { type: 'string', default: process.env.LDPLAYER_DIR },
  adb: { type: 'string', default: process.env.ADB_PATH },
  'max-size': { type: 'string', default: process.env.MAX_SIZE ?? '1280' },
  'bit-rate': { type: 'string', default: process.env.BIT_RATE ?? '8M' },
  'max-fps': { type: 'string', default: process.env.MAX_FPS ?? '60' },
  encoder: { type: 'string', default: process.env.VIDEO_ENCODER },
  // A key frame every 3 s lets a viewer that dropped frames recover without resetting the encoder.
  'codec-options': { type: 'string', default: process.env.CODEC_OPTIONS ?? 'i-frame-interval:int=3' },
  'max-buffered-kb': { type: 'string', default: process.env.MAX_BUFFERED_KB ?? '256' },
};

function parseBitRate(s) {
  const m = /^(\d+(?:\.\d+)?)([kKmM]?)$/.exec(s);
  if (!m) throw new Error(`Invalid bit rate: ${s}`);
  const mult = { '': 1, k: 1e3, K: 1e3, m: 1e6, M: 1e6 }[m[2]];
  return Math.round(Number(m[1]) * mult);
}

/** Locates LDPlayer/adb and returns a Hub plus a device lister. */
export async function createDeviceEnv(opts) {
  const ldDir = opts['ldplayer-dir'] ?? (await findLdplayerDir());
  const ldconsole = ldDir && existsSync(join(ldDir, 'ldconsole.exe')) ? new LdConsole(ldDir) : null;
  // Prefer LDPlayer's bundled adb: two different adb versions keep killing each other's daemon.
  const adbPath = opts.adb ?? (ldDir && existsSync(join(ldDir, 'adb.exe')) ? join(ldDir, 'adb.exe') : 'adb');
  const adb = new Adb(adbPath);

  if (!existsSync(SERVER_PATH)) {
    throw new Error(`Missing ${SERVER_PATH}. Run: npm run fetch-server`);
  }

  const hub = new Hub({
    adb,
    serverPath: SERVER_PATH,
    maxBufferedBytes: Number(opts['max-buffered-kb']) * 1024,
    video: {
      maxSize: Number(opts['max-size']),
      bitRate: parseBitRate(opts['bit-rate']),
      maxFps: Number(opts['max-fps']),
      encoder: opts.encoder,
      codecOptions: opts['codec-options'],
    },
  });

  /** One entry per LDPlayer instance (running or not) plus any other adb device. `id` is the adb serial. */
  async function listDevices() {
    const [instances, adbDevices] = await Promise.all([
      ldconsole ? ldconsole.list().catch(() => []) : [],
      adb.devices().catch(() => []),
    ]);
    const ready = new Set(adbDevices.filter((d) => d.state === 'device').map((d) => d.serial));
    const claimed = new Set();
    const devices = instances.map((inst) => {
      const candidates = serialsForIndex(inst.index);
      candidates.forEach((s) => claimed.add(s));
      const serial = candidates.find((s) => ready.has(s)) ?? null;
      return {
        id: serial,
        name: inst.title,
        index: inst.index,
        running: inst.running,
        adb: Boolean(serial),
      };
    });
    for (const d of adbDevices) {
      if (claimed.has(d.serial)) continue;
      const ok = d.state === 'device';
      devices.push({ id: ok ? d.serial : null, name: d.serial, index: null, running: true, adb: ok });
    }
    return devices;
  }

  return { ldDir, adbPath, adb, hub, listDevices };
}
