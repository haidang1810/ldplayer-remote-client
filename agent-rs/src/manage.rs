// LDPlayer management commands sent by the relay on behalf of the web page: create / copy / launch
// / quit / rename / remove instances, screenshots, installed apps, and APK installs.
//
// Requests: `{ type: "cmd", reqId, action, ... }` → `{ type: "result", reqId, ok: true, ... }` or
// `{ type: "result", reqId, error }`. APK installs have no reqId: the relay polls nothing, we push
// `{ type: "job", job, state, progress?, message? }` updates instead.
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::hub::Hub;
use crate::ldplayer::{self, LdEnv};
use crate::util::{no_window, uri_encode};

/// Long ldconsole jobs (copying a big instance) keep running; the web page is told it is pending.
const REPLY_WITHIN: Duration = Duration::from_secs(40);
const THUMB_LONG_SIDE: u32 = 540;

pub fn handle(msg: Value, cfg: Config, hub: Arc<Hub>, out: mpsc::UnboundedSender<Message>) {
    let send = move |v: Value| {
        let _ = out.send(Message::Text(v.to_string().into()));
    };
    let action = msg.get("action").and_then(Value::as_str).unwrap_or_default().to_string();
    if action == "install-apk" {
        tokio::spawn(install_apk(msg, cfg, hub, send));
        return;
    }
    let req_id = msg.get("reqId").cloned().unwrap_or(Value::Null);
    tokio::spawn(async move {
        let started = Instant::now();
        let reply = match run(&action, &msg, &hub.ld).await {
            Ok(mut v) => {
                v["ok"] = json!(true);
                v
            }
            Err(e) => {
                tracing::warn!("command {action}: {e:#}");
                json!({ "error": friendly(&e) })
            }
        };
        if action != "screenshot" {
            tracing::info!("command {action} {} ({} ms)", short(&msg), started.elapsed().as_millis());
        }
        let mut reply = reply;
        reply["type"] = json!("result");
        reply["reqId"] = req_id;
        send(reply);
    });
}

fn short(msg: &Value) -> String {
    let mut parts = Vec::new();
    for k in ["index", "from", "name", "package"] {
        if let Some(v) = msg.get(k) {
            parts.push(format!("{k}={v}"));
        }
    }
    parts.join(" ")
}

fn friendly(e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    s.chars().take(300).collect()
}

fn index_of(msg: &Value) -> Result<u32> {
    msg.get("index").and_then(Value::as_u64).map(|i| i as u32).ok_or_else(|| anyhow!("thiếu index"))
}

fn str_of<'a>(msg: &'a Value, key: &str) -> Result<&'a str> {
    msg.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).ok_or_else(|| anyhow!("thiếu {key}"))
}

fn package_of(msg: &Value) -> Result<&str> {
    let p = str_of(msg, "package")?;
    if !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_') {
        bail!("tên gói không hợp lệ");
    }
    Ok(p)
}

async fn run(action: &str, msg: &Value, ld: &LdEnv) -> Result<Value> {
    match action {
        "create" | "copy" => create(action, msg, ld).await,
        "launch" | "quit" | "reboot" => {
            let index = index_of(msg)?;
            let inst = instance(ld, index).await?;
            if action == "launch" && !inst.running {
                enable_adb(ld, index).await;
            }
            console(ld, &[action, "--index", &index.to_string()], Duration::from_secs(30)).await?;
            Ok(json!({}))
        }
        "rename" => {
            let index = index_of(msg)?;
            instance(ld, index).await?;
            console(ld, &["rename", "--index", &index.to_string(), "--title", str_of(msg, "name")?], Duration::from_secs(20)).await?;
            Ok(json!({}))
        }
        "remove" => {
            let index = index_of(msg)?;
            let inst = instance(ld, index).await?;
            if inst.running {
                console(ld, &["quit", "--index", &index.to_string()], Duration::from_secs(30)).await?;
                wait_until(Duration::from_secs(30), || async { instance(ld, index).await.is_ok_and(|i| !i.running) }).await;
            }
            // ldconsole reports the instance stopped before its VM has exited, and a remove issued
            // too early leaves an empty "LDPlayer-N" entry behind: check and retry.
            for attempt in 0..6 {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                console(ld, &["remove", "--index", &index.to_string()], Duration::from_secs(60)).await?;
                tokio::time::sleep(Duration::from_millis(1500)).await;
                if instance(ld, index).await.is_err() {
                    return Ok(json!({}));
                }
            }
            bail!("LDPlayer chưa xoá được máy #{index}, thử lại sau")
        }
        "screenshot" => {
            let serial = serial(ld, index_of(msg)?).await?;
            let jpeg = screenshot(ld, &serial).await?;
            Ok(json!({ "mime": "image/jpeg", "data": base64::engine::general_purpose::STANDARD.encode(jpeg) }))
        }
        "apps" => {
            let serial = serial(ld, index_of(msg)?).await?;
            let out = adb(ld, &serial, &["shell", "pm", "list", "packages", "-3"], Duration::from_secs(20)).await?;
            let mut apps: Vec<&str> = out.lines().filter_map(|l| l.trim().strip_prefix("package:")).collect();
            apps.sort_unstable();
            Ok(json!({ "apps": apps.iter().map(|p| json!({ "package": p, "label": "" })).collect::<Vec<_>>() }))
        }
        "app-launch" => {
            let index = index_of(msg)?;
            let package = package_of(msg)?;
            let serial = serial(ld, index).await?;
            // `monkey` is missing on newer LDPlayer images: resolve the launcher activity instead.
            let resolved = adb(ld, &serial, &["shell", "cmd", "package", "resolve-activity", "--brief", "-c", "android.intent.category.LAUNCHER", package], Duration::from_secs(20)).await.unwrap_or_default();
            match resolved.lines().map(str::trim).rfind(|l| l.contains('/')) {
                Some(component) => {
                    let out = adb(ld, &serial, &["shell", "am", "start", "-n", component], Duration::from_secs(20)).await?;
                    if out.contains("Error") {
                        bail!("{}", out.trim());
                    }
                }
                None => {
                    console(ld, &["runapp", "--index", &index.to_string(), "--packagename", package], Duration::from_secs(20)).await?;
                }
            }
            Ok(json!({}))
        }
        "app-stop" => {
            let serial = serial(ld, index_of(msg)?).await?;
            adb(ld, &serial, &["shell", "am", "force-stop", package_of(msg)?], Duration::from_secs(20)).await?;
            Ok(json!({}))
        }
        "app-uninstall" => {
            let serial = serial(ld, index_of(msg)?).await?;
            let out = adb(ld, &serial, &["uninstall", package_of(msg)?], Duration::from_secs(60)).await?;
            if !out.contains("Success") {
                bail!("gỡ thất bại: {}", out.trim());
            }
            Ok(json!({}))
        }
        _ => bail!("agent chưa hỗ trợ lệnh \"{action}\" (hãy cập nhật ldremote.exe)"),
    }
}

/// Runs ldconsole from the LDPlayer folder. Its exit code is not a status (`add` returns the new
/// index), so callers check the outcome themselves; the output is returned and logged.
async fn console(ld: &LdEnv, args: &[&str], timeout: Duration) -> Result<String> {
    let dir = ld.dir().await.ok_or_else(|| anyhow!("không tìm thấy thư mục LDPlayer"))?;
    console_in(&dir, args, timeout).await
}

async fn console_in(dir: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = Command::new(dir.join("ldconsole.exe"));
    cmd.args(args).current_dir(dir).kill_on_drop(true);
    no_window(&mut cmd);
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| anyhow!("ldconsole {} quá thời gian", args.first().unwrap_or(&"")))?
        .context("chạy ldconsole")?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    tracing::info!("ldconsole {}: exit {:?} {}", args.join(" "), out.status.code(), text.chars().take(200).collect::<String>());
    Ok(text)
}

async fn adb(ld: &LdEnv, serial: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let mut full = vec!["-s", serial];
    full.extend_from_slice(args);
    ldplayer::run(&ld.adb().await, &full, timeout).await
}

async fn instance(ld: &LdEnv, index: u32) -> Result<ldplayer::Instance> {
    ld.instances().await?.into_iter().find(|i| i.index == index).ok_or_else(|| anyhow!("không có máy #{index}"))
}

/// adb serial of a running instance.
async fn serial(ld: &LdEnv, index: u32) -> Result<String> {
    let devices = ld.adb_devices().await?;
    ldplayer::serials_for_index(index)
        .into_iter()
        .find(|s| devices.iter().any(|(d, st)| d == s && st == "device"))
        .ok_or_else(|| anyhow!("máy #{index} chưa chạy hoặc chưa bật ADB"))
}

async fn wait_until<F, Fut>(limit: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    while start.elapsed() < limit {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    false
}

async fn create(action: &str, msg: &Value, ld: &LdEnv) -> Result<Value> {
    let name = str_of(msg, "name")?.to_string();
    let before: Vec<u32> = ld.instances().await?.iter().map(|i| i.index).collect();
    let command = if action == "copy" { "copy" } else { "add" };
    let mut args = vec![command.to_string(), "--name".into(), name.clone()];
    if action == "copy" {
        let from = msg.get("from").and_then(Value::as_u64).ok_or_else(|| anyhow!("thiếu máy nguồn"))?;
        instance(ld, from as u32).await?;
        args.extend(["--from".into(), from.to_string()]);
    }
    let dir = ld.dir().await.ok_or_else(|| anyhow!("không tìm thấy thư mục LDPlayer"))?;
    // Copying a large instance can take minutes: let it finish in the background if need be.
    let task = tokio::spawn(async move {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        console_in(&dir, &refs, Duration::from_secs(1800)).await
    });
    match tokio::time::timeout(REPLY_WITHIN, task).await {
        Ok(joined) => {
            joined.map_err(|e| anyhow!("{e}"))??;
            let new = ld.instances().await?.into_iter().find(|i| !before.contains(&i.index)).map(|i| i.index);
            let Some(new) = new else { bail!("LDPlayer không tạo được máy mới") };
            enable_adb(ld, new).await;
            Ok(json!({ "index": new }))
        }
        Err(_) => Ok(json!({ "pending": true })),
    }
}

/// Turns on "ADB debugging (local)" in a stopped instance's config: new instances have it off, and
/// without adb the web page can neither show nor control them. Takes effect at the next launch.
async fn enable_adb(ld: &LdEnv, index: u32) {
    let Some(dir) = ld.dir().await else { return };
    let path = dir.join("vms").join("config").join(format!("leidian{index}.config"));
    let Ok(text) = tokio::fs::read_to_string(&path).await else { return };
    const KEY: &str = "\"basicSettings.adbDebug\"";
    let updated = match text.find(KEY) {
        Some(at) => {
            let rest = &text[at + KEY.len()..];
            let value: String = rest.chars().skip_while(|c| *c == ':' || c.is_whitespace()).take_while(|c| c.is_ascii_digit()).collect();
            if value != "0" {
                return; // already on (1 = local, 2 = remote)
            }
            let start = at + KEY.len() + rest.find('0').unwrap_or(0);
            format!("{}1{}", &text[..start], &text[start + 1..])
        }
        None => match text.find('{') {
            Some(brace) => format!("{}{{
    {KEY}: 1,{}", &text[..brace], &text[brace + 1..]),
            None => return,
        },
    };
    match tokio::fs::write(&path, updated).await {
        Ok(()) => tracing::info!("enabled ADB debugging for instance #{index}"),
        Err(e) => tracing::warn!("enable ADB for #{index}: {e}"),
    }
}

// ---------------------------------------------------------------- screenshots

async fn adb_bytes(ld: &LdEnv, serial: &str, args: &[&str], timeout: Duration) -> Result<Vec<u8>> {
    let mut cmd = Command::new(ld.adb().await);
    cmd.arg("-s").arg(serial).args(args).kill_on_drop(true);
    no_window(&mut cmd);
    let out = tokio::time::timeout(timeout, cmd.output()).await.map_err(|_| anyhow!("adb timed out"))??;
    if !out.status.success() {
        bail!("adb: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Raw `screencap` (no PNG encoding on the Android side), scaled down to a JPEG thumbnail.
async fn screenshot(ld: &LdEnv, serial: &str) -> Result<Vec<u8>> {
    let raw = adb_bytes(ld, serial, &["exec-out", "screencap"], Duration::from_secs(10)).await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        if raw.len() < 12 {
            bail!("screencap trả về rỗng");
        }
        let w = u32::from_le_bytes(raw[0..4].try_into()?);
        let h = u32::from_le_bytes(raw[4..8].try_into()?);
        let pixels = (w as usize) * (h as usize) * 4;
        if w == 0 || h == 0 || raw.len() < pixels + 12 {
            bail!("screencap không đọc được ({w}x{h}, {} bytes)", raw.len());
        }
        // Header is 12 bytes (Android ≤ 8) or 16 (with a colour space field).
        let header = raw.len() - pixels;
        let img = image::RgbaImage::from_raw(w, h, raw[header..].to_vec()).ok_or_else(|| anyhow!("bad image"))?;
        let scale = (THUMB_LONG_SIDE as f64 / w.max(h) as f64).min(1.0);
        let (tw, th) = (((w as f64 * scale) as u32).max(1), ((h as f64 * scale) as u32).max(1));
        let small = image::imageops::thumbnail(&img, tw, th);
        let rgb = image::DynamicImage::ImageRgba8(small).to_rgb8();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 75).encode_image(&rgb)?;
        Ok(jpeg)
    })
    .await?
}

// ---------------------------------------------------------------- APK install

async fn install_apk(msg: Value, cfg: Config, hub: Arc<Hub>, send: impl Fn(Value) + Send + Sync + 'static) {
    let Some(job) = msg.get("job").and_then(Value::as_str).map(str::to_string) else { return };
    let report = |state: &str, progress: Option<f64>, message: &str| {
        send(json!({ "type": "job", "job": job, "state": state, "progress": progress, "message": message }));
    };
    let file = std::env::temp_dir().join(format!("ldremote-{job}.apk"));
    let result = install_inner(&msg, &cfg, &hub.ld, &job, &file, &report).await;
    let _ = tokio::fs::remove_file(&file).await;
    match result {
        Ok(()) => {
            let name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            tracing::info!("apk {name} installed");
            report("done", Some(1.0), "");
        }
        Err(e) => {
            tracing::warn!("apk install: {e:#}");
            report("error", None, &friendly(&e));
        }
    }
}

async fn install_inner(msg: &Value, cfg: &Config, ld: &LdEnv, job: &str, file: &Path, report: &impl Fn(&str, Option<f64>, &str)) -> Result<()> {
    let index = index_of(msg)?;
    let size = msg.get("size").and_then(Value::as_u64).unwrap_or(0);
    let name = msg.get("name").and_then(Value::as_str).unwrap_or("app.apk");
    if !name.to_ascii_lowercase().ends_with(".apk") {
        bail!("chỉ cài được file .apk");
    }
    instance(ld, index).await?;

    // Download from the relay over a WebSocket (same TLS stack and API key as the control link).
    report("downloading", Some(0.0), "");
    let mut ws = crate::relay::connect(cfg, &format!("/agent/file?job={}", uri_encode(job))).await.context("tải file từ server")?;
    let mut out = tokio::fs::File::create(file).await.context("tạo file tạm")?;
    let mut got: u64 = 0;
    let mut last = Instant::now();
    while let Some(m) = tokio::time::timeout(Duration::from_secs(60), ws.next()).await.map_err(|_| anyhow!("tải file quá lâu không có dữ liệu"))? {
        match m.context("tải file từ server")? {
            Message::Binary(b) => {
                out.write_all(&b).await?;
                got += b.len() as u64;
                if last.elapsed() > Duration::from_millis(500) && size > 0 {
                    report("downloading", Some(got as f64 / size as f64), "");
                    last = Instant::now();
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    out.flush().await?;
    drop(out);
    if size > 0 && got != size {
        bail!("file tải về không đủ ({got}/{size} bytes)");
    }

    // Start the instance if needed and wait for Android to boot.
    let serial = match serial(ld, index).await {
        Ok(s) => s,
        Err(_) => {
            report("installing", None, "đang bật máy ảo");
            if !instance(ld, index).await?.running {
                enable_adb(ld, index).await;
            }
            console(ld, &["launch", "--index", &index.to_string()], Duration::from_secs(30)).await?;
            let start = Instant::now();
            loop {
                if let Ok(s) = serial(ld, index).await {
                    if adb(ld, &s, &["shell", "getprop", "sys.boot_completed"], Duration::from_secs(10)).await.is_ok_and(|o| o.trim() == "1") {
                        break s;
                    }
                }
                if start.elapsed() > Duration::from_secs(180) {
                    bail!("máy ảo không khởi động kịp");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    };

    report("installing", None, "");
    let path: PathBuf = file.to_path_buf();
    let out = adb(ld, &serial, &["install", "-r", &path.to_string_lossy()], Duration::from_secs(600)).await?;
    if !out.contains("Success") {
        bail!("{}", out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("cài đặt thất bại").trim());
    }
    Ok(())
}
