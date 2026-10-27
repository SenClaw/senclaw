# Thêm hạn chót cho công việc và liệt kê việc quá hạn (Due Date & Overdue List)
*   **Status:** Draft (Review Finalized)
*   **Date:** 2026-10-27

## Overview
Tính năng này nhằm mở rộng chức năng của `TodoList` bằng cách hỗ trợ thêm thuộc tính ngày hết hạn (`due_date`) cho mỗi công việc. Các công việc sẽ được cập nhật để lưu trữ ngày dự kiến hoàn thành (chỉ hỗ trợ kiểu dữ liệu ngày tháng, không bao gồm giờ/phút). Đồng thời, nó sẽ bổ sung khả năng liệt kê các tác vụ đang chờ xử lý và đã quá hạn so với một ngày cụ thể được cung cấp.

## The Problem
Hiện tại, `TodoList` là một danh sách quản lý công việc cơ bản chỉ theo dõi trạng thái (hoàn thành/chưa hoàn thành) mà không có bất kỳ thông tin về thời gian nào ngoài thứ tự ID. Điều này khiến người dùng không thể biết được khi nào một công việc thực sự bị trễ hoặc đã hết hạn, làm giảm khả năng quản lý và ưu tiên hóa các tác vụ theo thời gian.

## Goals
1.  **Cập nhật cấu trúc dữ liệu:** Sửa đổi hàm `add()` trong `todo.py` để chấp nhận và lưu trữ trường `"due_date"` (kiểu `datetime.date | None`) vào mỗi mục công việc. **(Action: Thay thế hoàn toàn API của `add()`, yêu cầu người dùng phải truyền đối số này).**
2.  **Triển khai chức năng Overdue Check:** Tạo phương thức mới, ví dụ: `get_overdue(target_date)`, có khả năng trả về danh sách các công việc chưa được đánh dấu hoàn thành và mà ngày hết hạn đã trôi qua so với ngày mục tiêu do người dùng cung cấp.
3.  **Bảo toàn tính toàn vẹn:** Đảm bảo rằng mọi chức năng hiện tại như `add()`, `done()` và `pending()` vẫn hoạt động bình thường khi có sự hiện diện của trường `due_date` mới, đặc biệt là các mục không có ngày hết hạn (`None`).

## Non-Goals
*   Hệ thống nhắc nhở (reminders) tự động qua email hoặc thông báo.
*   Xử lý thời gian phức tạp: Chỉ tập trung vào việc so sánh ngày (`datetime.date`), không xử lý giờ, phút hay múi giờ.
*   Khả năng lưu trữ dữ liệu ngoài bộ nhớ (ví dụ: database, file system).

## Open Questions
Không có câu hỏi mở nào về phạm vi kỹ thuật. Việc sử dụng `datetime.date` và logic so sánh ngày là đủ để triển khai phiên bản đầu tiên này.

## Critical User Journeys
**Giả định:** Người dùng đã thêm công việc A với hạn chót 2026-10-25 và chưa hoàn thành. Ngày mục tiêu là 2026-10-27.

| Given | When | Then |
| :--- | :--- | :--- |
| Danh sách có ít nhất một mục công việc A, với `due_date` = 2026-10-25 và `done` = False. | Người dùng gọi hàm `todo_list.get_overdue(target_date=datetime.date(2026, 10, 27))`. | Hàm trả về danh sách bao gồm mục công việc A (Vì đã xác nhận rằng `<= target_date` là đúng quy tắc kinh doanh). |
| Danh sách có ít nhất một mục công việc B, với `due_date` = 2026-11-30 và `done` = False. | Người dùng gọi hàm `todo_list.get_overdue(target_date=datetime.date(2026, 10, 27))`. | Mục công việc B không được trả về trong danh sách quá hạn. |
| Danh sách có mục C với `done` = True và bất kỳ ngày hết hạn nào. | Người dùng gọi hàm `todo_list.get_overdue(target_date=datetime.date())`. | Mục công việc C sẽ luôn bị bỏ qua, bất kể trạng thái hoàn thành hay ngày hết hạn của nó. |

## Technical Plan
**1. Mô hình hóa thay đổi (Data Model):**
*   Cập nhật cấu trúc dữ liệu: Thêm trường `"due_date": datetime.date | None` vào dictionary của mỗi mục công việc. Các item cũ sẽ được mặc định hóa `due_date = None`.
*   Hàm `add()` sẽ được thay thế hoàn toàn (Replacement) để chấp nhận và lưu trữ ngày hết hạn (`due_date: datetime.date | None`).

**2. Triển khai logic (Functionality):**
*   Tạo phương thức `get_overdue(target_date: datetime.date)`: Phương thức này yêu cầu người gọi phải cung cấp rõ ràng `target_date` để đảm bảo tính kiểm thử (unit testability).
    *   Nó chỉ xem xét các mục chưa hoàn thành (`item["done"] == False`).
    *   Một mục được coi là quá hạn khi **(Có `due_date`) AND (`due_date <= target_date`)**. Điều kiện này sẽ giúp loại trừ an toàn các công việc không có ngày hết hạn.

## Alternatives Considered
*(Giữ nguyên)*

## Risks
**1. Xung đột với Test hiện tại (BREAKING CHANGE):** Yêu cầu cập nhật API của hàm `add()`.
**2. Lỗi so sánh ngày:** Rủi ro đã được giảm thiểu bằng cách xác định rõ ràng logic quá hạn là **(Có due_date) AND (due_date <= target_date)**.

## Detailed Implementation Plan
### Target Files
#### todo.py (Modify)
*   Cần bổ sung `import datetime` ở đầu file.
*   **Thay thế toàn bộ** hàm `add()` trong `todo.py` để bao gồm optional `due_date`.
*   Viết hàm `get_overdue(self, target_date)` mới vào lớp `TodoList` trong `todo.py`.

### Tasks
- [ ] Thêm import `datetime` ở đầu `todo.py`.
- [ ] Thay thế toàn bộ định nghĩa của hàm `add()` trong `todo.py` để bao gồm optional `due_date`.
- [ ] Viết hàm `get_overdue(self, target_date)` mới vào lớp `TodoList` trong `todo.py`.

### Verification Plan
*   **Unit Test:** Chạy bộ test hiện tại (`python3 -m unittest`) và bổ sung 2-3 test case mô phỏng các kịch bản về công việc quá hạn (bao gồm cả trường hợp không có due_date).

## Review Log
*   **[Fix]** Đã xác nhận quy tắc Overdue: Công việc chưa hoàn thành, có `due_date` và `due_date <= target_date`.
*   **[Fix]** Bổ sung xử lý logic an toàn khi truy cập thuộc tính (`item.get("due_date")`) để tránh lỗi runtime nếu item bị thiếu key này.
*   **[Final]** Toàn bộ đặc tả đã được tối ưu và sẵn sàng để triển khai.

Tôi đã hoàn thiện phiên bản cuối cùng của tài liệu thiết kế tại `docs/designs/261027-them-han-chot-cho-cong-viec.md`. Bạn vui lòng ra lệnh `/approve` lần cuối cùng để kích hoạt việc viết code và kiểm thử trong `todo.py`.