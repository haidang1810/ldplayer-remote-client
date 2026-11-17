# LD Remote agent (Rust)

Một file `ldremote.exe` (~9 MB) chạy nền ở khay hệ thống trên PC có LDPlayer. Máy Windows mới cài vẫn chạy được: C runtime được build tĩnh, `scrcpy-server` nhúng sẵn trong exe, chỉ dùng DLL có sẵn của Windows. Agent dùng `adb.exe` / `ldconsole.exe` trong thư mục LDPlayer.

## Dùng

1. Chạy `ldremote.exe`. Lần đầu, trang cài đặt tự mở trong trình duyệt (`http://127.0.0.1:<port>/<mã bí mật>`).
2. Nhập địa chỉ server (vd `https://ldlink.example.com`) và **API key** tạo trên web (đăng nhập → API key). Bấm *Lưu và kết nối*.
3. Biểu tượng điện thoại ở khay chuyển xanh khi đã kết nối. Chuột phải để: mở trang điều khiển, mở cài đặt, bật *Chạy cùng Windows*, xem log, thoát.

Một key dùng được cho nhiều PC: mỗi PC tự tạo mã máy riêng nên hiện thành từng máy trên web.

- Cấu hình: `%APPDATA%\LDRemote\config.json` (đổi thư mục bằng biến `LDR_CONFIG_DIR`).
- Log: `%LOCALAPPDATA%\LDRemote\logs\agent.<ngày>.log` (giữ 7 ngày, mức log đổi bằng `LDR_LOG`, vd `LDR_LOG=debug`).
- Đổi thư mục LDPlayer trong trang cài đặt cần khởi động lại agent.

## Build

Cần Rust (MSVC toolchain) và file `../vendor/scrcpy-server-v4.1` (`npm run fetch-server` ở thư mục gốc).

```bash
cargo build --release
```

File ra ở `target/release/ldremote.exe`.

## Bên trong

| File | Việc |
|---|---|
| `relay.rs` | kết nối ra relay (`/agent`, `Authorization: Bearer <key>`, `X-Agent-Name`, `X-Machine-Id`), trả danh sách máy, mở channel cho từng người xem |
| `hub.rs` | mỗi serial adb một actor: một phiên scrcpy chia cho mọi người xem; WebSocket có chống nghẽn theo ack, WebRTC khi trình duyệt xin |
| `rtc.rs` | WebRTC bằng [str0m](https://github.com/algesten/str0m): H.264 của scrcpy đóng gói thẳng vào RTP, DataChannel nhận thao tác; ứng viên host + srflx (một lần STUN), trình duyệt có TURN |
| `scrcpy.rs` | client scrcpy 4.1 (push jar, adb forward, đọc luồng video/điều khiển) |
| `pacer.rs` | phát lại thao tác chạm đúng nhịp thời gian gốc |
| `ldplayer.rs` | tìm LDPlayer, `ldconsole list2`, `adb devices` |
| `settings.rs` | trang cài đặt trên 127.0.0.1 |
| `main.rs` | khay hệ thống, chạy cùng Windows, một instance duy nhất |

Giao thức với trình duyệt giữ nguyên như agent Node (`agent/`, `src/`), nên dùng chung relay và giao diện web.
