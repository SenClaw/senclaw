# Datagen — sinh dataset trong daemon

Datagen chạy **job nền** để tạo dataset: mỗi job ghi một thư mục
`~/senclaw/datasets/<tên>/` (gốc là `$SENCLAW_DATA_HOME/datasets`) gồm:

| File | Nội dung |
|---|---|
| `data.jsonl` | một bản ghi mỗi dòng, ghi ngay khi được chấp nhận, nên huỷ hoặc crash vẫn giữ phần đã có |
| `manifest.json` | tham số, model, số chấp nhận / loại **theo lý do**, số lần gọi LLM, token, trạng thái |

Thư mục quyền 0700, file quyền 0600, vì bản export chat có thể chứa nội dung file agent đã đọc.

Mã nguồn: [`src/datagen/`](../src/datagen/mod.rs). Nghiên cứu nền:
[research-261012-2244-camel-flows-datagen-memory.md](../plans/reports/research-261012-2244-camel-flows-datagen-memory.md) §3.

## Pipeline

### `expand_seeds` — mở rộng câu mẫu

Đưa vài câu yêu cầu mẫu, nhận về nhiều câu mới. Mục đích chính là sinh **bộ eval có nhãn** (ví dụ cho flow router).

| Tham số | Mặc định | Ý nghĩa |
|---|---|---|
| `seeds` | bắt buộc | chuỗi, hoặc `{"text", "label"}` (`instruction` là bí danh của `text`). Hoặc gắn nhãn hết, hoặc không gắn câu nào |
| `labels` | `{}` | mô tả cho từng nhãn, dùng cho cả bước sinh lẫn bước phân loại |
| `mode` | `new` | `new`: viết câu mới cùng loại. `evolve`: viết lại một câu theo phương pháp `add_constraints` → `concretize` → `deepen` → `reword` → `shift_topic` |
| `target` | 50 | số bản ghi cần (tối đa 2000) |
| `language` | ngôn ngữ của seed | vd. `"Vietnamese"` |
| `checkLabel` | bật khi có ≥ 2 nhãn | một lần gọi LLM riêng phân loại từng lô **mà không biết nhãn dự định**; chỉ giữ câu khớp nhãn |
| `bannedPhrases` | `[]` | cụm từ cấm, so **nguyên từ** ("data" không chặn "database") |
| `minWords` / `maxWords` | 3 / 150 | độ dài |
| `maxSimilarity` | 0.7 | ROUGE-L với seed và các câu đã nhận; vượt ngưỡng là `too_similar` |
| `batch` | 5 | số câu mỗi lần gọi (tối đa 10) |
| `maxLlmCalls` | target ÷ batch × số lần gọi mỗi vòng × 3, trong khoảng 10–500 | trần cứng 2000 |
| `profile` | model đang dùng | id hoặc label của LLM config |

Cách job dừng:
- đủ `target`
- hết `maxLlmCalls`
- 5 vòng liền không nhận được câu nào
- model lỗi 3 lần liền

Job kết thúc mà **không có bản ghi nào** thì là `failed`, kèm lý do: lỗi của model, hoặc thống kê kiểu
`5 duplicate, 3 too_similar`.

Các nhãn được giữ cân bằng:
- `target` chia đều cho các nhãn.
- Nhãn đang thiếu nhiều nhất được sinh trước.
- Nhãn nào 3 vòng liền không ra câu nào thì nhường lượt cho nhãn khác, nên một nhãn khó không làm dừng cả job.

Câu đã nhận được dùng lại làm ví dụ cho lần gọi sau.

Tham số REST dùng camelCase. Khoá lạ (vd. `max_llm_calls`) bị **từ chối** với 422, kèm danh sách khoá hợp lệ; không bị lờ đi.

Kiểm nhãn **fail-closed**: trả lời của bộ phân loại không đọc được (thử lại 1 lần) thì cả lô bị loại với lý do
`label_check_error`, không bao giờ được nhận.

Mỗi bản ghi:

```json
{"id":"<job>-1","text":"…","label":"team_dag","method":"new","from":["ví dụ đã đưa"],"model":"…"}
```

### `doc_qa` — hỏi đáp nhiều bước từ tài liệu

Cắt tài liệu thành đoạn (~350 token). Mỗi đoạn cần ≥ 30 từ và ≥ 2 câu. Mỗi đoạn có một lần gọi LLM viết các cặp hỏi
đáp kèm **trích dẫn** các dữ kiện dùng để trả lời. Một cặp chỉ được giữ khi:
- mỗi trích dẫn có thật trong đoạn: ≥ 80% số từ của nó khớp **đúng thứ tự** trong một khúc liền của đoạn văn, khúc
  này dài hơn trích dẫn tối đa 2 từ. Kiểm bằng code chứ không bằng model. Ghép chữ từ hai câu khác nhau thì không
  tính (nếu không: `ungrounded`);
- khi bật `multiHop`, các dữ kiện phải đến từ **≥ 2 câu khác nhau**; hai nửa của cùng một câu chỉ tính là một
  (nếu không: `single_hop`);
- câu hỏi không gần trùng câu đã có (`too_similar`);
- với `verify` bật, một lần gọi riêng, thấy cả đoạn văn lẫn cặp hỏi đáp, xác nhận đoạn văn đủ để trả lời
  (nếu không: `unsupported`). Trả lời của bước này không đọc được thì cả lô bị loại (`verify_check_error`).

| Tham số | Mặc định | Ý nghĩa |
|---|---|---|
| `texts` / `paths` | cần ít nhất một | văn bản trực tiếp; hoặc đường dẫn tuyệt đối tới file `.md` / `.markdown` / `.txt` hay thư mục chứa chúng (tối đa 500 file, 2 MB mỗi file, tổng 20 MB). Xem giới hạn đọc file bên dưới |
| `target` | 50 | số cặp (tối đa 2000) |
| `perPassage` | 3 | số cặp hỏi mỗi đoạn (tối đa 6) |
| `multiHop` | `true` | |
| `verify` | `true` | |
| `language`, `maxLlmCalls`, `profile` | | như trên |

Giới hạn khi đọc file:
- file loại khác bị từ chối;
- không bao giờ đọc thư mục của chính SenClaw (`~/.senclaw`, nơi chứa API key và toàn bộ chat);
- không đi theo symlink;
- bỏ qua file ẩn và các thư mục `node_modules`, `target`, `Library`, `dist`, `build`, `vendor`.

Các tài liệu được xen kẽ nhau, nên target nhỏ vẫn lấy từ mọi tài liệu. Mỗi đoạn chỉ dùng một lần; hết đoạn thì job
dừng và ghi chú lại.

Bản ghi mang theo đoạn văn trong `context`, nên dùng được làm eval cho retrieval:

```json
{"id":"…","question":"…","answer":"…","reasoning_steps":["…"],"supporting_facts":["…","…"],
 "context":"<đoạn văn>","source":{"doc":"<file>","passage":0},"model":"…"}
```

### `reasoning_traces` — lời giải từng bước có kiểm tra

Với mỗi bài toán, quy trình như sau:
1. Model giải bài, kết thúc bằng dòng `Answer: …`. Nếu không có dòng đó thì đọc `\boxed{…}`, xử lý được ngoặc lồng.
2. Nếu có đáp án tham chiếu, đáp án được so **bằng code**: bỏ định dạng, so số theo giá trị.
3. Một lần gọi riêng chấm 3 điểm từ 0 đến 1: correctness, clarity, completeness.
4. Bài chỉ đạt khi **mọi** điểm ≥ `threshold` và đáp án đúng.
5. Chưa đạt thì cải thiện theo nhận xét, tối đa `maxIterations` vòng.

Chỗ khác với CAMEL: model giải bài **không bao giờ thấy đáp án tham chiếu**. Nó chỉ được báo là "đáp án sai", nên lời
giải được giữ là lời giải thật, không phải lời biện hộ cho đáp án có sẵn. Bước chấm cũng không thấy đáp án: đúng hay
sai đã do code quyết. Nếu nhận xét của bước chấm vẫn nhắc tới đáp án (vì tự tính ra), đáp án được xoá khỏi nhận xét
trước khi gửi lại cho model giải bài. Lý do loại: `wrong_answer`, `below_threshold`, `judge_check_error`. Số chỉ bỏ dấu
phẩy khi đó là dấu phân cách hàng nghìn (`1,000`); `1,5` (thập phân kiểu Việt) không bị đọc thành 15.

| Tham số | Mặc định | Ý nghĩa |
|---|---|---|
| `problems` | bắt buộc | chuỗi, hoặc `{"problem"/"question", "answer"/"final_answer"}` (đáp án có thể là số) |
| `target` | mọi bài | |
| `maxIterations` | 3 | số vòng cải thiện (tối đa 5) |
| `threshold` | 0.7 | trong khoảng 0–1, áp cho **từng** điểm |
| `maxLlmCalls` | 4 × target | |

```json
{"id":"…","problem":"…","trace":"…","answer":"51","reference":"51",
 "scores":{"correctness":1,"clarity":0.9,"completeness":0.9},"iterations":1,"model":"…"}
```

### `executable_qa` — bài toán tính được, nhãn từ việc chạy code

Model viết bài mới cùng loại với seed, kèm một chương trình Python 3 (chỉ dùng thư viện chuẩn) in ra đáp án.
Chương trình chạy trong **sandbox của SenClaw**, không có mạng, và **stdout chính là đáp án**: nhãn đến từ việc thực
thi, không từ lời model.

Khác với CAMEL (chạy trong virtualenv trên máy thật), code do model viết **không bao giờ chạy mà không bị cách ly**:
- Job bị từ chối ngay từ đầu nếu máy không có cách ly thật (Seatbelt, bubblewrap, AppContainer hoặc Docker).
- Một lần chạy trả về mà không bị cách ly thì job dừng.
- Công tắc *Run Python* ở Plugins → Sandbox vẫn được tôn trọng, và được kiểm **trước** lượt gọi LLM đầu tiên.

Lý do loại: `code_failed`, `timeout`, `empty_output`, `long_output` (> 200 ký tự), `bad_code`, `too_similar`.

| Tham số | Mặc định | Ý nghĩa |
|---|---|---|
| `seeds` | bắt buộc | chuỗi, hoặc `{"question", "answer"?, "code"?}` |
| `target` | 50 | tối đa 1000 |
| `batch` | 3 | số bài mỗi lần gọi (tối đa 5) |
| `timeoutMs` | 10000 | cho mỗi chương trình, trong khoảng 1000–60000 |

```json
{"id":"…","question":"…","answer":"80","code":"print(80)","isolation":"seatbelt","from":["…"],"model":"…"}
```

### `export_logs` — xuất lịch sử chat

Chuyển các turn đã ghi trajectory ([docs/evals.md](evals.md); trajectory **tắt mặc định**, bật theo từng chat)
thành dataset. Không gọi LLM.

| Tham số | Mặc định | Ý nghĩa |
|---|---|---|
| `chats` | bắt buộc qua REST | danh sách jid, hoặc `["*"]` là mọi chat. Gọi từ chat (MCP) thì **chỉ** xuất được chính chat đó |
| `format` | `sharegpt` | `sharegpt` (giữ tool call / observation), `alpaca` (`instruction` / `input` / `output`), `messages` (kiểu OpenAI, đã bỏ trường nội bộ) |
| `maxRecords` | 10 000 | tối đa 100 000 |

Turn bị bỏ khi:
- bị ngắt (`interrupted`) hoặc lỗi (`errored`)
- không kết thúc bằng câu trả lời (`no_answer`)
- trùng một bản ghi đã có (`duplicate`)

Mỗi bản ghi có thêm `source: {chat, turn}`.

## Dùng thế nào

**Từ giao diện:** trang **Datagen** trên web (`/datagen`, `web-app/src/pages/DatagenPage.tsx`) và desktop (thanh điều hướng → Datagen, `desktop/lib/features/datagen/`). Cả hai cho chạy mọi pipeline, xem tiến độ và lý do loại, huỷ job, xem bản ghi theo trang, xoá dataset. Định dạng dòng trong form giống nhau:
- `nhãn | câu yêu cầu` cho `expand_seeds`;
- `câu hỏi || đáp án` cho `reasoning_traces` và `executable_qa`;
- tài liệu ngăn bằng một dòng `---` cho `doc_qa`.

Hai trang hiển thị mọi job, không lọc theo chat.

**Từ chat** (MCP `senclaw-datagen`):

| Tool | Làm gì |
|---|---|
| `datagen_start` | trả về `jobId` ngay. Ngoài tham số của từng pipeline (viết snake_case), nhận thêm `seeds_file` / `problems_file`: file `.json` (mảng) hoặc `.jsonl` |
| `datagen_status` | không có `job_id` thì liệt kê job gần đây và các dataset của **chat này** (không kèm `params`) |
| `datagen_cancel` | dừng job của chat này, giữ các bản ghi đã ghi |

Gọi `datagen_start` đi qua permission prompt như mọi tool MCP khác.

Đường dẫn tương đối (`seeds_file`, `problems_file`, `paths`) được tính theo **thư mục làm việc của chat**
(workspace), không theo thư mục của tiến trình. Nếu không biết thư mục của chat thì đường dẫn tương đối bị từ chối; khi
đó hãy đưa đường dẫn tuyệt đối.

Mọi tool chỉ thấy job và dataset do chính chat đó tạo. Job được gắn `owner` là chat đó.

Ba tool này nằm trong roster của mọi chat, tốn khoảng 1k token mỗi turn. Chúng có ở cả chế độ MCP bundled (mặc
định) lẫn `SENCLAW_MCP_BUNDLED=false`.

**REST:**

```text
POST /api/datagen/jobs               body: {"pipeline": "...", "name"?: "...", "owner"?: "<jid>", ...tham số}
GET  /api/datagen/jobs[?owner=]      job của lần chạy daemon này
GET  /api/datagen/jobs/:id[?owner=]
POST /api/datagen/jobs/:id/cancel[?owner=]
GET  /api/datagen/datasets[?owner=]  manifest mọi dataset trên đĩa
GET  /api/datagen/datasets/:name/records?offset=&limit=[&owner=]   một trang bản ghi (tối đa 200) + tổng số
DELETE /api/datagen/datasets/:name[?owner=]                         xoá dataset (409 khi job của nó còn chạy)
```

`POST /api/datagen/jobs` nhận body tới 8 MB (văn bản dán trực tiếp, danh sách seed). Danh sách dataset không kèm
`params`; tham số đầy đủ vẫn nằm trong `manifest.json`. Mỗi trang bản ghi tối đa 200 bản ghi và 2 MB.

Mã lỗi:

| Mã | Khi nào |
|---|---|
| 400 | tham số sai giá trị, hoặc chưa cấu hình model |
| 422 | body không đọc được: khoá lạ, sai kiểu, `pipeline` không tồn tại |
| 409 | đã đủ 2 job đang chạy; huỷ một job không còn chạy; xoá dataset đang được ghi |
| 404 | không có job hay dataset đó, hoặc của chat khác |
| 500 | không tạo được thư mục dataset |

Ví dụ sinh bộ eval cho router:

```json
{
  "pipeline": "expand_seeds",
  "name": "router-eval-vi",
  "seeds": [
    {"text": "chia việc làm báo cáo thị trường cho 3 người rồi gộp lại", "label": "team_dag"},
    {"text": "mở trang shopee tìm tai nghe dưới 500k", "label": "browser"},
    {"text": "mỗi sáng 7h tóm tắt email chưa đọc", "label": "scheduled"}
  ],
  "labels": {
    "team_dag": "split into parts for several specialists, run in parallel, then merge",
    "browser": "open, read or fill web pages",
    "scheduled": "repeat or run later at a set time"
  },
  "target": 150,
  "language": "Vietnamese"
}
```

## Giới hạn

- Tối đa **2 job chạy cùng lúc**.
- Trạng thái job nằm trong bộ nhớ của daemon. Sau khi daemon khởi động lại, dataset vẫn còn trên đĩa; manifest nào
  còn ghi `running` sẽ hiện là `interrupted`.
- Mỗi lần gọi LLM có trần 600 s. Huỷ job ngắt cuộc gọi đang chạy ngay lập tức.
- Các pipeline sinh dữ liệu chạy tuần tự trong một job: mỗi job một luồng gọi LLM.
