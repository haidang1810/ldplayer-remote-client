// LD Remote agent: a system-tray app that keeps an outbound connection to the relay and streams
// LDPlayer instances (scrcpy) to browsers. Single .exe, static CRT, no runtime to install.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
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
use tray_icon::{Icon, TrayIconBuilder};

use config::{Config, Shared, Status};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "LDRemote";

fn main() {
    if !single_instance() {
        // Already running: just show its settings page would need IPC; keep it simple.
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

    if !shared.config().is_complete() {
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

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(300));
        if let Event::NewEvents(StartCause::Init) = event {
            tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu.clone()))
                .with_tooltip("LD Remote")
                .with_icon(make_icon(false))
                .build()
                .ok();
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
                util::open(&util::local_dir().join("logs").to_string_lossy());
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
        key.set_value(RUN_VALUE, &format!("\"{}\"", exe.display()))
    } else {
        match key.delete_value(RUN_VALUE) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}
