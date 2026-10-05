// Agent mode (runs on the LDPlayer PC): keeps an outbound connection to the relay, so the PC is
// reachable from anywhere without opening ports. For each viewer the relay asks us to dial back a
// channel socket, which is attached to the hub exactly like a local browser socket.
import { hostname } from 'node:os';
import { parseArgs } from 'node:util';
import WebSocket from 'ws';
import { STREAM_CLI_OPTIONS, createDeviceEnv } from '../server/devices.js';

const { values: opts } = parseArgs({
  options: {
    ...STREAM_CLI_OPTIONS,
    relay: { type: 'string', default: process.env.RELAY_URL },
    key: { type: 'string', default: process.env.AGENT_KEY },
    name: { type: 'string', default: process.env.AGENT_NAME ?? hostname() },
  },
});

if (!opts.relay || !opts.key) {
  console.error('Set RELAY_URL and AGENT_KEY (in agent.env or as --relay / --key).');
  process.exit(1);
}

const relayUrl = opts.relay.replace(/^http/, 'ws').replace(/\/+$/, '');
const name = opts.name.replace(/[^\w.-]/g, '').slice(0, 40);
const headers = { Authorization: `Bearer ${opts.key}`, 'X-Agent-Name': name };
const wsOptions = { headers, perMessageDeflate: false, handshakeTimeout: 10000 };

const env = await createDeviceEnv(opts);
console.log(`LDPlayer dir : ${env.ldDir ?? '(not found)'}`);
console.log(`adb          : ${env.adbPath}`);
console.log(`stream       : ${JSON.stringify(env.hub.video)}`);
console.log(`relay        : ${relayUrl} as "${name}"`);

// The relay pings every 15 s; silence for longer means the connection is dead (sleep, NAT reset).
const RELAY_SILENCE_MS = 40000;
let backoff = 1000;

function openChannel(id, serial) {
  const channel = new WebSocket(`${relayUrl}/agent/channel?id=${encodeURIComponent(id)}`, wsOptions);
  channel.on('open', () => env.hub.attach(serial, channel));
  channel.on('error', (err) => console.warn(`channel ${id.slice(0, 8)}: ${err.message}`));
}

function connect() {
  const ws = new WebSocket(`${relayUrl}/agent`, wsOptions);
  let silenceTimer = null;
  const alive = () => {
    clearTimeout(silenceTimer);
    silenceTimer = setTimeout(() => ws.terminate(), RELAY_SILENCE_MS);
  };

  ws.on('open', () => {
    console.log('connected to relay');
    backoff = 1000;
    alive();
  });
  ws.on('ping', alive);
  ws.on('unexpected-response', (_req, res) => {
    console.error(`relay refused the connection: HTTP ${res.statusCode}${res.statusCode === 401 ? ' (wrong AGENT_KEY?)' : ''}`);
    ws.terminate();
  });
  ws.on('error', (err) => console.warn(`relay connection error: ${err.message}`));
  ws.on('close', () => {
    clearTimeout(silenceTimer);
    console.log(`disconnected from relay, retrying in ${backoff / 1000}s`);
    setTimeout(connect, backoff);
    backoff = Math.min(backoff * 2, 30000);
  });

  ws.on('message', async (data, isBinary) => {
    if (isBinary) return;
    let msg;
    try {
      msg = JSON.parse(data.toString());
    } catch {
      return;
    }
    if (msg.type === 'devices') {
      const devices = await env.listDevices().catch(() => []);
      ws.send(JSON.stringify({ type: 'devices', reqId: msg.reqId, devices }));
    } else if (msg.type === 'open' && typeof msg.channel === 'string' && typeof msg.serial === 'string') {
      openChannel(msg.channel, msg.serial);
    }
  });
}

connect();

function shutdown() {
  env.hub.shutdown();
  setTimeout(() => process.exit(0), 500);
}
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
