# `senclaw-sdk/` — SDK viết Space App, bản publish được

Mỗi thư mục ở đây là **một package độc lập, publish lên registry công khai**, để
người viết Space App ngoài monorepo chỉ cần `npm install` / `pip install` /
`go get` chứ không phải clone cả repo SenClaw.

| Thư mục | Package | Registry |
|---|---|---|
| [`senclaw-app-sdk/`](senclaw-app-sdk) | `@senclaw/space-sdk` | npm |
| [`senclaw-app-sdk-python/`](senclaw-app-sdk-python) | `senclaw-space-sdk` | PyPI |
| [`senclaw-app-sdk-go/`](senclaw-app-sdk-go) | `github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go` | Go modules (chính repo này) |

Bản **Rust** vẫn ở [`../app-space-sdk`](../app-space-sdk): nó là workspace member
của chính repo này, và repo ngoài trỏ tới nó bằng git dependency chứ không qua
crates.io — xem [docs/space-app-sdk-publish-guide.md](../docs/space-app-sdk-publish-guide.md).

Cả bốn nói cùng một hợp đồng manifest: chế độ vòng đời (`background` /
`session`), `requires`, `sandbox`, `runner` —
[docs/space-app-lifecycle.md](../docs/space-app-lifecycle.md).

> English: [README.md](README.md).

## Ngang tính năng với bản Rust

Cột Rust là chuẩn — `app-space-sdk` là bản đầy đủ nhất vì phần lớn app trong
`apps/*` viết bằng Rust. Ba bản kia bám theo.

| | Rust | Node | Python | Go |
|---|:--:|:--:|:--:|:--:|
| `llm.request` (system/prompt/maxTokens/**profile**) | ✅ | ✅ | ✅ | ✅ |
| Trả về đủ `text` + `model` + `finish` + `usage` | `llm_request_usage` | `llmDetailed` | `llm_detailed` | `LLMDetailed` |
| `agent.run` (agent đủ tool, nhiều bước) | — ¹ | ✅ | ✅ | ✅ |
| `knowledge.save` / `.search` / `.recall` | ✅ | ✅ | ✅ | ✅ |
| `usage.report` (app tự cầm key provider) | ✅ | ✅ | ✅ | ✅ |
| Liệt kê / đổi model đang hoạt động | ✅ | ✅ | ✅ | ✅ |
| `capabilities` — hỏi daemon nó làm được gì | — ¹ | ✅ | ✅ | ✅ |
| Config + SQLite riêng của app | — ¹ | ✅ | ✅ | ✅ |
| Đăng ký MCP server | — ¹ | ✅ | ✅ | ✅ |
| Máy chủ MCP dựng sẵn | — ² | `/mcp` | `McpServer` | `MCPServer` |
| Dispatch (poll/heartbeat/reclaim/finalize) | ✅ | `/dispatch` | `dispatch.py` | `/dispatch` |
| Manifest: định nghĩa + kiểm + CLI | — ³ | `/lifecycle` + `senclaw-manifest` | `manifest.py` + `-m` | `manifest` + `cmd/senclaw-manifest` |
| `bind_host` / `PORT` / tắt êm | thủ công | `/lifecycle` | `serve()` | `Serve()` |

¹ App Rust gọi thẳng `POST /api/space/apps/<id>/bridge` bằng `reqwest` —
`SpaceClient::bridge_action` là private nên chưa có wrapper công khai. Không
phải thiếu năng lực, chỉ là chưa bọc.
² App Rust dùng `rmcp` trực tiếp, không cần lớp bọc.
³ Manifest của app Rust do người viết tay; test
[`space_app_lifecycle_manifests.rs`](../tests/space_app_lifecycle_manifests.rs)
là thứ bắt lỗi chính tả cho toàn repo.

`events` / `fs` / `net` của bản Rust **không có bản tương ứng, và không cần**:
chúng chỉ là lớp mô phỏng `EventEmitter`, `fs.readFile`, `net.createServer` của
Node cho Rust. Node, Python và Go đã có sẵn trong thư viện chuẩn.

Mỗi SDK mang theo app mẫu chạy được của chính nó trong `examples/` — cài bằng
`register-local` là daemon chạy được ngay, không cần build gì thêm.

## Chọn bản nào

| | |
|---|---|
| **Rust** | App nằm trong monorepo này ở `apps/*`, hoặc cần khởi động nhanh nhất và nhẹ nhất |
| **Node** | App chủ yếu là web UI, hoặc dựa vào một thư viện npm |
| **Python** | App dựa vào hệ sinh thái Python (ML, scraping, dữ liệu). Không phụ thuộc thì không có bước cài |
| **Go** | Một binary tĩnh, máy người dùng không cần cài runtime — nhưng app Go **không có bước install**, nên phải ship bản đã build hoặc biên dịch ngay trong `start` ([chi tiết](senclaw-app-sdk-go/README-vn.md#đọc-trước-app-go-không-có-bước-cài-đặt)) |

## Publish

```bash
# npm
cd senclaw-app-sdk && npm publish        # `prepare` tự build lại dist/ trước khi đóng gói

# PyPI
cd senclaw-app-sdk-python && python -m build && python -m twine upload dist/*

# Go — không có bước registry: một git tag là một bản phát hành.
git tag senclaw-sdk/senclaw-app-sdk-go/v0.1.0 && git push origin --tags
```

npm và PyPI bump version bằng tay trong `package.json` / `pyproject.toml` —
không có bước sinh version tự động, và một version đã publish là không sửa được.
Go lấy version từ tag, và tag **bắt buộc** mang tiền tố thư mục con của module,
nếu không `go get` sẽ không thấy.
