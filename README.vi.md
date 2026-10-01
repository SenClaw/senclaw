<p align="center">
  <img src="docs/images/senclaw-logo.png" alt="SenClaw logo" width="160" />
</p>

<h1 align="center">SenClaw</h1>

<p align="center">
  <em>Một framework đa năng để xây dựng AI agent cá nhân.</em>
</p>

<p align="center">
  <a href="https://github.com/SenClaw/senclaw/releases/latest"><img src="https://img.shields.io/github/v/tag/SenClaw/senclaw?label=version" alt="Latest version" /></a>
  <a href="https://github.com/SenClaw/senclaw/actions/workflows/release.yml"><img src="https://github.com/SenClaw/senclaw/actions/workflows/release.yml/badge.svg" alt="Build status" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-yellow.svg" alt="License: MIT" /></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <strong>Tiếng Việt</strong>
</p>

SenClaw cung cấp lớp hạ tầng chạy quanh mô hình ngôn ngữ lớn: phân quyền, bộ nhớ, tác vụ định kỳ, điều phối nhiều agent, kết nối nhiều kênh chat, Space Apps, mô hình chạy cục bộ và ứng dụng desktop. Mục tiêu là biến một model provider hoặc local model thành một hệ thống AI cá nhân có thể dùng hằng ngày.

---

## Về dự án

SenClaw là một **trạm làm việc AI cá nhân, local-first**: một daemon Rust duy nhất chạy toàn bộ agent, và app desktop Flutter native giám sát nó. Dữ liệu của bạn — hội thoại, ghi chú, lịch, bộ nhớ, wiki — nằm trong SQLite tại `~/.senclaw/` trên máy của bạn; model có thể là cloud provider **hoặc chạy hoàn toàn offline** trên Apple Silicon qua MLX.

Cụ thể gồm:

- **Một trợ lý, mọi nơi** — trò chuyện với cùng một bộ agent từ app desktop, Telegram / Feishu / WeChat, app di động (qua relay), hoặc extension trình duyệt.
- **Agent làm việc thật** — phân quyền tool với phê duyệt human-in-the-loop, chế độ Plan, và team nhiều agent theo DAG (Cowork) cho các tác vụ lớn.
- **Bộ nhớ tích lũy** — knowledge graph nhận thức cộng với các file `memory/*.md` được tự động chắt lọc khi nén ngữ cảnh, và gợi nhớ lại trong các lượt sau.
- **Không gian cá nhân (Space)** — ghi chú, lịch với nhắc hẹn đẩy thông báo hệ thống, và schedule định kỳ chạy agent theo cron.
- **Space Apps** — mini-app full-stack cài thêm được (SSH Manager, DeepWiki, Email, …) mang theo UI, MCP tools và skills riêng.
- **Local models** — inference MLX native cho LLM (Gemma, Qwen, DeepSeek, …), Whisper speech-to-text, TTS, OCR và embeddings — không cần GPU cloud.

### Ảnh màn hình

| Dashboard | Chat |
| --- | --- |
| ![Dashboard](docs/images/screenshots/senclaw-dashboard.png) | ![Chat](docs/images/screenshots/senclaw-chat.png) |

| Plugins (Skills / MCP / Subagents) | Space (Notes · Calendar · Schedules) |
| --- | --- |
| ![Plugins](docs/images/screenshots/senclaw-plugins.png) | ![Space](docs/images/screenshots/senclaw-space.png) |

---

## Điểm nổi bật

- **Runtime cho agent cá nhân**: quản lý vòng đời agent, quyền dùng tool, luồng hỏi lại người dùng, trạng thái workspace và persona riêng cho từng agent.
- **Bộ nhớ và tri thức**: bộ nhớ lai FTS/vector, curated auto-memory, nhật ký hằng ngày và wiki cá nhân quản lý bằng Git.
- **Điều phối nhiều agent**: chạy team theo DAG, virtual workers, dispatch bridge và hỗ trợ subagent.
- **Tác vụ định kỳ**: hỗ trợ chế độ thông báo, script, agent và script kết hợp agent.
- **Gateway đa kênh**: Telegram, Feishu/Lark, WeChat, WebSocket, HTTP API và Web UI.
- **Space Apps**: các micro-app tách biệt như SSH Manager, DeepWiki, Email, Google Workspace và Test Manager, cung cấp tool qua MCP.
- **Tùy chọn AI cục bộ**: inference bằng MLX/Candle, embedding cục bộ, OCR, Whisper speech-to-text và TTS local.
- **Ứng dụng desktop**: app Flutter native (macOS/Windows/Linux/web), giám sát daemon như tiến trình con.
- **App di động**: app Flutter qua relay — chat, sessions, workflows, Space Apps, thông báo đẩy và đồng bộ nền.
- **Bộ chặn an toàn tất định**: chặn SSRF fail-closed cho các tool fetch URL (loopback / link-local / cloud-metadata / dải private) và phân loại lệnh shell chỉ-đọc, áp dụng trước khi model thấy lời gọi rủi ro — bên cạnh ranh giới tin cậy prompt/tool.

---

## Các LLM local được hỗ trợ

SenClaw có **runtime MLX native riêng** (Rust `mlx-rs`, Apple Silicon — không Python, không llama.cpp) cùng backend **Candle** đa nền tảng. Bất kỳ checkpoint HuggingFace nào có `model_type` khớp kiến trúc hỗ trợ đều tải và chạy được từ *Settings → Local Models*:

| Kiến trúc (`model_type`) | Họ model | Ghi chú |
|---|---|---|
| `qwen3`, `qwen2` | Qwen 2.5 / Qwen 3 | Đủ tool-calling + thinking markers; mặc định khuyên dùng cho agent. |
| `qwen3_5` | Qwen 3.5 hybrid | Hybrid GatedDeltaNet linear-attention + attention (quant OptiQ); prefix cache hồi quy bỏ qua ~90% prefill trong vòng lặp agent nhiều lượt. |
| `gemma2`, `gemma3`, `gemma4` | Google Gemma | Gemma 4 chạy backbone text (Per-Layer Embeddings, chia sẻ KV liên tầng); tower vision đang phát triển. |
| `llama` | Llama 3.x | Transformer chuẩn. |
| `deepseek_v2` | DeepSeek-Coder-V2-Lite | Multi-head Latent Attention + Mixture-of-Experts. |
| `ouro` | Ouro LoopLM | Universal transformer lặp theo chiều sâu, có knob `recurrence_steps`. |
| `mamba2`, `falcon_mamba` | State-space models | Decode SSM không attention. |
| `bonsai_q1` | Bonsai Q1 | Kiến trúc thử nghiệm gọn nhẹ. |

Bộ tải một-click đã kiểm chứng end-to-end: **Qwen3 4B Instruct 4-bit** (tools/agent), **Qwen2.5 0.5B** (chat nhanh), **Qwen3.5 0.8B OptiQ** (chat + agentic), **Gemma 4 E2B-it 4-bit / OptiQ** (văn bản dài). Có thể thêm repo tùy ý theo HF id.

Tiện ích runtime: **TurboQuant KV-cache quantization** (TQ3/TQ4 — ngữ cảnh dài với một phần nhỏ RAM), prefix caching, và parser streaming tool-call/thinking theo dialect từng model. Ngoài LLM, stack local còn gồm **Whisper** speech-to-text (có tiếng Việt), **TTS** (giọng native macOS; ZipVoice đang phát triển), **PaddleOCR** (Metal/CoreML) và **embeddings cục bộ**. Build `make run-release` / `make app-build` để có đủ bộ feature.

---

## Cài đặt

### Cài bằng 1 lệnh (macOS / Linux)

```bash
curl -fsSL https://raw.githubusercontent.com/SenClaw/senclaw/main/scripts/install.sh | bash
```

### Windows (PowerShell)

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/SenClaw/senclaw/main/scripts/install.ps1 | iex"
```

Cả hai script tải binary release mới nhất về `~/.senclaw/bin` (`%USERPROFILE%\.senclaw\bin` trên Windows) và tự thêm vào PATH. Muốn cài một phiên bản cụ thể, đặt `SENCLAW_VERSION=v0.3.0` trước khi chạy.

Binary `senclaw` mặc định **không** kèm Web UI hay app desktop — chúng được tải về khi cần:

```bash
senclaw web               # tải bundle Web UI (lần đầu), rồi khởi động daemon phục vụ UI
senclaw install desktop   # tải & cài app desktop native cho nền tảng hiện tại
senclaw uninstall desktop # gỡ app desktop
```

- `senclaw web` lưu bundle UI tại `~/.senclaw/web/dist` và phục vụ tại `http://127.0.0.1:18788`. Dùng `--force` để tải lại, `--version v0.3.0` để chọn phiên bản.
- `senclaw install desktop` tải bundle từ release của repo [`desktop`](https://github.com/SenClaw/desktop/releases), kiểm tra sha256, rồi cài vào `/Applications` (macOS), `%LOCALAPPDATA%\SenClaw\Desktop` kèm shortcut Desktop + Start Menu (Windows), hoặc `~/.senclaw/desktop` kèm shortcut launcher (Linux). Có bản dựng sẵn cho macOS (Apple Silicon), Windows x64 và Linux x64; `--version v0.3.0` để chọn phiên bản. Sau khi cài, app tự cập nhật.

---

## Chạy nhanh (từ mã nguồn)

### 1. Tải mã nguồn

```bash
git clone https://github.com/SenClaw/senclaw.git
cd senclaw
```

### 2. Build giao diện Web

```bash
cd web
npm install
npm run build
cd ..
```

### 3. Build và chạy daemon

```bash
cargo run
```

Để build bản tối ưu:

```bash
cargo build --release
./target/release/senclaw
```

Sau đó mở Web UI:

```bash
open http://127.0.0.1:18788
```

Trên Linux, nếu không có lệnh `open`, hãy mở URL trên bằng trình duyệt.

---

## Cấu hình

SenClaw có thể chạy ở chế độ chỉ Web UI, nhưng các phiên agent cần ít nhất một cấu hình LLM. Sau khi chạy lần đầu, mở:

```text
Settings -> LLM
```

Thêm một provider như OpenAI, Anthropic, DeepSeek, Qwen, OpenRouter, Ollama hoặc endpoint tương thích. Cấu hình được lưu tại:

```text
~/.senclaw/config.json
```

Các thiết lập kênh chat và runtime có thể cấu hình qua `.env`:

```bash
cp .env.example .env
```

Một số giá trị thường dùng:

```env
TELEGRAM_BOT_TOKEN=
ADMIN_TELEGRAM_USER_ID=
FEISHU_APP_ID=
FEISHU_APP_SECRET=
QQ_APP_ID=
QQ_APP_SECRET=
GATEWAY_UI_PORT=18788
GATEWAY_PORT=18789
MAX_CONCURRENT_AGENTS=5
MAX_MESSAGES_PER_GROUP=100
SCHEDULER_INTERVAL_SEC=60
```

Nếu không cấu hình token cho các kênh chat, SenClaw vẫn có thể dùng qua Web UI.

---

## Lệnh thường dùng

```bash
# Kiểm tra code Rust
cargo check

# Chạy test Rust
cargo test

# Chạy daemon
cargo run

# Chạy với các feature local model theo Makefile
make run

# Chạy bản tối ưu cho local model
make run-release

# Chạy Web UI ở chế độ dev
make run-web

# Build browser extension
make build-extension
```

---

## Ứng dụng desktop

SenClaw có app desktop **Flutter** native trong `desktop_app/` (macOS / Windows / Linux / web). Nó thay thế Tauri shell cũ: kết nối trực tiếp tới daemon qua HTTP/WebSocket và **giám sát daemon `senclaw` như tiến trình con** (spawn binary đi kèm, stream log, restart khi cần).

Khi mở app có **cổng khởi động (startup gate)**: nếu daemon đã chạy sẵn thì vào thẳng giao diện; nếu chưa, app spawn daemon đi kèm, hiện màn hình "Starting daemon" cho tới khi HTTP API phản hồi rồi mới chuyển vào màn hình chính (kèm màn hình lỗi có nút Retry nếu daemon không khởi động được).

```bash
# Development (chạy app Flutter; tự adopt daemon đang chạy hoặc spawn mới)
make app-dev

# Bundle production (build daemon với đầy đủ feature Apple Silicon
# — MLX LLM, Whisper ASR, TTS, OCR Metal, embeddings — và nhúng binary
# vào Contents/Resources để supervisor khởi chạy)
make app-build          # macOS
make app-build-windows  # Windows
make app-build-linux    # Linux
make app-build-web      # web

# Cài bản .app vừa build vào /Applications và mở (macOS)
make app-install
```

---

## Cấu trúc dữ liệu runtime

Mặc định SenClaw lưu dữ liệu runtime trong home directory, tách thành phần config/state ẩn và workspace hiển thị với người dùng:

```text
~/.senclaw/                         # config, database, cache model
├── config.json                     # config toàn cục (LLM profiles, toggles)
├── senclaw.db                      # SQLite chính (groups, messages, tasks, events)
├── senclaw_cognitive.db            # graph bộ nhớ nhận thức
├── dispatch-state.json             # trạng thái DAG team
├── hooks.json                      # hooks người dùng
├── llm_logs/                       # log LLM theo từng request
├── models/  local-models/          # local LLM đã tải (MLX/Candle)
├── whisper-models/  tts-models/  ocr-models/
├── managed/skills/                 # skills do Space Apps cài
├── space-apps-data/{app-id}/       # dữ liệu riêng từng app
├── plans/                          # plan đã lưu (Plan mode)
└── workspace-state-{folder}.json   # trạng thái workspace từng agent

~/senclaw/                          # workspace hiển thị với người dùng
├── agents/{folder}/                # mỗi profile agent một thư mục
│   ├── SOUL.md                     # persona
│   ├── memory/                     # curated memory (*.md + index MEMORY.md)
│   └── .sema/sessions/             # các phiên hội thoại
├── workspace/{folder}/             # thư mục làm việc của các chat
├── workspace/space-apps/{app-id}/  # Space Apps đã cài (binary + web_dist)
├── wiki/                           # knowledge base quản lý bằng Git
├── quicknotes/                     # ghi chú Space
├── workflows/  workflow-data/      # workflow đã lưu và dữ liệu chạy
└── virtual-agents/                 # thư mục virtual worker của DAG
```

Hầu hết đường dẫn có thể ghi đè qua `.env` hoặc `~/.senclaw/config.json`.

---

## Cấu trúc dự án

```text
SenClaw/
├── src/                    # Daemon Rust và core runtime
│   ├── agent/              # Agent pool, phân quyền, personas, DAG dispatch
│   ├── zen_core/           # Engine agent (sessions, tools, gọi LLM)
│   ├── channels/           # Adapter Telegram, Feishu/Lark, WeChat, app-relay
│   ├── gateway/            # Gateway HTTP + WebSocket, routing, UI server
│   ├── mcp/                # Các MCP server cung cấp cho agent (space, memory, browser, …)
│   ├── memory/             # Bộ nhớ FTS/vector, curated auto-memory, nhật ký
│   ├── scheduler/          # Task cron/interval/một-lần + nhắc hẹn sự kiện
│   ├── local_model/        # Inference cục bộ MLX/Candle (LLM/ASR/TTS)
│   ├── browser/            # Backend tự động hoá trình duyệt cho senclaw-browser
│   ├── clawhub/            # Marketplace skill ClawHub + relay client
│   ├── marketplace/        # Quản lý marketplace plugin
│   ├── skills/  subagents/ # Skills đi kèm và định nghĩa subagent
│   ├── workflow/           # Trình chạy workflow nhiều agent
│   ├── cli/                # Các lệnh CLI senclaw
│   ├── db/                 # Tầng lưu trữ rusqlite
│   └── wiki/               # Knowledge base quản lý bằng Git
├── desktop_app/            # App desktop Flutter (macOS/Windows/Linux/web)
├── channel_app/            # App di động Flutter (kết nối qua relay)
├── web/                    # Web UI React + Vite (legacy; daemon phục vụ)
├── apps/                   # Space Apps (ssh-manager, deepwiki, email, …)
├── app-space-sdk/          # SDK xây dựng Space Apps
├── hub-backend/            # Backend relay hub (kênh app di động)
├── senclaw-extension-chrome/ # Extension Chrome (điều khiển trình duyệt)
├── skills/                 # Định nghĩa skills đi kèm
├── scripts/                # Script cài đặt install.sh / install.ps1
├── examples/               # Ví dụ app và cách dùng SDK
├── docs/                   # Tài liệu kiến trúc và tính năng
└── tests/                  # Test tích hợp
```

---

## Tài liệu

| Tài liệu | Nội dung |
|---|---|
| [Quick Start](docs/QUICK_START.md) | Cài đặt, cấu trúc runtime và ghi chú sử dụng. |
| [Architecture](docs/ARCHITECTURE.md) | Các lớp hệ thống, luồng khởi động và luồng dữ liệu. |
| [Memory](docs/memory.md) | Thiết kế bộ nhớ và luồng truy xuất. |
| [Curated Memory](docs/curated-memory-design.md) | Auto-memory kiểu Claude Code: chắt lọc khi nén ngữ cảnh + recall. |
| [DAG Team](docs/DAG_Team.md) | Phân rã và thực thi tác vụ nhiều agent. |
| [Cowork](docs/COWORK_DESIGN.md) | Team agent bền vững xây trên DAG dispatch. |
| [Workflows](docs/workflow.md) | Pipeline DAG lưu sẵn, tham số hoá, gồm bước agent + script. |
| [Space Apps](docs/workspace-feature-design.md) | Cách thiết kế và đăng ký Space Apps. |
| [Flutter Desktop](docs/flutter-desktop-migration.md) | Kiến trúc app desktop (supervisor, startup gate). |
| [Mobile Channel App](docs/CHANNEL_APP_DESIGN.md) | App di động Flutter qua relay. |
| [Runtime protocol](docs/runtime-protocol.md) | Hợp đồng giữa daemon và các runtime engine (MLX, llama.cpp, decision, OCR, Whisper, TTS). |
| [Chrome Extension](docs/senclaw-extension-design.md) | Thiết kế extension điều khiển trình duyệt. |
| [ClawHub Plugins & Skills](docs/clawhub-plugins-skills.md) | Marketplace skill và hệ thống plugin. |
| [Prompt Injection Security](docs/prompt-injection-security.md) | Ghi chú bảo mật cho ranh giới tool và prompt. |

---

## Ghi chú phát triển

SenClaw là một crate Rust duy nhất — daemon không link bất kỳ code inference nào (không MLX, không Candle, không ONNX Runtime), nên `cargo build`/`cargo run`/`cargo test` thông thường không cần feature flag hay toolchain riêng theo nền tảng. Web UI là repo riêng (`web-app`, React/Vite); một số Space Apps có manifest riêng trong `apps/`.

Để dùng local model, decision, OCR, Whisper hay TTS, cài runtime tương ứng thay vì bật build feature:

```bash
senclaw runtime list                       # đã cài gì, mỗi slot đang chạy runtime nào
senclaw runtime install llama.cpp-metal    # GGUF chat/embedding/vision (bản build Apple Silicon)
senclaw runtime install sen-ocr            # OCR
```

Xem [Runtime protocol](docs/runtime-protocol.md) để biết danh sách đầy đủ và hợp đồng daemon/runtime.

---

## Đóng góp

Bạn có thể đóng góp bằng issue, pull request, thử nghiệm hoặc thảo luận thiết kế. Hãy giữ thay đổi gọn theo mục tiêu, ghi lại hành vi có ảnh hưởng tới người dùng và thêm test cho các thay đổi runtime có rủi ro.

---

## Giấy phép

[MIT](LICENSE)

---

## Ghi nhận

SenClaw là bản viết lại bằng Rust, phát triển từ — và lấy cảm hứng sâu sắc từ — [**SemaClaw**](https://github.com/midea-ai/SemaClaw) (midea-ai), dự án gốc bằng TypeScript về gateway AI agent đa kênh. SenClaw chạy trên agent runtime [sema-code-core](https://github.com/midea-ai/sema-code-core).

SenClaw cũng tích hợp với marketplace plugin [ClaWHub](https://github.com/openclaw/clawhub), lấy cảm hứng từ [OpenClaw](https://github.com/openclaw/openclaw), [Model Context Protocol](https://modelcontextprotocol.io) và hệ sinh thái công cụ agent mã nguồn mở.
