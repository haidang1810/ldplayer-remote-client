import { execFile } from 'node:child_process';
import { existsSync } from 'node:fs';
import { join } from 'node:path';

const CANDIDATE_DIRS = ['C', 'D', 'E', 'F'].flatMap((drive) =>
  ['LDPlayer14', 'LDPlayer9', 'LDPlayer4.0', 'LDPlayer64', 'LDPlayer'].flatMap((name) => [
    `${drive}:\\LDPlayer\\${name}`,
    `${drive}:\\${name}`,
    `${drive}:\\Program Files\\LDPlayer\\${name}`,
  ]),
);

function runningPlayerDir() {
  return new Promise((resolve) => {
    execFile(
      'powershell.exe',
      ['-NoProfile', '-Command', '(Get-Process dnplayer -ErrorAction SilentlyContinue | Select-Object -First 1).Path'],
      { windowsHide: true, timeout: 10000 },
      (err, stdout) => {
        const exe = !err && stdout.trim();
        resolve(exe ? exe.replace(/\\[^\\]+$/, '') : null);
      },
    );
  });
}

/** Locates the LDPlayer install folder (the one containing ldconsole.exe and adb.exe). */
export async function findLdplayerDir() {
  if (process.platform !== 'win32') return null;
  const fromProcess = await runningPlayerDir();
  if (fromProcess && existsSync(join(fromProcess, 'ldconsole.exe'))) return fromProcess;
  return CANDIDATE_DIRS.find((dir) => existsSync(join(dir, 'ldconsole.exe'))) ?? null;
}

export class LdConsole {
  constructor(dir) {
    this.exe = join(dir, 'ldconsole.exe');
  }

  /**
   * `ldconsole list2` prints one CSV line per instance:
   * index,title,topHwnd,bindHwnd,isRunning,pid,vboxPid,width,height,dpi
   */
  list() {
    return new Promise((resolve, reject) => {
      execFile(this.exe, ['list2'], { windowsHide: true, timeout: 10000 }, (err, stdout) => {
        if (err) return reject(err);
        const instances = stdout
          .split(/\r?\n/)
          .map((line) => line.trim())
          .filter(Boolean)
          .map((line) => {
            const f = line.split(',');
            return {
              index: Number(f[0]),
              title: f[1],
              running: f[4] === '1',
              width: Number(f[7]),
              height: Number(f[8]),
              dpi: Number(f[9]),
            };
          })
          .filter((inst) => Number.isInteger(inst.index));
        resolve(instances);
      });
    });
  }
}

/** LDPlayer instance N exposes adb as emulator-(5554+2N), or 127.0.0.1:(5555+2N) after `adb connect`. */
export function serialsForIndex(index) {
  return [`emulator-${5554 + 2 * index}`, `127.0.0.1:${5555 + 2 * index}`];
}
