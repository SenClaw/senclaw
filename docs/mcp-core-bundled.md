# MCP gom: `senclaw-core`

Một tiến trình MCP duy nhất phục vụ tool của **mọi** built-in server, thay cho mười bốn tiến trình con mỗi phiên agent.

Mã nguồn: [`src/mcp/core_server.rs`](../src/mcp/core_server.rs) · cấu hình: [`src/mcp/helper.rs`](../src/mcp/helper.rs) · nơi dùng: [`src/agent/agent_pool/pool.rs`](../src/agent/agent_pool/pool.rs)

## 1. Vấn đề

Mỗi built-in server là một subcommand của chính binary `senclaw`, nói MCP qua stdio và nhận cấu hình bằng biến môi trường:

```
senclaw wiki-server        SENCLAW_WIKI_DIR=…
senclaw workspace-server   SENCLAW_WORKSPACE_STATE_FILE=… SENCLAW_DEFAULT_WORKSPACE=…
senclaw memory-server      SENCLAW_DB_PATH=… SENCLAW_FOLDER=… SENCLAW_AGENTS_DIR=…
…
```

`AgentPool::ensure_agent` đẩy **mười bốn** config như vậy cho mỗi phiên chat. Hệ quả:

- mười bốn tiến trình con cho mỗi agent đang mở — nhân lên theo số chat;
- bốn trong số đó (`schedule`, `background`, `usage`, `space`) mở **cùng một file SQLite**, tức bốn kết nối và bốn WAL reader cho cùng một dữ liệu;
- mười bốn lần khởi động binary, mười bốn lần bắt tay MCP, trước khi agent trả lời được câu đầu tiên.

## 2. Cách gom

`core-server` **không** phải một server viết lại. Nó nạp chính các struct server cũ trong cùng một tiến trình rồi trộn bảng tool của chúng:

```
                 ┌─ McpWikiServer      ─ tool_router() ─┐
core-server ───┼─ McpWorkspaceServer ─ tool_router() ─┼──► một bảng tool duy nhất
   (stdio)       ├─ McpMemoryServer    ─ tool_router() ─┤
                 └─ …                                   ┘
```

Ba mảnh ghép:

**`from_env() -> Result<Option<Self>>` trên mỗi server.** `None` nghĩa là "phiên này không cấu hình con đó" — không phải lỗi. Nhờ vậy một phiên chỉ có `SENCLAW_WIKI_DIR` vẫn chạy được: các con khác bị bỏ qua im lặng, thay vì kéo sập cả tiến trình. Biến có mặt nhưng hỏng (đường dẫn DB không mở được) thì vẫn là lỗi thật.

**`vis = "pub"` trên `#[rmcp::tool_router]`.** Macro sinh ra `fn tool_router() -> ToolRouter<Self>`; mặc định nó private nên chỉ module đó dùng được. Mở public để `core_server` gọi được từ ngoài.

**`ServerHandler` viết tay.** `list_tools` nối bảng tool của mọi con đang sống; `call_tool` tra tên tool ra chủ sở hữu rồi chuyển tiếp nguyên `CallToolRequestParams` nhận được. Không phải khai báo lại một tool nào — thêm tool mới vào `wiki_server.rs` là nó tự xuất hiện ở đây.

Router của mỗi con dựng **một lần** lúc khởi động, không dựng lại mỗi lần gọi: `tool_router()` dựng lại toàn bộ map, lãng phí trên đường nóng.

### DB dùng chung

[`helper::shared_env_db()`](../src/mcp/helper.rs) mở `SENCLAW_DB_PATH` **một lần cho cả tiến trình** (`OnceLock`) và trả `Arc<Db>` cho cả bốn con cần nó. Bốn kết nối SQLite thành một. Lỗi mở được nhớ lại dưới dạng chuỗi để mọi lượt gọi sau thấy đúng kết luận như lượt đầu — `anyhow::Error` không `Clone`.

## 3. Cấu hình

`core_mcp_config()` không tự liệt kê biến môi trường. Nó **gọi chính các builder cũ** (`wiki_mcp_config`, `workspace_mcp_config`, …) rồi trộn map env của chúng lại. Thêm một biến vào builder nào thì biến đó tự tới server gom, không ai phải nhớ cập nhật hai chỗ. Test `bundled_config_is_a_superset_of_the_separate_ones` khoá tính chất này.

`SENCLAW_CORE_SERVERS` (danh sách ngăn cách bằng dấu phẩy) chọn tập con phục vụ; mặc định là `DEFAULT_CORE_SERVERS` — cả mười bốn.

## 4. Bật/tắt

| | |
|---|---|
| Cờ | `mcp.bundled` trong `~/.senclaw/config.json` |
| Env | `SENCLAW_MCP_BUNDLED` |
| Mặc định | `true` |

Đặt `false` để quay về cách cũ: mười bốn config riêng, đúng như trước. Nhánh cũ vẫn nằm nguyên trong `pool.rs` chứ không bị xoá — con nào cũng còn subcommand riêng (`senclaw wiki-server`, …), nên vẫn chạy tách được khi cần soi lỗi một server.

## 5. Đã chạy thật

Kịch bản kiểm chứng: [`scripts/core_mcp_smoke.py`](../scripts/core_mcp_smoke.py) — dựng tiến trình thật rồi nói MCP qua stdio.

```bash
cargo build --bin senclaw
mkdir -p /tmp/zk && python3 scripts/core_mcp_smoke.py target/debug/senclaw /tmp/zk
```

Chạy `senclaw core-server` với **chỉ** biến của wiki + workspace:

```
serverInfo.name : senclaw-core
tools           : 41
  wiki_*        : wiki_mkdir, wiki_read, wiki_search, wiki_stats, wiki_status, wiki_tree, wiki_write
  workspace_*   : workspace_info, workspace_reset, workspace_switch
  core_*      : core_status
  con khác      : 30 tool (js, litho, sandbox — không cần biến nào nên luôn sống)
```

`wiki_status`, `workspace_info` và `core_status` đều gọi được qua **một** kết nối. Các con thiếu cấu hình (`memory`, `space`, `send`, …) vắng mặt mà không làm chết tiến trình — đúng hợp đồng `from_env() -> Option<Self>`.

Lần chạy này bắt được một lỗi mà không unit test nào thấy: server tự giới thiệu là `"rmcp"`. `ServerInfo::new` điền `serverInfo` từ build env của **thư viện rmcp**, nên phải `with_server_info` một cách tường minh. Đã sửa, và khoá lại bằng test `handshake_announces_the_senclaw_name`.

## 6. Nhìn từ ngoài

`GET /api/mcp-servers` liệt kê `senclaw-core` như một built-in, kèm tool `core_status` cho biết những con nào đang sống trong tiến trình. Agent Core đọc danh sách này và hiển thị nhóm "đi kèm Core" tách khỏi server người dùng tự thêm.

## 7. Giới hạn

- **Một tiến trình, một số phận.** Trước đây một server sập chỉ mất tool của nó; nay sập là mất cả bộ. Đổi lại đường khởi động ngắn hơn nhiều và ít tài nguyên hơn hẳn. Cần cách ly một server nghi ngờ thì tắt cờ.
- **Trùng tên tool.** Bảng tool gộp theo tên; hai con cùng đặt tên một tool thì con đăng ký sau bị bỏ qua và ghi log cảnh báo. Hiện không có cặp nào trùng — tiền tố `wiki_`/`workspace_`/`memory_`… giữ chúng tách nhau.
- **`senclaw-cognitive` và `senclaw-admin` không nằm trong danh sách.** Cognitive đã thành tool nội bộ (`tools::cognitive`), còn admin không được `AgentPool` đẩy vào phiên chat nào.
