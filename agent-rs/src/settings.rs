// Settings page served on 127.0.0.1 (a random port and a secret path), opened in the default
// browser from the tray. Works on any Windows install without extra UI libraries.
use std::io::Read;
use std::sync::Arc;
use tiny_http::{Header, Method, Response, Server};

use crate::config::Shared;
use crate::util::{form_decode, html_escape};

pub struct Settings {
    pub url: String,
}

pub fn start(shared: Arc<Shared>) -> std::io::Result<Settings> {
    let server = Server::http("127.0.0.1:0").map_err(|e| std::io::Error::other(e.to_string()))?;
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
    let token = uuid::Uuid::new_v4().simple().to_string();
    let url = format!("http://127.0.0.1:{port}/{token}");
    let path = format!("/{token}");
    std::thread::Builder::new()
        .name("settings".into())
        .spawn(move || {
            for mut req in server.incoming_requests() {
                // Only the secret path; other local pages can't guess it.
                let ok_path = req.url() == path || req.url() == format!("{path}/status");
                if !ok_path {
                    let _ = req.respond(Response::from_string("Not found").with_status_code(404));
                    continue;
                }
                if req.url().ends_with("/status") {
                    let body = serde_json::json!({
                        "status": shared.status().label(),
                        "viewers": shared.viewers.load(std::sync::atomic::Ordering::Relaxed),
                    });
                    let _ = req.respond(Response::from_string(body.to_string()).with_header(header("Content-Type", "application/json")));
                    continue;
                }
                let mut notice = String::new();
                if *req.method() == Method::Post {
                    let mut body = String::new();
                    let _ = req.as_reader().take(16 * 1024).read_to_string(&mut body);
                    let mut cfg = shared.config();
                    for pair in body.split('&') {
                        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                        let v = form_decode(v).trim().to_string();
                        match k {
                            "relay_url" => cfg.relay_url = v,
                            "api_key" if !v.is_empty() => cfg.api_key = v,
                            "name" if !v.is_empty() => cfg.name = v.chars().take(40).collect(),
                            "ldplayer_dir" => cfg.ldplayer_dir = v,
                            "video_source" if v == "auto" || v == "scrcpy" => cfg.video_source = v,
                            _ => {}
                        }
                    }
                    notice = match cfg.save() {
                        Ok(()) => "Đã lưu. Agent đang kết nối lại…".into(),
                        Err(e) => format!("Không lưu được: {e}"),
                    };
                    *shared.config.lock().unwrap() = cfg;
                    shared.reconnect.notify_one();
                }
                let html = page(&shared, &notice);
                let _ = req.respond(
                    Response::from_string(html)
                        .with_header(header("Content-Type", "text/html; charset=utf-8"))
                        .with_header(header("Cache-Control", "no-store"))
                        .with_header(header("X-Frame-Options", "DENY"))
                        .with_header(header("Referrer-Policy", "no-referrer")),
                );
            }
        })?;
    Ok(Settings { url })
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn page(shared: &Shared, notice: &str) -> String {
    let cfg = shared.config();
    let key_hint = if cfg.api_key.is_empty() {
        "Chưa có".to_string()
    } else {
        let k = cfg.api_key.trim();
        format!("Đã lưu ({}…)", html_escape(&k.chars().take(10).collect::<String>()))
    };
    let notice = if notice.is_empty() { String::new() } else { format!(r#"<div class="notice">{}</div>"#, html_escape(notice)) };
    format!(
        r#"<!doctype html>
<html lang="vi"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>LD Remote · Cài đặt</title>
<style>
:root {{ --bg:#0f1115; --card:#171a21; --line:#2a2f3a; --text:#e6e8ee; --muted:#8b93a5; --accent:#34d399; color-scheme: dark; }}
* {{ box-sizing: border-box; }}
body {{ margin:0; background:var(--bg); color:var(--text); font:15px/1.5 system-ui,"Segoe UI",sans-serif; display:grid; place-items:center; min-height:100vh; padding:16px; }}
.card {{ width:min(480px,100%); background:var(--card); border:1px solid var(--line); border-radius:14px; padding:24px; }}
h1 {{ font-size:20px; margin:0 0 4px; }} p {{ color:var(--muted); margin:0 0 18px; }}
label {{ display:block; font-size:13px; color:var(--muted); margin:14px 0 6px; }}
input, select {{ width:100%; padding:10px 12px; font:inherit; color:var(--text); background:#1f232c; border:1px solid var(--line); border-radius:9px; }}
input:focus, select:focus {{ outline:2px solid var(--accent); outline-offset:1px; }}
button {{ margin-top:20px; width:100%; padding:11px; font:inherit; font-weight:600; border:0; border-radius:9px; background:var(--accent); color:#062b1e; cursor:pointer; }}
.status {{ display:flex; gap:8px; align-items:center; padding:10px 12px; background:#1f232c; border-radius:9px; }}
.dot {{ width:10px; height:10px; border-radius:50%; background:var(--muted); }} .dot.on {{ background:var(--accent); }}
.notice {{ margin-bottom:14px; padding:10px 12px; border:1px solid var(--accent); border-radius:9px; }}
small {{ color:var(--muted); }} a {{ color:var(--accent); }}
</style></head><body>
<form class="card" method="post" autocomplete="off">
  <h1>LD Remote</h1>
  <p>Agent đang chạy nền ở <b>khay hệ thống</b> (góc dưới bên phải, cạnh đồng hồ). Đóng trang này không tắt agent; bấm vào biểu tượng điện thoại ở khay để mở lại trang này, chuột phải để thoát.</p>
  {notice}
  <div class="status"><span class="dot" id="dot"></span><span id="status">…</span></div>
  <label for="relay_url">Địa chỉ server</label>
  <input id="relay_url" name="relay_url" value="{relay}" placeholder="https://ten-mien-cua-ban" required>
  <label for="api_key">API key <small>({key_hint})</small></label>
  <input id="api_key" name="api_key" placeholder="ldr_… (để trống nếu giữ key cũ)" spellcheck="false">
  <small>Tạo key trên trang web: đăng nhập → Tài khoản → API key. Một key dùng được cho nhiều PC.</small>
  <label for="name">Tên PC hiển thị trên web</label>
  <input id="name" name="name" value="{name}" maxlength="40">
  <label for="ldplayer_dir">Thư mục LDPlayer <small>(để trống: tự tìm; đổi xong cần thoát và mở lại agent)</small></label>
  <input id="ldplayer_dir" name="ldplayer_dir" value="{ld}" placeholder="D:\LDPlayer\LDPlayer14">
  <label for="video_source">Nguồn hình <small>(đổi xong cần thoát và mở lại agent)</small></label>
  <select id="video_source" name="video_source">
    <option value="auto"{auto_sel}>Tự động: chụp cửa sổ LDPlayer, nén bằng GPU (nét, mượt hơn)</option>
    <option value="scrcpy"{scrcpy_sel}>scrcpy: nén bên trong máy ảo (không cần hiện cửa sổ)</option>
  </select>
  <small>Chế độ GPU cần cửa sổ LDPlayer nằm trên màn hình (bị che vẫn được). Agent tự mở lại cửa sổ đang thu nhỏ và đưa nó xuống dưới cùng khi có người xem.</small>
  <button type="submit">Lưu và kết nối</button>
</form>
<script>
async function poll() {{
  try {{
    const r = await fetch(location.pathname + '/status', {{ cache: 'no-store' }});
    const s = await r.json();
    document.getElementById('status').textContent = s.status + (s.viewers ? ` · ${{s.viewers}} người đang xem` : '');
    document.getElementById('dot').className = 'dot' + (s.status === 'Đã kết nối' ? ' on' : '');
  }} catch {{ document.getElementById('status').textContent = 'Agent đã tắt'; }}
}}
poll(); setInterval(poll, 2000);
</script>
</body></html>"#,
        relay = html_escape(&cfg.relay_url),
        name = html_escape(&cfg.name),
        ld = html_escape(&cfg.ldplayer_dir),
        auto_sel = if cfg.video_source == "scrcpy" { "" } else { " selected" },
        scrcpy_sel = if cfg.video_source == "scrcpy" { " selected" } else { "" },
    )
}
