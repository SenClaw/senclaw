# Đồng bộ lịch và ghi chú từ bên ngoài vào Space

Ba nguồn đổ vào đúng hai bảng mà giao diện Space đọc (`space_events`,
`space_notes`) — không có bản sao lịch nào ở chỗ khác.

| Nguồn | Giao thức | Chiều | Điều kiện |
|---|---|---|---|
| Google Calendar | REST v3 + OAuth | đọc, incremental | access token có scope calendar |
| Apple / CalDAV | CalDAV (PROPFIND + REPORT) | đọc | Apple ID + **app-specific password** |
| Apple Notes | `osascript` → Notes.app | đọc | **chỉ macOS** + quyền Automation |

## Trước đây chúng nói dối

Cả ba tool nhận credential, trả `"Token received and stored"` rồi **không làm
gì**. Người dùng đọc câu đó là "đã đồng bộ xong". Giờ mỗi tool hoặc đồng bộ
thật, hoặc nói thẳng vì sao không được — không có trạng thái thứ ba.

## Hình dạng trả về

Cả ba trả cùng một thứ, nên agent không cần biết backend nào chạy:

```json
{ "synced": 12, "created": 3, "updated": 9, "errors": [], "needsReauth": false }
```

- **`needsReauth: true` là lệnh dừng**, không phải gợi ý thử lại. Credential
  đã chết; gọi lại chỉ đốt quota. Với Apple Notes nó còn có nghĩa "macOS chưa
  cấp quyền Automation".
- **`errors` không rỗng vẫn có thể là một lần đồng bộ thành công.** Một event
  hỏng không được làm mất bốn mươi event còn lại. Lỗi trùng nhau gộp thành
  một, tối đa 20 dòng.
- `GET /api/space/sync/status` trả lần chạy cuối của từng nguồn. `last: null`
  nghĩa là **chưa từng chạy** — client không được vẽ nó thành "đã đồng bộ".

## Những bẫy mà code này tồn tại để tránh

- **Chạy lần hai phải cập nhật, không được chèn thêm.** Id của remote lưu ở
  `link` dạng `<nguồn>:<id>`; thiếu nó thì mỗi lần chạy nhân đôi lịch trong
  khi báo cáo vẫn nói thành công. Hai nguồn có cùng UID vẫn là hai hàng khác
  nhau vì tiền tố nguồn nằm trong khoá.
- **Event bị huỷ ở xa phải biến mất ở đây**, nên `STATUS:CANCELLED` (hoặc
  `status: "cancelled"` của Google) là soft-delete chứ không phải bỏ qua.
  Google báo xoá bằng một bản ghi **chỉ có id và status** — đòi `start` trước
  sẽ vứt mất nó.
- **`syncToken` chỉ được ghi khi cả lượt chạy xong.** Ghi giữa chừng sẽ nhảy
  qua những event chưa kịp viết. HTTP 410 nghĩa là token quá cũ: xoá con trỏ
  để lần sau tải lại toàn bộ, đó là cách xử lý đúng chứ không phải lỗi.
- **Recurrence giữ nguyên chuỗi `RRULE` gốc, không bao giờ khai triển.** Khai
  triển là bịa ra những lần lặp mà server không gửi. Vì vậy lịch nhập vào là
  **chỉ đọc**: sửa ở đây không đi ngược về server.
- **iCal phải unfold trước khi tách dòng** (RFC 5545 §3.1) — cắt theo `\n`
  làm đứt đôi tiêu đề dài, trông y như dữ liệu hỏng từ server.
- **`VALARM` nằm trong `VEVENT` và có `DTSTART`/`SUMMARY` riêng.** Quét
  phẳng theo thuộc tính sẽ lấy giờ báo thức làm giờ sự kiện.
- **`href` đầu tiên trong phản hồi CalDAV là chính URL vừa hỏi.** Câu trả lời
  nằm *bên trong* `<prop>`; lấy nhầm cái đầu là đi vòng tròn vô hạn. iCloud
  còn trả URL tuyệt đối trỏ sang shard khác, nên phải nối đúng gốc.
- **Bộ sưu tập `calendar-home` và `schedule-inbox` không phải lịch.** Query
  chúng trả về rỗng và tốn một vòng.
- **iCloud từ chối mật khẩu tài khoản** khi bật 2FA (luôn luôn). Phải là
  app-specific password, và phải kèm `username` — mật khẩu không tự nói nó
  thuộc về ai.
- **iCloud Notes không đi qua IMAP nữa.** Stub cũ hứa như vậy là sai. Không có
  đường cross-platform, nên trên Linux/Windows tool **báo lý do** thay vì trả
  về 0 note — một thành công rỗng không phân biệt được với "tài khoản trống".

## Kiểm chứng

Đã có (chạy trong `cargo test`): 33 test cho parser iCal, ánh xạ Google, trích
XML CalDAV, và ghi vào DB — gồm cả "lần hai cập nhật chứ không nhân đôi",
"event huỷ thành soft-delete", "alarm không cướp giờ sự kiện", "href trong
prop chứ không phải href đầu tiên".

**Chưa có**: chạy thật với một tài khoản Google và một tài khoản iCloud. Khi
làm, ghi kết quả vào bảng dưới kèm ngày — đừng đánh dấu xong vì code biên dịch.

| Nguồn | Tài khoản thật | Ngày | Kết quả |
|---|---|---|---|
| Google Calendar | chưa | — | — |
| CalDAV (iCloud) | chưa | — | — |
| Apple Notes | chưa | — | — |

## Giao diện

Thanh công cụ lịch (web) hiện dòng "Synced &lt;thời điểm&gt;" lấy từ
`/api/space/sync/status`, hover ra chi tiết từng nguồn.

**Cố ý chưa làm:** form nhập credential trong UI. Token Google và app-specific
password hiện đi qua hội thoại (agent gọi tool), và dựng một form nhận mật
khẩu là quyết định về luồng bảo mật cần người dùng chốt, không phải việc suy ra.
