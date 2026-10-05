import { execFile, spawn } from 'node:child_process';

export class Adb {
  constructor(adbPath) {
    this.adbPath = adbPath;
  }

  exec(args, { timeout = 20000 } = {}) {
    return new Promise((resolve, reject) => {
      execFile(this.adbPath, args, { timeout, windowsHide: true, maxBuffer: 16 * 1024 * 1024 }, (err, stdout, stderr) => {
        if (err) {
          err.message = `adb ${args.join(' ')} failed: ${(stderr || stdout || err.message).trim()}`;
          reject(err);
        } else {
          resolve(stdout);
        }
      });
    });
  }

  /** @returns {Promise<{serial: string, state: string}[]>} */
  async devices() {
    const out = await this.exec(['devices']);
    return out
      .split(/\r?\n/)
      .slice(1)
      .map((line) => line.trim().split(/\s+/))
      .filter((parts) => parts.length >= 2)
      .map(([serial, state]) => ({ serial, state }));
  }

  push(serial, local, remote) {
    return this.exec(['-s', serial, 'push', local, remote], { timeout: 60000 });
  }

  forward(serial, localPort, abstractName) {
    return this.exec(['-s', serial, 'forward', `tcp:${localPort}`, `localabstract:${abstractName}`]);
  }

  removeForward(serial, localPort) {
    return this.exec(['-s', serial, 'forward', '--remove', `tcp:${localPort}`]);
  }

  /** Long-running `adb shell` process; the caller owns its lifetime. */
  spawnShell(serial, args) {
    return spawn(this.adbPath, ['-s', serial, 'shell', ...args], { windowsHide: true });
  }

  shell(serial, args, opts) {
    return this.exec(['-s', serial, 'shell', ...args], opts);
  }
}
