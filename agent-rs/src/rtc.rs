// WebRTC transport for one viewer (str0m, sans-IO): the scrcpy H.264 stream is packetized straight
// into RTP (no re-encode) on a send-only video track, and input arrives on a DataChannel. The
// viewer's WebSocket carries the signaling (offer/answer/candidates).
//
// We gather a host and a server-reflexive candidate ourselves (one STUN binding request). The
// browser has TURN, so when no direct path exists it reaches us through its relay candidate.
use anyhow::{anyhow, Context, Result};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::ChannelId;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub enum RtcCmd {
    Answer(String),
    Candidate(String),
    /// Annex B access unit and its scrcpy PTS in microseconds.
    Frame(Arc<[u8]>, u64),
}

pub enum RtcEvent {
    Offer(String),
    /// Media can flow (ICE + DTLS up); carries a description of the selected path.
    Connected(String),
    KeyframeRequest,
    /// Send-side bandwidth estimate (TWCC), bits per second.
    Bandwidth(u64),
    Data(bool, Vec<u8>),
    Failed(String),
}

pub struct RtcPeer {
    tx: mpsc::UnboundedSender<RtcCmd>,
    task: JoinHandle<()>,
}

impl RtcPeer {
    pub fn send(&self, cmd: RtcCmd) {
        let _ = self.tx.send(cmd);
    }
}

impl Drop for RtcPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// STUN servers from the browser-format ICE list the relay sends ({urls, username?, credential?}).
pub fn stun_servers(ice_browser: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    for server in ice_browser.as_array().into_iter().flatten() {
        let urls = match server.get("urls") {
            Some(serde_json::Value::String(s)) => vec![s.clone()],
            Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => vec![],
        };
        for u in urls {
            if let Some(rest) = u.strip_prefix("stun:") {
                out.push(rest.to_string());
            }
        }
    }
    // Public fallback in case the relay's STUN port is firewalled.
    out.push("stun.l.google.com:19302".into());
    out
}

/// `max_bps`: the most this viewer's video could use; BWE probes up to it.
pub fn start(stun: Vec<String>, max_bps: u64, events: impl Fn(RtcEvent) + Send + Sync + 'static) -> RtcPeer {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        if let Err(e) = run(stun, max_bps, rx, &events).await {
            events(RtcEvent::Failed(format!("{e:#}")));
        }
    });
    RtcPeer { tx, task }
}

/// The address of the interface that routes to the Internet (no packet is sent).
fn primary_ip() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    Some(s.local_addr().ok()?.ip())
}

/// One STUN binding request (RFC 5389) from `socket`; returns our public address.
async fn stun_binding(socket: &UdpSocket, server: &str) -> Result<SocketAddr> {
    let addr = tokio::net::lookup_host(server).await?.find(SocketAddr::is_ipv4).ok_or_else(|| anyhow!("no IPv4 for {server}"))?;
    let txid: [u8; 12] = rand::random();
    let mut req = vec![0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xa4, 0x42];
    req.extend_from_slice(&txid);
    let mut buf = [0u8; 1500];
    for attempt in 0..3 {
        socket.send_to(&req, addr).await?;
        let wait = Duration::from_millis(300 * (attempt + 1));
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Ok((n, from))) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
            if from != addr || n < 20 || buf[0..2] != [0x01, 0x01] || buf[8..20] != txid {
                continue;
            }
            let mut i = 20;
            while i + 4 <= n {
                let ty = u16::from_be_bytes([buf[i], buf[i + 1]]);
                let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
                let v = &buf[i + 4..(i + 4 + len).min(n)];
                if ty == 0x0020 && v.len() >= 8 && v[1] == 0x01 {
                    let port = u16::from_be_bytes([v[2], v[3]]) ^ 0x2112;
                    let ip = [v[4] ^ 0x21, v[5] ^ 0x12, v[6] ^ 0xa4, v[7] ^ 0x42];
                    return Ok(SocketAddr::from((ip, port)));
                }
                i += 4 + len.div_ceil(4) * 4;
            }
        }
    }
    Err(anyhow!("no STUN response from {server}"))
}

async fn run(stun: Vec<String>, max_bps: u64, mut rx: mpsc::UnboundedReceiver<RtcCmd>, events: &(impl Fn(RtcEvent) + Send + Sync)) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.context("binding UDP")?;
    let port = socket.local_addr()?.port();
    let host_ip = primary_ip().ok_or_else(|| anyhow!("no network interface"))?;
    let local = SocketAddr::new(host_ip, port);

    let mut rtc = RtcConfig::new()
        .clear_codecs()
        .enable_h264(true)
        .enable_bwe(Some(Bitrate::bps(max_bps.min(4_000_000))))
        .build(Instant::now());
    rtc.bwe().set_bitrate(Bitrate::kbps(300), Bitrate::bps(max_bps));
    rtc.add_local_candidate(Candidate::host(local, "udp")?);
    let mut public_addr = None;
    for server in &stun {
        match stun_binding(&socket, server).await {
            Ok(public) => {
                if public != local {
                    if let Ok(c) = Candidate::server_reflexive(public, local, "udp") {
                        rtc.add_local_candidate(c);
                    }
                }
                public_addr = Some(public);
                break;
            }
            Err(e) => tracing::warn!("webrtc: STUN {server} failed: {e:#}"),
        }
    }
    tracing::info!("webrtc: local candidates host {local}, public {}", public_addr.map_or("none (STUN failed)".to_string(), |a| a.to_string()));

    let mut api = rtc.sdp_api();
    let mid: Mid = api.add_media(MediaKind::Video, Direction::SendOnly, Some("ldplayer".into()), Some("video".into()), None);
    let _control: ChannelId = api.add_channel("control".into());
    let (offer, pending) = api.apply().ok_or_else(|| anyhow!("no SDP changes"))?;
    let mut pending: Option<SdpPendingOffer> = Some(pending);
    // Browsers start trickling candidates while they create the answer, so some arrive before it;
    // keep them until the answer is applied or they would be lost (and remote viewers never connect).
    let mut early_candidates: Vec<Candidate> = Vec::new();
    let mut remote_kinds: Vec<String> = Vec::new();
    events(RtcEvent::Offer(offer.to_sdp_string()));

    let mut connected = false;
    let mut buf = vec![0u8; 2000];
    let mut last_pts: Option<u64> = None;
    let mut rtp_time: u64 = rand::random::<u32>() as u64;
    let mut last_estimate_log = Instant::now();
    let mut video_pt = None;

    loop {
        // Drain output until str0m wants to wait.
        let deadline = loop {
            match rtc.poll_output()? {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Output::Event(ev) => match ev {
                    Event::Connected => {
                        connected = true;
                        events(RtcEvent::Connected(format!("local {host_ip}, remote candidates: {}", remote_kinds.join("/"))));
                    }
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        return Err(anyhow!("ICE disconnected"));
                    }
                    Event::IceConnectionStateChange(s) => tracing::debug!("webrtc: ICE {s:?}"),
                    Event::KeyframeRequest(_) => events(RtcEvent::KeyframeRequest),
                    Event::ChannelData(d) => events(RtcEvent::Data(d.binary, d.data)),
                    Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                        if last_estimate_log.elapsed() > Duration::from_secs(30) {
                            last_estimate_log = Instant::now();
                            tracing::debug!("bandwidth estimate: {estimate}");
                        }
                        events(RtcEvent::Bandwidth(estimate.as_u64()));
                    }
                    Event::Closed => return Err(anyhow!("closed")),
                    _ => {}
                },
            }
        };

        let wait = tokio::time::Instant::from_std(deadline.max(Instant::now()));
        tokio::select! {
            r = socket.recv_from(&mut buf) => {
                let (n, source) = r?;
                if let Ok(contents) = buf[..n].try_into() {
                    let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents });
                    if rtc.accepts(&input) {
                        rtc.handle_input(input)?;
                    }
                }
            }
            _ = tokio::time::sleep_until(wait) => {
                rtc.handle_input(Input::Timeout(Instant::now()))?;
            }
            cmd = rx.recv() => match cmd {
                None => return Ok(()),
                Some(RtcCmd::Answer(sdp)) => {
                    let answer = SdpAnswer::from_sdp_string(&sdp).map_err(|e| anyhow!("bad answer: {e}"))?;
                    if let Some(p) = pending.take() {
                        rtc.sdp_api().accept_answer(p, answer)?;
                        for c in early_candidates.drain(..) {
                            rtc.add_remote_candidate(c);
                        }
                    }
                }
                Some(RtcCmd::Candidate(c)) => {
                    // Browsers hide host addresses behind mDNS names; those fail to parse and are skipped.
                    if let Ok(c) = Candidate::from_sdp_string(c.trim_start_matches("a=")) {
                        let kind = format!("{:?}", c.kind()).to_lowercase();
                        if !remote_kinds.contains(&kind) {
                            tracing::info!("webrtc: remote candidate {kind} {}", c.addr());
                            remote_kinds.push(kind);
                        }
                        if pending.is_some() {
                            early_candidates.push(c);
                        } else {
                            rtc.add_remote_candidate(c);
                        }
                    }
                }
                Some(RtcCmd::Frame(data, pts)) => {
                    if !connected {
                        continue;
                    }
                    // scrcpy restarts PTS when capture resets; RTP time must keep moving forward.
                    if let Some(prev) = last_pts {
                        let delta = pts.wrapping_sub(prev);
                        rtp_time += if pts > prev && delta < 1_000_000 { delta * 9 / 100 } else { 1500 };
                    }
                    last_pts = Some(pts);
                    if let Some(writer) = rtc.writer(mid) {
                        if video_pt.is_none() {
                            // scrcpy sends constrained-baseline H.264 in FU-A packets: needs
                            // packetization-mode=1; prefer a baseline (0x42…) profile entry.
                            let params: Vec<_> = writer.payload_params().map(|p| (p.pt(), p.spec().format)).collect();
                            let score = |f: &str0m::format::FormatParams| {
                                (f.packetization_mode == Some(1)) as u8 * 2 + f.profile_level_id.is_some_and(|id| id >> 16 == 0x42) as u8
                            };
                            video_pt = params.iter().max_by_key(|(_, f)| score(f)).map(|(pt, _)| *pt);
                            tracing::info!("webrtc video payload type {video_pt:?} (of {})", params.len());
                        }
                        if let Some(pt) = video_pt {
                            if let Err(e) = writer.write(pt, Instant::now(), MediaTime::new(rtp_time, Frequency::NINETY_KHZ), data) {
                                tracing::debug!("rtc write: {e}");
                            }
                        }
                    }
                }
            }
        }
    }
}
