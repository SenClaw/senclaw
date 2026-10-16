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

### `import` — nhập dataset do người viết

Đưa một dataset có sẵn vào `datasets/`, chuẩn hoá về một trong bốn **dạng** mà các pipeline khác đọc thẳng được. Không
gọi LLM. Mã nguồn: [`import.rs`](../src/datagen/import.rs), [`mapping.rs`](../src/datagen/mapping.rs),
[`catalog.rs`](../src/datagen/catalog.rs). Thiết kế:
[import-defaults.html](../plans/261014-2217-datagen-integration/import-defaults.html).

| Dạng | Trường | Đọc bởi |
|---|---|---|
| `labeled_text` | `text`, `label?` | seed cho `expand_seeds`; bộ đo router |
| `qa_context` | `question`, `answer`, `context` | bộ đo tìm kiếm; seed kiểu `doc_qa` |
| `problem_answer` | `problem`, `answer?` | `reasoning_traces`, `executable_qa` |
| `chat` | `user`, `assistant`, `system?` | dữ liệu fine-tune, giống `export_logs` |

`source` là đúng một trong bốn nguồn:
- `{"catalog": "<id>"}`: một mục trong catalog (bên dưới), mang sẵn dạng, ánh xạ, bảng đổi nhãn và license;
- `{"hf": {"dataset", "config"?, "split"?}}`: dataset công khai trên Hugging Face, đọc qua dataset viewer API
  (`/rows`, 100 dòng mỗi lần). Dataset bị khoá (gated) hoặc riêng tư bị từ chối;
- `{"url": "https://…"}`: file JSONL / JSON / CSV / TSV, tối đa 32 MB;
- `{"file": {"name", "content"}}`: như trên, gửi kèm trong request, tối đa 8 MB.

| Tham số | Mặc định | Ý nghĩa |
|---|---|---|
| `kind` | bắt buộc (trừ catalog) | một trong bốn dạng |
| `mapping` | đoán theo tên cột | trường → cột, vd. `{"text": "utterance", "label": "intent"}`. Cột là đường dẫn (`answers.text.0`); một trường có thể ghép nhiều cột (`{"from": ["a", "b"]}`) hoặc chỉ lấy phần sau một dấu (`{"from": "answer", "after": "####"}`) |
| `labelMap` | không | tên bảng có sẵn (`massive-senclaw`) hoặc `{nhãn gốc: nhãn mới}`. Nhãn không có trong bảng, hoặc ánh xạ sang `null`, bị bỏ (`unmapped_label`) |
| `license` | bắt buộc (trừ catalog) | license của nguồn, hoặc `private` / `unknown`. Ghi vào manifest cùng bản ghi |
| `attribution` | không | tác giả, nguồn |
| `maxRecords` | 20 000 | tối đa 100 000 |
| `acceptLicense` | `false` | bắt buộc `true` với mục catalog có điều kiện (share-alike, chỉ để đo…) — người dùng phải đồng ý trước |

Mỗi dòng bị bỏ khi:
- thiếu trường bắt buộc (`missing_<trường>`);
- quá dài (`too_long`);
- không có trong bảng đổi nhãn (`unmapped_label`);
- bị dataset viewer cắt ngắn (`cut_by_viewer`);
- trùng một bản ghi đã có (`duplicate`).

Manifest có thêm `provenance`:

```json
{"kind":"labeled_text","license":"CC-BY-4.0","attribution":"MASSIVE, Amazon …","flags":{"noTraining":true},
 "source":{"type":"hf","catalog":"massive-vi","dataset":"mteb/amazon_massive_intent","config":"vi","split":"test",
           "revision":"940fd47a81…","pinnedRevision":"940fd47a81…"},
 "labelMap":"massive-senclaw","mapping":{"id":"id","text":"text","label":"label"},"sourceRows":2974,"dataSha256":"…"}
```

`dataSha256` là sha256 của `data.jsonl`. Dataset viewer chỉ phục vụ **phiên bản hiện tại** của dataset (`/rows` không nhận
revision). Vì vậy commit ghim trong catalog chỉ để **kiểm**: import ghi lại commit thực sự đã đọc, và job có ghi chú nếu
commit đó khác commit ghim. Dataset viewer giới hạn tốc độ: mỗi trang nghỉ 250 ms, và mỗi lần gọi thử lại tối đa 8 lần
khi gặp 429 / 5xx.

Bản ghi:

```json
{"id":"3","text":"im lặng","label":"none","sourceLabel":"audio_volume_mute","source":{"row":1}}
```

`accept` có thêm khi bảng đổi nhãn cho phép nhiều nhãn đúng (vd. `weather_query` → `["web-research", "agent-browser",
"widget"]`).

#### Catalog

`GET /api/datagen/catalog` liệt kê các mục, mỗi mục kèm `installed` (đã có một lần import xong) và `dataset`. Mục *có
sẵn* nằm trong binary nên cài không cần mạng. Chỉ dữ liệu SenClaw tự viết hoặc có license MIT / Apache / CC BY mới được
nhúng sẵn; dữ liệu share-alike chỉ tải về khi bấm Cài.

| Mục | Nguồn | Dòng | Dạng | License |
|---|---|---|---|---|
| `senclaw-flows-vi` | có sẵn, SenClaw viết | 144 | `labeled_text`, 12 nhãn = skill có sẵn + `none` | MIT |
| `massive-vi` | `mteb/amazon_massive_intent` vi/test | 2 974 (giữ 2 030 sau khi đổi nhãn) | `labeled_text` | CC BY 4.0, chỉ để đo |
| `xquad-vi` | `google/xquad` xquad.vi/validation | 1 190 | `qa_context` | CC BY-SA 4.0 |
| `gsm8k` | `openai/gsm8k` main/test | 1 319 | `problem_answer` | MIT |
| `self-instruct-seed` | `HuggingFaceH4/self-instruct-seed` | 175 | `labeled_text` | Apache-2.0 |
| `dolly-15k` | `databricks/databricks-dolly-15k` | 15 011 | `labeled_text` (nhãn = loại việc) | CC BY-SA 3.0 |

Các mục khai trong [`assets/datasets/catalog.json`](../assets/datasets/catalog.json), bảng đổi nhãn trong
`assets/datasets/label-maps/`. Test kiểm:
- mọi nhãn của `senclaw-flows-vi` và của `massive-senclaw` là tên một skill có thật trong `skills/` (hoặc `none`);
- mục nào có điều kiện thì có `notice`;
- mục có điều kiện không bao giờ được nhúng sẵn.

Một Zen Kit có thể thêm mục vào catalog qua khối `datasets` trong manifest ([zen-kits.md](zen-kits.md)):
- cài kit chỉ ghi các mục vào `<kits_dir>/datasets/<kit>.json`, không tải gì;
- mục hiện trong catalog với id `<kit>/<id>` và nhãn "Kit · <kit>";
- nguồn là Hugging Face hoặc URL, không bao giờ là dữ liệu nhúng sẵn;
- gỡ kit thì các mục biến khỏi catalog; dataset đã nhập từ chúng vẫn giữ lại.

Manifest có mục không cài được (nhúng dữ liệu, ánh xạ sai dạng, cần đồng ý mà không có `notice`, URL nội bộ) bị từ chối
ngay khi đọc kit.

Dataset nhập từ catalog đọc được từ **mọi** chat (dữ liệu công khai). Dataset khác chỉ chat tạo ra nó thấy.

#### Xem trước

`POST /api/datagen/import/preview` với `{source, kind?, mapping?, labelMap?}` không ghi gì. Trả về:
- `columns`, theo thứ tự cột của file / dataset;
- tối đa 20 `rows`;
- `total`;
- `suggested`: dạng và ánh xạ đoán được;
- `records`: mỗi dòng thành bản ghi, hoặc `rejected` kèm lý do;
- với Hugging Face: `splits`, `config`, `split`, `license`.

### Dùng một dataset làm đầu vào

`expand_seeds` và `executable_qa` nhận `seedsFrom`, `reasoning_traces` nhận `problemsFrom`:

```json
{"pipeline": "expand_seeds", "seedsFrom": {"dataset": "massive-vi", "perLabel": 3}, "target": 100}
```

- `limit`: số bản ghi tối đa, mặc định là trần của pipeline.
- `perLabel`: số bản ghi tối đa cho mỗi nhãn.
- Bản ghi được chọn ở các vị trí rải đều trong file, nên dataset xếp theo nhãn vẫn cho đủ các nhãn.
- Bản ghi được đọc khi job bắt đầu và ghi vào `params` của manifest như seed thường.
- Cho cả `seeds` lẫn `seedsFrom` thì bị từ chối.
- Dataset đang được ghi thì bị từ chối.

## Dùng thế nào

Hướng dẫn từng bước, có kết quả chạy thử thật (16/10/2026):
[usage-guide.html](../plans/261014-2217-datagen-integration/usage-guide.html).

**Từ giao diện:** trang **Datagen** trên web (`/datagen`, `web-app/src/pages/DatagenPage.tsx`) và desktop (thanh điều hướng → Datagen, `desktop/lib/features/datagen/`). Cả hai cho chạy mọi pipeline, xem tiến độ và lý do loại, huỷ job, xem bản ghi theo trang, xoá dataset. Cả hai còn có:
- tab **Dataset gốc**: thẻ catalog, mỗi thẻ có nút Cài (mục có điều kiện hỏi đồng ý trước);
- nút **Nhập file / URL…**: tab File / URL / Hugging Face, có xem trước, chọn dạng, chọn cột và license;
- nút **Dùng làm seed / Dùng làm bài toán** trên mỗi dataset, mở form job mới với dataset đó đã được chọn. Định dạng dòng trong form giống nhau:
- `nhãn | câu yêu cầu` cho `expand_seeds`;
- `câu hỏi || đáp án` cho `reasoning_traces` và `executable_qa`;
- tài liệu ngăn bằng một dòng `---` cho `doc_qa`.

Hai trang hiển thị mọi job, không lọc theo chat.

**Từ chat** (MCP `senclaw-datagen`):

| Tool | Làm gì |
|---|---|
| `datagen_start` | trả về `jobId` ngay. Ngoài tham số của từng pipeline (viết snake_case), nhận thêm `seeds_file` / `problems_file`: file `.json` (mảng) hoặc `.jsonl`. `import` nhận `source` (`{"file": "<đường dẫn>"}` được đọc trong chat rồi gửi kèm; file trong thư mục của SenClaw bị từ chối), `kind`, `mapping`, `label_map`, `license`, `accept_license`, `max_records`. Các pipeline sinh nhận `seeds_from` / `problems_from` (tên dataset) và `per_label` |
| `datagen_status` | không có `job_id` thì liệt kê job gần đây, các dataset của **chat này** và dataset nhập từ catalog (không kèm `params`), và catalog |
| `datagen_cancel` | dừng job của chat này, giữ các bản ghi đã ghi |

Gọi `datagen_start` đi qua permission prompt như mọi tool MCP khác.

Hai skill có sẵn dạy agent dùng các tool này:

| Skill | Agent làm gì |
|---|---|
| [`datagen`](../skills/datagen/SKILL.md) | chọn pipeline (ưu tiên catalog, 0 lời gọi LLM), gọi `datagen_start`, đặt `schedule_watch` trên `datagen_status` rồi kết thúc lượt, khi được đánh thức thì báo số liệu thật và 2–3 bản ghi mẫu. Không tự đồng ý license thay người dùng |
| [`datagen-author`](../skills/datagen-author/SKILL.md) | tự soạn bộ nhỏ: taxonomy nhãn có định nghĩa, câu đa dạng và câu đối lập. Lưu JSONL vào thư mục làm việc, nhập bằng `import` (license `private`), rồi đề xuất `expand_seeds` |

Đã chạy thử qua `senclaw acp` với model local `google/gemma-4-e4b` (16/10/2026):
- `datagen`: agent tự nạp skill, nhập 40 bài GSM8K, đặt watch; khi watch xong thì báo 40/40 kèm 3 bản ghi thật đọc từ
  `data.jsonl`.
- `datagen-author`: agent soạn 3 nhãn × 4 câu, ghi file, nhập 12/12, rồi hỏi trước khi mở rộng.

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
GET  /api/datagen/catalog[?owner=]   catalog dataset gốc, mỗi mục kèm installed / dataset
POST /api/datagen/import/preview     body: {"source", "kind"?, "mapping"?, "labelMap"?} — xem trước, không ghi gì
```

Import là `POST /api/datagen/jobs` với `"pipeline": "import"`. Không có route riêng.

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

## Model có bước suy nghĩ

Model local như `gemma-4-e4b` trên LM Studio suy nghĩ khoảng 1000 token trước khi viết JSON. Vì vậy trần token của mỗi
lần gọi có thêm `THINKING_ROOM` (1024).

Một trả lời bị nhà cung cấp cắt ở trần (`finish_reason: "length"`) và không đọc được thì bị loại với lý do `truncated`,
không phải `unparseable_output`. Hai lý do này cần cách sửa khác nhau:
- `truncated`: tăng trần, hoặc dùng model ít suy nghĩ hơn;
- `unparseable_output`: model không viết đúng định dạng.

Đo ngày 16/10/2026: `doc_qa` trên `docs/zen-kits.md` lên từ 0/6 (5 `unparseable_output`) thành 6/6 sau khi thêm phần
này.

## Giới hạn

- Tối đa **2 job chạy cùng lúc**.
- Trạng thái job nằm trong bộ nhớ của daemon. Sau khi daemon khởi động lại, dataset vẫn còn trên đĩa; manifest nào
  còn ghi `running` sẽ hiện là `interrupted`.
- Mỗi lần gọi LLM có trần 600 s. Huỷ job ngắt cuộc gọi đang chạy ngay lập tức.
- Các pipeline sinh dữ liệu chạy tuần tự trong một job: mỗi job một luồng gọi LLM.
