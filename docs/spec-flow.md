# Luồng spec: thiết kế trước, code sau

**Trạng thái:** cài đặt 2026-10-27 trên nhánh `feat/spec-flow`. Thiết kế (đã qua review 4 hướng):
[plans/261027-1505-code-spec-flow/design.md](../plans/261027-1505-code-spec-flow/design.md). Nguồn ý tưởng: Elephant-Goldfish
Model của Google ([nghiên cứu](../plans/reports/research-261027-1505-egm-elephant-goldfish.md)).

## Dùng thế nào

| Lệnh | Làm gì |
|---|---|
| `/brainstorm <ý tưởng>` | Mở một spec cho thư mục (`docs/designs/<YYMMDD>-<slug>.md`). Agent đọc code liên quan, hỏi **từng câu một**, xin bạn xác nhận ý tưởng trong 2-3 câu, rồi ghi brief. |
| `/draft [đường dẫn]` | Agent viết spec đầy đủ theo template, dừng hỏi bạn sau phần thiết kế rồi mới viết `## Detailed Implementation Plan`. |
| `/review [đường dẫn]` | Bốn reviewer **không thấy cuộc trò chuyện** đọc spec: **Cảnh** (toàn cảnh: gốc vấn đề, thừa, dùng lại), **Kiểm** (code được từ đây không, khớp code thật không — cộng một danh sách kiểm bằng máy), **Hiểu** (người mới đọc có hiểu), **Phản biện** (phương án khác, điểm hỏng, rủi ro). Agent xử lý từng finding (FIX / CONTEST / ESCALATE / DECLINE) và ghi vào `## Review Log`. |
| `/approve` · `/approve force` | Duyệt bản spec hiện tại. Cần review của **đúng phiên bản này**, đủ 4 hướng, 0 Blocker; `force` bỏ qua phần review (vẫn cần đã có kế hoạch triển khai). |
| `/spec` · `/spec use <path>` · `/spec done` · `/spec skip` | Trạng thái · mở spec có sẵn · đóng · bỏ qua cho một sửa nhỏ (chỉ khi `required`). |

Một brief (chưa có kế hoạch triển khai) cũng `/review` được: bốn hướng chạy ở chế độ brainstorm (có cần làm không / migration /
dễ dùng không / chi phí và điểm hỏng).

## Điều gì được bảo đảm

- **Thiết kế trước khi code.** Thư mục có spec đang mở mà chưa duyệt → mọi `Edit`/`Write`/`NotebookEdit` vào file ngoài
  `docs/designs/*.md` và mọi `Bash` không chỉ-đọc trong thư mục đó bị từ chối, kể cả khi `codeAutonomy = full`. Lời từ chối nói
  lệnh nào mở khoá.
- **Mọi chat xem spec trước khi làm tiếp.** Mỗi lượt của mọi chat làm việc trong thư mục đó có khối `<spec>` (đường dẫn, giai
  đoạn, mục tiêu, việc còn lại, luật). Sau khi duyệt, lần sửa đầu tiên của một chat chưa đọc bản spec hiện tại bị từ chối **kèm toàn
  văn spec trong lời từ chối**; sửa lại lần nữa thì được (model nhỏ không tự `Read` khi được bảo). Spec dài hơn 24.000 ký tự thì phải
  `Read` trọn file. Xoá lịch sử chat (`reset`, "clear context", compaction) thì phải xem lại.
- **Câu trả lời không nói sai việc đã làm.** Khi cổng đã chặn mà file chưa đổi, câu trả lời có dòng "⚠️ Luồng spec đã chặn…". Sau
  `/review`, nếu agent không sửa spec mà vẫn còn Blocker thì bị nhắc một lần, và câu trả lời có dòng 📋 cho biết thiết kế đổi hay không.
- **Chỉ người duyệt.** `/approve` chỉ được nhận từ tin nhắn người gửi (web/desktop, kênh, REST chat của phiên Code); lịch hẹn,
  DAG, kanban không duyệt được; tool của agent không ghi được kho duyệt.
- **Sửa thiết kế sau khi duyệt thì khoá lại.** Tick `- [x]`, đổi `**Status:**`/`**Date:**`, ghi thêm vào `## Review Log` cuối
  file, đổi CRLF/NFC không tính là sửa.

## Setting

Settings → Agent behavior → **Spec flow** (`specFlow` trong `config.json`, `/api/agent-behavior`):

| Giá trị | Nghĩa |
|---|---|
| `required` (mặc định) | Phiên Code và chat dự án phải có spec được duyệt trước lần sửa đầu tiên; `/spec skip` cho sửa nhỏ. Chat thường trong git repo theo `on`. |
| `on` | Chỉ thư mục đã có spec đang mở mới bị giữ. |
| `off` | Tắt khối `<spec>`, cổng và lệnh. File trong repo không bị đụng. |

## Lưu ở đâu

- Spec: `<thư mục>/docs/designs/*.md` — agent ghi, bạn đọc và commit cùng code.
- Trạng thái (spec nào đang mở, review cuối, duyệt và hash): `~/.senclaw/specs/<16 hex của đường dẫn>/state.json`; báo cáo
  review đầy đủ (kèm nguyên văn từng hướng): `…/reviews/<slug>-<thời gian>.md`. Chỉ daemon ghi.

## Giới hạn đã chấp nhận

- `Bash` ghi đè `state.json` khi `codeAutonomy = full` (ở `ask`/`auto` thì hỏi).
- Không chặn: tool MCP ghi file (`doc_write`, wiki), lịch `script`, subagent trong git worktree riêng (sửa nằm trên nhánh, người merge).
- Review trên model local chậm: 4 lệnh gọi tuần tự, tối đa 600 s mỗi hướng; nút Stop không huỷ một review đang chạy.
- `Bash` được xét theo thư mục nó chạy: chat có thư mục là thư mục **cha** của một thư mục có spec có thể `cd` vào đó.
- Provider đi qua `dispatch_completion` (vd. Antigravity) không báo lý do dừng, nên một câu trả lời bị cắt ở giới hạn token không bị nhận ra.
- Cổng chỉ ép "đã duyệt" và "đã đọc"; "làm đúng spec" là lời dặn trong khối `<spec>`.
