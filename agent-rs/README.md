# LD Remote agent (Rust)

Một file `ldremote.exe` (~9 MB) chạy nền ở khay hệ thống trên PC có LDPlayer. Máy Windows mới cài vẫn chạy được: C runtime được build tĩnh, `scrcpy-server` nhúng sẵn trong exe, chỉ dùng DLL có sẵn của Windows. Agent dùng `adb.exe` / `ldconsole.exe` trong thư mục LDPlayer.

## Dùng

1. Chạy `ldremote.exe`. Lần đầu, trang cài đặt tự mở trong trình duyệt (`http://127.0.0.1:<port>/<mã bí mật>`).
2. Nhập địa chỉ server (vd `https://ldlink.example.com`) và **API key** tạo trên web (đăng nhập → API key). Bấm *Lưu và kết nối*.
3. Biểu tượng điện thoại ở khay chuyển xanh khi đã kết nối. Chuột phải để: mở trang điều khiển, mở cài đặt, bật *Chạy cùng Windows*, xem log, thoát.

Một key dùng được cho nhiều PC: mỗi PC tự tạo mã máy riêng nên hiện thành từng máy trên web.

Mở lại `ldremote.exe` khi agent đang chạy, hoặc bấm vào biểu tượng ở khay, đều mở trang cài đặt. Khi tự chạy cùng Windows (`--autostart`) thì agent không mở trình duyệt, trừ khi chưa có API key. *Xem log* mở file log hôm nay bằng Notepad.

## Quản lý từ web

Trang web gửi lệnh qua relay, agent chạy bằng `ldconsole.exe` / `adb.exe` của LDPlayer: tạo máy mới hoặc nhân bản, bật / tắt / khởi động lại, đổi tên, xoá, chụp màn hình, cài APK, mở / dừng / gỡ ứng dụng. Agent tự bật *ADB debugging* cho máy mới tạo và cho máy được bật từ web (máy LDPlayer mới mặc định tắt ADB, khi đó web không xem được). Nhân bản máy lớn có thể mất vài phút: web nhận "đang xử lý" và máy mới hiện ra khi xong.

Chạy thêm một agent để thử (cùng lúc với agent chính) bằng thư mục cấu hình riêng: `LDR_CONFIG_DIR=<thư mục> ldremote.exe`.

## Nguồn hình

| | **Tự động (GPU)**, mặc định | **scrcpy** |
|---|---|---|
| Lấy hình | chụp cửa sổ LDPlayer (Windows Graphics Capture), cắt đúng vùng Android | bên trong Android |
| Nén | encoder phần cứng của card (NVENC / AMF / Quick Sync qua Media Foundation) | encoder phần mềm của máy ảo |
| Chất lượng | độ phân giải gốc (vd 1600×900), ~60 fps, bitrate tự chỉnh 0,8–20 Mbps theo mạng | 1280×720, ~40 fps, bitrate cố định |
| Điều kiện | cửa sổ LDPlayer phải nằm trên màn hình (bị che vẫn được) | không |

Ở chế độ GPU, scrcpy chỉ dùng để điều khiển (`video=false`); toạ độ chạm được đổi từ kích thước video sang kích thước màn hình Android. Khi có người xem, agent tự mở cửa sổ đang bị thu nhỏ (hoặc kéo nó về nếu nằm ngoài màn hình), đặt xuống dưới cùng mọi cửa sổ, và thu nhỏ lại khi hết người xem. LDPlayer vẽ Android đúng bằng kích thước cửa sổ, nên nếu cửa sổ nhỏ hơn độ phân giải Android (vd 852×480 cho máy 1280×720) thì agent phóng to cửa sổ lên (vừa màn hình) để hình nét, rồi trả lại kích thước cũ khi hết người xem. Không tìm thấy cửa sổ, không có encoder phần cứng, hoặc capture lỗi thì tự quay về scrcpy.

Bitrate ưu tiên chất lượng: người xem WebRTC bắt đầu ở 12 Mbps, agent giữ tối thiểu 8 Mbps (đệm khi màn hình đứng yên) để ước lượng băng thông TWCC của str0m không tụt, rồi dùng 90 % ước lượng (tối đa `gpu_max_bit_rate`); người xem WebSocket bắt đầu 12 Mbps (tối đa 12 Mbps), giảm 30 % khi bị nghẽn, tăng 15 % sau mỗi 3 giây ổn định. Encoder dùng GOP dài và chỉ tạo keyframe khi cần (người xem mới, mất gói), nên không còn các đợt keyframe định kỳ làm giật hình.

Đổi nguồn hình trong trang cài đặt (cần mở lại agent), hoặc `video_source` (`auto` | `scrcpy`) và `gpu_max_bit_rate` trong `config.json`.

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
| `gpu.rs` | chế độ GPU: WGC → cắt + đổi NV12 bằng D3D11 video processor → H.264 phần cứng (Media Foundation), bitrate và keyframe điều khiển được |
| `scrcpy.rs` | client scrcpy 4.1 (push jar, adb forward, đọc luồng video/điều khiển; hoặc chỉ điều khiển) |
| `pacer.rs` | phát lại thao tác chạm đúng nhịp thời gian gốc |
| `manage.rs` | lệnh từ web: tạo / nhân bản / bật / tắt / đổi tên / xoá máy (`ldconsole`), ảnh chụp màn hình (`screencap` thô → JPEG nhỏ), danh sách / mở / dừng / gỡ ứng dụng, cài APK (tải từ relay qua `/agent/file`, bật máy nếu đang tắt, `adb install -r`) |
| `ldplayer.rs` | tìm LDPlayer, `ldconsole list2`, `adb devices` |
| `settings.rs` | trang cài đặt trên 127.0.0.1 |
| `main.rs` | khay hệ thống, chạy cùng Windows, một instance duy nhất |

Giao thức với trình duyệt giữ nguyên như agent Node (`agent/`, `src/`), nên dùng chung relay và giao diện web.
