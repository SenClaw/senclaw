---
name: datagen
description: Tạo hoặc nhập DATASET trong nền bằng Datagen của SenClaw — bộ câu có nhãn để đo router/bộ phân loại, hỏi đáp có trích dẫn từ tài liệu, lời giải từng bước có kiểm, bài toán có đáp án từ việc chạy code, xuất lịch sử chat, hoặc nhập dataset do người viết (catalog có sẵn, Hugging Face, URL, file). Dùng khi việc cần là DỮ LIỆU để đo, kiểm thử hay huấn luyện một thành phần AI — hàng chục đến hàng nghìn bản ghi có kiểm — không phải một câu trả lời.
version: 1.0.0
when-to-use: khi người dùng cần một bộ dữ liệu — "tạo bộ eval cho router", "sinh 200 câu yêu cầu có nhãn", "làm câu hỏi kiểm tra từ tài liệu này", "dữ liệu fine-tune", "lời giải từng bước cho các bài này", "xuất chat thành sharegpt", "nhập MASSIVE / GSM8K / dataset Hugging Face", "nhập file CSV này thành dataset" — hoặc khi chính bạn cần đo một thứ (prompt, skill, router) trên nhiều ví dụ có nhãn thay vì vài câu thử tay. KHÔNG dùng cho dưới ~20 ví dụ cần ngay trong câu trả lời (tự viết, xem skill `datagen-author`), và không dùng để trả lời một câu hỏi.
triggers:
  - datagen
  - sinh dữ liệu
  - sinh dataset
  - tạo dataset
  - tạo bộ dữ liệu
  - bộ dữ liệu
  - dataset
  - bộ eval
  - bộ đo
  - eval set
  - evaluation set
  - dữ liệu huấn luyện
  - dữ liệu fine-tune
  - training data
  - fine-tune data
  - synthetic data
  - dữ liệu tổng hợp
  - câu hỏi kiểm tra từ tài liệu
  - hỏi đáp từ tài liệu
  - lời giải từng bước
  - reasoning traces
  - nhập dataset
  - import dataset
  - hugging face dataset
  - gsm8k
  - massive
  - xuất lịch sử chat
  - sharegpt
allowed-tools:
  - mcp__senclaw-datagen__datagen_start
  - mcp__senclaw-datagen__datagen_status
  - mcp__senclaw-datagen__datagen_cancel
  - mcp__senclaw-schedule__schedule_watch
  - Read
---

# Datagen

Datagen chạy **job nền** trong daemon. Mỗi job ghi một thư mục `~/senclaw/datasets/<tên>/` gồm `data.jsonl` (một bản
ghi mỗi dòng) và `manifest.json` (tham số, model, số giữ / loại **theo lý do**, số lời gọi LLM). Job sinh dữ liệu gọi
model của người dùng nhiều lần — tốn tiền và thời gian — nên chọn đúng pipeline và chạy thử nhỏ trước.

Tool: `mcp__senclaw-datagen__datagen_start`, `mcp__senclaw-datagen__datagen_status`,
`mcp__senclaw-datagen__datagen_cancel`. Chúng được nạp cùng skill này; chỉ `ToolSearch` nếu chúng **không** có trong danh
sách tool của bạn. Tham số viết snake_case.

## 1. Chọn đường

| Người dùng cần | Làm |
|---|---|
| Một bộ đã có người viết sẵn (intent tiếng Việt, toán có đáp án, hỏi đáp có đoạn văn) | `import` từ catalog — **0 lời gọi LLM**. Gọi `datagen_status` (không `job_id`) để xem `catalog` |
| Dữ liệu của chính họ (CSV, JSONL, JSON) | `import` với `source: {"file": "<đường dẫn>"}` |
| Nhiều câu yêu cầu mới, có nhãn (bộ đo router / phân loại) | `expand_seeds` với `seeds` hoặc `seeds_from` |
| Câu hỏi + đáp án có trích dẫn từ tài liệu (đo tìm kiếm, ôn tập) | `doc_qa` với `paths` hoặc `texts` |
| Lời giải từng bước được chấm, có đối chiếu đáp án | `reasoning_traces` với `problems` hoặc `problems_from` |
| Bài toán tính được, đáp án lấy từ việc chạy code | `executable_qa` (cần sandbox) |
| Lịch sử chat này thành dữ liệu huấn luyện | `export_logs` (chỉ có dữ liệu khi trajectory đang bật cho chat) |
| Dưới ~20 ví dụ, hoặc chưa có seed nào | Tự soạn trước — skill `datagen-author` — rồi quay lại đây để mở rộng |

Catalog hiện có:

| id | dạng | dòng | license | dùng cho |
|---|---|---|---|---|
| `senclaw-flows-vi` | câu có nhãn = skill SenClaw | 144 | MIT, có sẵn, offline | seed / bộ đo router |
| `massive-vi` | câu có nhãn (đổi sang skill SenClaw) | 2.974 → 2.030 | CC BY 4.0, **chỉ để đo** | bộ đo router |
| `xquad-vi` | hỏi đáp + đoạn văn | 1.190 | CC BY-SA 4.0 | đo tìm kiếm |
| `gsm8k` | bài toán + đáp án | 1.319 | MIT | `reasoning_traces`, `executable_qa` |
| `self-instruct-seed` | yêu cầu tiếng Anh | 175 | Apache-2.0 | seed |
| `dolly-15k` | yêu cầu tiếng Anh, nhãn = loại việc | 15.011 | CC BY-SA 3.0 | seed |

Kit đã cài có thể thêm mục dạng `<kit>/<id>`.

## 2. Mẫu lời gọi

**Nhập từ catalog.** Mục MIT / Apache (`needsConsent: false`, vd. `gsm8k`, `senclaw-flows-vi`) nhập thẳng, **không**
kèm `accept_license`:

```json
{"pipeline": "import", "source": {"catalog": "gsm8k"}, "max_records": 200}
```

Mục có điều kiện (`needsConsent: true`: share-alike, chỉ để đo — `massive-vi`, `xquad-vi`, `dolly-15k`) bị từ chối nếu
thiếu `accept_license`. **Không bao giờ tự đặt `accept_license: true`.** Đọc điều kiện cho người dùng, chờ họ nói đồng ý
trong chat, rồi mới gửi:

```json
{"pipeline": "import", "source": {"catalog": "massive-vi"}, "accept_license": true}
```

**Nhập file của người dùng** (đường dẫn tương đối tính theo thư mục làm việc của chat; file trong `~/.senclaw` bị từ chối):

```json
{"pipeline": "import", "source": {"file": "data/yeu-cau.csv"}, "kind": "labeled_text",
 "mapping": {"text": "Nội dung", "label": "Loại"}, "license": "private"}
```

`kind`: `labeled_text` (`text`, `label?`), `qa_context` (`question`, `answer`, `context`), `problem_answer` (`problem`,
`answer?`), `chat` (`user`, `assistant`, `system?`). Bỏ `mapping` thì Datagen đoán theo tên cột; đoán không được thì lỗi
kèm danh sách cột. `license` bắt buộc (`private` / `unknown` được).

**Mở rộng câu yêu cầu từ một dataset:**

```json
{"pipeline": "expand_seeds", "name": "router-eval-vi", "seeds_from": "senclaw-flows-vi", "per_label": 3,
 "target": 60, "language": "Vietnamese",
 "labels": {"calendar": "tạo, sửa, tìm sự kiện và nhắc hẹn", "schedule": "tác vụ agent chạy định kỳ tự động"}}
```

Hoặc `seeds` gõ trực tiếp: `[{"text": "…", "label": "…"}]` — gắn nhãn hết hoặc không câu nào. Có ≥ 2 nhãn thì mỗi lô
được một bộ phân loại **không thấy nhãn dự định** kiểm lại; câu lệch nhãn bị loại (`label_mismatch`).

**Lời giải từng bước cho bài đã nhập:**

```json
{"pipeline": "reasoning_traces", "problems_from": "gsm8k", "target": 20, "language": "Vietnamese"}
```

**Hỏi đáp từ tài liệu** (đường dẫn `.md` / `.txt`, file hoặc thư mục):

```json
{"pipeline": "doc_qa", "paths": ["docs/huong-dan.md"], "target": 30, "per_passage": 2, "language": "Vietnamese"}
```

**Bài toán chạy code:** `{"pipeline": "executable_qa", "seeds_from": "gsm8k", "target": 20, "language": "Vietnamese"}`.

## 3. Quy tắc để không tốn công

1. **Chạy thử nhỏ trước.** Lần đầu với một cấu hình: `target` 10–20. Đọc lý do loại rồi mới chạy lớn.
2. **Luôn đặt `language`.** Không đặt thì model nhỏ có thể tự đổi ngôn ngữ (đã gặp: lời giải viết bằng tiếng Tây Ban Nha).
3. **3–6 nhãn mỗi job, có mô tả nhãn.** Với 12 nhãn, bộ phân loại loại 30 câu và job hết ngân sách trước khi đủ.
4. **Ngân sách.** Mặc định khoảng 3 lần số lời gọi cần cho một lần chạy không câu nào bị loại (trần 500). Việc ước chừng
   trên 200 lời gọi thì nói với người dùng con số trước khi chạy.
5. **Tối đa 2 job cùng lúc.** Gặp lỗi 409 thì chờ hoặc huỷ bớt, không thử lại liên tục.

## 4. Sau khi bắt đầu: đừng chờ trong lượt này

`datagen_start` trả về `jobId` ngay. Một job sinh dữ liệu chạy vài phút đến hàng giờ. **Không** `sleep`, không gọi
`datagen_status` lặp lại trong lượt, không bảo người dùng "hỏi lại sau". Hãy đặt watch rồi kết thúc lượt:

```json
mcp__senclaw-schedule__schedule_watch {
  "tool": "mcp__senclaw-datagen__datagen_status",
  "args": {"job_id": "<jobId>"},
  "done_path": "job.state",
  "done_op": "in",
  "done_values": ["done", "failed", "cancelled"],
  "interval_secs": 60,
  "timeout_secs": 7200,
  "resume_prompt": "Datagen job <jobId> (<tên dataset>) đã kết thúc: {{result}}. Báo người dùng: số giữ/mục tiêu, số lời gọi LLM, các lý do loại nhiều nhất và ý nghĩa của chúng, đường dẫn dataset, 2–3 bản ghi mẫu (đọc vài dòng đầu của data.jsonl), rồi đề xuất bước tiếp."
}
```

`datagen_status {"job_id": …}` trả `{"job": {"state": "running" | "done" | "failed" | "cancelled", …}}`, nên `job.state`
là đường dẫn đúng. `args` là **object** JSON, không phải chuỗi. Giữ `{{result}}` trong `resume_prompt`: đó là nơi số liệu
của job được điền vào. Job import từ catalog có sẵn hay từ file xong gần như ngay — với chúng chỉ cần gọi
`datagen_status` một lần.

**Khi watch đánh thức bạn:** chỉ báo những số có trong kết quả (`accepted`, `target`, `llmCalls`, `rejected`, `dir`).
Thiếu số nào thì gọi `datagen_status {"job_id": …}` để lấy. Bản ghi mẫu thì đọc `<dir>/data.jsonl` bằng `Read`. Không bao
giờ để chỗ trống kiểu "(dữ liệu sẽ hiện ở đây)", và không đoán đường dẫn: lấy `dir` từ kết quả.

## 5. Đọc kết quả và báo lại

- `accepted` / `target`, `llmCalls` / `maxLlmCalls`, `rejected` (lý do → số), `note` (vì sao dừng sớm), `error`.
- Bản ghi nằm ở `<dir>/data.jsonl`. Đọc vài dòng đầu bằng `Read` để cho người dùng xem mẫu.

| Lý do | Nghĩa → làm gì |
|---|---|
| `label_mismatch` | bộ phân loại xếp câu vào nhãn khác → thêm mô tả nhãn, giảm số nhãn |
| `too_similar`, `duplicate` | gần trùng hoặc trùng → bình thường; nhiều quá thì thêm seed đa dạng, hoặc dùng `mode: "evolve"` |
| `truncated` | model dừng ở trần token (thường do model suy nghĩ dài) → dùng model ít suy nghĩ hơn |
| `unparseable_output`, `*_check_error` | trả lời không đọc được; bước kiểm không đọc được thì cả lô bị loại → thử lại, hoặc đổi `profile` |
| `ungrounded`, `single_hop`, `unsupported` | `doc_qa`: trích dẫn không có trong đoạn văn / chỉ dựa vào một câu / đoạn văn không đủ → dùng tài liệu mạch lạc hơn |
| `wrong_answer`, `below_threshold` | `reasoning_traces`: đáp án sai / điểm chấm dưới ngưỡng |
| `code_failed`, `empty_output`, `timeout` | `executable_qa`: chương trình lỗi / không in gì / chạy quá giờ |
| `missing_<trường>`, `unmapped_label` | `import`: cột ánh xạ sai, hoặc nhãn không có trong bảng đổi nhãn |

Job `failed` thì không có bản ghi nào được giữ; đọc `error`. Nói thật với người dùng: lỗi của model khác với "mọi câu đều
bị loại vì trùng", và cũng khác với cấu hình sai.

Bản ghi qua kiểm tra tự động **chưa chắc đã hợp lý**: `executable_qa` có thể giải một đề vô lý (đếm món đồ ra 5,5555),
và câu sinh ra có thể nằm ở ranh giới giữa hai nhãn. Khi dataset dùng để huấn luyện, nhắc người dùng đọc lướt.

## 6. Giới hạn và an toàn

- Dữ liệu import ghi `license` và nguồn vào manifest. Dataset CC BY-SA khi dùng lại phải ghi nguồn và giữ cùng license.
  Tập test của MASSIVE không dùng để huấn luyện.
- Không đưa API key, token hay dữ liệu cá nhân vào `seeds` / `texts`. Dataset chỉ chat này thấy (trừ dataset nhập từ
  catalog), nhưng nằm trên đĩa của người dùng.
- `export_logs` từ chat chỉ xuất **chính chat này**.
- Huỷ: `mcp__senclaw-datagen__datagen_cancel {"job_id": …}` — các bản ghi đã viết vẫn được giữ.
- Người dùng xem và quản lý mọi job, dataset ở trang **Datagen** (web và desktop): tab "Dataset gốc", nút "Dùng làm seed".
