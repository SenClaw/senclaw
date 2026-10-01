# Plan: SenBrowser v2 (Jev + LLM browser runtime)

Trạng thái: **xong 59/59 check** (ak-loop + 2 vòng sửa theo review bảo mật 29/09/2026 + giảm độ trễ hành động 30/09/2026: L1–L8, E4) · UI desktop + web: xong, gồm thẻ duyệt thao tác và cài/quản lý runtime sen-browser · Thiết kế: [design.html](design.html) (v0.2) · Nghiên cứu: [../reports/research-260929-0143-jev-ultrafast.md](../reports/research-260929-0143-jev-ultrafast.md)

## Quyết định đã chốt (29/09/2026)

D1 vòng lặp trong control plane daemon · D2 cả hai chế độ (Chrome riêng + extension) · D3 backend `auto` (local mặc định) ·
D4 model viết chữ: local nhỏ nếu có, không thì model chat hiện tại · D5 extension viết lại thành driver mỏng ·
D6 mini-browser: lõi CDP làm tham chiếu (không sửa app) · D7 MVP có shadow root mở, iframe cùng origin, popup, PRESS_KEY; chưa upload/canvas ·
D8 extension nối vào daemon `/browser/ext` · D9 tab nền trong nhóm "SenClaw" · D10 mặc định Chrome riêng.

## Workspace

| Repo | Nơi làm | Nhánh |
|---|---|---|
| `sen-browser` (mới) | `/Users/benji/Projects/SenClaw/sen-browser` | `main` |
| `senclaw` (daemon) | worktree `/Users/benji/Projects/SenClaw/.worktrees/senclaw-browser` (target riêng, không đụng `senclaw/target/debug/senclaw` của daemon thật) | `feat/sen-browser-v2` |
| `senclaw-extension` | checkout chính | `feat/driver-v2` |
| `web-app` | worktree `/Users/benji/Projects/SenClaw/.worktrees/web-app-browser` | `feat/browser-engine-settings` |
| `desktop` | worktree `/Users/benji/Projects/SenClaw/.worktrees/desktop-browser` | `feat/browser-engine-settings` |

Không push. Không đụng daemon thật (18788/18789, `~/.senclaw`). E2E: `HOME=<scratch>`, cổng 28788/28789.

**Đã merge vào `main` (cục bộ, chưa push) 29/09 08:59:** senclaw → 666fb50, desktop → 15651ff, web-app → 8ea0341, senclaw-extension → 732c90c (fast-forward cả bốn); sen-browser vốn ở `main` (3e85547). Thay đổi chưa commit của phiên khác được giữ nguyên (11 file trùng được gộp 3-way sạch, kiểm tra byte-by-byte). `cargo check`, `flutter analyze` + test, `tsc` trên `main` sau merge đều sạch.

## Phases

1. **Runtime `sen-browser`**: SDK scaffold; CDP client (WS JSON-RPC); driver managed (Chrome + profile riêng, DevToolsActivePort); tab (isolated world, focus emulation); script đã duyệt (snapshot/guard/act/settle) + hash; observe/act có guard; navigate/read/screenshot; handover gating; driver extension (WS relay `/v1/drivers/extension`); allowlist CDP.
2. **SDK + daemon plumbing**: `Browser` type/slot/capability; proxy `/api/browser/*`; client runtime browser.
3. **Browser Loop (daemon)** `src/browser_agent/`: encoder jev-full + laya-v3, budget, validate, band, policy (tầng rủi ro, PII, domain, chọn driver), LLM (viết chữ, fallback, verify), loop + trace, REST `/api/browser-agent/*`.
4. **MCP tools** (engine v2): `browser_task/look/do/open/read/tabs/screenshot/handover/approve` gọi REST daemon; core-server chọn engine.
5. **Extension**: `/browser/ext` (Origin + pairing + token, `pair approve <CODE>`), pipe vào runtime; extension v0.2 relay `chrome.debugger` + allowlist + side panel; gỡ content-script executor.
6. **E2E**: daemon cô lập + sen-browser + sen-sysone (`laya-browser`) chạy `browser_task` trên fixture local, cả Chrome riêng lẫn extension.

## Acceptance checks (thước đo vòng lặp — cố định)

Runtime: R1 health/info · R2 managed launch + tab · R3 observe (role/label/value, bỏ password, text ≤ 6000, isolated world) · R4 click trusted · R5 fill thay giá trị · R6 select · R7 stale → 409 không input · R8 covered → 409 · R9 combobox settle · R10 press Enter · R11 scroll/wait · R12 allowlist chặn method/script lạ · R13 relay extension (fake extension trên CDP thật) · R14 handover chặn đọc/ghi · R15 navigate/read/screenshot.

Daemon: D1 SDK slot browser · D2 jev-full khớp `choose()` Python · D3 validate 6 ca sai · D4 laya-v3 khớp `to_format_v3()` Python · D5 budget · D6 band p(op)×p(target) · D7 text writer (JSON, null, cache) · D8 loop (mock) + no-progress + DONE verify · D9 tầng rủi ro · D10 che PII · D11 chọn driver · D12 `/browser/ext` Origin + pairing · D13 MCP tools v2 · D14 REST browser-agent.

Extension: X1 build · X2 allowlist + hash · X3 relay (mock chrome) · X4 hello/pairing · X5 manifest (debugger, không executor).

E2E: E1 Chrome riêng + laya-browser hoàn thành task fixture · E2 qua extension hoàn thành task fixture.

Bảo mật (thêm sau [review](reports/review-browser-engine-security.md)): S1 allowlist theo tham số · S2 tầng rủi ro (nhãn mua hàng, Enter theo cái nó gửi, dialog) · S3 model chỉ được thêm điểm dừng · S4 chat chỉ thấy/điều khiển tab của mình, chặn API của chính SenClaw · S5 `browser_approve` không qua được khi phiên không hỏi ai · S6 vòng đời pipe extension · S7 proxy `/api/browser/*` · S8 giới hạn vòng lặp llm-only · S9 rút tab đã chia sẻ · E3 mua hàng dừng chờ người duyệt, duyệt ở Settings → Browser mới đặt hàng.

Vòng kiểm tra lại ([review-browser-engine-security-fixes.md](reports/review-browser-engine-security-fixes.md)) và quản lý runtime: S10 daemon chỉ tin máy này khi `Host` là loopback và không có `Origin` của site khác (chặn DNS rebinding, `lvh.me`, WebSocket cross-site) · S11 allowlist phím/chuột không chạm clipboard · S12 trang dừng ở API của SenClaw không được trả về, chat chỉ duyệt/tiếp tục task của mình · U1 sen-browser có trong catalog runtime, log runtime không còn mã màu.

Tổng: 50 (36 ban đầu + 14). Kết quả từng vòng: [loop-results.tsv](loop-results.tsv).

## Rủi ro

Build daemon lâu (worktree = build mới) · laya-browser 1,3 GB, CPU ~0,75 s/bước · `Extensions.loadUnpacked` cần Chrome for Testing hoặc cờ debug · hành vi focus emulation qua `chrome.debugger` chưa đo.

## Câu hỏi mở

Xem design.html §20.

## Kết quả (29/09/2026)

50/50 check qua (`python3 acceptance.py --report`, 2 phút 28 giây; E2E dựng môi trường cô lập mới). Trước khi sửa theo review: 36/36; sau vòng 1: 46/46. Chi tiết từng vòng: [loop-results.tsv](loop-results.tsv).

| Repo | Commit |
|---|---|
| sen-browser (`main`) | 7730b94 runtime · 67822c6 mock keychain khi HOME không có keychain · 372779a allowlist theo tham số · fd28872 id có epoch, che giá trị ô nhạy cảm, ngữ cảnh Enter · 9556287 rút tab chia sẻ · 7539992 test OTP · f983a21 phím/chuột không chạm clipboard · 3e85547 CI phát hành gói theo tag |
| senclaw (`feat/sen-browser-v2`) | 2c216da slot browser · d76346d engine · ba9ca4b chống vòng lặp/gõ sau click ô/quote criteria/parse LLM · 7db3d79 REST settings + giữ `browserAgent` khi lưu config · 2733669 `browser_approve` trước skip flag · 8576572 proxy `/api/browser/*` · aafd840 sửa engine theo review · 312760d test giới hạn llm-only · c079151 câu hỏi rủi ro dạng lựa chọn · 8503d3f tin máy này chỉ khi `Host`/`Origin` là local · 6444d3b trang dừng ở API SenClaw, submit không chữ, duyệt chéo chat · f717adc sen-browser trong catalog · 6ec91eb log runtime không mã màu · 666fb50 lời mô tả catalog |
| senclaw-extension (`feat/driver-v2`) | 6b6a5ea driver chrome.debugger v0.2 · e3dd99d cổng daemon cho bản test · e9d9ecc allowlist theo tham số · 4434ed1 `pipe_closed` + rút tab chia sẻ · a2686b5 phím/chuột không chạm clipboard · 732c90c chia sẻ lại sau khi kết nối lại |
| web-app (`feat/browser-engine-settings`) | 01865a5 Settings → Browser · 447c90b thẻ "Đang chờ bạn duyệt" · 8ea0341 runtime cài từ gói cục bộ hiện "Đã cài" |
| desktop (`feat/browser-engine-settings`) | ad7bcba Settings → Browser (analyze sạch, 19 test mới + 327 test cũ qua) · f347d66 thẻ duyệt · eba72ea đồng bộ luật "Đã từ chối" với web · b29bbe2 runtime cài từ gói cục bộ hiện "Đã cài" · 15651ff ghi chú CLAUDE.md (flutter test 346 qua) |

E2E (`e2e/managed.sh`, `e2e/extension.sh`): gate = task điều hướng mà model local làm được ổn định (Help, Deals) qua MCP → REST → loop → runtime → Chrome riêng / Chrome for Testing + extension thật (ghép cặp qua REST). Task khó hơn chạy kèm, **không gate**, in kết quả.

### Phát hiện từ E2E

- **laya-browser (322M) yếu ngoài 16 task huấn luyện**: form trống thì bấm Search trước; không chọn SELECT trừ khi goal nói "select"; hay "Back to search"; DONE yếu. Chỉ tin được với điều hướng đơn giản. Task form/filter trên site lạ cần Jev hosted hoặc model tốt hơn.
- **Verifier multilingual cho false positive** (trang Economy được chấm "Business class" p=0.84) → thêm luật: text trong ngoặc kép của criterion phải có trên trang.
- **LLM 2B local** viết giá trị sai khi trang có nhiều chữ (chép cả kết quả vào ô Destination); sen-mlx bỏ qua `temperature`/`max_tokens` (luôn 0.7 / 8192) → một bước fallback chạy 122 giây → loop giới hạn 60 giây/lần gọi LLM; lỗi sen-mlx để task riêng.
- **Chrome 154 treo mọi request mạng khi HOME không có keychain** (scratch HOME, service account) → runtime thêm `--use-mock-keychain` chỉ khi đó.
- **`GlobalConfig` xoá key lạ khi lưu** → `browserAgent` mất sau lần lưu LLM config bất kỳ → thêm field raw.
- `lsof -iTCP` mất 76 giây trên máy này → harness dùng connect probe; 2 test `space_mcp` (reclaim port) fail vì lý do này (lỗi có sẵn, task riêng).

### Sửa theo review bảo mật (29/09/2026)

Review: [reports/review-browser-engine-security.md](reports/review-browser-engine-security.md) (mục *Resolution* ở cuối: từng lỗi → cách sửa → commit → check). Đã sửa C1, H1–H4, M1–M5, M7, M8, L1–L8, L10; M6 một phần (loop tự dừng ở phút 14, trước timeout MCP 15 phút). Còn lại: L9 (`--remote-debugging-pipe`), registry task chạy nền (M6 đầy đủ), `browser_resume` truyền giá trị cho `needs_input`, đóng tab rảnh sau 30 phút.

- **Duyệt thao tác ngoài chat**: `GET /api/browser-agent/approvals` + thẻ "Đang chờ bạn duyệt" ở Settings → Browser (web + desktop). Phiên không hỏi ai (workflow, chạy nền, tắt prompt) bị từ chối `browser_approve` — thao tác chờ người duyệt ở đây. Đã thử trên UI web thật: bấm Duyệt → đơn hàng fixture mới được đặt.
- **Model local không dùng được làm bộ phân loại rủi ro dạng có/không**: multilingual trả "có" 0,999 cho link Help, 0,98 cho Next; laya-browser trả 0,009 cho "Confirm" chuyển khoản ngân hàng. Bản đầu của check `browser.risk` vì vậy chặn mọi task (E1/E2 fail, 44/46). Đổi sang câu hỏi lựa chọn view / adjust / commit: Help/Next/Search ≤ 0,03, "Continue" ở trang thanh toán 0,83, Send 0,99, Delete account 0,81 — nhưng "Confirm" chuyển khoản vẫn chỉ 0,08 và "Add to cart" 0,94 (dừng thừa). Nên từ khoá vẫn là lớp chính, model chỉ thêm điểm dừng.
- Popconfirm của antd không nhận click chuột trong pane trình duyệt ẩn (animation đứng ở `enter-prepare`) — chỉ là môi trường kiểm thử; bấm OK bằng JS thì luồng chạy đúng.

### Quản lý và cài sen-browser trên desktop/web (29/09/2026)

Người dùng báo: Runtime trên desktop chưa quản lý/cài được sen-browser. Nguyên nhân:
- **Chưa có trong catalog**: `runtimes/index.json` không có mục sen-browser → không app nào đề nghị cài. Đã thêm (như sen-turbo-fieldfare: *Not published yet* cho tới bản phát hành đầu).
- **Chưa từng phát hành gói**: repo GitHub `SenClaw/sen-browser` trống, không release; repo chưa có workflow phát hành → đã thêm `.github/workflows/release.yml` (tag `v*` → gói darwin-arm64, linux-x64, windows-x64). **Chưa push/tag** — cần người dùng đồng ý.
- **App đang chạy là nhánh `main`**: desktop (`desktop/build/...`) và daemon thật đều từ `main`, chưa có các nhánh này → không có slot Browser, không có mục Browser. Cần merge các nhánh.
- Catalog sống được tải từ GitHub `SenClaw/senclaw` main (`runtimes/index.json`) → mục mới chỉ tới máy người dùng sau khi merge + push.

Đã kiểm tra thật trên bản desktop build từ nhánh này (bundle id riêng, `HOME` scratch, daemon cô lập 28788): slot **Browser**; mục **SenClaw Browser Engine** trong Engines & Frameworks; **Uninstall**; **Install from folder or archive…** qua hộp chọn file macOS với gói `make package`; slot tự chọn lại; Settings → Browser → *Show open tabs* khởi động runtime; danh sách **Running** (port, uptime, số lần chạy) và **Stop**; **View logs**. Web: slot Browser, mục catalog "Đã cài", cài từ đường dẫn cục bộ. Sửa theo kết quả: chip "Đã cài" thay cho "Not published yet" khi đã cài từ gói cục bộ (desktop + web); log runtime bỏ mã màu ANSI (daemon lọc khi đọc log, SDK chỉ tô màu khi là terminal); lời mô tả catalog hợp cả hai app.

### "Không search được" (29/09/2026, sau khi merge)

Nguyên nhân, đọc từ log runtime và bảng `tool_executions` (chỉ đọc):
- `browser_search` chạy trong Chrome riêng **headless**; user agent `HeadlessChrome` → DuckDuckGo trả trang "bots use DuckDuckGo too" thay cho kết quả. Google (`/sorry/`), Cloudflare (sjc.com.vn) cũng chặn; vnexpress trả 406 cho cả curl.
- Mặc định của người dùng là `defaultDriver: extension`, nhưng extension trong Chrome là bản cũ (bundle `54933a…`, runtime cần `e768515…`) → runtime từ chối → `extension_not_connected`.

Sửa: sen-browser 0dffd81 (headless dùng UA Chrome thường + tắt `AutomationControlled`, test Chrome thật đọc UA qua trang echo) → phát hành cục bộ **0.1.1** (3dc89a0), đã cài vào daemon đang chạy và cho slot Browser theo bản mới nhất (0.1.0 đang chạy tự đổi khi rảnh). senclaw 1183cff: `browser_search` thử DuckDuckGo rồi Bing khi gặp bot check, trả `results` là URL thật (bỏ redirect/quảng cáo), báo `blocked` rõ ràng khi mọi engine đều chặn. Kiểm tra cô lập qua MCP: "giá vàng hôm nay" → 10 kết quả (24h, vtcnews, pnj…). Extension: bản trong `dist/chrome-mv3` đã đúng bundle và cổng 18789 — chỉ cần reload trong chrome://extensions.

### Lần thử sau đó: "Antigravity request failed" + "Session has been reset" (29/09/2026 11:53)

- Chat đó vẫn dùng bản cũ: tiến trình MCP của chat (core-server) khởi động từ trước khi build lại → `browser_search` cũ; sen-browser 0.1.0 cũ vẫn chạy vì được dùng liên tục (không kịp rảnh để đổi sang 0.1.1); extension chưa reload.
- Lỗi model: yêu cầu tới Google hỏng ở tầng mạng (trước khi có phản hồi). `LlmError::classify` chỉ đọc thông điệp ngoài cùng ("Antigravity request failed") nên xếp `UNKNOWN_ERROR` → pool xoá session (mất ngữ cảnh), thay vì `NETWORK_ERROR` (giữ ngữ cảnh). Sửa ở senclaw b521cf9: phân loại mạng theo cả chuỗi lỗi, thông điệp nêu nguyên nhân gốc (che URL vì có provider đặt API key trong URL), kết nối rớt trước khi có phản hồi được thử lại 1 lần sau 1,5 giây (timeout không thử lại). 2670 test daemon qua. Không đọc được log cụ thể trong terminal Cursor (cửa sổ ở Space khác) — nguyên nhân mạng cụ thể chưa xác nhận.

### Độ trễ hành động: "mở TikTok, xem bài, like — sau vài giây" (30/09/2026)

Báo cáo đầy đủ, số đo và đánh đổi: [reports/research-260930-2059-jev-ultrafast-action-latency.md](reports/research-260930-2059-jev-ultrafast-action-latency.md).

- Phần tay-mắt đã ngang jev-ultrafast (click 50 ms, quyết định local 270 ms, ~0,35 s/bước). Chậm nằm ở: mỗi tool call = 1 lượt LLM chat 6–11 s và agent đi từng bước vì skill cũ dạy tool cũ; `laya-browser` chưa cài trên máy thật (mỗi bước loop rơi về LLM, không ai báo); extension trong Chrome là bản cũ; mở trang chờ `load` (DuckDuckGo kết quả 3,1–3,7 s, TikTok có lần 15,6 s) hoặc trả về vỏ app.
- sen-browser **0.1.2** (b43c1f3, d1aa0ff): `wait_ready` — sẵn sàng theo trạng thái trang (load + không phải app shell / mạng yên + nội dung đứng yên / đứng yên 1,5 s / parse + 4 s), áp dụng cho navigate, click điều hướng, back; snapshot đọc `aria-pressed`. Bundle script mới `85fde660…` → extension **0.2.1** (ab52375, thêm `Page.setLifecycleEventsEnabled` vào allowlist).
- senclaw (b61285a, 4fdcb57, 307370c, 8be7508): criteria viết song song; DONE chưa chắc kiểm chứng trước khi hỏi LLM; verify 1 call; fallback không xin lý do; nạp checkpoint khi trang đang mở (tuần tự); `stats.timing` + `notes`; `browser_read {url}`; `decisionModel.installed`; `SKILL.browser-v2.md` cho `agent-browser`/`web-research`; không bấm tắt toggle đã thoả mục tiêu.
- web-app d30ade7, desktop a9b7fbd: cảnh báo thiếu decision model ở Settings → Browser.
- Đo lại: `e2e/runtime-latency.py` (runtime, có `--sites`), `e2e/latency.sh` (loop), `e2e/feed.sh` (E4). Fixture feed: `e2e/fixtures/clips.html`, `clip.html`; `e2e/serve.py` thay `http.server` (có `?delay=`).
- Đã fast-forward `main` cả 5 repo (chưa push); runtime 0.1.2 cài cạnh bản cũ trong `~/.senclaw`; `dist/chrome-mv3` build 0.2.1.
- Còn của người dùng: reload extension; import `laya-browser`; khởi động lại daemon (binary `main` đã build 23:14). Việc tách: sen-sysone nạp 2 checkpoint cùng lúc có thể hỏng một cái (đã có chip task).
