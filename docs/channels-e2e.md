# Kênh nhắn tin: trạng thái media, đã kiểm chứng đến đâu

Bảng này ghi **đúng những gì đã chạy thật** cho từng kênh. Một dòng "code xong,
chưa verify" là một dòng trung thực, không phải một dòng thiếu sót giấu đi: nó
nói rằng đường đi đã viết và biên dịch, nhưng chưa có tài khoản thật gửi một
bức ảnh qua nó.

## Trạng thái (2026-09-12)

| Kênh | Nhận text | Nhận ảnh/tệp | Gửi text | Gửi tệp | Kiểm chứng thật |
|---|---|---|---|---|---|
| Telegram | ✅ | ✅ ảnh + ảnh-gửi-dạng-tệp | ✅ | ✅ | ✅ đã chạy với bot thật |
| Feishu/Lark | ✅ | ✅ image / file / audio / media + ảnh trong `post` | ✅ | ✅ | ⏳ code xong, chưa có app thật |
| WeChat (iLink) | ✅ | ✅ image / file / video (giải mã AES-128-ECB) | ✅ | ✅ ảnh / video / tệp | ⏳ code xong, chưa có tài khoản thật |
| Mobile (`channel_app`) | ✅ | ✅ ảnh đi kèm trong thân tin nhắn | ✅ | ✅ frame `FILE_SEND` (≤ 8 MiB) | ⏳ code xong, chưa chạy trên máy thật |

Telegram là đường tham chiếu: [`src/channels/telegram.rs`](../src/channels/telegram.rs)
`download_media`. Mọi kênh khác đi cùng một kiểu dữ liệu
[`types::MessageAttachment`](../src/types.rs) nên vision/OCR và trích xuất tài
liệu hoạt động giống hệt nhau — xem mục *Chat attachments* trong
[CLAUDE.md](../CLAUDE.md).

## Cách kiểm chứng khi có tài khoản thật

Mỗi kênh cần đúng hai lượt, và phải xem log daemon chứ không chỉ xem câu trả
lời — một câu trả lời "hợp lý" cho ảnh có thể là model đoán.

1. **Ảnh vào**: gửi cho bot một bức ảnh có chữ (ví dụ ảnh chụp một hoá đơn), hỏi
   "trong ảnh viết gì". Log phải có `downloaded N bytes of image/... media`.
   Với model không vision, câu trả lời phải nói rõ là bản chép từ OCR.
2. **Tệp vào**: gửi một `.pdf` hoặc `.txt`, hỏi nội dung. Agent phải đọc được,
   hoặc nói rõ định dạng không đọc được **và** nêu đường dẫn đã lưu.
3. **Tệp ra**: bảo agent gửi lại một tệp. Trên WeChat, ảnh phải hiện ra là ảnh
   (item type 2) chứ không phải tệp đính kèm.

Điền kết quả vào bảng trên kèm ngày, đừng đánh dấu ✅ dựa trên việc code biên dịch.

## Những bẫy đã biết

- **Khoá AES sai trên WeChat không báo lỗi** — nó cho ra rác. Ảnh vì thế chỉ
  được đính kèm khi byte đầu khớp chữ ký PNG/JPEG/GIF/WebP; không khớp thì bỏ
  qua và ghi log, phần chữ vẫn tới agent. Khoá có hai cách viết (base64 của 16
  byte thô, hoặc base64 của chuỗi hex 32 ký tự) và ảnh còn có `aeskey` dạng hex
  riêng — hex thắng khi có cả hai.
- **Feishu thiếu scope `im:resource` trả về 403 kèm thân JSON**, không phải lỗi
  HTTP kiểu khác. Nếu lưu thẳng byte nhận được, agent sẽ nhận một trang lỗi đội
  lốt bức ảnh; daemon kiểm `content-type: application/json` trước.
- **Tin nhắn media không có chữ** sẽ tới agent như một lượt rỗng, đọc ra thành
  "người dùng không nói gì". Cả ba kênh thay bằng nhãn `[Ảnh đính kèm]`, giống
  Telegram.
- **`content` của Feishu là JSON**, nên một tin ảnh mà rơi vào nhánh text sẽ
  đưa `{"image_key":…}` vào miệng người dùng. `parse_text_content` trả nhãn
  riêng cho image/file/audio/media/sticker.
- **Relay của mobile chở tệp trong một frame** nên có trần 8 MiB
  (`MAX_RELAY_FILE_BYTES`); quá cỡ thì `send_file` trả lỗi nêu rõ giới hạn,
  không cắt bớt rồi gửi.
- **iLink không có nút bấm.** Menu đánh số là cách duy nhất đưa lựa chọn, và số
  người dùng gõ được chuyển thành nhãn của lựa chọn đó như thể họ tự gõ.
