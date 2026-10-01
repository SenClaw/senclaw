# Research: jev-ultrafast xử lý hành động website — và độ trễ của engine SenClaw

Ngày: 2026-09-30 (Asia/Saigon) · Nguồn: `browser-use/jev-ultrafast` @ `1231850` (`browser.py`, `agent.py`, `model.py`, `questions.py`, `docs/performance.md`), log thật của máy (`~/.senclaw/llm_logs`, `control-plane/traces`, `logs/runtimes`), đo trong môi trường cô lập (scratch HOME, cổng 28788/28789/28795/28796).
Báo cáo trước: [`research-260929-0143-jev-ultrafast.md`](../../reports/research-260929-0143-jev-ultrafast.md).

## Tóm tắt

- **Phần "tay-mắt" của ta đã ngang jev-ultrafast.** Click 50 ms, observe 1,3 ms, quyết định local 270 ms → ~0,35 s/bước. Chậm "vài giây mỗi hành động" không nằm ở đây.
- **Trên máy thật, độ trễ đến từ 4 chỗ** (đo từ log, không suy đoán):
  1. Mỗi tool call = một lượt LLM chat **6–11 s** (system prompt 102.669 ký tự, 49 tool, `cache_read = 0`). Agent đi từng bước `browser_open → browser_do → browser_read` vì skill `agent-browser` vẫn dạy bộ tool cũ; thêm 1 lượt `ToolSearch` (7 s) mỗi chat. `browser_task` chưa từng được gọi.
  2. `laya-browser` **chưa cài** trên máy thật (`~/.senclaw/local-models/laya/` chỉ có `multilingual`) → mỗi bước của loop rơi về LLM. Không có gì báo.
  3. Extension đang nạp trong Chrome là **bản cũ** (bundle `54933a…`, runtime cần `e768515…`) → driver extension bị từ chối từ 29/09.
  4. Mở trang chờ `load` event: trang kết quả DuckDuckGo 3,1–3,7 s, TikTok có lần chạm trần **15,6 s**; hoặc ngược lại trả về vỏ app (15/65 phần tử).
- **Đã sửa** (chưa push, xem "Đã làm"): readiness theo trạng thái trang thay vì `load`; bỏ các lượt model thừa trong loop; skill + tool mô tả cho engine v2 (1 việc = 1 call); `browser_read {url}`; cảnh báo thiếu decision model; trạng thái toggle (like) và chặn bấm ngược.
- **Còn lại là việc của người dùng**: reload extension, cài `laya-browser`, build lại daemon.

## jev-ultrafast xử lý một hành động thế nào

| Bước | jev-ultrafast | SenClaw (trước đợt này) |
|---|---|---|
| Quyết định | 1 request `/v1/systemone` cho operation + mọi target head; HTTP/2 giữ kết nối; median 178 ms (hosted) | giống; local `laya-browser` ~270 ms |
| Kiểm tra trước input | `fresh()`: guard hẹp (click/select) hoặc marker đầy đủ — 1 `Runtime.evaluate` | giống (`GUARD` / `SNAPSHOT markerOnly`) |
| Input | 1 evaluate lấy toạ độ + hit-test, rồi `mousePressed`/`mouseReleased`; fill = selectAll + `insertText` | giống, thêm `mouseMoved` |
| Chờ sau input | ≤ 2 animation frame hoặc 50 ms; combobox vừa gõ: chờ option hiện, ≤ 200 ms; `WAIT` = 100 ms | giống (`settle.js`) |
| Quan sát lại | 1 evaluate; document đang đổi → thử lại 10 × 20 ms | giống (50 ms/lần, ≤ 10 s) |
| Text cho field | LLM nhỏ, tắt reasoning, JSON `{"text"}`; cache theo context | giống |
| DONE | chấp nhận ngay khi trang còn "fresh" | kiểm chứng độc lập (rule + noul) |
| Rủi ro | không có | word list + model (raise-only) |
| Mở trang | `Page.navigate`, poll `readyState == complete` mỗi 20 ms, ≤ 15 s — **loại khỏi số đo 7,07 s** | chờ `loadEventFired`, ≤ 15 s |

Bài học của upstream ("chờ trạng thái có ích, không chờ delay tuỳ ý") ta đã áp dụng đủ ở mức hành động. Chỗ upstream **không** giải quyết — và ta chậm — là mở trang, và mọi thứ quanh vòng lặp (LLM, agent).

Phát hiện phụ khi dò CDP: sau một click làm trang điều hướng, Chrome **giữ mọi lệnh tới trang** cho tới khi document mới commit (`1+1` mất 816 ms với server trả chậm 800 ms). Nên "settle" sau click điều hướng thực chất là chờ commit, rồi quan sát trang mới lúc nó mới nạp dở.

## Đo: thời gian đi đâu

### Runtime `sen-browser` (release, Chrome headless, fixture local)

| Thao tác | 0.1.1 | 0.1.2 |
|---|---:|---:|
| observe | 1,3 ms | 1,3 ms |
| click (bật/tắt like) | 50 ms (input 14, settle 31) | 50 ms |
| scroll | 67 ms | 68 ms |
| click mở trang khác | 59–71 ms | 57–61 ms |
| …server trả sau 800 ms | 864–880 ms | 841–852 ms |
| quay lại (bfcache) | — | 7–10 ms |
| mở trang thường / trang nhiều nội dung | 9–18 ms | 10–23 ms |
| mở trang có resource giữ `load` 3 s | **3.020 ms** | **621 ms** |
| mở app shell (nội dung do script tải về sau 600 ms) | 9–19 ms, **0 clip** | 837 ms, đủ clip |
| cả hai | 3.020 ms | 832 ms |

### Trang thật (profile mới; mỗi trang 2 lần: cold / warm)

| Trang | 0.1.1 | 0.1.2 | Lần nhìn đầu |
|---|---:|---:|---|
| html.duckduckgo.com (kết quả tìm kiếm — `browser_search` dùng) | 3.702 / 3.082 ms | 1.673 / 1.394 ms | đủ |
| tiktok.com/explore | 3.309 / 1.255 ms (một lần khác: **15.569**) | 3.210 / 1.482 ms | 15/65 phần tử → 43–65/65 |
| bbc.com/news | 1.305 / 907 | 1.474 / 1.148 | đủ |
| wikipedia, github trending | như cũ | như cũ | đủ |
| duckduckgo.com (trang thưa, có script) | 982 / 169 | 1.790 / 925 | đủ |
| youtube.com (trang consent) | 2.744 / 792 (10/14) | 3.830 / 2.044 | đủ |

Số trang thật dao động mạnh theo mạng; cái ổn định là cấu trúc: trần 15 s → parsed + 4 s; trang `load` trễ nhanh hơn ~2 s; app shell trả về đủ nội dung; trang thưa có script chậm thêm 0,6–1,3 s (đánh đổi, xem dưới).

### Vòng lặp (daemon, `laya-browser` + LLM local Qwen 2B, fixture feed)

| Kịch bản | Trước | Sau |
|---|---:|---:|
| "Like the clip" (1 click + DONE có kiểm chứng) | 721 ms | 688–747 ms (không đổi) |
| "Open the clip" | 682 ms | 688–705 ms (không đổi) |
| …cần tầng LLM fallback | llm 4.829 ms (2 lần sai contract) | llm 800–822 ms |
| …không đưa `done_criteria` | 2.651 ms (verify 1.972) | 2,0–2,5 s (verify 1,2–1,7 s; chỉ chồng lấp được bằng thời gian task chạy, ~0,65 s) |
| task đầu sau khi model bị unload, file model còn trong cache OS | 2.907 ms | 1.661–1.720 ms (8 lần) |
| …file model cũng nguội (1 mẫu mỗi bên) | 5.515 ms | 3.937 ms |
| bằng tay: open / do / do / read (chỉ thời gian tool) | 12 / 98 / 108 / 2 ms | 13 / 98 / 108 / 3 ms |

Một bước = decide ~270 + risk ~45 + act ~50 ms. `stats.timing` giờ in ra đúng bảng này cho mọi task.

### Máy thật (chat 29/09 16:52, `gemini-pro-agent`)

```
16:52:11 REQ 121k ký tự → 8 s → Skill
16:52:19 REQ 140k        → 7 s → ToolSearch
16:52:26 REQ 145k        → 6 s → browser_search   (tool 4 s)
16:52:36 REQ 152k        → 7 s → browser_open     (tool 1 s)
16:52:44 REQ 156k        → 7 s → browser_do       (tool < 1 s)
16:52:51 REQ 156k        → 11 s → browser_read    (tool < 1 s, trả 38k ký tự)
16:53:02 REQ 197k        → 10 s → trả lời
```

61 s, trong đó tool ~6 s. `decisions: []` ở mọi trace → decision model chưa bao giờ trả lời cho browser trên máy thật.

## Đã làm

| Repo (branch) | Commit | Nội dung |
|---|---|---|
| sen-browser (`perf/action-latency`) | `b43c1f3` | `wait_ready`: theo sự kiện trang tới document mới; sẵn sàng khi `load` + không phải app shell, hoặc mạng yên (`Page.lifecycleEvent`) + nội dung đứng yên, hoặc đứng yên 1,5 s, hoặc 4 s sau khi parse. Áp dụng cho `navigate`, click điều hướng, back |
| | `d1aa0ff` | snapshot đọc `aria-pressed` (like/follow) thành `checked`; vào guard |
| | `6fdb15d` | 0.1.2 |
| senclaw-extension (`perf/action-latency`) | `ab52375` | allowlist `Page.setLifecycleEventsEnabled`; bundle mới; 0.2.1 |
| senclaw (`perf/browser-action-latency`) | `b61285a` | loop: criteria viết song song, DONE chưa chắc → kiểm chứng trước khi hỏi LLM, verify 1 call, fallback không xin "reason", nạp checkpoint khi trang đang mở, `stats.timing`, `notes` |
| | `4fdcb57` | `browser_read {url}`; mô tả tool hướng về 1 call; `decisionModel.installed` trong settings/status |
| | `307370c` | `SKILL.browser-v2.md` cho `agent-browser` và `web-research` (đọc khi engine là v2); test chặn tên tool không tồn tại |
| | `8be7508` | không bấm tắt một toggle đã thoả mục tiêu |
| | `236a4e9`, `d5a6258` | CLAUDE.md |
| web-app, desktop (`perf/browser-latency`) | `d30ade7`, `a9b7fbd` | cảnh báo "decision model chưa cài" ở Settings → Browser |

Đã fast-forward vào `main` của cả 5 repo (local, **chưa push**; WIP của session khác giữ nguyên). Runtime 0.1.2 đã cài cạnh 0.1.0/0.1.1 trong `~/.senclaw/runtimes/sen-browser/` (slot browser lấy bản mới nhất), `senclaw-extension/dist/chrome-mv3` đã build 0.2.1 cùng bundle (`85fde660…`).

Dữ liệu thô của các bảng trên: [`latency-data/`](latency-data/).

Kiểm chứng: sen-browser 34 test, daemon 2.659 test lib + integration, extension 35, desktop 350, web `tsc -b`; `acceptance.py` **59/59** (50 cũ + L1–L8 + E4, gồm E1–E4 chạy thật qua Chrome); `e2e/feed.sh`, `e2e/latency.sh`, `e2e/runtime-latency.py` để đo lại.

## Ước lượng cho luồng "mở TikTok → xem bài → like"

| | Lượt LLM chat | Thời gian |
|---|---|---|
| Hôm qua (từng bước, skill cũ) | Skill + ToolSearch + open + look/do + do + read + trả lời ≈ 7 lượt | ~50–65 s (+15 s nếu `load` kẹt) |
| Sau khi sửa, **có** `laya-browser` | `browser_task` + trả lời = 2 lượt | ~7 + (mở 1,5–4 s + 2–3 bước × 0,35 s) + 7–10 ≈ 17–22 s |
| Sau khi sửa, **chưa** cài `laya-browser` | 2 lượt | thêm 3–7 s mỗi bước (LLM chọn bước); `notes` sẽ nói rõ |

Phần còn lại (2 lượt × ~7 s) là chi phí của chính model chat với prompt 100k ký tự không cache — ngoài engine browser.

## Đánh đổi và giới hạn

- **Trang thưa có script** (trang consent, trang chủ tìm kiếm): chậm thêm 0,6–1,3 s vì chờ mạng yên. Không phân biệt được "trang tĩnh thưa" với "vỏ app đang chờ API" nếu không thấy request đang bay; domain `Network` bị cấm trên Chrome của người dùng (lộ cookie). Trang không nạp script, hoặc nhiều nội dung, không bị.
- **Trang không bao giờ yên và chữ đổi liên tục**: trả về sau parse + 4 s (trước: tới `load`, có thể sớm hơn).
- **`laya-browser` bỏ qua trạng thái toggle**: bấm Like lần nữa (0,95) dù đã like. Đã chặn bằng code khi criteria đã thoả; nếu criteria yếu thì vẫn có thể bấm ngược.
- **sen-sysone**: hai checkpoint nạp cùng lúc có thể làm hỏng một cái (`onnx load: Encountered unknown exception in Initialize()`, thấy 1 lần). Warm-up của loop nạp tuần tự để tránh; gate + browser nạp cùng lúc từ hai chat vẫn có thể gặp.
- Checkpoint bị unload sau 15 phút rảnh (`decisionConfig.local.idleUnloadMinutes`): lần đầu sau đó trả 2–3 s nạp lại (1,2 GB × 2). Warm-up chỉ che được phần trùng với lúc mở trang.
- Số đo loop là trên fixture và model local; chưa đo task dài trên trang thật.

## Việc cần người dùng làm

1. **Reload extension** ở `chrome://extensions` (bản build mới 0.2.1 đi với runtime 0.1.2; bản đang nạp trong Chrome vẫn là bản cũ hơn cả 0.1.1).
2. **Cài `laya-browser`**: Settings → Decision (Laya) → import thư mục `Laya-jev/models/laya-browser` (1,2 GB). Settings → Browser hiện cảnh báo cho tới khi có. Chưa làm hộ vì nó đổi model quyết định của bản đang dùng.
3. **Khởi động lại daemon**: `senclaw/target/debug/senclaw` đã build lại từ `main` lúc 23:14 (gồm cả WIP đang có của session khác — build sạch). `web-app/dist` chưa build lại (cảnh báo trên web cần `npm run build`); desktop cần build lại để có cảnh báo.
4. Tuỳ chọn: tăng `decisionConfig.local.idleUnloadMinutes` (đang 15) nếu chấp nhận giữ ~2,4 GB RAM để khỏi nạp lại model.

## Câu hỏi chưa giải quyết

- Mỗi lượt chat 6–11 s với `cache_read = 0`: prompt cache của provider Antigravity có bật được không? Đây là phần lớn nhất còn lại.
- Có nên cho extension tự đếm request đang bay (chỉ gửi con số về runtime) để bỏ khoản 0,6–1,3 s trên trang thưa?
- `laya-browser` chạy ONNX CPU 270 ms/quyết định; CoreML/Metal EP trong `sen-sysone` có đáng làm không?
- Like/follow có nên vào danh sách cần người dùng duyệt (`policy::RISKY`)? Hiện là thao tác tự động.
