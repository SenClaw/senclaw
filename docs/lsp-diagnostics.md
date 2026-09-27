# LSP diagnostics sau khi sửa file

> Trạng thái: **đã cài** (2026-09-12, code-v2 phase 4). Nguồn:
> [`src/lsp/`](../src/lsp/) (`client.rs` JSON-RPC stdio, `mod.rs` quản lý
> server + render), hook trong `zen_core/run_tools.rs`, tool ở
> [`src/tools/lsp_tools.rs`](../src/tools/lsp_tools.rs), REST ở
> [`src/gateway/ui_server/lsp.rs`](../src/gateway/ui_server/lsp.rs).

## Luồng

```
Edit/Write/NotebookEdit xong
  → lsp::diagnostics_after_write(working_dir, path)
      → server cho ngôn ngữ (spawn nếu chưa có; 1 tiến trình / workspace / ngôn ngữ)
      → didOpen (lần đầu) hoặc didChange (toàn văn) + didSave
      → chờ publishDiagnostics **của đúng file đó** ≤ timeout (mặc định 5 s)
  → nối block "LSP diagnostics (rust-analyzer) for src/x.rs: 1 error(s)…"
    vào tool result; data.lspDiagnostics cho UI
```

Model thấy `error[E0308] mismatched types` ngay trong kết quả Edit, trước khi
tự nghĩ ra chạy `cargo check`. Không có server → không nối gì, không lỗi.

## Server

Chỉ dùng cái **đã có trên PATH**; SenClaw không tải gì.

| Ngôn ngữ | Lệnh mặc định |
|---|---|
| rust | `rust-analyzer` |
| typescript / tsx / javascript | `typescript-language-server --stdio` |
| python | `pyright-langserver --stdio` |
| go | `gopls` |
| dart | `dart language-server --protocol=lsp` |
| c / cpp | `clangd` |

Ghi đè hoặc thêm ngôn ngữ trong `~/.senclaw/lsp.json`:

```json
{ "enabled": true, "timeoutMs": 5000,
  "servers": { "rust": { "command": "/opt/ra/rust-analyzer" },
               "ruby": { "command": "solargraph", "args": ["stdio"] } } }
```

`SENCLAW_LSP=0` tắt toàn bộ. Server rảnh 10 phút bị tắt; server khởi động
hỏng 2 lần liên tiếp bị vô hiệu cho workspace đó (xem `lsp_status`).

## Ba bẫy đã tránh (từ OpenCode issue #12288, #16353, #16880)

1. **Luôn gửi `didChange`** cho file đã mở — server giữ bản trong bộ nhớ, không
   đọc lại đĩa; thiếu bước này diagnostics mãi cũ.
2. **Chờ theo URI**: diagnostics của file khác (cả dự án, dự án bên cạnh) được
   lưu nhưng không bao giờ báo là của file vừa sửa.
3. **Deadline do người gọi chọn**, mặc định 5 s (OpenCode 3 s bỏ sót
   rust-analyzer). Hết hạn trả về cái đang biết và đánh dấu `stale`.

## Tool & API

- `lsp_diagnostics {path?}` — hỏi lại một file (đồng bộ lại rồi chờ), hoặc mọi
  file server đang báo. `lsp_status` (deferred) — server nào đang chạy/cài/bị
  vô hiệu.
- `GET /api/lsp/status`, `GET|PUT /api/lsp/settings` (toàn bộ `lsp.json`;
  `timeoutMs` 500–60000).

## Kiểm chứng

- Unit: render (lỗi trước, cắt 20, một dòng/thông điệp), URI, settings.
- Thật (`--ignored`): `cargo test --lib lsp::tests::a_real_server_reports_a_type_error -- --ignored --nocapture`
  tạo dự án tạm có lỗi kiểu (`var x int = "no"` với gopls, hoặc
  `let x: u32 = "no";` với rust-analyzer) và nhận lỗi ở dòng 2 qua giao thức
  thật. Đo trên máy dev: gopls trả `IncompatibleAssign` sau **393 ms**.
- Bẫy đã gặp khi đo: `~/.cargo/bin/rust-analyzer` là **proxy của rustup**;
  thiếu component thì nó thoát ngay với "Unknown binary". Client giữ đuôi
  stderr để báo đúng câu đó, và manager vô hiệu server sau 2 lần hỏng thay vì
  thử lại ở mỗi lần Edit.
