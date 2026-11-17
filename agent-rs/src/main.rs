// LD Remote agent: a system-tray app that keeps an outbound connection to the relay and streams
// LDPlayer instances (scrcpy) to browsers. Single .exe, static CRT, no runtime to install.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod gpu;
mod hub;
mod ldplayer;
mod pacer;
mod relay;
mod rtc;
mod scrcpy;
mod settings;
mod util;

use std::sync::Arc;
use std::time::{Duration, Instant};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use config::{Config, Shared, Status};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "LDRemote";

/// Started by Windows at logon (Run key): stay quiet in the tray.
const AUTOSTART_ARG: &str = "--autostart";

fn settings_url_file() -> std::path::PathBuf {
    util::local_dir().join("settings.url")
}

fn main() {
    #[cfg(windows)]
    unsafe {
        // Real pixel coordinates for window capture and cropping on scaled (125 %, 150 %) displays.
        use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    if std::env::args().nth(1).as_deref() == Some("--gpu-test") {
        return gpu_test();
    }
    let autostarted = std::env::args().any(|a| a == AUTOSTART_ARG);
    if !single_instance() {
        // Already running (the user double-clicked the exe again): show its settings page.
        if let Ok(url) = std::fs::read_to_string(settings_url_file()) {
            util::open(url.trim());
        }
        return;
    }
    #[cfg(windows)]
    unsafe {
        // 1 ms timer resolution: touch replay and frame pacing rely on short sleeps.
        windows_sys::Win32::Media::timeBeginPeriod(1);
    }
    let _log_guard = init_logging();
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing::info!("LD Remote {} starting", env!("CARGO_PKG_VERSION"));

    let shared = Shared::new(Config::load());
    let settings = match settings::start(shared.clone()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("settings page: {e}");
            return;
        }
    };

    // Agent runtime on its own threads; the main thread runs the tray's message loop.
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("tokio runtime");
    let hub = {
        let shared = shared.clone();
        rt.block_on(async move {
            let jar = scrcpy::extract_server(&util::local_dir()).expect("extract scrcpy-server");
            let ld = Arc::new(ldplayer::LdEnv::new(&shared.config().ldplayer_dir));
            hub::Hub::new(shared, ld, jar)
        })
    };
    rt.spawn(relay::run(shared.clone(), hub.clone()));

    let _ = std::fs::write(settings_url_file(), &settings.url);
    if autostart_enabled() {
        let _ = set_autostart(true); // refresh the Run entry (path and --autostart flag)
    }
    // Opened by hand: show the settings page right away so it is obvious the agent is running.
    if !autostarted || !shared.config().is_complete() {
        util::open(&settings.url);
    }

    run_tray(shared, hub, rt, settings.url);
}

fn run_tray(shared: Arc<Shared>, hub: Arc<hub::Hub>, rt: tokio::runtime::Runtime, settings_url: String) -> ! {
    let event_loop = EventLoopBuilder::new().build();
    let status_item = MenuItem::new("Đang khởi động…", false, None);
    let open_web = MenuItem::new("Mở trang điều khiển", true, None);
    let open_settings = MenuItem::new("Cài đặt (API key, server)…", true, None);
    let autostart = CheckMenuItem::new("Chạy cùng Windows", true, autostart_enabled(), None);
    let open_logs = MenuItem::new("Xem log", true, None);
    let quit = MenuItem::new("Thoát", true, None);
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &open_web,
        &open_settings,
        &autostart,
        &open_logs,
        &PredefinedMenuItem::separator(),
        &quit,
    ]);

    let mut tray = None;
    let mut last_status: Option<(Status, usize)> = None;
    let mut rt = Some(rt);
    let menu_rx = MenuEvent::receiver();
    let tray_rx = TrayIconEvent::receiver();

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(300));
        if let Event::NewEvents(StartCause::Init) = event {
            tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu.clone()))
                .with_menu_on_left_click(false)
                .with_tooltip("LD Remote")
                .with_icon(make_icon(false))
                .build()
                .ok();
        }

        // Left click on the tray icon opens the settings page; right click shows the menu.
        while let Ok(ev) = tray_rx.try_recv() {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                util::open(&settings_url);
            }
        }

        while let Ok(ev) = menu_rx.try_recv() {
            if ev.id == open_web.id() {
                let cfg = shared.config();
                if cfg.relay_url.is_empty() {
                    util::open(&settings_url);
                } else {
                    util::open(&cfg.web_url());
                }
            } else if ev.id == open_settings.id() {
                util::open(&settings_url);
            } else if ev.id == autostart.id() {
                let want = autostart.is_checked();
                if let Err(e) = set_autostart(want) {
                    tracing::warn!("autostart: {e}");
                    autostart.set_checked(!want);
                }
            } else if ev.id == open_logs.id() {
                open_latest_log();
            } else if ev.id == quit.id() {
                tracing::info!("quitting");
                hub.shutdown();
                if let Some(rt) = rt.take() {
                    // Give stream actors a moment to stop scrcpy sessions (kills adb shells).
                    rt.shutdown_timeout(Duration::from_millis(800));
                }
                tray = None;
                *flow = ControlFlow::Exit;
                return;
            }
        }

        let now = (shared.status(), shared.viewers.load(std::sync::atomic::Ordering::Relaxed));
        if last_status.as_ref() != Some(&now) {
            let (status, viewers) = &now;
            let mut text = status.label();
            if *viewers > 0 {
                text.push_str(&format!(" · {viewers} người xem"));
            }
            status_item.set_text(&text);
            if let Some(t) = &tray {
                let _ = t.set_tooltip(Some(format!("LD Remote: {text}")));
                let _ = t.set_icon(Some(make_icon(*status == Status::Connected)));
            }
            last_status = Some(now);
        }
    })
}

/// 32×32 tray icon drawn in code: a phone outline, green when connected, grey otherwise.
fn make_icon(connected: bool) -> Icon {
    let (r, g, b) = if connected { (0x34, 0xd3, 0x99) } else { (0x8b, 0x93, 0xa5) };
    let size = 32usize;
    let mut rgba = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let (x0, x1, y0, y1) = (8, 23, 2, 29);
            let inside = x >= x0 && x <= x1 && y >= y0 && y <= y1;
            let corner = (x <= x0 + 1 || x >= x1 - 1) && (y <= y0 + 1 || y >= y1 - 1) && !(x > x0 && x < x1 && (y == y0 + 1 || y == y1 - 1));
            let screen = x >= x0 + 3 && x <= x1 - 3 && y >= y0 + 4 && y <= y1 - 6;
            let button = (x as i32 - 15).abs() <= 1 && y >= y1 - 3 && y <= y1 - 2;
            let i = (y * size + x) * 4;
            if inside && !corner && !screen && !button {
                rgba[i..i + 4].copy_from_slice(&[r, g, b, 255]);
            } else if screen {
                rgba[i..i + 4].copy_from_slice(&[0x17, 0x1a, 0x21, 255]);
            }
        }
    }
    Icon::from_rgba(rgba, size as u32, size as u32).expect("icon")
}

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let dir = util::local_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("agent")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("LDR_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter).with(fmt::layer().with_ansi(false).with_writer(writer));
    if cfg!(debug_assertions) {
        registry.with(fmt::layer().with_writer(std::io::stderr)).init();
    } else {
        registry.init();
    }
    Some(guard)
}

fn single_instance() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = "Local\\LDRemoteAgent\0".encode_utf16().collect();
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        // Leak the handle: it must live as long as the process.
        !handle.is_null() && GetLastError() != ERROR_ALREADY_EXISTS
    }
    #[cfg(not(windows))]
    true
}

fn autostart_enabled() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
        .is_ok()
}

fn set_autostart(on: bool) -> std::io::Result<()> {
    let (key, _) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
    if on {
        let exe = std::env::current_exe()?;
        key.set_value(RUN_VALUE, &format!("\"{}\" {AUTOSTART_ARG}", exe.display()))
    } else {
        match key.delete_value(RUN_VALUE) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Opens today's (newest) log file in Notepad; the folder if there is none yet.
fn open_latest_log() {
    let dir = util::local_dir().join("logs");
    let newest = std::fs::read_dir(&dir).ok().and_then(|entries| {
        entries
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("agent"))
            .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
            .map(|e| e.path())
    });
    match newest {
        Some(file) => {
            if let Err(e) = std::process::Command::new("notepad.exe").arg(file).spawn() {
                tracing::warn!("open log: {e}");
            }
        }
        None => util::open(&dir.to_string_lossy()),
    }
}

/// `ldremote --gpu-test <top hwnd> <render hwnd> <seconds> <out.h264> [WxH]`: runs the GPU pipeline alone.
fn gpu_test() {
    let a: Vec<String> = std::env::args().collect();
    let (top, render, secs, out) = (a[2].parse().unwrap(), a[3].parse().unwrap(), a[4].parse::<u64>().unwrap(), a[5].clone());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let display = a.get(6).and_then(|s| s.split_once('x')).and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((0, 0));
    let src = gpu::start(gpu::GpuOpts { top, render, bit_rate: 12_000_000, max_fps: 60, max_size: 0, display }, tx);
    let start = Instant::now();
    let mut file = Vec::new();
    let (mut frames, mut keys, mut bytes) = (0u64, 0u64, 0usize);
    let mut first = None;
    while start.elapsed() < Duration::from_secs(secs) {
        match rx.try_recv() {
            Ok(gpu::GpuEvent::Size { width, height }) => println!("size {width}x{height}"),
            Ok(gpu::GpuEvent::Packet { key, data, .. }) => {
                first.get_or_insert(start.elapsed());
                frames += 1;
                keys += key as u64;
                bytes += data.len();
                file.extend_from_slice(&data);
                if frames == 90 {
                    src.send(gpu::GpuCtl::Keyframe);
                    src.send(gpu::GpuCtl::Bitrate(2_000_000));
                }
            }
            Ok(gpu::GpuEvent::Closed(e)) => {
                println!("closed: {e:?}");
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    drop(src);
    let _ = std::fs::write(&out, &file);
    println!("frames={frames} keys={keys} bytes={bytes} first={first:?} avg_kbps={}", bytes as f64 * 8.0 / 1000.0 / secs as f64);
}
