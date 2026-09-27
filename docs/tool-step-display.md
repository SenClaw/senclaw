# Tool calls read as steps

**Trạng thái:** đã cài đặt 2026-09-12 (web, desktop, mobile).

## Trước

Một loạt tool call liên tiếp thu thành một dòng: `Ran a command ×3`. Mở ra thì
mỗi hàng là tên tool cộng đối số. Ba lệnh khác nhau hoàn toàn trông giống nhau,
nên dòng đó không nói được agent vừa làm gì.

## Sau

Mỗi tool call là **một bước có tiêu đề nói nó để làm gì**:

```
✓ 4 bước · Survey existing skills and the wiki capability        ›
  ① Survey existing skills and the wiki capability               ›
  ② Checked whether the code and wiki skills already cover docs  ›
  ③ Đọc tệp · src/zen_core/workbench.rs                          ›
  ④ Compile the daemon                                    error  ›
```

**Mỗi bước đúng một dòng.** Đây không phải chuyện thẩm mỹ: `summary` của một
tool MCP là **JSON thô của kết quả**, nên in nó dưới tiêu đề biến mỗi bước
trình duyệt thành ba dòng JSON đã escape — đúng kiểu hiển thị mà thay đổi này
định bỏ. Lệnh, đầu ra, diff, payload MCP đều nằm sau dấu mũi tên.

Bấm vào một bước mở đúng phần chi tiết như cũ — lệnh, đầu ra, diff, danh sách
khớp. Phần detail không đổi; chỉ đường dẫn đến nó đổi.

## Dòng của một bước lấy ở đâu

**Từ chính lời mô hình viết, không phải suy ra.** `Bash` và `Task` đều **bắt
buộc** tham số `description` ("Clear, concise description of what this command
does in 5-10 words"), và tham số đó trước giờ chỉ dùng cho thẻ xin quyền rồi bỏ.
Nay [`step_description`](../src/zen_core/run_tools.rs) đọc nó ra ở chỗ phát sự
kiện và nó đi kèm suốt: `ToolExecutionCompleteData` → `ToolExecutionEvent` →
khung WS `tool:execution` → cột `tool_executions.description` → cả ba client.

Quy tắc gắn vào **tên tham số, không phải bảng tra theo tool**: tool nào sau này
có thêm `description` thì tự động có tiêu đề, không phải sửa gì ở đây.

Tool không có `description` (`Read`, `Grep`, `Edit`…) trả về **chuỗi rỗng**, và
client ghép một dòng từ động từ của tool cộng thứ nó tác động lên:
`Đọc tệp · src/zen_core/workbench.rs`. Nguồn của vế sau là `title` (lệnh,
đường dẫn, truy vấn) — **không bao giờ** là `summary`. Đó là điểm quan trọng
nhất của thiết kế: một câu tóm tắt đúng còn hơn một câu tự bịa. Hàng lưu trước
khi có cột này cũng rơi vào nhánh đó, nên lịch sử cũ vẫn đọc được.

Ba trường hợp riêng, đều là thứ đọc lên thấy ngay là thừa:

- Tool MCP lạ không có động từ nào có nghĩa ("Used a tool"), nên lấy thẳng
  `title` của nó.
- `title` trùng tên tool (`ToolSearch`) thì bỏ, vì nó chỉ lặp lại động từ bằng
  một cách viết khác.
- Dòng tiêu đề gộp chỉ xem trước câu **do mô hình viết**. "10 bước · Discovered
  a tool" không nói thêm gì so với "10 bước".

## `title` phải nói được **để làm gì**, không chỉ **là tool nào**

"Browser Search" không nói gì cả. Hai chỗ trong daemon vì thế được sửa để đưa
đối số của lệnh vào tiêu đề, nên không cần thêm cột DB nào:

| Tool | Trước | Sau |
|---|---|---|
| tool MCP bất kỳ | `Browser Search` | `Browser Search: giá vàng hôm nay 12/9` |
| `browser_navigate` | `Browser Navigate` | `Browser Navigate: https://laodong.vn/…` |
| `browser_extract_text` | `Browser Extract Text` | `Browser Extract Text: Đoán tỷ số Lịch Đăng nhập…` |
| `ToolSearch` | `ToolSearch` | `senclaw-browser` (chính truy vấn) |

Hai hàm trong [`run_tools.rs`](../src/zen_core/run_tools.rs) làm việc đó:

- **`argument_hint`** đọc đối số **đầu vào**, theo một danh sách khoá xếp theo
  mức thông tin (`query` → `url` → `command` → `file_path` → … → `tab_id`).
  Chỉ nhận giá trị vô hướng: đem một object đi serialize chính là cách JSON thô
  lọt vào danh sách bước lần đầu.
- **`result_excerpt`** dùng cho những lệnh mà đầu vào không nói gì:
  `browser_extract_text` chỉ nhận `tab_id` và `selector`, nên câu trả lời cho
  "trích được gì" nằm ở **kết quả**. Nó mở vỏ `content[].text` của MCP trước
  (vỏ là JSON bọc JSON), và nếu bên trong vẫn là cấu trúc thì **trả về rỗng**
  chứ không in object ra.

Danh sách khoá ngắn có chủ ý: dài ra là bắt đầu khớp vào mấy trường sổ sách và
in ra rác.

## Những chỗ dễ làm sai

- **Lỗi một bước không nhuộm đỏ cả dòng tiêu đề.** Bốn bước mà một bước lỗi thì
  tô đỏ cả dòng đọc như cả bốn đều lỗi. Chỉ biểu tượng trạng thái đỏ, còn bước
  lỗi được đánh dấu đúng chỗ của nó bên trong. Khi bỏ màu của dòng thì phải cấp
  màu riêng cho biểu tượng lỗi, nếu không nó thừa hưởng màu xám.
- **Dòng phụ chỉ hiện khi nó thêm thông tin.** Với tool không có mô tả riêng,
  tiêu đề chính là động từ, nên in lại đối số bên dưới là đủ; ngược lại, tiêu đề
  là câu của mô hình thì đối số (lệnh, đường dẫn) mới là thứ đáng in.
- **Cột DB thêm bằng ALTER có bảo vệ.** `tool_executions` được tạo trong
  `apply_schema`, còn `run_migrations` chạy trước một số bảng khác — ALTER không
  kiểm tra bảng tồn tại đã từng làm sập 177 test.
- **Mobile không có thân chi tiết.** Relay chuyển nguyên khung nên `description`
  tới được, nhưng `content` của tool thì client mobile không dựng — nên bước ở
  mobile không mở ra được lệnh. Ghi rõ ở đây để không ai đi tìm chỗ hỏng.

## Đã kiểm chứng

| Việc | Cách |
|---|---|
| `step_description` đọc đúng, không bịa, cắt đúng biên ký tự | 3 test trong `run_tools.rs`, có case tiếng Việt nhiều byte |
| `argument_hint` đúng thứ tự ưu tiên, bỏ qua cấu trúc, không bịa | 2 test trong `run_tools.rs` |
| `result_excerpt` mở vỏ MCP thay vì in nó ra | 1 test, có case bên trong vẫn là JSON |
| Cột mới đi tròn vòng qua SQLite | `db::tool_executions` round-trip test |
| Database cũ (đã có bảng, chưa có cột) lên đời được | test dựng đúng bảng bản cũ rồi chạy `apply_schema`; **đỏ** khi gỡ ALTER |
| Giao diện bước trên desktop | 7 widget test (`desktop_app/test/tool_step_card_test.dart`) — **cả 5 đỏ** khi bỏ dây nối `description`, xanh khi nối lại |
| Giao diện bước trên web | dựng thật trong trình duyệt, sáng và tối, tiếng Anh và tiếng Việt; mở bước thấy `exit 0` và stdout |
| Biên dịch | `cargo test --lib` 2364 xanh, `cargo check --all-targets` sạch, `flutter analyze` sạch hai app, `flutter test` 260 xanh, `npx tsc` sạch, `npm run build` xong |

## Chưa kiểm chứng

- **Chưa chạy trên daemon thật.** Daemon cần build lại mới phát trường mới; bản
  app đang cài của người dùng chưa có.
- **Mobile chưa chạy** — không có tài khoản relay trong phiên này. Phần sửa là
  cùng một trường, nhưng chưa ai xem nó trên máy.
