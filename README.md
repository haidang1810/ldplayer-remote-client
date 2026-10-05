# LDPlayer Remote: client (agent)

Phần chạy trên **PC có LDPlayer**: lấy hình từ bên trong Android bằng `scrcpy-server` v4.1
(H.264), nhận lệnh cảm ứng và bàn phím, rồi phát luồng ra trình duyệt. Có hai chế độ:

| Chế độ | Lệnh | Dùng khi |
|---|---|---|
| **Agent** | `npm run agent` | Truy cập từ bất cứ đâu, qua [ldplayer-remote-server](https://github.com/haidang1810/ldplayer-remote-server) trên VPS |
| **Local** | `npm start` | Không cần VPS: dùng trên `localhost`, LAN (`--https`) hoặc Tailscale |

```
[Trình duyệt]──https──▶[VPS: server]◀──wss (PC tự kết nối ra)──[PC: agent]──adb──▶[LDPlayer]
```

- Hình lấy từ bên trong Android nên tắt màn hình PC, thu nhỏ hay che LDPlayer vẫn chạy.
- Điều khiển: multi-touch, chuột (chuột phải = Back, chuột giữa = Home), cuộn, bàn phím. Chữ có
  dấu (tiếng Việt) được dán qua clipboard. Nút "Phím game" gửi mã phím thô.
- Chống trễ dồn: trình duyệt báo nhận từng frame. Nếu hàng đợi trên đường truyền vượt RTT + 250 ms
  thì agent bỏ frame và gửi keyframe mới.

## Chuẩn bị

1. LDPlayer → Cài đặt → Cài đặt khác → Gỡ lỗi ADB → **Kết nối cục bộ** → Lưu → khởi động lại máy ảo.
   (Đừng bật "kết nối từ xa".)
2. Node.js 22+, rồi `npm install`. Lệnh này tự tải `scrcpy-server` và kiểm tra SHA-256.

## Chế độ agent (qua VPS)

1. Chép `agent.env.example` thành `agent.env`, điền:
   - `RELAY_URL=wss://tên-miền-của-bạn`
   - `AGENT_KEY=` khoá in ra khi chạy `npm run setup` trên server
2. Chạy thử: `npm run agent`. Thấy `connected to relay` là xong.
3. Cho agent tự chạy mỗi khi đăng nhập Windows:
   `powershell -ExecutionPolicy Bypass -File scripts\install-agent-task.ps1` (log ghi ra `agent.log`)
4. Tắt chế độ ngủ của PC (Settings → Power → Sleep: Never).

## Chế độ local

```bash
npm start
```

Mở link `http://localhost:8080/?token=…` in ra ở console. Token (lưu trong `.token`) cũng chính là mật khẩu.
Từ máy khác trong LAN thì chạy `npm start -- --https`, vì WebCodecs chỉ chạy trên https hoặc localhost.

## Tuỳ chọn luồng

| Biến môi trường / cờ | Mặc định | Ghi chú |
|---|---|---|
| `BIT_RATE` / `--bit-rate` | `8M` | 4G nên `3M`–`4M` |
| `MAX_SIZE` / `--max-size` | `0` (gốc) | cạnh dài tối đa, ví dụ `1280` |
| `MAX_FPS` / `--max-fps` | `60` | |
| `VIDEO_ENCODER` / `--encoder` | tự chọn | xem danh sách: `npm run encoders` |

## Cấu trúc

- `agent/`: kết nối ra server; với mỗi người xem thì mở thêm một channel
- `server/`: giao thức scrcpy, gom luồng cho nhiều người xem (`hub.js`), adb/ldconsole, đăng nhập, chế độ local
- `public/`: web client (WebCodecs + canvas). Bản sao giống hệt trong repo server
