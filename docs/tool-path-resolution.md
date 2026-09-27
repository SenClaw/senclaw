# Đường dẫn model gửi được phân giải ở đâu

> **Trạng thái: ĐÃ VÁ** · 2026-09-22 · phát hiện bởi
> [sổ sự cố](failure-ledger.md) ngay lượt chạy thật đầu tiên.
> Code: [`util::paths::resolve_in_workspace`](../src/util/paths.rs),
> seam ở [`run_tools::resolve_path_inputs`](../src/zen_core/run_tools.rs),
> khai báo qua `Tool::path_fields` ([`zen_core/mod.rs`](../src/zen_core/mod.rs)).

## Cái đã hỏng

`Read`/`Edit`/`Write`/`NotebookEdit` đưa thẳng chuỗi `file_path` cho
`std::fs`, nên một đường dẫn **tương đối** được phân giải theo **cwd của tiến
trình daemon**. Trên bản desktop cwd đó là `Contents/Resources` của app bundle
— `daemon_supervisor.dart` khởi động daemon tại thư mục chứa binary. Kiểm tra
trên tiến trình đang chạy:

```
$ lsof -p <pid daemon> | awk '$4=="cwd"{print $NF}'
Desktop.app/Contents/Resources
```

Ba hệ quả, xếp theo mức tệ:

1. **`Write` ghi vào trong app bundle đã ký.** Thành công, im lặng, sai chỗ —
   và mất sạch ở lần update kế tiếp. `create_dir_all(parent)` còn tạo cả cây
   thư mục ở đó.
2. **`Read` có thể *thành công* trên file khác.** `Cargo.toml`, `senclaw`,
   `mlx.metallib` đều tồn tại trong cwd đó; agent nhận nội dung hợp lý của một
   project hoàn toàn khác và không có lỗi nào để lần ra.
3. **`Read`/`Edit` báo "File not found" cho file đang nằm ngay trong project.**
   Đây là ca sổ sự cố bắt được: `File not found: app.py` trong khi `app.py` có
   thật trong workspace.

Điều khiến nó thành chuyện thường xuyên chứ không phải hiếm: `Glob` **trả về**
đường dẫn tương đối ([glob.rs](../src/tools/glob.rs) strip_prefix theo
working_dir) và `Grep` hiển thị tương đối. Vòng `Glob` → `Read` vì thế **luôn**
hỏng, dù schema của cả bốn tool đều ghi "Absolute path".

`Glob` và `Grep` mắc cùng lỗi với tham số `path` tường minh: mặc định là
working_dir (tuyệt đối) nhưng `PathBuf::from` với giá trị model gửi. Với
`Grep`, `PathBuf::from("src").parent()` là `Some("")` → tìm ở thư mục rỗng.

## Quy tắc mới

Một hàm, một chỗ gọi:

```rust
// `~` trước, tuyệt đối giữ nguyên, tương đối join vào working dir,
// working dir rỗng thì để yên.
pub fn resolve_in_workspace(path: &str, working_dir: &str) -> PathBuf
```

Phân giải xảy ra **trong `run_tools`, trước khi bất cứ thứ gì đọc input** —
trước cả kiểm tra schema. Tool khai báo trường nào là đường dẫn:

| Tool | `path_fields()` |
|---|---|
| Read, Edit, Write | `["file_path"]` |
| NotebookEdit | `["notebook_path"]` |
| Glob, Grep | `["path"]` |

## Vì sao không phân giải trong tool

Đây là chỗ thiết kế dễ sai nhất, và cả hai nửa đều là bẫy thật:

- **`validate_input` chỉ *mượn* input** (`&serde_json::Value`) và trả
  `Result<(), String>`. Phân giải ở đó không đi đâu cả: lớp quyền và `call`
  vẫn đọc input gốc.
- **`gen_tool_permission` — hàm dựng thẻ xin quyền người dùng bấm — không nhận
  `ToolContext`.** Nếu chỉ phân giải trong `call`, người dùng duyệt một đường
  dẫn còn tool ghi một đường dẫn khác.
- Ngược lại, **chỉ sửa `call` mà không sửa `validate_input`** thì tool từ chối
  file hợp lệ trước khi `call` kịp chạy — `validate_input` chạy *trước* ở
  `run_tools`.

Vì thế: một seam ở `run_tools` cho toàn bộ đường đi (schema → validate → hook
→ quyền → call → hook LSP → checkpoint → hàng `tool_executions`), **cộng** một
lần gọi phòng thân trong từng tool. Lần trong tool không dư: nó đỡ hai ca
`run_tools` không phủ — test gọi tool trực tiếp, và một hook `PreToolUse` trả
về `updated_input` chứa đường dẫn tương đối.

Gọi hai lần chỉ an toàn nhờ **một guard**: `working_dir` *tương đối* được coi
như không có neo, đường dẫn trả về nguyên vẹn. Join vào nó thì kết quả vẫn
tương đối, và lượt thứ hai join tiếp: `"src"` + `"a.txt"` → `"src/a.txt"` →
`"src/src/a.txt"`. Đây không phải ca giả định — `workspace_switch` làm
`PathBuf::from(target)` rồi **ghi thẳng chuỗi đó vào state**, không tuyệt đối
hoá, và pool đọc lại nguyên xi.

Wrapper phải chuyển tiếp khai báo: `AliasedTool` ủy quyền mọi thứ khác, và khi
nó *không* chuyển tiếp `path_fields` thì một alias của `Edit`/`Write` đi vòng
qua seam — ghi vẫn đúng nhờ lớp thứ hai, nhưng thẻ xin quyền hiện đường dẫn
thô. Vì lớp thứ hai che mất, **thiếu khai báo không làm hỏng test nào** ngoài
`tools::path_field_tests`.

## Nó phân giải, nó KHÔNG giam

Không có chỗ nào ở đây từ chối `..`, và đó là quyết định có chủ ý:

- working_dir **mặc định là HOME của người dùng** cho chat chưa từng gọi
  `set_working_dir`. Giam theo working_dir sẽ chặn `/tmp`, `/etc` và mọi
  project bên cạnh — rộng hơn hẳn cái bug đang vá.
- `/tmp` là symlink tới `/private/tmp`, `$TMPDIR` nằm dưới `/private/var`. So
  sánh prefix sau `canonicalize` một bên là đúng công thức từ chối oan —
  `bash.rs::check_cd_safety` đã có sẵn lỗi đó.
- `Path::starts_with` **không** chuẩn hoá `..`: `/a/b/../x` vẫn
  `starts_with("/a/b")`. Một cái giam viết kiểu đó không giam gì.

## Rules for Claude

- **Đừng phân giải theo tên trường trên mọi tool.** `path` của một MCP tool có
  thể là trang wiki hay path của URL; viết lại nó thành đường dẫn hệ thống là
  làm hỏng lời gọi. Khai báo qua `path_fields` của từng tool.
- **Thông điệp lỗi phải in đường dẫn đã phân giải.** `File not found: app.py`
  che đúng cái cần biết — là mình đã tìm ở thư mục nào.
- **Đừng bắt producer trả đường dẫn tuyệt đối.** `Glob`/`Grep`/repo-map trả
  tương đối là cố ý, và test của repo-map ghim đúng dạng đó.
- **`working_dir` rỗng là ca thật**, không phải lỗi lập trình:
  `OneShotOptions::working_dir` mặc định rỗng. Phân giải phải để nguyên đường
  dẫn khi đó, đừng join vào `""`.

## Ba thứ phát hiện kèm, CHƯA vá

Ghi ra để không ai phải tìm lại:

1. **`allowed_paths` là núm chết.** Có cột DB, có field, lưu được, sửa được từ
   Web UI, truyền khắp stack — và **không có dòng code nào so đường dẫn tool
   với nó**. Người dùng đặt nó tưởng đã giới hạn agent, thực tế không giới hạn
   gì. (Cùng loại với `ADMIN_TELEGRAM_USER_ID` mà CLAUDE.md đã ghi.)
2. ~~**Alias một tool file-edit là lách được hộp thoại quyền.**~~ **ĐÃ VÁ**
   (cùng ngày). `AliasedTool::name()` trả về alias, nên `is_file_edit_tool`
   không khớp, và lời gọi rơi vào nhánh "tool khác không read-only → cho phép
   mặc định": ghi file **không hỏi gì**. Nay mọi phân loại quyền đi qua
   `Tool::permission_name()` — tên của tool **thực sự chạy** — đúng quy tắc
   `run_tools` đã dùng sẵn khi phân loại read-only.
3. **`NotebookEdit` không bao giờ nhận diagnostics LSP.** Nó phát
   `notebook_path`, còn hook chỉ tra `data["path"]` rồi `data["file_path"]`.
   Vô hại hôm nay vì `language_id` không biết `.ipynb`, nhưng tên nó vẫn nằm
   trong danh sách match — là code chết.
