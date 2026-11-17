// Replays a viewer's touch events with their original spacing.
//
// scrcpy stamps each MotionEvent with the device clock at injection time, and Android derives
// scroll/fling velocity and touch resampling from those stamps. Over a network (or when a browser
// delivers several coalesced points at once) events arrive in bursts, which Android sees as a
// jerky finger. The browser therefore sends the time each point was captured; we hold events for
// a small adaptive jitter delay and release them on that timeline.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::util::mono_ms;

const ACTION_DOWN: u8 = 0;
const ACTION_UP: u8 = 1;
const MIN_DELAY_MS: f64 = 2.0;
const MAX_DELAY_MS: f64 = 60.0;
const INITIAL_DELAY_MS: f64 = 10.0;
/// When a late burst has pushed the timeline back, play up to this much faster to catch up.
const CATCH_UP_RATIO: f64 = 0.25;

pub type Touch = [u8; 32];
/// Where touch messages go: the current scrcpy session's control socket (None while stopped).
pub type ControlSlot = Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>>;

struct Pacer {
    queue: VecDeque<(f64, Touch)>,
    delay: f64,
    offset: Option<f64>,
    min_transit: f64,
    max_jitter: f64,
    last_client_ts: f64,
    down: HashMap<u64, Touch>,
}

impl Pacer {
    fn new() -> Self {
        Self { queue: VecDeque::new(), delay: INITIAL_DELAY_MS, offset: None, min_transit: f64::INFINITY, max_jitter: 0.0, last_client_ts: 0.0, down: HashMap::new() }
    }

    fn push(&mut self, client_ts: f64, msg: Touch) {
        let now = mono_ms();
        let transit = now - client_ts; // clock offset + network delay; only its variation matters
        let action = msg[1];
        let pointer = u64::from_be_bytes(msg[2..10].try_into().unwrap());

        if self.offset.is_none() || (action == ACTION_DOWN && self.down.is_empty() && self.queue.is_empty()) {
            // A new gesture re-anchors the timeline, so delay from a past spike never carries over.
            self.offset = Some(transit + self.delay);
            self.min_transit = transit;
            self.max_jitter = 0.0;
            self.last_client_ts = client_ts;
        }
        self.min_transit = self.min_transit.min(transit);
        self.max_jitter = self.max_jitter.max(transit - self.min_transit);

        let mut offset = self.offset.unwrap();
        // Drain delay accumulated by late events by slightly compressing later intervals.
        let excess = offset - (self.min_transit + self.delay);
        if excess > 0.0 {
            offset -= excess.min((client_ts - self.last_client_ts).max(0.0) * CATCH_UP_RATIO);
        }
        self.last_client_ts = self.last_client_ts.max(client_ts);

        let mut due = client_ts + offset;
        if due < now {
            // Arrived after its slot: send now and shift the rest of the gesture to keep its spacing.
            offset += now - due;
            due = now;
        }
        self.offset = Some(offset);
        if let Some(&(last, _)) = self.queue.back() {
            due = due.max(last);
        }
        self.queue.push_back((due, msg));

        if action == ACTION_UP {
            self.down.remove(&pointer);
            if self.down.is_empty() {
                let target = (self.max_jitter * 1.25 + 1.0).clamp(MIN_DELAY_MS, MAX_DELAY_MS);
                self.delay = self.delay * 0.6 + target * 0.4;
            }
        } else {
            self.down.insert(pointer, msg);
        }
    }

    fn take_due(&mut self, out: &mut Vec<Touch>) {
        let now = mono_ms();
        while self.queue.front().is_some_and(|&(due, _)| due <= now + 0.5) {
            out.push(self.queue.pop_front().unwrap().1);
        }
    }

    fn release_all(&mut self, out: &mut Vec<Touch>) {
        out.extend(self.queue.drain(..).map(|(_, m)| m));
        for (_, mut m) in self.down.drain() {
            m[1] = ACTION_UP;
            m[22] = 0; // pressure
            m[23] = 0;
            out.push(m);
        }
        self.offset = None;
    }
}

/// Spawns a pacer task for one viewer. Dropping the returned sender releases any finger still down.
pub fn spawn(control: ControlSlot) -> mpsc::UnboundedSender<(f64, Touch)> {
    let (tx, mut rx) = mpsc::unbounded_channel::<(f64, Touch)>();
    tokio::spawn(async move {
        let mut p = Pacer::new();
        let mut out = Vec::new();
        let send = |out: &mut Vec<Touch>| {
            if let Some(ctl) = control.lock().unwrap().as_ref() {
                for m in out.iter() {
                    let _ = ctl.send(m.to_vec());
                }
            }
            out.clear();
        };
        loop {
            let next = p.queue.front().map(|&(due, _)| due);
            let msg = match next {
                Some(due) => {
                    let wait = (due - mono_ms() - 0.5).max(0.0);
                    tokio::select! {
                        m = rx.recv() => m.map(Some),
                        _ = tokio::time::sleep(std::time::Duration::from_secs_f64(wait / 1000.0)) => Some(None),
                    }
                }
                None => rx.recv().await.map(Some),
            };
            match msg {
                Some(Some((ts, touch))) => p.push(ts, touch),
                Some(None) => {}
                None => {
                    p.release_all(&mut out);
                    send(&mut out);
                    return;
                }
            }
            p.take_due(&mut out);
            send(&mut out);
        }
    });
    tx
}
