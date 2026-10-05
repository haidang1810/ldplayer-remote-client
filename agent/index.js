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

// The relay pings every 5 s; this much silence means the connection is dead (network blip, NAT
// reset) even if TCP has not noticed yet, so drop it and reconnect instead of hanging.
const RELAY_SILENCE_MS = 12000;
let backoff = 1000;

const stamp = () => new Date().toLocaleTimeString('vi-VN', { hour12: false });
const log = (...args) => console.log(stamp(), ...args);
const warn = (...args) => console.warn(stamp(), ...args);

/** Terminates the socket when the relay stops pinging it. */
function watchSilence(ws) {
  let timer = null;
  const alive = () => {
    clearTimeout(timer);
    timer = setTimeout(() => ws.terminate(), RELAY_SILENCE_MS);
  };
  ws.on('open', () => {
    ws._socket.setKeepAlive(true, 5000);
    alive();
  });
  ws.on('ping', alive);
  ws.on('close', () => clearTimeout(timer));
}

function openChannel(id, serial) {
  const channel = new WebSocket(`${relayUrl}/agent/channel?id=${encodeURIComponent(id)}`, wsOptions);
  watchSilence(channel);
  channel.on('open', () => env.hub.attach(serial, channel));
  channel.on('error', (err) => warn(`channel ${id.slice(0, 8)}: ${err.message}`));
}

function connect() {
  const ws = new WebSocket(`${relayUrl}/agent`, wsOptions);
  watchSilence(ws);

  ws.on('open', () => {
    log('connected to relay');
    backoff = 1000;
  });
  ws.on('unexpected-response', (_req, res) => {
    warn(`relay refused the connection: HTTP ${res.statusCode}${res.statusCode === 401 ? ' (wrong AGENT_KEY?)' : ''}`);
    ws.terminate();
  });
  ws.on('error', (err) => warn(`relay connection error: ${err.message}`));
  ws.on('close', (code, reason) => {
    log(`disconnected from relay (code ${code}${reason.length ? `, ${reason}` : ''}), retrying in ${backoff / 1000}s`);
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
