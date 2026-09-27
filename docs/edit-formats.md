# Edit format theo model

> Trạng thái: **đã cài** (2026-09-12, code-v2 phase 6). Nguồn:
> `EditFormat` ở [`src/zen_core/mod.rs`](../src/zen_core/mod.rs), matching ở
> [`src/tools/edit_apply.rs`](../src/tools/edit_apply.rs), tool ở
> [`src/tools/edit.rs`](../src/tools/edit.rs), reminder ở
> `ZenEngine::assemble_system_prompt`.

## Vì sao

Quan sát của Aider: model lớn chép `old_string` đúng từng byte; model nhỏ
(Qwen 0.8B–4B local, Gemma E2B) lệch một khoảng trắng hoặc một ký tự, và
`Edit` exact từ chối → lượt sửa hỏng dù model *mô tả* đúng thay đổi. Cho
model đó một cách khớp khoan dung hơn, hoặc bảo nó gửi unified diff / ghi cả
file, giảm hẳn số lần apply thất bại.

## Bốn format

| `editFormat` | Edit làm gì | Prompt nói gì |
|---|---|---|
| `exact` (mặc định) | `old_string` phải có nguyên văn (hành vi cũ) | — |
| `fuzzy` | exact → không thấy thì khớp bỏ qua khác biệt khoảng trắng → không thấy thì cửa sổ dòng giống ≥ 0.9 (duy nhất); kết quả ghi rõ "matched by whitespace/similar" | nhắc: khác biệt nhỏ được tha, nhưng vẫn chép từ file vừa đọc |
| `udiff` | như `fuzzy`, và model được bảo dùng tham số `patch` | nhắc: gửi hunk `' '/'-'/'+'`, số dòng `@@` bị bỏ qua, mỗi hunk tìm theo nội dung |
| `whole` | như `fuzzy` | nhắc: sau khi đọc, ghi cả file bằng `Write`; `Edit` chỉ cho một dòng ngắn |

Tham số `patch` của `Edit` **luôn** có, ở mọi format (kể cả exact): hunk
được tìm theo nội dung — exact, rồi bỏ qua khoảng trắng, rồi tương tự — và
**mọi hunk phải khớp** hoặc không áp dụng gì (lỗi nêu đúng hunk hỏng).

## Đặt ở đâu

- **Settings → Models**: mỗi config LLM có `editFormat` (`GET/PUT
  /api/llm-config`, trường `editFormat`), lưu trong `config.json`.
- **Space App LLM** (`senclaw-manifest.json`): `"llm": { …, "editFormat":
  "fuzzy" }` — áp cho mọi model app phục vụ, lưu vào bảng
  `space_app_llm_providers`. Giá trị sai chính tả là **lỗi parse** (như
  `adapt`), không im lặng về `exact`.
- Không đặt = `exact`.

Luồng: `LlmConfig.edit_format` → `ModelProfile.edit_format` →
`RunContext.hook_profile` → `ToolContext.edit_format` → `Edit`. Reminder
vào system prompt sau `<repo_map>`, trước `AGENTS.md`.

## Đo

`GET /api/code/edit-stats` → `{attempts, exact, fuzzy, udiff, failed}` kể từ
khi daemon chạy. So sánh `failed/attempts` trước và sau khi đổi format cho
một model. (Bench thật trên Qwen3 4B local chưa chạy trong lần cài này —
cần app `mlx-lm` đang chạy; xem `plans/260912-0141-code-v2/phase-6-edit-format.md`.)

## Kiểm chứng

`cargo test --lib tools::edit` (exact từ chối lệch; fuzzy áp dụng và nói rõ;
`patch` hoạt động ở mọi format, hunk sai → không đổi file) và
`cargo test --lib tools::edit_apply` (whitespace, similar, mơ hồ → từ chối;
udiff theo nội dung; lỗi có tên hunk và atomic).
