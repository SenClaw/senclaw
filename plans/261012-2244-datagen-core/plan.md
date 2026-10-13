# Plan: datagen lõi trong daemon

Status: done (review fixes applied) · Ngày: 2026-10-12 · Nhánh: `feat/datagen-core` (từ `feat/claude-code-cowork` @ 543b677)
Nguồn: [research-261012-2244-camel-flows-datagen-memory.md](../reports/research-261012-2244-camel-flows-datagen-memory.md) §3

## Outcome

Daemon sinh dataset như một job nền có thể huỷ, ghi JSONL + manifest tái lập được. Có 2 pipeline:
- `expand_seeds`: mở rộng seed (có nhãn) thành nhiều yêu cầu mới, dùng để sinh bộ eval cho flow router.
- `export_logs`: chuyển trajectory của chat thành dataset ShareGPT / Alpaca / messages, không gọi LLM.

Agent dùng được qua MCP; client dùng được qua REST.

## Quyết định (người dùng chọn 2026-10-12)

- Đặt **trong daemon** (`src/datagen/`), không làm Space App.
- Phạm vi: lõi (verdict, filters, generator, store, jobs) + `export_logs` + `expand_seeds`.

## Non-goals

- Chưa làm `doc_qa`, `reasoning_traces`, `executable_qa` (Phase 4 của báo cáo).
- Chưa làm flow router, chưa có UI web/desktop.
- Không tự đẩy dataset lên Hugging Face.

## Thiết kế

```text
src/datagen/
  mod.rs          doc module, kiểu chung (JobSpec, JobStatus), hàm start/status/list/cancel
  verdict.rs      Verdict {Success, Failure, Error, Timeout}; retry chỉ khi Error/Timeout
  filters.rs      FilterChain có lý do loại: độ dài, cụm từ cấm (nguyên từ), trùng chính xác, ROUGE-L
  generator.rs    AcceptLoop: accept-until-N, budget số lần gọi LLM, đếm lý do loại
  llm.rs          trait Llm + DaemonLlm (llm_config::chat_completion + record_completion)
  store.rs        $SENCLAW_DATA_HOME/datasets/<name>/{data.jsonl, manifest.json}; dir 0700, file 0600
  jobs.rs         registry toàn cục (OnceLock), spawn tokio task, CancellationToken
  expand_seeds.rs mode new|evolve, kiểm nhãn theo lô (fail-closed)
  export_logs.rs  trajectory → sharegpt|alpaca|messages
src/gateway/ui_server/datagen.rs   REST
src/mcp/datagen_server.rs          MCP senclaw-datagen: datagen_start, datagen_status, datagen_cancel
```

- REST:
  - `POST /api/datagen/jobs`
  - `GET /api/datagen/jobs`
  - `GET /api/datagen/jobs/:id`
  - `POST /api/datagen/jobs/:id/cancel`
  - `GET /api/datagen/datasets`
- Registry toàn cục thay vì field mới trong `UiState`, vì có 14 chỗ dựng struct literal.
- MCP server chỉ là cầu loopback giống `patterns_server`, có gắn token qua `internal_auth::header_for`.
- Gọi `datagen_start` vẫn qua permission prompt mặc định của MCP. Prompt hiển thị pipeline, target và `max_llm_calls`.

## Sửa lỗi của CAMEL khi port

- Lọc cụm từ cấm theo nguyên từ, không theo substring (CAMEL: "data" chặn luôn "database").
- Item sinh ra được đưa lại làm ví dụ cho lần sau (CAMEL `SelfInstructGenerator` không bao giờ append).
- Output rỗng hoặc parse lỗi được tính là `Error` có retry, không crash (`generated_tasks[0]` của CAMEL).
- Kiểm nhãn **fail-closed**: parse lỗi thì không nhận item.
- Ghi từng dòng ngay khi chấp nhận, không ghi đè cả file mỗi bản ghi.

## Acceptance

- [x] `cargo test` cho `datagen` xanh (34 test + 3 test MCP): filters, ROUGE-L, AcceptLoop budget, parse JSON lỏng, expand_seeds với LLM giả (kiểm nhãn, dedupe), export_logs 3 format, store quyền file, cancel.
- [x] `cargo build` sạch, `cargo test --workspace` xanh (2797 lib + integration).
- [x] `docs/openapi-daemon.yaml` refresh, `committed_spec_matches_the_routers` xanh.
- [x] Chạy daemon cách ly (`SENCLAW_HOME=<scratch>`, port 28788/28789): `export_logs` trên trajectory thật ra file; `expand_seeds` qua model đã cấu hình ra dataset có manifest.
- [x] Tài liệu: `docs/datagen.md`, CLAUDE.md có thêm dòng `senclaw-datagen` trong bảng registry.

## Rủi ro

- Roster tool tăng thêm 3 tool mỗi chat. Giữ 3 tool, mô tả ngắn.
- Chi phí LLM: `max_llm_calls` mặc định giới hạn, có trần cứng; job huỷ được giữa chừng.
- Dataset có thể chứa nội dung file agent đã đọc (export trajectory) → quyền 0600, phải chỉ rõ chat; `*` nghĩa là tất cả.

## Kết quả E2E (2026-10-12, daemon cách ly, model giả OpenAI-compatible)

- `expand_seeds` 3 nhãn, target 12: 12/12 trong 6 lần gọi, cân bằng 4/4/4. Item gán nhãn sai bị loại (`label_mismatch`).
- Lần chạy đầu cho ra 5/5/2. Đã sửa bằng quota đều cho từng nhãn.
- `export_logs` sharegpt: 2/3 turn, turn bị ngắt bị bỏ. Có giữ `function_call` / `observation`.
- MCP stdio: `tools/list` có đúng 3 tool. `export_logs` mặc định lấy chat hiện tại. Job id có đường dẫn bị từ chối. `core-server` bundled có `datagen_*`.
- Usage được ghi vào `llm_usage_log` với jid `datagen:<tên>`.

## Review (code-reviewer, 2026-10-12) và cách xử lý

| # | Phát hiện | Xử lý |
|---|---|---|
| H1 | `seeds_file` tương đối bị đọc theo cwd của daemon (MCP child không có `current_dir`) | Tính theo thư mục workspace của chat; không biết thư mục thì từ chối; báo lỗi kèm đường dẫn đã resolve; sửa CLAUDE.md và docs |
| M1 | Một nhãn khó làm dừng cả job | Đếm stall theo từng nhãn; nhãn stall 3 vòng nhường lượt |
| M2 | Dedupe export giữ cả bản ghi trong RAM; I/O chặn async | Dedupe bằng hash u64; đọc file trong `spawn_blocking` |
| M3 | Trần token của classifier quá thấp với reasoning model | Sinh 512+160n, phân loại 256+48n; 0 bản ghi → `failed` kèm lý do |
| M4 | Tool MCP không giới hạn theo chat | Job mang `owner`; `?owner=` cho list/status/cancel/datasets; listing bỏ `params`; export từ chat chỉ xuất chính chat đó |
| L1 | Pipeline panic giữ slot mãi | Chạy trong task con; panic → `failed` |
| L2 | Model luôn lỗi + budget nhỏ → `done` 0 bản ghi | 0 bản ghi luôn là `failed` |
| L3 | Huỷ giữa chừng làm mất bộ đếm; huỷ sát lúc xong bị ghi `cancelled` | Publish trước khi trả về; pipeline trả `Outcome` |
| L4 | Phương thức evolve lặp theo nhãn | Đếm vòng theo từng nhãn |
| L5 | So nhãn chỉ không phân biệt hoa/thường với ASCII | So bằng `to_lowercase` (Unicode) |
| L7 | Khoá REST lạ bị lờ đi | `deny_unknown_fields` → 422 |
| nit | clippy (`is_multiple_of`, `Known::len`), docs nhãn usage, mã HTTP | Đã sửa; 409/404/500 |

Không làm: fsync manifest (rủi ro thấp). Chi phí roster ~1k token mỗi turn được chấp nhận và đã ghi vào docs.

## E2E sau khi sửa (2026-10-13, daemon cách ly 38788/38789)

- MCP chạy với cwd `/`: `seeds_file: "seeds.jsonl"` resolve theo workspace của chat, ra 6/6 bản ghi.
- Export không truyền `chats` thì chỉ xuất chat gọi tool (2 bản ghi).
- Chat khác không thấy, không đọc, không huỷ được job (404).
- Listing không lộ `params`.
- `bannedPhrases` chặn mọi câu: job `failed`, lỗi `no record passed (25 banned_phrase)`.
- Khoá lạ: 422. Huỷ id không tồn tại: 404.

Sự cố trong lúc test:
- Cổng 28788 đang bị daemon test của **session khác** giữ (cùng quy ước cổng). Một job thử nghiệm đã lỡ tạo trên daemon đó, đã huỷ với 0 lần gọi LLM. Folder rỗng `mcp-relative-seeds` còn nằm trong scratch của session kia.
- Ổ đĩa đầy (còn 117 MB). Đã xoá cache tự sinh lại được `target/debug/incremental` (9.7 GB) và các file `*.rcgu.o` còn sót trong `target/debug/deps` (8.3 GB). Build sau đó dùng `CARGO_INCREMENTAL=0`.

## Hoàn thiện 100% (2026-10-13, theo yêu cầu "đảm bảo hoàn thành 100% tính năng")

Phạm vi mở rộng:
- 3 pipeline còn lại: `doc_qa`, `reasoning_traces`, `executable_qa` (chạy trong sandbox thật).
- API `GET /datasets/:name/records`, `DELETE /datasets/:name`.
- fsync cho manifest.
- MCP ở chế độ không bundled.
- UI web (`DatagenPage.tsx`) và desktop (`lib/features/datagen/`). Mobile ghi N/A kèm lý do trong `docs/client-parity.md`.

Kiểm chứng:
- daemon: 58 test datagen + 6 test MCP; chạy E2E trên daemon cách ly:
  - `doc_qa`: loại cặp có trích dẫn bịa (`ungrounded`).
  - `reasoning_traces`: sửa lời giải sai sau 1 vòng.
  - `executable_qa`: chạy thật trong sandbox Seatbelt, đáp án = stdout (66, 91, 105); chương trình crash bị loại `code_failed`.
  - xoá dataset, phân trang bản ghi, chặn `..` và chặn chat khác (404).
- web: `npm run build` sạch; đã dùng thử trên trình duyệt với daemon cách ly (danh sách, xem bản ghi, form tạo job → 6/6).
- desktop: `flutter analyze` sạch, `flutter test` 367 qua (gồm 7 test mới).

## Review vòng 2 (2026-10-13) và cách xử lý

| # | Phát hiện | Xử lý |
|---|---|---|
| H1 | Đáp án tham chiếu lọt về model giải bài qua nhận xét của judge | Judge không còn thấy đáp án; đáp án được xoá khỏi nhận xét; có test với nhận xét chứa "51" |
| H2 | Form desktop gửi ngầm `target=50` cho `reasoning_traces` | Đổi pipeline thì form mới; thêm ô Target cho `reasoning_traces`; có test chuyển pipeline |
| M1 | Kiểm trích dẫn không xét thứ tự từ | Khớp LCS ≥ 80% trong cửa sổ dài hơn trích dẫn tối đa 2 từ; multi-hop đếm theo câu khác nhau |
| M2 | `doc_qa` đọc mọi loại file, kể cả `~/.senclaw` | Chỉ `.md`/`.txt`; cấm `senclaw_home()`; không theo symlink; bỏ `node_modules`/`target`…; MCP bỏ dòng đường dẫn trống |
| M3 | Danh sách dataset nặng mà lại bị poll | Bỏ `params` khỏi danh sách; đọc bằng `spawn_blocking` |
| M4 | So đáp án xoá hết dấu phẩy | Chỉ bỏ dấu phẩy phân cách hàng nghìn |
| M5 | Form web mang `target` sang pipeline khác | `preserve={false}` và mount lại form theo pipeline |
| L1–L6 | Đếm trùng `llm_error`; preflight sandbox; giới hạn byte mỗi trang; xoá chặn luồng async; body > 2 MB; phản hồi phân trang đến muộn | Đã sửa hết. `POST /jobs` nhận body tới 8 MB |

Không làm:
- `Registry::start` vẫn giữ lock trong lúc tạo thư mục; việc này nhanh và bảo vệ giới hạn số job chạy.
- Chưa huỷ được lúc đang duyệt thư mục; đã có giới hạn 500 file và độ sâu 5.

Kết quả cuối:
- Rust: 2900 test qua (60 test datagen), clippy không cảnh báo ở code mới.
- web: `npm run build` sạch.
- desktop: `flutter analyze` sạch, 368 test qua (8 test datagen).
