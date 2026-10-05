// Downloads the pinned scrcpy-server release from GitHub and verifies its SHA-256.
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { SCRCPY_VERSION, SCRCPY_SERVER_SHA256 } from '../server/scrcpy-version.js';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const target = join(root, 'vendor', `scrcpy-server-v${SCRCPY_VERSION}`);
const url = `https://github.com/Genymobile/scrcpy/releases/download/v${SCRCPY_VERSION}/scrcpy-server-v${SCRCPY_VERSION}`;

const sha256 = (buf) => createHash('sha256').update(buf).digest('hex');

if (existsSync(target) && sha256(readFileSync(target)) === SCRCPY_SERVER_SHA256) {
  console.log(`scrcpy-server v${SCRCPY_VERSION} already present`);
  process.exit(0);
}

console.log(`Downloading ${url}`);
const res = await fetch(url);
if (!res.ok) {
  console.error(`Download failed: HTTP ${res.status}`);
  process.exit(1);
}
const data = Buffer.from(await res.arrayBuffer());
const digest = sha256(data);
if (digest !== SCRCPY_SERVER_SHA256) {
  console.error(`SHA-256 mismatch: expected ${SCRCPY_SERVER_SHA256}, got ${digest}`);
  process.exit(1);
}
mkdirSync(dirname(target), { recursive: true });
writeFileSync(target, data);
console.log(`Saved ${target} (${data.length} bytes, sha256 OK)`);
