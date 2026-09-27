# Repo map & symbol tools

> Trạng thái: **đã cài** (2026-09-12, code-v2 phase 3). Nguồn:
> [`src/repo_map/`](../src/repo_map/) (`lang`, `scan`, `parse`, `rank`,
> `render`), tool ở [`src/tools/repo_map_tools.rs`](../src/tools/repo_map_tools.rs),
> chèn vào prompt ở `ZenEngine::assemble_system_prompt`.

## Ý tưởng (Aider)

Parse mọi file nguồn bằng tree-sitter, lấy **định nghĩa** và **tham chiếu**
theo `tags.scm` của từng grammar, dựng đồ thị file→file (file tham chiếu tên
→ file định nghĩa tên đó), chạy PageRank cá nhân hoá theo file đang được nhắc
trong prompt, rồi cho model xem ~2 000 token chữ ký tốt nhất:

```
src/agent/agent_pool/pool.rs:
│  41│ pub struct AgentPool {
│ 928│ pub fn run_agent(&self, jid: &str) -> Result<()> {
```

Model biết *ở đâu* trước khi Grep. Cùng chỉ mục đó phục vụ 4 tool.

## Khi nào có map

- Working directory là **git repo** hoặc có manifest dự án (`Cargo.toml`,
  `package.json`, `pyproject.toml`, `go.mod`, `pubspec.yaml`, `pom.xml`,
  `CMakeLists.txt`, `Makefile`, …). `$HOME` và `/` không bao giờ được quét.
- Ngôn ngữ: Rust, Python, TypeScript/TSX, JavaScript, Go, Java, C, C++, C#.
  Dart chưa có (grammar `tree-sitter-dart` không tương thích API 0.25).
- Chỉ file ≤ 1 MB; bỏ `node_modules/`, `target/`, `.venv/`, `build/`,
  `dist/`, `vendor/`…; trong git repo dùng `git ls-files` nên `.gitignore`
  được tôn trọng đúng.
- **Không bao giờ chặn lượt chat.** Lượt đầu trong cây mới không có map;
  chỉ mục dựng nền và lượt sau có. Chỉ mục lưu ở `~/.senclaw/repo-map/`
  nên restart daemon không parse lại. Làm mới tăng dần theo mtime/size, tối
  đa 45 s một lần.
- Tắt: `SENCLAW_REPO_MAP_TOKENS=0`; đổi ngân sách: `SENCLAW_REPO_MAP_TOKENS=4000`.

## Tool

| Tool | Làm gì |
|---|---|
| `find_symbol {name, kind?, fuzzy?}` | nơi **định nghĩa** một hàm/struct/class/type — `path:line [kind] signature` |
| `find_references {name}` | file nào **gọi/dùng** tên đó, kèm số dòng (bán kính ảnh hưởng) |
| `symbol_body {name, path?}` | nguồn đầy đủ của một định nghĩa (≤ 300 dòng) không cần đọc cả file |
| `repo_map {focus?, budget_tokens?}` | outline theo yêu cầu, tập trung vào các file cho trước |

Trong thư mục không phải dự án, tool trả `not_a_project` và bảo dùng Grep.
Lần gọi đầu trong cây mới, tool **chờ** chỉ mục dựng xong (khác prompt block).

## Số đo (repo SenClaw, Apple Silicon, debug build)

| | |
|---|---|
| File được chỉ mục | 898 |
| Quét lạnh (parse tất cả) | 1.2 s |
| Làm mới nóng | 26 ms |
| Render map | 170 ms |
| Map 2 000 token ngân sách | ≈ 1 000 token thật (8.3 KB) |

Chạy lại: `cargo test --lib repo_map::bench -- --ignored --nocapture`.

## Chi tiết xếp hạng

- Cạnh file A → file B khi A tham chiếu tên mà B định nghĩa; trọng số
  `sqrt(số lần)/số file định nghĩa`.
- Tên định nghĩa ở **> 3 file** (`Error`, `new`, `Result`) và tên < 3 ký tự bị
  bỏ — chúng không nói file nào phụ thuộc file nào.
- Rust được bổ sung 3 mẫu ngoài `tags.scm` upstream: gọi qua path
  (`util::helper()`), generic call, và mọi `type_identifier` là tham chiếu
  kiểu. TypeScript nạp cả query của JavaScript (upstream TS chỉ có signature).
- Điểm một định nghĩa = rank(file)/số định nghĩa × (1 + tham chiếu vào từ file
  khác). Chọn theo điểm đến hết ngân sách; hiển thị theo thứ tự đường dẫn.
