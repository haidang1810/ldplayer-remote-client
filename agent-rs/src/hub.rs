// One stream actor per adb serial: a single scrcpy session fanned out to every connected viewer.
// Viewers receive video over their WebSocket (a relay channel); WebRTC is added in rtc.rs.
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::config::Shared;
use crate::ldplayer::LdEnv;
use crate::pacer::{self, ControlSlot, Touch};
use crate::rtc::{self, RtcCmd, RtcEvent, RtcPeer};
use crate::scrcpy::{self, control, Event, Session, VideoOpts};
use crate::util::{epoch_ms, mono_ms};

const MSG_VIDEO: u8 = 1;
/// Viewer → agent: [0xF0][f64 capture time ms][32-byte scrcpy touch message].
const MSG_TIMED_TOUCH: u8 = 0xf0;
const TIMED_TOUCH_SIZE: usize = 41;
const IDLE_STOP: Duration = Duration::from_secs(10);
/// RESET_VIDEO restarts capture + encoder and yields a large key frame; rate-limit it hard.
const KEYFRAME_REQUEST_COOLDOWN: Duration = Duration::from_millis(1500);
/// The encoder emits a key frame every 3 s; only ask for a reset if none came for this long.
const KEYFRAME_STALE: Duration = Duration::from_secs(4);
/// A frame unacked for longer than the base RTT plus this budget means data is queueing: drop.
const QUEUE_BUDGET_MS: f64 = 250.0;
const MAX_INFLIGHT_FRAMES: usize = 240;
const RTT_SAMPLES: usize = 120;
const DIAG_MAX_PER_MINUTE: u32 = 30;
/// Give WebRTC this long to connect before the viewer stays on the WebSocket path.
const RTC_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DIAG_NUMBERS: [&str; 10] = ["ms", "keyframes", "fps", "mbps", "lost", "lossPct", "rtt", "buffer", "freezes", "freezeMs"];
const DIAG_STRINGS: [&str; 3] = ["transport", "path", "reason"];

enum Cmd {
    Add(ViewerConn),
    Msg(u64, Message),
    Remove(u64),
    Shutdown,
}

/// What the stream actor needs to talk to one viewer.
struct ViewerConn {
    id: u64,
    out: mpsc::UnboundedSender<Message>,
    queued: Arc<AtomicUsize>,
    ice: Value,
}

struct Viewer {
    conn: ViewerConn,
    /// ICE servers in browser format, from the relay.
    ice: Value,
    need_key: bool,
    paused: bool,
    /// WebRTC peer while negotiating or active. Video goes over the WebSocket until the browser
    /// reports a rendered RTP frame (rtc-ready), then only over RTP.
    rtc: Option<RtcPeer>,
    rtc_gen: u64,
    rtc_sending: bool,
    rtc_need_key: bool,
    rtc_active: bool,
    rtc_deadline: Option<Instant>,
    inflight: VecDeque<(u32, f64)>,
    rtt_samples: VecDeque<f64>,
    base_rtt: Option<f64>,
    pacer: mpsc::UnboundedSender<(f64, Touch)>,
    diag_window: Instant,
    diag_count: u32,
}

impl Viewer {
    fn send(&self, msg: Message) {
        let len = match &msg {
            Message::Binary(b) => b.len(),
            Message::Text(t) => t.len(),
            _ => 0,
        };
        self.conn.queued.fetch_add(len, Ordering::Relaxed);
        let _ = self.conn.out.send(msg);
    }

    fn send_json(&self, v: Value) {
        self.send(Message::Text(v.to_string().into()));
    }

    fn close(&self, code: u16, reason: &str) {
        let _ = self.conn.out.send(Message::Close(Some(CloseFrame { code: CloseCode::from(code), reason: reason.to_string().into() })));
    }

    fn on_ack(&mut self, seq: u32) {
        let t = mono_ms();
        while let Some(&(s, sent)) = self.inflight.front() {
            if s > seq {
                break;
            }
            self.inflight.pop_front();
            if s == seq {
                self.rtt_samples.push_back(t - sent);
                if self.rtt_samples.len() > RTT_SAMPLES {
                    self.rtt_samples.pop_front();
                }
                self.base_rtt = self.rtt_samples.iter().copied().reduce(f64::min);
            }
        }
    }

    fn is_congested(&self, max_buffered: usize) -> bool {
        if self.conn.queued.load(Ordering::Relaxed) > max_buffered || self.inflight.len() > MAX_INFLIGHT_FRAMES {
            return true;
        }
        match self.inflight.front() {
            Some(&(_, sent)) => mono_ms() - sent > self.base_rtt.unwrap_or(100.0) + QUEUE_BUDGET_MS,
            None => false,
        }
    }
}

#[derive(PartialEq)]
enum State {
    Idle,
    Starting,
    Running,
}

struct Stream {
    hub: Arc<Hub>,
    serial: String,
    viewers: HashMap<u64, Viewer>,
    session: Option<Session>,
    generation: u64,
    state: State,
    device_name: String,
    size: Option<(u32, u32)>,
    config: Option<Bytes>,
    seq: u32,
    last_keyframe_request: Option<Instant>,
    last_keyframe_at: Instant,
    idle_since: Option<Instant>,
    control: ControlSlot,
    itx: Option<mpsc::UnboundedSender<Internal>>,
}

enum Internal {
    Started(u64, anyhow::Result<(Session, String)>),
    Event(u64, Event),
    /// (viewer id, that viewer's rtc generation, event)
    Rtc(u64, u64, RtcEvent),
}

impl Stream {
    fn log(&self, msg: impl std::fmt::Display) {
        tracing::info!("[{}] {msg}", self.serial);
    }

    async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
        let (itx, mut irx) = mpsc::unbounded_channel::<Internal>();
        self.itx = Some(itx.clone());
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            let idle_deadline = self.idle_since.map(|t| tokio::time::Instant::from_std(t + IDLE_STOP));
            tokio::select! {
                _ = tick.tick() => self.check_rtc_timeouts(),
                cmd = cmds.recv() => match cmd {
                    Some(Cmd::Add(conn)) => self.add_viewer(conn, &itx),
                    Some(Cmd::Msg(id, m)) => self.handle_message(id, m),
                    Some(Cmd::Remove(id)) => self.remove_viewer(id),
                    Some(Cmd::Shutdown) | None => {
                        self.stop_session();
                        return;
                    }
                },
                Some(ev) = irx.recv() => self.on_internal(ev),
                _ = async { tokio::time::sleep_until(idle_deadline.unwrap()).await }, if idle_deadline.is_some() => {
                    self.idle_since = None;
                    if self.viewers.is_empty() && self.state != State::Idle {
                        self.log("no viewers left, stopping");
                        self.stop_session();
                    }
                }
            }
        }
    }

    fn add_viewer(&mut self, conn: ViewerConn, itx: &mpsc::UnboundedSender<Internal>) {
        self.idle_since = None;
        let ice = conn.ice.clone();
        let viewer = Viewer {
            conn,
            ice,
            need_key: true,
            paused: false,
            rtc: None,
            rtc_gen: 0,
            rtc_sending: false,
            rtc_need_key: true,
            rtc_active: false,
            rtc_deadline: None,
            inflight: VecDeque::new(),
            rtt_samples: VecDeque::new(),
            base_rtt: None,
            pacer: pacer::spawn(self.control.clone()),
            diag_window: Instant::now(),
            diag_count: 0,
        };
        let id = viewer.conn.id;
        self.viewers.insert(id, viewer);
        self.hub.shared.viewers.fetch_add(1, Ordering::Relaxed);
        if self.state == State::Running {
            self.send_hello(id);
            self.request_keyframe("viewer joined");
        } else {
            self.viewers[&id].send_json(json!({ "type": "status", "state": "starting" }));
            if self.state == State::Idle {
                self.start(itx.clone());
            }
        }
    }

    fn remove_viewer(&mut self, id: u64) {
        if self.viewers.remove(&id).is_some() {
            self.hub.shared.viewers.fetch_sub(1, Ordering::Relaxed);
        }
        if self.viewers.is_empty() && self.idle_since.is_none() {
            self.idle_since = Some(Instant::now());
        }
    }

    fn send_hello(&self, id: u64) {
        if let Some(v) = self.viewers.get(&id) {
            v.send_json(json!({
                "type": "hello",
                "serial": self.serial,
                "deviceName": self.device_name,
                "width": self.size.map(|s| s.0),
                "height": self.size.map(|s| s.1),
                "rtc": { "iceServers": v.ice },
            }));
        }
    }

    fn start(&mut self, itx: mpsc::UnboundedSender<Internal>) {
        self.state = State::Starting;
        self.size = None;
        self.config = None;
        self.generation += 1;
        let generation = self.generation;
        let hub = self.hub.clone();
        let serial = self.serial.clone();
        tokio::spawn(async move {
            let (etx, mut erx) = mpsc::unbounded_channel::<Event>();
            let adb = hub.ld.adb().await;
            let result = Session::start(&adb, &serial, &hub.jar, &hub.video, etx).await;
            let ok = result.is_ok();
            let _ = itx.send(Internal::Started(generation, result));
            if ok {
                while let Some(ev) = erx.recv().await {
                    if itx.send(Internal::Event(generation, ev)).is_err() {
                        break;
                    }
                }
            }
        });
    }

    fn stop_session(&mut self) {
        self.session = None;
        *self.control.lock().unwrap() = None;
        self.state = State::Idle;
        self.generation += 1; // ignore whatever the old session still reports
    }

    fn on_internal(&mut self, ev: Internal) {
        match ev {
            Internal::Rtc(id, g, ev) => self.on_rtc(id, g, ev),
            Internal::Started(g, _) | Internal::Event(g, _) if g != self.generation => {}
            Internal::Started(_, Ok((session, name))) => {
                *self.control.lock().unwrap() = Some(session.control_sender());
                self.session = Some(session);
                self.device_name = name;
                self.state = State::Running;
                self.last_keyframe_at = Instant::now();
                self.log(format!("streaming \"{}\"", self.device_name));
                if self.viewers.is_empty() {
                    // everyone left while it was starting
                    self.idle_since.get_or_insert(Instant::now());
                }
            }
            Internal::Started(_, Err(e)) => {
                self.log(format!("failed to start: {e:#}"));
                self.fail(&format!("{e:#}"));
            }
            Internal::Event(_, Event::Size { width, height }) => {
                let changed = self.size != Some((width, height));
                self.size = Some((width, height));
                if changed {
                    self.log(format!("video size {width}x{height}"));
                    let ids: Vec<u64> = self.viewers.keys().copied().collect();
                    for id in ids {
                        self.send_hello(id);
                    }
                }
            }
            Internal::Event(_, Event::Packet { config, key, pts, data }) => self.on_packet(config, key, pts, data),
            Internal::Event(_, Event::Clipboard(text)) => {
                for v in self.viewers.values() {
                    v.send_json(json!({ "type": "clipboard", "text": text }));
                }
            }
            Internal::Event(_, Event::Closed(err)) => {
                let msg = err.unwrap_or_else(|| "stream stopped".into());
                self.log(format!("session closed: {msg}"));
                self.fail(&msg);
            }
        }
    }

    fn fail(&mut self, message: &str) {
        self.stop_session();
        for v in self.viewers.values() {
            v.send_json(json!({ "type": "status", "state": "stopped", "message": message }));
            v.close(1011, "stream stopped");
        }
    }

    fn request_keyframe(&mut self, reason: &str) {
        if self.state != State::Running || self.last_keyframe_request.is_some_and(|t| t.elapsed() < KEYFRAME_REQUEST_COOLDOWN) {
            return;
        }
        let Some(ctl) = self.control.lock().unwrap().clone() else { return };
        self.last_keyframe_request = Some(Instant::now());
        self.log(format!("key frame reset ({reason})"));
        let _ = ctl.send(vec![control::RESET_VIDEO]);
    }

    fn on_packet(&mut self, config: bool, key: bool, pts: u64, data: Bytes) {
        if config {
            self.config = Some(data);
            return;
        }
        if key {
            self.last_keyframe_at = Instant::now();
        }
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let received_at = epoch_ms();
        let mut msg: Option<Bytes> = None;
        let mut rtc_payload: Option<Arc<[u8]>> = None;
        let mut waiting = false;
        let max_buffered = self.hub.max_buffered;
        for v in self.viewers.values_mut() {
            if v.rtc_sending {
                // UDP: loss is repaired by NACK/key frames instead of queueing, so no congestion gate.
                if key {
                    v.rtc_need_key = false;
                }
                if v.rtc_need_key {
                    waiting = true;
                } else if let Some(peer) = &v.rtc {
                    let payload = rtc_payload.get_or_insert_with(|| match (key, &self.config) {
                        (true, Some(cfg)) => [cfg.as_ref(), data.as_ref()].concat().into(),
                        _ => Arc::from(data.as_ref()),
                    });
                    peer.send(RtcCmd::Frame(payload.clone(), pts));
                }
            }
            if v.rtc_active || v.paused {
                continue;
            }
            if v.need_key && !key {
                waiting = true;
                continue;
            }
            if v.is_congested(max_buffered) {
                // Viewer cannot keep up: drop until the next (periodic) key frame instead of queueing.
                v.need_key = true;
                waiting = true;
                continue;
            }
            let m = msg.get_or_insert_with(|| {
                let prefix = if key { self.config.as_deref() } else { None };
                let payload_len = data.len() + prefix.map_or(0, <[u8]>::len);
                let mut buf = Vec::with_capacity(22 + payload_len);
                buf.push(MSG_VIDEO);
                buf.push(key as u8);
                buf.extend_from_slice(&seq.to_le_bytes());
                buf.extend_from_slice(&(pts as f64).to_le_bytes());
                buf.extend_from_slice(&received_at.to_le_bytes());
                if let Some(p) = prefix {
                    buf.extend_from_slice(p);
                }
                buf.extend_from_slice(&data);
                Bytes::from(buf)
            });
            if key {
                v.need_key = false;
            }
            v.inflight.push_back((seq, mono_ms()));
            v.send(Message::Binary(m.clone()));
        }
        if waiting && self.last_keyframe_at.elapsed() > KEYFRAME_STALE {
            self.request_keyframe("stale key frame");
        }
    }

    fn log_diag(&mut self, id: u64, msg: &Value) {
        let Some(v) = self.viewers.get_mut(&id) else { return };
        if v.diag_window.elapsed() > Duration::from_secs(60) {
            v.diag_window = Instant::now();
            v.diag_count = 0;
        }
        v.diag_count += 1;
        if v.diag_count > DIAG_MAX_PER_MINUTE {
            return;
        }
        let mut parts = Vec::new();
        for k in DIAG_NUMBERS {
            if let Some(n) = msg.get(k).and_then(Value::as_f64).filter(|n| n.is_finite()) {
                parts.push(format!("{k}={}", (n * 10.0).round() / 10.0));
            }
        }
        for k in DIAG_STRINGS {
            if let Some(s) = msg.get(k).and_then(Value::as_str) {
                let clean: String = s.chars().filter(|c| c.is_alphanumeric() || " .:()-".contains(*c)).take(80).collect();
                parts.push(format!("{k}={clean}"));
            }
        }
        let event: String = msg.get("event").and_then(Value::as_str).unwrap_or("?").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(24).collect();
        self.log(format!("viewer {event}: {}", parts.join(" ")));
    }

    fn start_rtc(&mut self, id: u64) {
        let Some(itx) = self.itx.clone() else { return };
        let Some(v) = self.viewers.get_mut(&id) else { return };
        if v.rtc.is_some() {
            return;
        }
        v.rtc_gen += 1;
        let generation = v.rtc_gen;
        let stun = rtc::stun_servers(&v.ice);
        v.rtc = Some(rtc::start(stun, move |ev| {
            let _ = itx.send(Internal::Rtc(id, generation, ev));
        }));
        v.rtc_deadline = Some(Instant::now() + RTC_CONNECT_TIMEOUT);
    }

    fn on_rtc(&mut self, id: u64, generation: u64, ev: RtcEvent) {
        let Some(v) = self.viewers.get_mut(&id).filter(|v| v.rtc.is_some() && v.rtc_gen == generation) else { return };
        match ev {
            RtcEvent::Offer(sdp) => v.send_json(json!({ "type": "rtc-offer", "sdp": sdp, "sdpType": "offer" })),
            RtcEvent::Connected(path) => {
                v.rtc_sending = true;
                v.rtc_need_key = true;
                self.log(format!("webrtc connected ({path})"));
                self.request_keyframe("webrtc opened");
            }
            RtcEvent::KeyframeRequest => {
                v.rtc_need_key = true;
                self.request_keyframe("webrtc PLI");
            }
            RtcEvent::Data(binary, data) => {
                let msg = if binary { Message::Binary(data.into()) } else { Message::Text(String::from_utf8_lossy(&data).into_owned().into()) };
                self.handle_message(id, msg);
            }
            RtcEvent::Failed(reason) => self.fail_rtc(id, &reason),
        }
    }

    /// Drops WebRTC for this viewer and resumes sending video over its WebSocket.
    fn fail_rtc(&mut self, id: u64, reason: &str) {
        let Some(v) = self.viewers.get_mut(&id).filter(|v| v.rtc.is_some()) else { return };
        v.rtc = None;
        v.rtc_sending = false;
        v.rtc_active = false;
        v.rtc_deadline = None;
        v.need_key = true;
        v.send_json(json!({ "type": "rtc-failed", "message": reason }));
        self.log(format!("webrtc {reason}, using websocket"));
        self.request_keyframe("webrtc fallback");
    }

    fn check_rtc_timeouts(&mut self) {
        let expired: Vec<u64> = self.viewers.iter().filter(|(_, v)| v.rtc_deadline.is_some_and(|d| Instant::now() > d)).map(|(id, _)| *id).collect();
        for id in expired {
            self.fail_rtc(id, "timeout");
        }
    }

    fn handle_message(&mut self, id: u64, msg: Message) {
        match msg {
            Message::Binary(data) => {
                if self.state != State::Running {
                    return;
                }
                if data.first() == Some(&MSG_TIMED_TOUCH) {
                    if data.len() == TIMED_TOUCH_SIZE && data[9] == control::INJECT_TOUCH_EVENT {
                        let ts = f64::from_be_bytes(data[1..9].try_into().unwrap());
                        let touch: Touch = data[9..41].try_into().unwrap();
                        if let Some(v) = self.viewers.get(&id) {
                            let _ = v.pacer.send((ts, touch));
                        }
                    }
                } else if scrcpy::is_valid_control(&data) {
                    if let Some(ctl) = self.control.lock().unwrap().as_ref() {
                        let _ = ctl.send(data.to_vec());
                    }
                }
            }
            Message::Text(text) => {
                let Ok(msg) = serde_json::from_str::<Value>(&text) else { return };
                match msg.get("type").and_then(Value::as_str).unwrap_or("") {
                    "ack" => {
                        let seq = msg.get("seq").and_then(Value::as_u64).unwrap_or(0) as u32;
                        if let Some(v) = self.viewers.get_mut(&id) {
                            v.on_ack(seq);
                        }
                    }
                    "ping" => {
                        if let Some(v) = self.viewers.get(&id) {
                            v.send_json(json!({ "type": "pong", "t": msg.get("t"), "serverTime": epoch_ms() }));
                        }
                    }
                    "keyframe" => {
                        if let Some(v) = self.viewers.get_mut(&id) {
                            v.need_key = true;
                            v.rtc_need_key = true;
                        }
                        self.request_keyframe("viewer request");
                    }
                    "rtc-ready" => {
                        // The browser is rendering RTP video: stop the WebSocket copy.
                        if let Some(v) = self.viewers.get_mut(&id).filter(|v| v.rtc_sending) {
                            v.rtc_active = true;
                            v.rtc_deadline = None;
                            v.inflight.clear();
                            self.log("viewer switched to webrtc");
                        }
                    }
                    "rtc-answer" => {
                        if let (Some(v), Some(sdp)) = (self.viewers.get(&id), msg.get("sdp").and_then(Value::as_str)) {
                            if let Some(peer) = &v.rtc {
                                peer.send(RtcCmd::Answer(sdp.to_string()));
                            }
                        }
                    }
                    "rtc-candidate" => {
                        if let (Some(v), Some(c)) = (self.viewers.get(&id), msg.get("candidate").and_then(Value::as_str)) {
                            if let Some(peer) = &v.rtc {
                                peer.send(RtcCmd::Candidate(c.to_string()));
                            }
                        }
                    }
                    "rtc-stop" => self.fail_rtc(id, "stopped by viewer"),
                    "diag" => self.log_diag(id, &msg),
                    "pause" => {
                        if let Some(v) = self.viewers.get_mut(&id) {
                            v.paused = true;
                        }
                    }
                    "resume" => {
                        if let Some(v) = self.viewers.get_mut(&id) {
                            v.paused = false;
                            v.need_key = true;
                        }
                        self.request_keyframe("tab visible");
                    }
                    "rtc-start" => self.start_rtc(id),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

pub struct Hub {
    pub shared: Arc<Shared>,
    pub ld: Arc<LdEnv>,
    jar: PathBuf,
    video: VideoOpts,
    max_buffered: usize,
    streams: Mutex<HashMap<String, mpsc::UnboundedSender<Cmd>>>,
    next_id: AtomicUsize,
}

impl Hub {
    pub fn new(shared: Arc<Shared>, ld: Arc<LdEnv>, jar: PathBuf) -> Arc<Self> {
        let cfg = shared.config();
        Arc::new(Self {
            shared,
            ld,
            jar,
            video: VideoOpts { max_size: cfg.max_size, bit_rate: cfg.bit_rate, max_fps: cfg.max_fps },
            max_buffered: cfg.max_buffered_kb as usize * 1024,
            streams: Mutex::new(HashMap::new()),
            next_id: AtomicUsize::new(1),
        })
    }

    fn stream_for(self: &Arc<Self>, serial: &str) -> mpsc::UnboundedSender<Cmd> {
        let mut streams = self.streams.lock().unwrap();
        if let Some(tx) = streams.get(serial).filter(|tx| !tx.is_closed()) {
            return tx.clone();
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let stream = Stream {
            hub: self.clone(),
            serial: serial.to_string(),
            viewers: HashMap::new(),
            session: None,
            generation: 0,
            state: State::Idle,
            device_name: serial.to_string(),
            size: None,
            config: None,
            seq: 0,
            last_keyframe_request: None,
            last_keyframe_at: Instant::now(),
            idle_since: None,
            control: Arc::new(Mutex::new(None)),
            itx: None,
        };
        tokio::spawn(stream.run(rx));
        streams.insert(serial.to_string(), tx.clone());
        tx
    }

    /// Attaches a viewer WebSocket (a relay channel) to the device stream for `serial`.
    pub fn attach<S>(self: &Arc<Self>, serial: String, ws: WebSocketStream<S>, ice: Option<Value>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) as u64;
        let (mut sink, mut source) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let queued = Arc::new(AtomicUsize::new(0));
        let tx = self.stream_for(&serial);

        // writer
        let q = queued.clone();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let len = match &msg {
                    Message::Binary(b) => b.len(),
                    Message::Text(t) => t.len(),
                    _ => 0,
                };
                let closing = matches!(msg, Message::Close(_));
                let ok = sink.send(msg).await.is_ok();
                q.fetch_sub(len.min(q.load(Ordering::Relaxed)), Ordering::Relaxed);
                if !ok || closing {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let ice = ice.unwrap_or_else(|| json!([{ "urls": "stun:stun.l.google.com:19302" }]));
        let _ = tx.send(Cmd::Add(ViewerConn { id, out: out_tx, queued, ice }));

        // reader (tungstenite answers pings itself while we poll)
        tokio::spawn(async move {
            while let Some(Ok(msg)) = source.next().await {
                match msg {
                    Message::Close(_) => break,
                    m @ (Message::Binary(_) | Message::Text(_)) => {
                        if tx.send(Cmd::Msg(id, m)).is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let _ = tx.send(Cmd::Remove(id));
        });
    }

    pub fn shutdown(&self) {
        for tx in self.streams.lock().unwrap().values() {
            let _ = tx.send(Cmd::Shutdown);
        }
    }
}
