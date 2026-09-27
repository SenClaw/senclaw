# SenClaw — Quick Start

> Bản Rust (`senclaw`, env `SENCLAW_*`). Chi tiết cài đặt, cấu hình và cấu
> trúc thư mục đầy đủ ở [README](../README.md) / [README.vi](../README.vi.md).
> Tài liệu này là đường ngắn nhất từ máy trống tới lượt chat đầu tiên.

## 1. Cài

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/NortonBen/SenClaw/main/scripts/install.sh | bash
# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/NortonBen/SenClaw/main/scripts/install.ps1 | iex"
```

Binary vào `~/.senclaw/bin`. Hoặc từ mã nguồn:

```bash
git clone https://github.com/NortonBen/SenClaw.git && cd SenClaw
npm run build:web          # giao diện web → web/dist
cargo build --release      # daemon → target/release/senclaw
```

## 2. Chạy daemon

```bash
senclaw web                # tải Web UI lần đầu, mở http://127.0.0.1:18788
```

Daemon bind `127.0.0.1` (UI 18788, WebSocket 18789). Muốn mở ra LAN:
`SENCLAW_UI_BIND_HOST=0.0.0.0` + token (`~/.senclaw/api_token`) — xem
[remote-access-security.md](remote-access-security.md).

Ứng dụng desktop (Flutter) tự spawn daemon: `senclaw install desktop`.

## 3. Chọn model

Web UI → **Settings → Models** → thêm config (OpenAI-compatible hoặc
Anthropic), hoặc **Sign in** với OAuth (Claude, Codex, …). Model local:
cài Space App `mlx-lm` (Apple Silicon) hoặc `candle`, model xuất hiện
cùng picker. Lưu ở `~/.senclaw/config.json`.

## 4. Chat đầu tiên

Web UI → **New chat** → chọn thư mục làm việc (tuỳ chọn) → gõ. Chat trong
một git repo có sẵn: checkpoint sau mỗi lần sửa (drawer **Changes**), outline
repo trong prompt, diagnostics từ language server nếu có trên PATH — xem
[code-session-api.md](code-session-api.md), [repo-map.md](repo-map.md),
[lsp-diagnostics.md](lsp-diagnostics.md).

## 5. Nối kênh nhắn tin

- **Telegram**: Settings → Channels → thêm bot token (`@BotFather`). Nhắn
  cho bot từ chat mới → bot trả **mã 8 ký tự** → duyệt ở Settings → Channels
  → Pairing (hoặc gõ `pair approve <MÃ>` trong chat web). Chi tiết:
  [telegram-pairing.md](telegram-pairing.md).
- **Feishu/Lark**, **WeChat (iLink)**: Settings → Channels; hướng dẫn trong
  skill `bot-channels` (`skills/bot-channels/assets/`).
- **Mobile**: app `channel_app` ghép đôi qua QR (relay), xem
  [CHANNEL_APP_DESIGN.md](CHANNEL_APP_DESIGN.md).

## 6. Biến môi trường hay dùng

| Biến | Mặc định | Ý nghĩa |
|---|---|---|
| `SENCLAW_UI_PORT` / `SENCLAW_WS_PORT` | 18788 / 18789 | cổng UI / WebSocket |
| `SENCLAW_UI_BIND_HOST` | `127.0.0.1` | `0.0.0.0` để mở ra ngoài (cần token) |
| `SENCLAW_AUTH_MODE` | `auto` | `always` sau reverse proxy, `off` khi ingress đã xác thực |
| `TELEGRAM_BOT_TOKEN` | — | bot mặc định (có thể đặt trong UI) |
| `SENCLAW_REPO_MAP_TOKENS` | 2000 | ngân sách repo map, `0` tắt |
| `SENCLAW_LSP` | `1` | `0` tắt diagnostics |
| `MAX_CONCURRENT_AGENTS` | 5 | agent chạy song song |

Đặt trong `.env` cạnh binary hoặc môi trường shell.

## 7. Dữ liệu nằm đâu

`~/.senclaw/` (config, `senclaw.db`, checkpoints, repo-map, worktrees,
trajectories, model cache) và `~/senclaw/` (agents, workspace, wiki, Space
Apps). Sơ đồ đầy đủ ở README mục *Runtime Layout*.

## 8. Lệnh hữu ích

```bash
senclaw web --force          # tải lại Web UI
senclaw pairing approve XXXX # duyệt mã ghép đôi từ terminal
senclaw create app my-app    # scaffold Space App
senclaw acp                  # chạy làm agent ACP cho Zed/JetBrains
cargo test                   # test daemon (từ mã nguồn)
```

## 9. Khi có lỗi

- Trắng màn hình / không kết nối: daemon có chạy? `lsof -nP -iTCP:18788`.
- Bot không trả lời: chat đã được duyệt pairing chưa? Log daemon
  `[SenClaw]`/`[Telegram]`.
- Model 400/401: kiểm Settings → Models → **Test**; OAuth hết hạn → Sign in lại.
