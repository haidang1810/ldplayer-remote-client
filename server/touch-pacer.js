// Replays a viewer's touch events with their original spacing.
//
// scrcpy stamps each MotionEvent with the device clock at injection time, and Android derives
// scroll/fling velocity and touch resampling from those stamps. Over a network (or when a browser
// delivers several coalesced points at once) events arrive in bursts, which Android sees as a
// jerky finger. The browser therefore sends the time each point was captured; we hold events for
// a small adaptive jitter delay and release them on that timeline.
import { performance } from 'node:perf_hooks';

const ACTION_DOWN = 0;
const ACTION_UP = 1;
const MIN_DELAY_MS = 2;
const MAX_DELAY_MS = 60;
const INITIAL_DELAY_MS = 10;
// When a late burst has pushed the timeline back, play up to this much faster to catch up.
const CATCH_UP_RATIO = 0.25;

export class TouchPacer {
  /** @param {(msg: Buffer) => void} send writes one scrcpy touch message to the device */
  constructor(send) {
    this.send = send;
    this.queue = []; // { due, msg } in release order
    this.timer = null;
    this.delay = INITIAL_DELAY_MS; // current jitter buffer, adapted after each gesture
    this.offset = null; // device-time = client-time + offset
    this.minTransit = Infinity;
    this.maxJitter = 0;
    this.lastClientTs = 0;
    this.down = new Map(); // pointer id → last message (to release it if the viewer vanishes)
  }

  push(clientTs, msg) {
    const now = performance.now();
    const transit = now - clientTs; // clock offset + network delay; only its variation matters
    const action = msg[1];
    const pointer = msg.readBigUInt64BE(2);

    if (this.offset === null || (action === ACTION_DOWN && this.down.size === 0 && this.queue.length === 0)) {
      // A new gesture re-anchors the timeline, so delay from a past spike never carries over.
      this.offset = transit + this.delay;
      this.minTransit = transit;
      this.maxJitter = 0;
      this.lastClientTs = clientTs;
    }
    this.minTransit = Math.min(this.minTransit, transit);
    this.maxJitter = Math.max(this.maxJitter, transit - this.minTransit);

    // Drain delay accumulated by late events by slightly compressing later intervals.
    const excess = this.offset - (this.minTransit + this.delay);
    if (excess > 0) this.offset -= Math.min(excess, Math.max(0, clientTs - this.lastClientTs) * CATCH_UP_RATIO);
    this.lastClientTs = Math.max(this.lastClientTs, clientTs);

    let due = clientTs + this.offset;
    if (due < now) {
      // Arrived after its slot: send now and shift the rest of the gesture to keep its spacing.
      this.offset += now - due;
      due = now;
    }
    const last = this.queue.at(-1);
    if (last && due < last.due) due = last.due;
    this.queue.push({ due, msg });

    if (action === ACTION_UP) {
      this.down.delete(pointer);
      if (this.down.size === 0) {
        const target = Math.min(MAX_DELAY_MS, Math.max(MIN_DELAY_MS, this.maxJitter * 1.25 + 1));
        this.delay = this.delay * 0.6 + target * 0.4;
      }
    } else {
      this.down.set(pointer, msg);
    }
    this.pump();
  }

  pump() {
    clearTimeout(this.timer);
    this.timer = null;
    const now = performance.now();
    while (this.queue.length && this.queue[0].due <= now + 0.5) this.send(this.queue.shift().msg);
    if (this.queue.length) {
      // Node timers fire ~1 ms late on Windows; aim slightly early.
      this.timer = setTimeout(() => this.pump(), Math.max(0, this.queue[0].due - now - 1));
    }
  }

  /** Sends everything queued and lifts any finger still down (viewer disconnected mid-gesture). */
  releaseAll() {
    clearTimeout(this.timer);
    this.timer = null;
    for (const { msg } of this.queue.splice(0)) this.send(msg);
    for (const msg of this.down.values()) {
      const up = Buffer.from(msg);
      up[1] = ACTION_UP;
      up.writeUInt16BE(0, 22); // pressure
      this.send(up);
    }
    this.down.clear();
    this.offset = null;
  }
}
