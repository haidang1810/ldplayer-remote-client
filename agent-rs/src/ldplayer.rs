// adb and ldconsole wrappers: finding LDPlayer, listing instances and adb devices.
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::Command;

use crate::util::no_window;

const NAMES: [&str; 5] = ["LDPlayer14", "LDPlayer9", "LDPlayer4.0", "LDPlayer64", "LDPlayer"];

fn candidate_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for drive in ['C', 'D', 'E', 'F', 'G'] {
        for name in NAMES {
            out.push(PathBuf::from(format!(r"{drive}:\LDPlayer\{name}")));
            out.push(PathBuf::from(format!(r"{drive}:\{name}")));
            out.push(PathBuf::from(format!(r"{drive}:\Program Files\LDPlayer\{name}")));
        }
    }
    out
}

fn is_ld_dir(dir: &Path) -> bool {
    dir.join("ldconsole.exe").exists()
}

/// Folder of a running dnplayer.exe, if any (covers custom install locations).
async fn running_player_dir() -> Option<PathBuf> {
    let out = run(
        Path::new("powershell.exe"),
        &["-NoProfile", "-Command", "(Get-Process dnplayer -ErrorAction SilentlyContinue | Select-Object -First 1).Path"],
        Duration::from_secs(10),
    )
    .await
    .ok()?;
    let exe = PathBuf::from(out.trim());
    exe.parent().map(Path::to_path_buf).filter(|d| is_ld_dir(d))
}

pub async fn run(exe: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = Command::new(exe);
    cmd.args(args).kill_on_drop(true);
    no_window(&mut cmd);
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| anyhow!("{} {} timed out", exe.display(), args.join(" ")))?
        .with_context(|| format!("running {}", exe.display()))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().to_string();
        return Err(anyhow!("{} {} failed: {msg}", exe.file_name().unwrap_or_default().to_string_lossy(), args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Serialize, Clone, Debug)]
pub struct DeviceInfo {
    /// adb serial when the instance is reachable over adb, else null.
    pub id: Option<String>,
    pub name: String,
    pub index: Option<u32>,
    pub running: bool,
    pub adb: bool,
}

#[derive(Debug, Clone)]
pub struct Instance {
    pub index: u32,
    pub title: String,
    pub running: bool,
    /// Top-level LDPlayer window and its Android render child (0 when not running).
    pub top_hwnd: i64,
    pub render_hwnd: i64,
}

/// Instance index for an adb serial of an LDPlayer instance.
pub fn index_for_serial(serial: &str) -> Option<u32> {
    if let Some(p) = serial.strip_prefix("emulator-") {
        let p: u32 = p.parse().ok()?;
        return (p >= 5554 && p % 2 == 0).then(|| (p - 5554) / 2);
    }
    let p: u32 = serial.strip_prefix("127.0.0.1:")?.parse().ok()?;
    (p >= 5555 && p % 2 == 1).then(|| (p - 5555) / 2)
}

/// LDPlayer instance N listens for adb as emulator-(5554+2N), or 127.0.0.1:(5555+2N) after `adb connect`.
pub fn serials_for_index(index: u32) -> [String; 2] {
    [format!("emulator-{}", 5554 + 2 * index), format!("127.0.0.1:{}", 5555 + 2 * index)]
}

pub struct LdEnv {
    configured: Option<PathBuf>,
    /// (found folder, when we last searched); async so concurrent callers wait for one search.
    probe: tokio::sync::Mutex<(Option<PathBuf>, Option<Instant>)>,
}

impl LdEnv {
    pub fn new(configured: &str) -> Self {
        let configured = (!configured.trim().is_empty()).then(|| PathBuf::from(configured.trim()));
        Self { configured, probe: tokio::sync::Mutex::new((None, None)) }
    }

    /// The LDPlayer folder; searched again (at most every 30 s) while it hasn't been found.
    pub async fn dir(&self) -> Option<PathBuf> {
        let mut probe = self.probe.lock().await;
        if probe.0.is_some() || probe.1.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
            return probe.0.clone();
        }
        probe.1 = Some(Instant::now());
        let found = match &self.configured {
            Some(d) if is_ld_dir(d) => Some(d.clone()),
            _ => match candidate_dirs().into_iter().find(|d| is_ld_dir(d)) {
                Some(d) => Some(d),
                None => running_player_dir().await,
            },
        };
        match &found {
            Some(d) => tracing::info!("LDPlayer folder: {}", d.display()),
            None => tracing::warn!("LDPlayer folder not found (set it in the settings page)"),
        }
        probe.0 = found.clone();
        found
    }

    /// LDPlayer's own adb: two different adb versions keep killing each other's server.
    pub async fn adb(&self) -> PathBuf {
        match self.dir().await {
            Some(d) if d.join("adb.exe").exists() => d.join("adb.exe"),
            _ => PathBuf::from("adb.exe"),
        }
    }

    pub async fn ldconsole(&self) -> Option<PathBuf> {
        self.dir().await.map(|d| d.join("ldconsole.exe"))
    }

    pub async fn instances(&self) -> Result<Vec<Instance>> {
        let Some(exe) = self.ldconsole().await else { return Ok(vec![]) };
        // list2: index,title,topHwnd,bindHwnd,isRunning,pid,vboxPid,width,height,dpi
        let out = run(&exe, &["list2"], Duration::from_secs(10)).await?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.trim().split(',').collect();
                if f.len() < 5 {
                    return None;
                }
                Some(Instance {
                    index: f[0].parse().ok()?,
                    title: f[1].to_string(),
                    running: f[4] == "1",
                    top_hwnd: f[2].parse().unwrap_or(0),
                    render_hwnd: f[3].parse().unwrap_or(0),
                })
            })
            .collect())
    }

    /// Window handles (top, render) of the running LDPlayer instance behind an adb serial.
    pub async fn windows_for_serial(&self, serial: &str) -> Option<(isize, isize)> {
        let index = index_for_serial(serial)?;
        let inst = self.instances().await.ok()?.into_iter().find(|i| i.index == index && i.running)?;
        (inst.top_hwnd != 0 && inst.render_hwnd != 0).then_some((inst.top_hwnd as isize, inst.render_hwnd as isize))
    }

    /// Android display size (`wm size`, an override wins), e.g. (1600, 900).
    pub async fn display_size(&self, serial: &str) -> Option<(u32, u32)> {
        let out = run(&self.adb().await, &["-s", serial, "shell", "wm", "size"], Duration::from_secs(10)).await.ok()?;
        let line = out.lines().filter(|l| l.contains("size:")).last()?;
        let (w, h) = line.rsplit(':').next()?.trim().split_once('x')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    }

    pub async fn adb_devices(&self) -> Result<Vec<(String, String)>> {
        let out = run(&self.adb().await, &["devices"], Duration::from_secs(20)).await?;
        Ok(out
            .lines()
            .skip(1)
            .filter_map(|l| {
                let mut p = l.split_whitespace();
                Some((p.next()?.to_string(), p.next()?.to_string()))
            })
            .collect())
    }

    /// One entry per LDPlayer instance (running or not) plus any other adb device.
    pub async fn list_devices(&self) -> Vec<DeviceInfo> {
        let (instances, adb) = tokio::join!(self.instances(), self.adb_devices());
        let instances = instances.unwrap_or_else(|e| {
            tracing::warn!("ldconsole: {e:#}");
            vec![]
        });
        let adb = adb.unwrap_or_else(|e| {
            tracing::warn!("adb devices: {e:#}");
            vec![]
        });
        let ready: HashSet<&str> = adb.iter().filter(|(_, s)| s == "device").map(|(d, _)| d.as_str()).collect();
        let mut claimed = HashSet::new();
        let mut devices: Vec<DeviceInfo> = instances
            .iter()
            .map(|inst| {
                let candidates = serials_for_index(inst.index);
                let serial = candidates.iter().find(|s| ready.contains(s.as_str())).cloned();
                claimed.extend(candidates);
                DeviceInfo { adb: serial.is_some(), id: serial, name: inst.title.clone(), index: Some(inst.index), running: inst.running }
            })
            .collect();
        for (serial, state) in &adb {
            if claimed.contains(serial) {
                continue;
            }
            let ok = state == "device";
            devices.push(DeviceInfo { id: ok.then(|| serial.clone()), name: serial.clone(), index: None, running: true, adb: ok });
        }
        devices
    }
}
