// Local mode: serves the web UI and streams directly from this PC (LAN / localhost / Tailscale).
import { createServer as createHttpServer } from 'node:http';
import { createServer as createHttpsServer } from 'node:https';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { networkInterfaces } from 'node:os';
import { join } from 'node:path';
import { parseArgs } from 'node:util';
import { WebSocketServer } from 'ws';
import { Auth, deriveKey, safeEqual, sendJson } from './auth.js';
import { ROOT, SERVER_PATH, STREAM_CLI_OPTIONS, createDeviceEnv } from './devices.js';
import { buildServerArgs, pushServer } from './scrcpy.js';
import { serveStatic } from './static.js';
import { SCRCPY_VERSION } from './scrcpy-version.js';

const PUBLIC_DIR = join(ROOT, 'public');

const { values: opts } = parseArgs({
  options: {
    ...STREAM_CLI_OPTIONS,
    port: { type: 'string', default: process.env.PORT ?? '8080' },
    host: { type: 'string', default: process.env.HOST ?? '0.0.0.0' },
    https: { type: 'boolean', default: process.env.HTTPS === '1' },
    token: { type: 'string', default: process.env.LDR_TOKEN },
    'list-encoders': { type: 'boolean', default: false },
    serial: { type: 'string' },
  },
});

const env = await createDeviceEnv(opts);
const { hub, listDevices } = env;

if (opts['list-encoders']) {
  const serial = opts.serial ?? (await listDevices()).find((d) => d.adb)?.id;
  if (!serial) {
    console.error('No adb device found. Enable ADB debugging in LDPlayer settings first.');
    process.exit(1);
  }
  await pushServer(env.adb, serial, SERVER_PATH);
  console.log(await env.adb.shell(serial, buildServerArgs({ list_encoders: true, log_level: 'info' })));
  process.exit(0);
}

function loadToken() {
  if (opts.token) return opts.token;
  const file = join(ROOT, '.token');
  if (existsSync(file)) return readFileSync(file, 'utf8').trim();
  const token = randomBytes(12).toString('base64url');
  writeFileSync(file, token);
  return token;
}
const TOKEN = loadToken();
// The access token doubles as the password; rotating it (delete .token) logs everyone out.
const auth = new Auth({ verify: (pw) => safeEqual(pw, TOKEN), key: deriveKey(TOKEN) });

function lanAddresses() {
  return Object.values(networkInterfaces())
    .flat()
    .filter((i) => i && i.family === 'IPv4' && !i.internal)
    .map((i) => i.address);
}

async function loadCertificate() {
  const dir = join(ROOT, 'certs');
  const certFile = join(dir, 'cert.pem');
  const keyFile = join(dir, 'key.pem');
  if (existsSync(certFile) && existsSync(keyFile)) {
    return { cert: readFileSync(certFile), key: readFileSync(keyFile) };
  }
  const { default: selfsigned } = await import('selfsigned');
  const notAfter = new Date();
  notAfter.setFullYear(notAfter.getFullYear() + 5);
  const pems = await selfsigned.generate([{ name: 'commonName', value: 'ldplayer-remote' }], {
    keyType: 'ec',
    algorithm: 'sha256',
    notAfterDate: notAfter,
    extensions: [
      {
        name: 'subjectAltName',
        altNames: [
          { type: 2, value: 'localhost' },
          { type: 7, ip: '127.0.0.1' },
          ...lanAddresses().map((ip) => ({ type: 7, ip })),
        ],
      },
    ],
  });
  mkdirSync(dir, { recursive: true });
  writeFileSync(certFile, pems.cert);
  writeFileSync(keyFile, pems.private);
  return { cert: pems.cert, key: pems.private };
}

async function handleRequest(req, res) {
  const { pathname } = new URL(req.url, 'http://localhost');
  if (pathname === '/api/login' && req.method === 'POST') return auth.handleLogin(req, res);
  if (pathname === '/api/logout' && req.method === 'POST') return auth.handleLogout(req, res);
  if (pathname.startsWith('/api/')) {
    if (!auth.isAuthenticated(req)) return sendJson(res, 401, { error: 'unauthorized' });
    if (pathname === '/api/session') return sendJson(res, 200, { ok: true });
    if (pathname === '/api/devices' && req.method === 'GET') {
      return sendJson(res, 200, { devices: await listDevices() });
    }
    return sendJson(res, 404, { error: 'not found' });
  }
  return serveStatic(req, res, PUBLIC_DIR);
}

const onRequest = (req, res) =>
  handleRequest(req, res).catch((err) => {
    console.error(err);
    if (!res.headersSent) sendJson(res, 500, { error: err.message });
  });
const server = opts.https ? createHttpsServer(await loadCertificate(), onRequest) : createHttpServer(onRequest);

const wss = new WebSocketServer({ noServer: true, perMessageDeflate: false, maxPayload: 1 << 19 });
server.on('upgrade', (req, socket, head) => {
  const url = new URL(req.url, 'http://localhost');
  const device = url.searchParams.get('device');
  if (url.pathname !== '/ws' || !device || !auth.isAuthenticated(req) || !auth.isSameOrigin(req)) {
    socket.end('HTTP/1.1 401 Unauthorized\r\n\r\n');
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => hub.attach(device, ws));
});

const port = Number(opts.port);
server.listen(port, opts.host, () => {
  const scheme = opts.https ? 'https' : 'http';
  console.log(`LDPlayer dir : ${env.ldDir ?? '(not found)'}`);
  console.log(`adb          : ${env.adbPath}`);
  console.log(`scrcpy       : v${SCRCPY_VERSION}, ${JSON.stringify(hub.video)}`);
  console.log(`Token        : ${TOKEN}`);
  console.log('Open:');
  console.log(`  ${scheme}://localhost:${port}/?token=${TOKEN}`);
  if (opts.host === '0.0.0.0') {
    for (const ip of lanAddresses()) console.log(`  ${scheme}://${ip}:${port}/?token=${TOKEN}`);
    if (!opts.https) console.log('  (LAN access needs --https: browsers only enable WebCodecs on https or localhost)');
  }
});

function shutdown() {
  hub.shutdown();
  // Give the `adb forward --remove` calls a moment to run.
  setTimeout(() => process.exit(0), 500);
}
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
