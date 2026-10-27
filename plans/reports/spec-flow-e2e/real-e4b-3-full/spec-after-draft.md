# Thêm hạn chót cho công việc và liệt kê việc quá hạn (Due Date & Overdue List)
*   **Status:** Draft
*   **Date:** 2026-10-27

## Overview
Tính năng này nhằm mở rộng chức năng của `TodoList` bằng cách hỗ trợ thêm thuộc tính ngày hết hạn (`due_date`) cho mỗi công việc. Đồng thời, nó sẽ bổ sung khả năng liệt kê các tác vụ đang chờ xử lý và đã quá hạn so với một ngày cụ thể được cung cấp.

## The Problem
Hiện tại, `TodoList` là một danh sách quản lý công việc cơ bản chỉ theo dõi trạng thái (hoàn thành/chưa hoàn thành) mà không có bất kỳ thông tin về thời gian nào ngoài thứ tự ID. Điều này khiến người dùng không thể biết được khi nào một công việc thực sự bị trễ hoặc đã hết hạn, làm giảm khả năng quản lý và ưu tiên hóa các tác vụ theo thời gian.

## Goals
1.  **Cập nhật cấu trúc dữ liệu:** Sửa đổi hàm `add()` trong `todo.py` để chấp nhận và lưu trữ trường `"due_date"` (kiểu `datetime.date | None`) vào mỗi mục công việc.
2.  **Triển khai chức năng Overdue Check:** Tạo phương thức mới, ví dụ: `get_overdue(target_date)`, có khả năng trả về danh sách các công việc chưa được đánh dấu hoàn thành và mà ngày hết hạn đã trôi qua so với một ngày xác định.
3.  **Bảo toàn tính toàn vẹn:** Đảm bảo rằng mọi chức năng hiện tại như `add()`, `done()` và `pending()` vẫn hoạt động bình thường khi có sự hiện diện của trường `due_date` mới.

## Non-Goals
*   Hệ thống nhắc nhở (reminders) tự động qua email hoặc thông báo.
*   Xử lý thời gian phức tạp: Chỉ tập trung vào việc so sánh ngày (`datetime.date`), không xử lý giờ, phút hay múi giờ.
*   Khả năng lưu trữ dữ liệu ngoài bộ nhớ (ví dụ: database, file system).

## Open Questions
Không có câu hỏi mở về phạm vi kỹ thuật. Việc sử dụng `datetime.date` và logic so sánh ngày là đủ để triển khai phiên bản ban đầu này.

## Critical User Journeys
**Giả định:** Người dùng đã thêm công việc A với hạn chót 2026-10-25 và chưa hoàn thành. Hôm nay là 2026-10-27.

| Given | When | Then |
| :--- | :--- | :--- |
| Danh sách có ít nhất một mục công việc A, với `due_date` = 2026-10-25 và `done` = False. | Người dùng gọi hàm `todo_list.get_overdue(target_date=datetime.date(2026, 10, 27))`. | Hàm trả về danh sách bao gồm mục công việc A, cùng với thông báo rõ ràng rằng nó đã quá hạn. |
| Danh sách có ít nhất một mục công việc B, với `due_date` = 2026-11-30 và `done` = False. | Người dùng gọi hàm `todo_list.get_overdue(target_date=datetime.date(2026, 10, 27))`. | Mục công việc B không được trả về trong danh sách quá hạn. |
| Danh sách có mục C với `done` = True và bất kỳ ngày hết hạn nào. | Người dùng gọi hàm `todo_list.get_overdue(target_date=datetime.date())`. | Mục công việc C không bao giờ bị tính là quá hạn, bất kể ngày hết hạn của nó là khi nào. |

## Technical Plan
**1. Mô hình hóa thay đổi (Data Model):**
*   Trong `todo.py`, cập nhật cấu trúc dữ liệu để thêm trường `"due_date": datetime.date | None`.
*   Cập nhật hàm `add()` để chấp nhận tham số ngày hết hạn (`due_date: datetime.date | None`) khi thêm một mục mới vào danh sách.

**2. Triển khai logic (Functionality):**
*   Tạo phương thức `get_overdue(target_date: datetime.date = None)`:
    *   Nếu `target_date` là `None`, sử dụng ngày hiện tại (`datetime.date.today()`).
    *   Lọc các mục thỏa mãn điều kiện: `item["done"] == False` VÀ (nếu `item["due_date"]` tồn tại, thì `item["due_date"] <= target_date`).

## Alternatives Considered
**1. Sử dụng trường "priority" thay vì "due_date":**
    *   *Ưu điểm:* Linh hoạt hơn về mặt logic nếu muốn thêm các mức độ ưu tiên khác ngoài thời gian (ví dụ: P1, P2).
    *   *Nhược điểm:* Không giải quyết được vấn đề cốt lõi của việc tính toán quá hạn theo lịch trình.

**2. Lưu trữ ngày dưới dạng chuỗi ISO 8601:**
    *   *Ưu điểm:* Giúp tương thích tốt hơn khi dữ liệu được nhập từ nguồn bên ngoài (ví dụ: API).
    *   *Nhược điểm:* Bắt buộc phải chuyển đổi ngược lại thành `datetime.date` object trong mã, làm tăng độ phức tạp và dễ xảy ra lỗi type casting.

## Risks
**1. Xung đột với Test hiện tại:** Các test case hiện tại chỉ kiểm tra các tính năng cơ bản (add, done, pending) có thể bị hỏng nếu không được cập nhật để xử lý trường `due_date` mới. **(Giảm thiểu: Bổ sung tests cho việc khởi tạo và thao tác với `None`)**
**2. Lỗi so sánh ngày:** Nếu logic so sánh ngày tháng không chính xác (ví dụ, quên kiểm tra null/none), chương trình sẽ trả về kết quả sai lệch nghiêm trọng.

## Detailed Implementation Plan
### Target Files
#### todo.py (Modify)
*   Cần bổ sung `import datetime` ở đầu file.
*   Sửa đổi hàm `add(self, title)` để chấp nhận `due_date`.
*   Thêm phương thức `get_overdue(self, target_date=None)` vào lớp `TodoList`.

### Tasks
- [ ] Thêm import `datetime` ở đầu `todo.py`.
- [ ] Sửa đổi signature và logic của hàm `add()` trong `todo.py` để chấp nhận optional `due_date`.
- [ ] Viết hàm `get_overdue(self, target_date)` mới vào lớp `TodoList` trong `todo.py`.

### Verification Plan
*   **Unit Test:** Chạy bộ test hiện tại (`python3 -m unittest`) và thêm 2-3 test case mô phỏng các kịch bản về công việc quá hạn (ví dụ: một job A quá hạn, một job B chưa đến hạn).

## Review Log
(Chờ phản hồi từ người dùng.)