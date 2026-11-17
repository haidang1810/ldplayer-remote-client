// Minimal scrcpy 4.1 client: starts scrcpy-server on the device over adb, opens the video and
// control sockets through an adb forward tunnel, and demuxes the video stream.
// Protocol reference: https://github.com/Genymobile/scrcpy/blob/v4.1/doc/develop.md
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ldplayer::run;
use crate::util::no_window;

pub const VERSION: &str = "4.1";
pub static SERVER_JAR: &[u8] = include_bytes!("../../vendor/scrcpy-server-v4.1");
const REMOTE_PATH: &str = "/data/local/tmp/scrcpy-server.jar";
const CODEC_ID_H264: u32 = 0x6832_3634;
const PTS_MASK: u64 = (1 << 61) - 1;

pub mod control {
    pub const INJECT_KEYCODE: u8 = 0;
    pub const INJECT_TEXT: u8 = 1;
    pub const INJECT_TOUCH_EVENT: u8 = 2;
    pub const INJECT_SCROLL_EVENT: u8 = 3;
    pub const BACK_OR_SCREEN_ON: u8 = 4;
    pub const EXPAND_NOTIFICATION_PANEL: u8 = 5;
    pub const EXPAND_SETTINGS_PANEL: u8 = 6;
    pub const COLLAPSE_PANELS: u8 = 7;
    pub const GET_CLIPBOARD: u8 = 8;
    pub const SET_CLIPBOARD: u8 = 9;
    pub const SET_DISPLAY_POWER: u8 = 10;
    pub const ROTATE_DEVICE: u8 = 11;
    pub const START_APP: u8 = 16;
    pub const RESET_VIDEO: u8 = 17;
}

const INJECT_TEXT_MAX: usize = 300;
const CLIPBOARD_TEXT_MAX: usize = (1 << 18) - 14;

/// Exactly one well-formed control message of an allowed type, so a browser can never
/// desynchronize the control stream or reach UHID/file messages.
pub fn is_valid_control(buf: &[u8]) -> bool {
    use control::*;
    let be32 = |o: usize| u32::from_be_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]) as usize;
    match buf.first() {
        Some(&INJECT_KEYCODE) => buf.len() == 14,
        Some(&INJECT_TEXT) => buf.len() >= 5 && be32(1) <= INJECT_TEXT_MAX && buf.len() == 5 + be32(1),
        Some(&INJECT_TOUCH_EVENT) => buf.len() == 32,
        Some(&INJECT_SCROLL_EVENT) => buf.len() == 21,
        Some(&BACK_OR_SCREEN_ON) | Some(&GET_CLIPBOARD) | Some(&SET_DISPLAY_POWER) => buf.len() == 2,
        Some(&EXPAND_NOTIFICATION_PANEL) | Some(&EXPAND_SETTINGS_PANEL) | Some(&COLLAPSE_PANELS) | Some(&ROTATE_DEVICE)
        | Some(&RESET_VIDEO) => buf.len() == 1,
        Some(&SET_CLIPBOARD) => buf.len() >= 14 && be32(10) <= CLIPBOARD_TEXT_MAX && buf.len() == 14 + be32(10),
        Some(&START_APP) => buf.len() >= 2 && buf.len() == 2 + buf[1] as usize,
        _ => false,
    }
}

#[derive(Clone, Debug)]
pub struct VideoOpts {
    pub max_size: u32,
    pub bit_rate: u32,
    pub max_fps: u32,
}

pub enum Event {
    Size { width: u32, height: u32 },
    Packet { config: bool, key: bool, pts: u64, data: Bytes },
    Clipboard(String),
    Closed(Option<String>),
}

/// Writes the embedded scrcpy-server next to the logs (adb push needs a file).
pub fn extract_server(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("scrcpy-server-v{VERSION}"));
    if std::fs::metadata(&path).map(|m| m.len() as usize != SERVER_JAR.len()).unwrap_or(true) {
        std::fs::write(&path, SERVER_JAR)?;
    }
    Ok(path)
}

/// Server arguments end up in a remote `sh` command line, so keep them to a safe charset.
fn safe(v: String) -> Result<String> {
    if v.chars().all(|c| c.is_ascii_alphanumeric() || "_.:=,-".contains(c)) {
        Ok(v)
    } else {
        Err(anyhow!("unsafe scrcpy argument: {v}"))
    }
}

pub struct Session {
    control_tx: mpsc::UnboundedSender<Vec<u8>>,
    tasks: Vec<JoinHandle<()>>,
    child: Option<Child>,
    adb: PathBuf,
    serial: String,
    port: u16,
}

impl Session {
    pub fn control_sender(&self) -> mpsc::UnboundedSender<Vec<u8>> {
        self.control_tx.clone()
    }

    /// Starts scrcpy-server and returns once the video stream header has been read.
    /// Returns the session and the device name.
    pub async fn start(adb: &Path, serial: &str, jar: &Path, video: &VideoOpts, events: mpsc::UnboundedSender<Event>) -> Result<(Self, String)> {
        let jar_s = jar.to_string_lossy().to_string();
        run(adb, &["-s", serial, "push", &jar_s, REMOTE_PATH], Duration::from_secs(60)).await?;

        let scid = format!("{:08x}", rand::random::<u32>() & 0x7fff_ffff);
        let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        run(adb, &["-s", serial, "forward", &format!("tcp:{port}"), &format!("localabstract:scrcpy_{scid}")], Duration::from_secs(20)).await?;

        let mut session = Session {
            control_tx: mpsc::unbounded_channel().0,
            tasks: vec![],
            child: None,
            adb: adb.to_path_buf(),
            serial: serial.to_string(),
            port,
        };

        let mut args: Vec<String> = vec![
            format!("CLASSPATH={REMOTE_PATH}"),
            "app_process".into(),
            "/".into(),
            "com.genymobile.scrcpy.Server".into(),
            VERSION.into(),
        ];
        let opts: Vec<(&str, String)> = vec![
            ("scid", scid.clone()),
            ("log_level", "info".into()),
            ("tunnel_forward", "true".into()),
            ("audio", "false".into()),
            ("control", "true".into()),
            ("video_codec", "h264".into()),
            ("max_size", video.max_size.to_string()),
            ("video_bit_rate", video.bit_rate.to_string()),
            ("max_fps", video.max_fps.to_string()),
            // A key frame every 3 s lets a viewer that dropped frames recover without a reset.
            ("video_codec_options", "i-frame-interval:int=3".into()),
            ("clipboard_autosync", "false".into()),
            ("cleanup", "true".into()),
        ];
        for (k, v) in opts {
            args.push(safe(format!("{k}={v}"))?);
        }

        let mut cmd = Command::new(adb);
        cmd.arg("-s").arg(serial).arg("shell").args(&args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null()).kill_on_drop(true);
        no_window(&mut cmd);
        let mut child = cmd.spawn().context("starting adb shell")?;
        let tag = serial.to_string();
        for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn AsyncRead + Unpin + Send>), child.stderr.take().map(|p| Box::new(p) as _)]
            .into_iter()
            .flatten()
        {
            let tag = tag.clone();
            session.tasks.push(tokio::spawn(async move {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        tracing::info!("[{tag}] scrcpy: {line}");
                    }
                }
            }));
        }
        session.child = Some(child);

        // adb accepts the local connection even before the server listens; the dummy byte is how
        // we know the tunnel really reached scrcpy-server.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut video_sock = loop {
            if let Some(Ok(Some(status))) = session.child.as_mut().map(|c| c.try_wait()) {
                bail!("scrcpy-server exited ({status}) before accepting a connection");
            }
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)).await {
                let mut b = [0u8; 1];
                if let Ok(Ok(1)) = tokio::time::timeout(Duration::from_secs(2), s.read(&mut b)).await {
                    s.set_nodelay(true)?;
                    break s;
                }
            }
            if tokio::time::Instant::now() > deadline {
                bail!("timed out connecting to scrcpy-server");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let control_sock = TcpStream::connect(("127.0.0.1", port)).await?;
        control_sock.set_nodelay(true)?;

        let mut name = [0u8; 64];
        video_sock.read_exact(&mut name).await?;
        let end = name.iter().position(|&b| b == 0).unwrap_or(64);
        let device_name = String::from_utf8_lossy(&name[..end]).into_owned();
        let codec = video_sock.read_u32().await?;
        match codec {
            0 => bail!("device disabled the video stream"),
            1 => bail!("scrcpy-server configuration error (see log)"),
            CODEC_ID_H264 => {}
            c => bail!("unexpected codec id {c:#x}"),
        }

        let closed = Arc::new(AtomicBool::new(false));
        let close = {
            let events = events.clone();
            let closed = closed.clone();
            move |err: Option<String>| {
                if !closed.swap(true, Ordering::SeqCst) {
                    let _ = events.send(Event::Closed(err));
                }
            }
        };

        // video
        {
            let events = events.clone();
            let close = close.clone();
            session.tasks.push(tokio::spawn(async move {
                let r = video_loop(video_sock, &events).await;
                close(Some(r.err().map(|e| e.to_string()).unwrap_or_else(|| "video stream ended".into())));
            }));
        }

        // control: reader (device messages) + writer (our messages)
        let (ctl_read, mut ctl_write) = control_sock.into_split();
        {
            let events = events.clone();
            let close = close.clone();
            session.tasks.push(tokio::spawn(async move {
                let r = control_read_loop(ctl_read, &events).await;
                close(Some(r.err().map(|e| e.to_string()).unwrap_or_else(|| "control stream ended".into())));
            }));
        }
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        session.control_tx = control_tx;
        session.tasks.push(tokio::spawn(async move {
            while let Some(buf) = control_rx.recv().await {
                if ctl_write.write_all(&buf).await.is_err() {
                    break;
                }
            }
        }));

        Ok((session, device_name))
    }

}

impl Drop for Session {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        if let Some(c) = self.child.as_mut() {
            let _ = c.start_kill();
        }
        let (adb, serial, port) = (self.adb.clone(), self.serial.clone(), self.port);
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = run(&adb, &["-s", &serial, "forward", "--remove", &format!("tcp:{port}")], Duration::from_secs(10)).await;
            });
        }
    }
}

async fn video_loop(mut sock: TcpStream, events: &mpsc::UnboundedSender<Event>) -> Result<()> {
    let mut header = [0u8; 12];
    loop {
        sock.read_exact(&mut header).await?;
        let hi = u32::from_be_bytes(header[0..4].try_into().unwrap());
        if hi & 0x8000_0000 != 0 {
            let width = u32::from_be_bytes(header[4..8].try_into().unwrap());
            let height = u32::from_be_bytes(header[8..12].try_into().unwrap());
            let _ = events.send(Event::Size { width, height });
            continue;
        }
        let config = hi & 0x4000_0000 != 0;
        let key = hi & 0x2000_0000 != 0;
        let pts = u64::from_be_bytes(header[0..8].try_into().unwrap()) & PTS_MASK;
        let size = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
        if size > 32 << 20 {
            bail!("video packet too large ({size} bytes)");
        }
        let mut data = vec![0u8; size];
        sock.read_exact(&mut data).await?;
        if events.send(Event::Packet { config, key, pts, data: Bytes::from(data) }).is_err() {
            return Ok(());
        }
    }
}

async fn control_read_loop(mut sock: tokio::net::tcp::OwnedReadHalf, events: &mpsc::UnboundedSender<Event>) -> Result<()> {
    loop {
        match sock.read_u8().await? {
            0 => {
                let len = sock.read_u32().await? as usize;
                if len > CLIPBOARD_TEXT_MAX {
                    bail!("clipboard message too large");
                }
                let mut buf = vec![0u8; len];
                sock.read_exact(&mut buf).await?;
                let _ = events.send(Event::Clipboard(String::from_utf8_lossy(&buf).into_owned()));
            }
            1 => {
                sock.read_u64().await?; // clipboard ack sequence
            }
            2 => {
                sock.read_u16().await?;
                let len = sock.read_u16().await? as usize;
                let mut buf = vec![0u8; len];
                sock.read_exact(&mut buf).await?;
            }
            t => bail!("unknown device message type {t}"),
        }
    }
}
