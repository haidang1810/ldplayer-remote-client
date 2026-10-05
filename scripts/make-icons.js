// Generates the app icons (PNG, no dependencies): a phone outline on the app background.
// Usage: node scripts/make-icons.js   → public/icon-180.png, icon-192.png, icon-512.png
import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { crc32, deflateSync } from 'node:zlib';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const BG = [0x0f, 0x11, 0x15];
const ACCENT = [0x34, 0xd3, 0x99];
const SCREEN = [0x17, 0x1a, 0x21];

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

function insideRoundRect(x, y, l, t, r, b, rad) {
  if (x < l || x > r || y < t || y > b) return false;
  const cx = Math.min(Math.max(x, l + rad), r - rad);
  const cy = Math.min(Math.max(y, t + rad), b - rad);
  return (x - cx) ** 2 + (y - cy) ** 2 <= rad ** 2;
}

function icon(size) {
  const s = size / 100; // design on a 100-unit grid
  const rows = [];
  for (let y = 0; y < size; y++) {
    const row = Buffer.alloc(1 + size * 3); // filter byte 0 + RGB
    for (let x = 0; x < size; x++) {
      const u = (x + 0.5) / s;
      const v = (y + 0.5) / s;
      let c = BG;
      if (insideRoundRect(u, v, 30, 14, 70, 86, 8)) c = ACCENT; // phone body
      if (insideRoundRect(u, v, 34, 20, 66, 74, 3)) c = SCREEN; // screen
      if ((u - 50) ** 2 + (v - 80) ** 2 <= 3.2 ** 2) c = SCREEN; // home button
      row[1 + x * 3] = c[0];
      row[2 + x * 3] = c[1];
      row[3 + x * 3] = c[2];
    }
    rows.push(row);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 2; // truecolor RGB
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(Buffer.concat(rows), { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

for (const size of [180, 192, 512]) {
  const file = join(root, 'public', `icon-${size}.png`);
  writeFileSync(file, icon(size));
  console.log(`wrote ${file}`);
}
