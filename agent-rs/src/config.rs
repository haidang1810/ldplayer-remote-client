use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use crate::util::config_dir;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// https://relay.example.com (ws/wss also accepted)
    pub relay_url: String,
    pub api_key: String,
    /// Name shown in the web UI (default: Windows computer name).
    pub name: String,
    /// Stable id so several PCs can share one API key and still show up separately.
    pub machine_id: String,
    /// LDPlayer folder (with ldconsole.exe and adb.exe); found automatically when empty.
    pub ldplayer_dir: String,
    /// Longest video side in pixels (0 = native).
    pub max_size: u32,
    pub bit_rate: u32,
    pub max_fps: u32,
    /// WebSocket viewers: drop frames once this many bytes are waiting to be sent.
    pub max_buffered_kb: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            relay_url: String::new(),
            api_key: String::new(),
            name: String::new(),
            machine_id: String::new(),
            ldplayer_dir: String::new(),
            max_size: 1280,
            bit_rate: 8_000_000,
            max_fps: 60,
            max_buffered_kb: 256,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("config.json")
    }

    pub fn load() -> Self {
        let mut cfg: Config = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let mut changed = false;
        if cfg.machine_id.is_empty() {
            cfg.machine_id = uuid::Uuid::new_v4().to_string();
            changed = true;
        }
        if cfg.name.trim().is_empty() {
            cfg.name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into());
            changed = true;
        }
        if changed {
            let _ = cfg.save();
        }
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(config_dir())?;
        let tmp = Self::path().with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, Self::path())
    }

    pub fn is_complete(&self) -> bool {
        !self.relay_url.trim().is_empty() && !self.api_key.trim().is_empty()
    }

    /// wss://host for the relay's WebSocket endpoints.
    pub fn ws_base(&self) -> String {
        let url = self.relay_url.trim().trim_end_matches('/');
        if let Some(rest) = url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else if url.starts_with("ws://") || url.starts_with("wss://") {
            url.to_string()
        } else {
            format!("wss://{url}")
        }
    }

    /// https://host for opening the web page.
    pub fn web_url(&self) -> String {
        let base = self.ws_base();
        if let Some(rest) = base.strip_prefix("wss://") {
            format!("https://{rest}")
        } else if let Some(rest) = base.strip_prefix("ws://") {
            format!("http://{rest}")
        } else {
            base
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    NeedsKey,
    Connecting,
    Connected,
    BadKey,
    Offline(String),
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::NeedsKey => "Chưa nhập API key".into(),
            Status::Connecting => "Đang kết nối…".into(),
            Status::Connected => "Đã kết nối".into(),
            Status::BadKey => "API key sai hoặc đã bị thu hồi".into(),
            Status::Offline(e) => format!("Mất kết nối: {e}"),
        }
    }
}

/// State shared by the tray (main thread), the settings page (its own thread) and the agent (tokio).
pub struct Shared {
    pub config: Mutex<Config>,
    pub status: Mutex<Status>,
    /// Wakes the relay loop to reconnect with new settings.
    pub reconnect: Notify,
    pub viewers: std::sync::atomic::AtomicUsize,
}

impl Shared {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config: Mutex::new(config),
            status: Mutex::new(Status::Connecting),
            reconnect: Notify::new(),
            viewers: Default::default(),
        })
    }

    pub fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    pub fn set_status(&self, s: Status) {
        *self.status.lock().unwrap() = s;
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
}
