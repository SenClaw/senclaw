# Luồng spec với model thật: gemma-4-e4b (LM Studio, LAN) — 3 lượt

**Ngày:** 2026-10-27 22:05–22:25 · **Bản build:** `feat/spec-flow` (senclaw 7f98e53) · **Model:** `google/gemma-4-e4b` tại
192.168.0.93:1234 (mọi lệnh gọi: agent và 4 hướng review) · **Daemon nháp:** cổng 48798/48799, `SENCLAW_HOME`/`HOME`/cwd riêng,
guard sandbox-exec · **Harness:** [spec-flow-e2e/up-lan.sh](spec-flow-e2e/up-lan.sh) + [real.mjs](spec-flow-e2e/real.mjs) · **Bằng chứng:**
[spec-flow-e2e/real-e4b/](spec-flow-e2e/real-e4b/) (transcript, 3 báo cáo review, spec sau draft/duyệt, log driver).

Project: module Python `todo.py` (TodoList: add/done/pending) + 3 test. Tính năng: "Thêm hạn chót cho công việc và liệt kê việc quá hạn".
"Người" là kịch bản: chỉ gõ lệnh và trả lời tiếng Việt như người thật; `specFlow` = `required` (mặc định).

## Thời gian

| Bước | Thời gian | Ghi chú |
|---|---|---|
| Trước khi có spec: "sửa pending()" | 50 s | cổng chặn `Edit` và `Bash` (chạy test) |
| `/brainstorm …` | 80 s | brief ghi ngay lượt đầu, 3 tool |
| `/draft` | 56 s | spec đủ mục, 1 `Write` |
| **`/review` vòng 1** | **168 s** (4 hướng: **71 s**, xử lý finding: 97 s) | 6 blocker · 14 major · 4 nit, cả 4 hướng `ok` |
| **`/review` vòng 2** | **175 s** (4 hướng: **73 s**, xử lý: 102 s) | 4 · 15 · 4 |
| **`/review` vòng 3** | **136 s** (4 hướng: **78 s**, xử lý: 58 s) | 4 · 19 · 3 |
| `/approve` ×3 | tức thì | từ chối đúng: "the last review has N blocker(s)" → driver dùng `/approve force` |
| build 1 / 2 / 3 | 175 / 77 / 242 s | 0 dòng code nào đổi (xem dưới) |
| **Tổng** | **19 phút** | |

Một lượt review 4 hướng trên e4b ≈ **71–78 s** với spec ~6.000 ký tự (mỗi hướng ~18–20 s); cả lượt `/review` (gồm agent xử lý) 136–175 s.

## Cái chạy đúng

- **Cổng giữ 100%.** 13 lời từ chối trong lịch sử chat (3 `NO_SPEC`, 2 `reviewed, not approved`, 7 `Read … in full first`…), không một
  byte code nào được ghi khi chưa đủ điều kiện; chỉ 2 `Write` thành công, cả hai vào `docs/designs/` (brief, spec).
- **Review đọc được và có ích:** 4/4 hướng trả đúng định dạng cả 3 vòng (không `unparsed`/`cut`). Finding đúng chỗ: chưa chốt kiểu
  `due_date`, múi giờ, việc không có hạn chót, kế hoạch quá chung ("Sửa add(title)" thay vì chữ ký cụ thể); kiểm máy bắt đúng `### Tasks`
  và `### Verification Plan` hỏng. Một blocker sai (Kiểm đòi outline có sẵn chữ ký mới).
- **`/approve` từ chối khi còn blocker, `force` ghi lại "(forced)"**, `/spec` cho thấy đường dẫn báo cáo.

## Cái chưa đạt (theo mức độ)

1. **Sau khi duyệt, e4b không bao giờ `Read` spec** — 7 lần nhận "Read … in full first", 0 lần gọi `Read` lên file spec, cứ thử `Edit` lại;
   ba lượt build kết thúc với `todo.py` y nguyên. Luật "đọc trước khi sửa" đúng về an toàn nhưng **chặn luôn công việc** với model nhỏ.
2. **e4b báo cáo sai điều đã làm**: bước đầu nói "đã sửa hàm pending() … dòng 20" (Edit bị chặn, file không đổi); review vòng 2 "mọi vấn
   đề chặn đã được khắc phục hoàn toàn", vòng 3 "tôi đã viết lại toàn bộ đặc tả" — hash spec **không đổi qua 3 vòng**. Cảnh báo
   "⚠️ No change was applied" có sẵn đã không bật vì `EditTally` bỏ qua lời từ chối của cổng (quyết định ở vòng review code).
3. **Xử lý finding không sửa spec**: 3 vòng, 0 lần sửa file spec; lượt duy nhất có ghi là một `Edit` vào `todo.py` (bị chặn).
4. **`Bash` chạy test bị chặn kích hoạt nhắc "check thất bại"** → model hiểu là test hỏng do thiếu spec.
5. **Template trong prompt bị chép nguyên**: `## Options   (2-3 approaches, …)`, mỗi task thành `### Tasks   (- [ ] T1) …` → không có checkbox
   (`/spec`: tasks 0/0), kiểm máy báo thiếu `### Tasks`.
6. **Bỏ qua "hỏi từng câu" và "dừng hỏi sau phần thiết kế"**: brief ghi từ một câu ý tưởng, câu trả lời soạn sẵn của người không có chỗ
   dùng — finding "chưa chốt kiểu due_date" là hệ quả trực tiếp.
7. Lời từ chối không phát event → UI (và driver) không thấy tool bị chặn; chỉ log daemon và lịch sử có.

## Đề xuất sửa (lượt 1) — đã làm, xem "Sau khi sửa"

| # | Sửa | Vì |
|---|---|---|
| A | `READ_FIRST` trả **luôn nội dung spec** trong lời từ chối và ghi receipt cho phiên bản đó (một lần mỗi phiên bản mỗi chat) | 1 — spec chắc chắn đã vào ngữ cảnh, không cần model tự `Read` |
| B | Câu cảnh báo tất định nối vào trả lời khi cổng đã chặn trong lượt ("⚠️ No change was made: … needs …"), và sau lượt REVIEW RESULTS nói version spec có đổi không | 2 — người không bị model nói sai |
| C | Lượt REVIEW RESULTS còn blocker mà không ghi gì vào spec → gửi lại 1 lần ("you changed nothing in <path>") theo mẫu nhắc sẵn có | 3 |
| D | Nhắc "check thất bại" bỏ qua kết quả `Error: [spec]` | 4 |
| E | Template không để chú thích trong dòng heading (chú thích thành dòng riêng; task mẫu là dòng `- [ ] T1 …` dưới `### Tasks`) | 5 |
| F | (cân nhắc) `/brainstorm` không cho ghi file spec trước khi người trả lời ít nhất một lượt | 6 — hiện chỉ là lời dặn |

## Sau khi sửa: A–E + một nguyên nhân mới tìm ra

Đã làm (nhánh `feat/spec-flow`): **A** `read_first_inline` (spec trong lời từ chối + receipt), **B** `blocked_warning` + dòng 📋 của
`TriageWatch.note`, **C** `TriageWatch` gửi lại 1 lần khi spec y nguyên mà còn blocker + câu "Your next tool call is Edit or Write on <spec>",
**D** `Bash` bị cổng chặn không tính là check, **E** template không chú thích trong heading. Lượt 2 lộ thêm nguyên nhân của mục 6: nhắc
"chưa làm gì mà hỏi/dán lệnh" của coding session đã bảo e4b *đừng hỏi* ngay giữa `/brainstorm` → **F'** tắt nhánh đó trên lượt spec
(`is_rewritten`). Thêm: ngôn ngữ của ghi chú lấy từ `<request>` (nhãn Cảnh/Kiểm… làm mọi ghi chú thành tiếng Việt).

Kiểm: `cargo test --lib` 3776 qua / 0 lỗi; e2e mock 43/43 (thêm: cảnh báo trên trả lời, nhắc triage, dòng 📋, spec trong lời từ chối).

### So sánh trên e4b (cùng project, cùng kịch bản người dùng)

| | Lượt 1 (trước) | Lượt 2 (A–E) | Lượt 3 (A–E + F', chỉ brainstorm) |
|---|---|---|---|
| Sửa khi chưa có spec | bị chặn; agent nói "đã sửa pending()", **không** cảnh báo | bị chặn; agent vẫn nói đã sửa, **trả lời có "⚠️ Luồng spec đã chặn — chưa có thay đổi nào…: todo.py, Bash"** | như lượt 2 |
| `/brainstorm` | ghi brief ngay, 0 câu hỏi | ghi brief ngay (nhắc "đừng hỏi" bật 1 lần) | **hỏi 1 câu** ("deadline lưu kiểu gì: chuỗi hay Date?"), ghi brief **sau** câu trả lời |
| Heading | `## Options   (2-3 …)`, 3× `### Tasks   (- [ ] T1)…` → tasks 0/0 | sạch; `/spec`: **tasks 0/5** | sạch |
| `/draft` | 1 lượt, 56 s | 3 lượt (93 + 364 + 49 s): lượt 1 thử sửa `todo.py`, lượt 2 `Edit` trượt trên spec | — |
| 4 hướng review | 71 / 73 / 78 s | 72 / 80 / 79 s | — |
| Cả lượt `/review` | 168 / 175 / 136 s | 217 / 274 / 338 s (dài hơn vì agent thật sự sửa + nhắc) | — |
| Spec có đổi sau xử lý finding? | 0/3 vòng (hash 74ef7fda suốt) | vòng 1 **có** (3d7e6dc2 → 420ba9f6); vòng 2–3 `Edit` trượt; nhắc triage bật 2 lần | — |
| `/approve` | force (4 blocker) | force (5 blocker) | — |
| Sau duyệt | 7× "Read … first", **0 `Read`, 0 dòng code** | **1 lời từ chối kèm spec → `Write`/`Edit` đi qua**: `todo.py` +59/−8, `utils.py` mới | — |
| Test sau build | 3 cũ xanh (không đổi gì) | **đỏ**: `'TaskItem' object is not subscriptable` (e4b đổi item sang class, quên `import utils`, `is_overdue` dùng `<=`) | — |
| Nhắc "check thất bại" vì test bị chặn | có, agent tưởng test hỏng | không (D) | — |
| Tổng | 19 phút | 29 phút | 2,6 phút |

**Kết luận.** Mọi điểm luồng spec chịu trách nhiệm đã chạy đúng trên e4b: cổng giữ, người không còn bị nói sai, spec được đưa vào ngữ
cảnh và code được viết sau duyệt, review sửa được spec ít nhất một vòng, brainstorm hỏi đúng câu cần hỏi. Phần còn lại là năng lực của
e4b: `Edit` trượt trên file markdown dài, code sai và làm vỡ test cũ — vòng kiểm thử sẵn có (nhắc chạy test, cảnh báo chưa kiểm chứng) là
thứ bắt nó, không phải luồng spec. Bằng chứng: `spec-flow-e2e/real-e4b/` (lượt 1), `real-e4b-2/` (lượt 2, kèm `code.diff`),
`real-e4b-3-brainstorm/` (lượt 3).

## Lượt 3 đầy đủ (A–E + F'), 2026-10-27 23:21–23:39

Cùng project, cùng kịch bản. Bằng chứng: `spec-flow-e2e/real-e4b-3-full/` (transcript, 3 báo cáo review, spec, `code.diff`).

| | Lượt 1 | Lượt 2 (A–E) | **Lượt 3 (A–E + F')** |
|---|---|---|---|
| `/brainstorm` | 1 lượt, 0 câu hỏi | 1 lượt, 0 câu hỏi | **3 lượt (21+88+27 s): hỏi 1 câu ("due_date kiểu gì?") rồi mới ghi; brief ghi `datetime.date \| None`** |
| `/draft` | 1 lượt, 56 s | 3 lượt, 506 s | 2 lượt, 228 s; kết bằng "Bạn vui lòng ra lệnh /approve" |
| Blocker vòng 1 / 2 / 3 | 6 / 4 / 4 (tb 4,7) | 3 / 4 / 5 (tb 4,0) | **3 / 3 / 4 (tb 3,3)** |
| Major vòng 1 / 2 / 3 | 14 / 15 / 19 | 15 / 13 / 15 | 14 / 11 / 13 |
| 4 hướng review | 71 / 73 / 78 s | 72 / 80 / 79 s | 75 / 73 / 83 s |
| Cả lượt `/review` | 168 / 175 / 136 s | 217 / 274 / 338 s | 314 / 139 / 132 s |
| Xử lý finding | spec không đổi 3/3 vòng | đổi ở vòng 1 | đổi ở vòng 3; **chuyển 1 quyết định lên người** bằng thẻ hỏi ("quy tắc ngày hạn chót"), quyết định được ghi vào spec |
| Sau duyệt | 7× "Read…", 0 code | 1 từ chối kèm spec → code; **test cũ hỏng** | **1 từ chối kèm spec → `Write` → tự chạy test: 3/3 xanh**, xong trong 1 lượt (69 s) |
| Tính năng (thử độc lập) | — | class `TaskItem`, UTC, vỡ test | `add(title, due_date=None)` kiểu `datetime.date` ✓, việc xong / không hạn bị loại ✓, biên `<=` đúng câu trả lời của người ✓; **thiếu "mặc định hôm nay"** (người có nói, spec làm rơi), không viết test mới |
| Tổng | 19 phút | 29 phút | **17,5 phút** |

**Có ít blocker hơn không?** Ít hơn một chút (trung bình 4,7 → 4,0 → 3,3), nhưng cùng một bản spec ở lượt 1 đã cho 6 rồi 4 blocker, nên chênh
lệch này nằm trong độ dao động của e4b — **chưa đủ để kết luận**. Cái thay đổi rõ là *loại* blocker: lượt 1 bị bắt lỗi đúng những điều người
có thể trả lời (kiểu ngày, múi giờ) vì không ai hỏi; lượt 3 các hướng Cảnh/Phản biện không còn nêu kiểu ngày.

**Blocker giả do thiết kế review (mới tìm ra):** cả 3 vòng lượt 3, hướng **Kiểm** báo "chưa xác định kiểu `due_date`" — spec ghi rõ
`datetime.date | None` ở Technical Plan, nhưng Kiểm (theo EGM) chỉ đọc từ `## Detailed Implementation Plan` trở xuống nên không thấy. Đề
xuất **G**: Kiểm đọc cả file, dặn tập trung vào kế hoạch (ước tính bớt 1 blocker mỗi vòng). Phản biện còn đòi "giới hạn số công việc tối đa"
cho một module trong bộ nhớ — mức BLOCKER quá tay; có thể dặn rõ hơn "BLOCKER = không viết đúng code được".

## G: Kiểm đọc cả file (2026-10-28 00:40–01:00)

Đã làm: `KiemScope::Whole` (mặc định) — Kiểm nhận cả spec, lời dặn "chấm phần kế hoạch, thứ phần thiết kế phía trên đã định nghĩa thì coi
là đã định nghĩa"; `KiemScope::Plan` (cách EGM cũ) chỉ còn cho phép đo. Phép đo: test `measure_kiem_scope_on_a_real_model` (ignored) gọi
thẳng `specs::review::run_with` trên **spec cuối của lượt 3** (`real-e4b-3-full/spec-approved.md`), **xen kẽ** Plan/Whole 5 cặp trong một
tiến trình. **Model:** máy LM Studio LAN (e4b) tắt lúc đo, người dùng chọn "dùng mặc định" → **Antigravity `gemini-3.8-flash-medium`**
(config + kho OAuth thật, đọc tại chỗ; không chép). Số liệu: [spec-flow-e2e/kiem-scope-measure/measure.jsonl](spec-flow-e2e/kiem-scope-measure/measure.jsonl).

| 5 lần mỗi bên | Plan (cũ) | Whole (G) |
|---|---|---|
| Blocker tổng (tb ± đl, min–max) | 5,8 ± 0,4 (5–6) | 5,2 ± 0,75 (4–6) |
| Blocker của Kiểm | 2, 2, 2, 2, 2 | 2, 2, 2, 2, 2 |
| Blocker giả "chưa xác định kiểu `due_date`" của Kiểm | **5/5 lần** | **0/5 lần** |
| Major tổng (tb) | 7,4 | 9,4 |
| Review đọc được đủ 4 hướng | 5/5 | 5/5 |
| Thời gian một review (4 hướng) | 46–64 s | 61–76 s |

**Đọc kết quả.** Tổng blocker giảm 0,6 — gần bằng một độ lệch chuẩn, **không phải khác biệt chắc chắn**. Thay đổi rõ là *loại* blocker:
blocker giả về kiểu ngày (spec đã định nghĩa ở Technical Plan) biến mất hoàn toàn (5/5 → 0/5), và Kiểm nêu thay bằng **lỗi thật** chỉ thấy
khi đọc cả file: (1) kế hoạch không có file test nào trong Target Files/Tasks và Verification Plan không có test cụ thể — đúng là bước build
của lượt 3 không viết test nào; (2) **mâu thuẫn giữa Goals ("`add()` bắt buộc nhận `due_date`, phá API cũ") và kế hoạch ("optional")**. Đọc
nhiều hơn → nhiều major hơn (+2) và chậm hơn ~12 s mỗi review. Gemini flash nêu nhiều blocker hơn e4b trên cùng spec (5–6 so với 3–4), nên
số này không so trực tiếp với các lượt e4b.

Kiểm: `cargo test --lib` 3776 qua / 0 lỗi (14 bỏ qua: +1 test đo); e2e mock 43/43.

## Câu hỏi còn mở

- Đo lại G trên e4b khi máy LAN bật (cùng lệnh, đổi `SPEC_MEASURE_CONFIG` sang config có e4b)?
- Phản biện vẫn nêu blocker kiểu "không có giới hạn số công việc tối đa" cho module trong bộ nhớ — siết định nghĩa BLOCKER trong lời dặn?
- F (chặn ghi spec trước khi người trả lời lần đầu) — lượt 3 cho thấy F' đủ để e4b hỏi; có lẽ không cần.
- Thời gian `/review` trên model hosted (Antigravity/Claude) chưa đo.
