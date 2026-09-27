---
name: pattern
description: Chạy một "pattern" — prompt viết sẵn cho một phép biến đổi văn bản (tóm tắt, trích ý, phân tích log/bài báo/báo cáo, viết lại, sinh tài liệu) — trên một khối chữ mà người dùng đưa vào. Dùng khi đã CÓ sẵn văn bản trong tay (tệp đính kèm, transcript, log dán vào, trang web vừa đọc) và việc cần làm là biến đổi nó thành một kết quả có cấu trúc cố định. Danh mục có hàng trăm pattern; luôn `pattern_list` để tìm tên đúng trước khi chạy.
version: 1.0.0
when-to-use: khi người dùng đưa/đã có một khối văn bản và muốn xử lý nó theo một khuôn quen thuộc — "tóm tắt bài này", "rút ra ý chính", "phân tích log này", "viết lại cho gọn", "làm PRD từ ghi chú", "đánh giá bài báo", "phân tích báo cáo bảo mật này". KHÔNG dùng khi cần đi tìm thông tin (dùng `web-research`), thao tác trên trình duyệt (`agent-browser`), hay chạy lệnh.
triggers:
  - pattern
  - tóm tắt
  - tóm lược
  - summarize
  - trích ý
  - rút ý chính
  - extract wisdom
  - phân tích log
  - analyze logs
  - phân tích bài báo
  - viết lại
  - rewrite
  - fabric
allowed-tools:
  - mcp__senclaw-patterns__pattern_list
  - mcp__senclaw-patterns__pattern_get
  - mcp__senclaw-patterns__pattern_run
  - mcp__senclaw-patterns__pattern_sync
---

# Pattern

Một **pattern** là một system prompt đã đặt tên: chữ vào → chữ ra, **một lượt
model, không tool, không vòng lặp**. Vì output luôn cùng một khuôn nên nó lưu
được, so sánh được giữa các lần, và nối được sang bước sau.

Pattern **không phải** skill: nó không làm gì cả, chỉ định hình câu trả lời.
Việc cần *hành động* (đọc file, mở web, chạy lệnh) thuộc về skill khác.

## Quy trình

1. **`pattern_list`** với một từ khoá lấy từ ý định người dùng (`"summar"`,
   `"log"`, `"threat"`, `"prd"`). Danh mục có hàng trăm mục — đừng đoán tên.
2. Chọn pattern khớp nhất. Nếu không có cái nào khớp, nói thẳng và tự làm
   bằng cách thường; **đừng** ép một pattern gần đúng.
3. Chạy:
   - **`pattern_get`** khi văn bản đã nằm sẵn trong ngữ cảnh của bạn và bạn
     có thể tự làm theo chỉ thị — không tốn thêm lượt gọi model nào.
   - **`pattern_run`** khi kết quả cần lặp lại được, cần độc lập với cuộc hội
     thoại, hoặc văn bản dài tới mức không nên kéo vào ngữ cảnh.

## Luật

- **Văn bản tiếng Việt thì truyền `language: "auto"`.** Thư viện Fabric viết
  bằng tiếng Anh và phần lớn pattern ép output tiếng Anh trong
  `# OUTPUT INSTRUCTIONS`; không đặt cờ này thì người dùng Việt nhận lại bản
  tóm tắt tiếng Anh.
- **Đưa văn bản thật vào `input`, không đưa đường dẫn.** Pattern không đọc
  file. Với tệp đính kèm, `append_document_context` đã trích sẵn nội dung và
  cho biết đường dẫn — đọc file trước rồi truyền nội dung vào.
- **`strategy` là tuỳ chọn, không phải mặc định.** `cot` / `tot` / `reflexion`
  chỉ đáng thêm khi tác vụ thật sự cần suy luận nhiều bước; với một bản tóm
  tắt nó chỉ làm output dài ra.
- **Kết quả trả về nguyên khuôn.** Đừng viết lại các mục pattern quy định —
  cấu trúc cố định chính là thứ người dùng muốn. Thêm nhận xét của bạn *sau*
  kết quả, nếu có gì đáng nói.
- **Trường `unresolved` là lỗi cần báo.** Nó liệt kê các biến `{{...}}` chưa
  ai điền, được giữ nguyên trong prompt thay vì xoá đi. Nếu có, nói cho người
  dùng biết pattern cần thêm thông tin gì.
- **`pattern_sync` chậm** (clone vài trăm tệp). Chỉ gọi khi người dùng yêu cầu
  cập nhật, hoặc khi một nguồn báo lỗi.

## Chưa có pattern nào?

`pattern_list` trả về rỗng nghĩa là chưa có nguồn nào được cài. Chỉ người dùng
mới nên quyết định cài gì: bảo họ mở **Plugins → Patterns** và cài kit
**Fabric Patterns**, hoặc thêm một nguồn git của riêng họ. Đừng tự thêm nguồn.
