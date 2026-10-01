# Research: browser-use/jev-ultrafast

Ngày: 2026-09-29 01:43 (Asia/Saigon) · Nguồn chính: mã nguồn clone tại commit `1231850` (main, 2026-09-18) + docs trong repo.
Đầu vào cho thiết kế: [`../260929-0143-sen-browser-runtime/design.html`](../260929-0143-sen-browser-runtime/design.html).

## Tóm tắt

- **Là gì:** browser agent "chọn thay vì sinh" của Browser Use × TypeSafe. Mỗi bước: 1 snapshot DOM → bảng phần tử đánh số → **1 request `POST https://api.typesafe.ai/v1/systemone`** hỏi đồng thời `operation` + các câu `<op>_target` (speculative fan-out) → code thực thi phần tử đã quan sát. LLM nhỏ chỉ viết chữ khi `operation = TYPE_TEXT`.
- **Kết quả công bố:** Google Flights Zürich → London trong **7,073 ms** (17 request Jev, median 178 ms, 11 thao tác, 2 lần gọi LLM viết chữ). Tối ưu runtime: median 9,450 → 7,092 ms (−25%), CDP call 1,092 → 101 — chỉ 3 cặp chạy, sign test p = 0,25.
- **Giá trị cho SenClaw:** cao. Wire `/v1/systemone` trùng với `sen-sysone`; mô hình local `laya-browser` v10s đã được fine-tune đúng trên format request này (nghiên cứu 25/09). Nên lấy **kỹ thuật** (snapshot một lần gọi, guard, chờ trạng thái có ích, head operation+target), không lấy nguyên code Python.
- **Giới hạn thật:** không shadow DOM / iframe / canvas / upload / popup / cuộn lồng; tên accessible chỉ xấp xỉ; mới chứng minh trên 3 task; phụ thuộc TypeSafe hosted; `browser-harness` gắn vào Chrome thật của người dùng và có telemetry opt-out.

## Phương pháp

- Đọc toàn bộ repo (40 file; lõi ~850 dòng: `agent.py` 174, `browser.py` 194, `model.py` 198, `questions.py` 26, `snapshot.js` 107, `demo.py` 145, `__init__.py` 6), test offline 320 dòng, 3 file JSON đo đạc, `docs/design.md`, `docs/performance*.md`.
- GitHub API (2026-09-29): tạo 2026-09-16, 21.093★, 1.458 fork, 163 issue mở, MIT, 3 commit trên `main` (Gregor Žunič).
- Đọc wheel `browser-harness==0.1.13` (dependency duy nhất cho trình duyệt).
- Docs TypeSafe: pattern fan-out.
- Đối chiếu với code SenClaw: `senclaw/src/browser`, `src/mcp/browser_server.rs`, `senclaw-extension`, `src/control_plane`, `sen-sysone`, `senclaw-app/apps/mini-browser`, `Laya-jev` (+ báo cáo `research-260925-0207-laya-browser-agent.md`).

## Phát hiện chính

### 1. Vòng lặp (agent.py)

```
observe (1 Runtime.evaluate) → choose (1 request, mọi head) → act (guard + CDP Input) → log → observe
```

- `choose()` gửi `state = {page{url,title,text}, elements[], recent_actions[-10:]}` và `questions = {operation, click_target, type_text_target, select_target?}`. Chỉ head khớp operation được validate và dùng; head khác bị bỏ.
- `validate_choice()` từ chối: choice ngoài tập, xác suất NaN/âm/thiếu, tổng lệch > 0,02, choice không phải argmax, confidence ngoài [0,1].
- Operation: `CLICK, TYPE_TEXT, SELECT, SCROLL_UP, SCROLL_DOWN, WAIT, DONE, BLOCKED` — chỉ đưa operation đang khả dụng.
- `TYPE_TEXT` → LLM tương thích OpenAI (`response_format: json_object`, `{"text": "..."}`, ≤ 2000 ký tự). Cache giá trị chỉ khi toàn bộ input của helper giống hệt (retry sau stale).
- Quyết định bị "tiêu thụ" trước mọi mutation → retry không thể double-click. Mutation **không bao giờ** retry. Hành động được ghi log **trước** khi quan sát lại.
- Dừng: DONE/BLOCKED, 60 thao tác, 120 request, 3 thao tác liên tiếp không đổi trang (trừ WAIT).

### 2. Snapshot & guard (snapshot.js, browser.py)

- Một lần `Runtime.evaluate`: selector control HTML/ARIA → role, tên (aria-labelledby → aria-label → label → text → title/placeholder), value, checked/selected/expanded; chỉ phần tử có tâm trong viewport; bỏ `password/file/hidden`; text hiển thị ≤ 6000 ký tự; ≤ 250 ứng viên.
- Danh tính node: `WeakMap` element → id tăng dần, `Map` id → element (tỉa node rời DOM). **Lưu ở `window.__jevFast` trong main world** — script của trang nhìn thấy được.
- Freshness: `marker` (timeOrigin, URL, scroll, viewport, title, text, semantics, giá trị form) cho TYPE/SCROLL/WAIT/DONE; guard hẹp cho CLICK/SELECT (identity, role, tên, value, trạng thái, href, text vùng form/dialog/row ≤ 6000).
- Trước input: kiểm connected, enabled, visible, readonly, trong viewport, `elementFromPoint` phải nằm trong target (chống bị che).
- Input thật qua CDP: `Input.dispatchMouseEvent`, `selectAll` + `Input.insertText`. `<select>`: set value + `input`/`change`; bị ngắt giữa chừng → dừng (có thể đã fire change).
- Chờ sau thao tác: ≤ 2 animation frame hoặc 50 ms; combobox vừa gõ: chờ option hiện, tối đa 200 ms. WAIT = 100 ms.
- Tab riêng chạy nền (`Target.createTarget background`) + `Emulation.setFocusEmulationEnabled` để rAF không bị throttle; viewport 1120×780.

### 3. Hiệu năng (docs/performance.md, *.json)

| Chỉ số | Giá trị |
|---|---|
| Flights (video) | 7,073 ms; search thực thi ở 5,217 ms |
| Jev | 17 request, median 178 ms, tổng 3,720 ms; model `jev-1.13.0` |
| Token Jev | 90.558 input / 6.325 output (~5,3k input/request) |
| LLM viết chữ | Mercury 2.5: "Zurich" 581 ms, "London" 346 ms; $0.00006272 |
| So khớp 3 cặp | 9,450 → 7,092 s median; request 22 → 17; CDP 1.092 → 101 |
| Task khác | Wikipedia 2,798 s; fixture khách sạn local 1,896 s |

Nguồn tăng tốc: bỏ invalidate theo mọi DOM mutation, bỏ đọc accessibility tree lặp lại, snapshot một lần gọi, guard hẹp, chờ combobox theo sự kiện. Hai ứng viên dùng AX tree chậm hơn (9,395 s và 10,157 s).

### 4. Bảo mật / quyền riêng tư

- Tốt: output model không bao giờ thành selector/toạ độ/JS/shell; text helper phải parse JSON; page text được đánh dấu untrusted trong prompt; inspector chỉ loopback, kiểm Host/Origin/token, khoá tuần tự.
- Rủi ro: text trang (có thể chứa PII) gửi tới TypeSafe; `window.__jevFast` lộ cho trang; tab dùng chung profile Chrome thật của người dùng.
- `browser-harness 0.1.13`: daemon giữ 1 CDP WebSocket + IPC (Unix socket 0600 / TCP loopback + token trên Windows); gắn vào Chrome thật qua `DevToolsActivePort` sau khi bật `chrome://inspect/#remote-debugging`; dò cổng 9222/9223; **telemetry opt-out gửi PostHog EU** (tắt bằng `BH_TELEMETRY`, `BROWSER_HARNESS_TELEMETRY` hoặc `ANONYMIZED_TELEMETRY`).

### 5. Speculative fan-out (docs TypeSafe)

Các câu hỏi trong một request được đánh giá song song; thêm câu hỏi hầu như không tăng độ trễ; câu hỏi không đọc được đáp án của nhau → target head phải tự nêu operation giả định.

## So sánh

| | Luồng cũ SenClaw | jev-ultrafast | mini-browser (senclaw-app) | Laya-jev extension |
|---|---|---|---|---|
| Ai chọn bước | LLM chính, mỗi bước 1 lượt | Jev (1 request/bước) | LLM (plan → act → verify) | laya-browser local |
| Kênh điều khiển | Extension content script | CDP (browser-harness) | CDP (chromiumoxide) | Extension content script |
| Input | Sự kiện giả `isTrusted=false` | CDP Input (thật) | CDP Input (thật) | Sự kiện giả |
| Địa chỉ phần tử | Index 1..n đánh lại | WeakMap node id + guard | `backendNodeId` ref ổn định | Port snapshot.js |
| Chống stale / bị che | Không | Có | Ref ổn định; getContentQuads | Có (port) |
| iframe / shadow DOM | Có (walker) | Không | Có (AX tree) | Không |
| Độ trễ quyết định | Nhiều giây/bước (chưa đo) | ~178 ms (hosted) | Theo LLM | ~0,75 s (CPU M4 Pro) |

## Khuyến nghị

1. Lấy vòng lặp Jev (operation + target trong 1 request) làm đường nhanh mặc định; LLM chỉ viết chữ, gỡ khi không chắc, xác minh và trích xuất.
2. Viết lại thành Rust: runtime `sen-browser` (CDP) giữ tay-mắt; control plane daemon giữ não (ladder, policy, trace) — chi tiết trong design.html.
3. Cải tiến so với bản gốc: snapshot chạy trong **isolated world**; thêm deep snapshot (iframe/shadow DOM) khi coverage báo thiếu; `PRESS_KEY`, `GO_BACK`, tab popup; policy gate cho thao tác không đảo ngược; handover đăng nhập.
4. Local-first: `laya-browser` qua `sen-sysone` + encoder format v3 + candidate budget; hosted Jev là tuỳ chọn theo domain policy.
5. Không dùng `browser-harness` (Python, gắn Chrome thật, telemetry) — tự quản CDP trong Rust.

## Sai lầm cần tránh

- Coi `DONE` của model là bằng chứng — luôn kiểm tra độc lập.
- Retry mutation sau lỗi mạng/điều hướng → double action.
- Invalidate quyết định theo mọi DOM mutation (animation) → chậm, request thừa.
- Đo tốc độ trên 1–3 task rồi khái quát.

## Nguồn

- https://github.com/browser-use/jev-ultrafast (commit `1231850`) — README, `docs/design.md`, `docs/performance.md`, `docs/*.json`, `jev_ultrafast/*`, `tests/test_agent.py`
- https://docs.typesafe.ai/patterns/fan-out · https://docs.typesafe.ai/introduction
- https://github.com/browser-use/browser-harness (wheel 0.1.13: `daemon.py`, `admin.py`, `telemetry.py`)
- `Laya-jev/plans/reports/research-260925-0207-laya-browser-agent.md`, `Laya-jev/laya_daemon/browser_head.py`, `Laya-jev/models/laya-browser/rl_agent_config.json`
- `senclaw-app/apps/mini-browser/README.md`

## Câu hỏi chưa giải quyết

- Điều khoản TypeSafe có cho phép dùng đáp án Jev để fine-tune model local (distill) không?
- Độ trễ thật từ Việt Nam tới `api.typesafe.ai` (số 178 ms đo ở nơi khác)?
- Giá TypeSafe theo request hay theo token (response chỉ có token count)?
