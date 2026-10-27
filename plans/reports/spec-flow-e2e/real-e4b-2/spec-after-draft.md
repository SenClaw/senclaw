# Thêm tính năng hạn chót và hiển thị công việc quá hạn

*   **Status:** Approved (Được duyệt)
*   **Date:** 2026-10-27

## Overview
Việc mở rộng hệ thống quản lý danh sách việc cần làm (`TodoList`) để cho phép gán ngày hết hạn (deadline) cho mỗi công việc. Đồng thời, bổ sung chức năng để lọc và liệt kê các mục công việc đã quá ngày hết hạn (overdue).

## The Problem
Hiện tại, `TodoList` chỉ theo dõi trạng thái hoàn thành (`done: True/False`) nhưng không có cơ chế nào quản lý tính thời gian hay nhắc nhở về deadline. Điều này làm mất khả năng nhận biết và hiển thị các công việc quan trọng đã bị lỡ hẹn, gây khó khăn cho người dùng trong việc lập kế hoạch và theo dõi tiến độ dự án thực tế.

## Goals
1.  **Cập nhật cấu trúc dữ liệu:** Thêm trường `due_date` (kiểu ngày) vào mỗi item của danh sách công việc.
2.  **Thao tác với Deadline:** Cho phép người dùng thiết lập hoặc cập nhật hạn chót khi thêm/sửa công việc.
3.  **Lọc Overdue:** Bổ sung phương thức mới (ví dụ: `overdue()`) để trả về một danh sách chỉ chứa các mục thỏa mãn điều kiện: 1) chưa được đánh dấu hoàn thành, và 2) ngày hiện tại lớn hơn `due_date`.

## Non-Goals
*   Thiết lập hệ thống nhắc nhở tự động (notifications) cho người dùng.
*   Quản lý nhiều dự án/danh sách công việc độc lập; chỉ tập trung vào logic của một `TodoList` instance.
*   Cho phép sửa ngày hết hạn sau khi công việc đã được đánh dấu hoàn thành.

## Open Questions
1.  **Định dạng ngày tháng:** Sử dụng `datetime.date` (Đối tượng ngày chuẩn) để đảm bảo tính đồng nhất trong Python và so sánh thời gian chính xác.

## Critical User Journeys
### 1. Thêm công việc với deadline
*   **Given:** `TodoList` instance đang hoạt động.
*   **When:** Người dùng gọi phương thức `add()` và cung cấp giá trị cho ngày hết hạn (`due_date`).
*   **Then:** Một item mới được tạo, bao gồm trường `due_date`, và hệ thống nhận diện được thời điểm này là một công việc cần theo dõi deadline.

### 2. Lấy danh sách pending (cần làm)
*   **Given:** `TodoList` instance chứa các mục đã hoàn thành và chưa hoàn thành.
*   **When:** Người dùng gọi phương thức `pending()`.
*   **Then:** Hệ thống trả về một danh sách các item chưa hoàn thành, được sắp xếp theo ID tăng dần (và có thể lọc bỏ các công việc quá hạn nếu logic yêu cầu).

### 3. Lấy công việc quá hạn (Overdue)
*   **Given:** `TodoList` instance chứa ít nhất một mục đã vượt qua ngày hết hạn (`due_date < today`).
*   **When:** Người dùng gọi phương thức mới `overdue()`.
*   **Then:** Hệ thống trả về một danh sách các item thỏa mãn điều kiện: chưa hoàn thành và quá hạn, được sắp xếp theo ID.

## Technical Plan
Việc triển khai yêu cầu này sẽ tập trung vào việc mở rộng lớp `TodoList` trong `todo.py`, tận dụng thư viện chuẩn `datetime` của Python để xử lý ngày tháng. Các thay đổi chính bao gồm: cập nhật cấu trúc dữ liệu item, sửa phương thức `add()` để nhận và lưu trữ ngày hết hạn (sử dụng `datetime.date`), và thêm hàm `overdue()`.

## Alternatives Considered
- **A. Thêm trường `due_date` và phương thức `overdue()`:** Phương án được chọn vì tính đơn giản, trực tiếp và đủ khả năng đáp ứng yêu cầu nghiệp vụ.
- **B. Sử dụng State Machine:** Loại bỏ do quá phức tạp so với nhu cầu hiện tại.

## Risks
1.  **Time Zone Handling:** Cần chú ý đến việc sử dụng `datetime.date.today()` để đảm bảo tính chính xác múi giờ (Giả định hệ thống đang hoạt động theo múi giờ cục bộ).
2.  **Thiếu Validation Input:** Cần thêm bước validation cho ngày hết hạn trong hàm `add()`.

## Detailed Implementation Plan
### Target Files
*   `todo.py`: Modify (Cập nhật cấu trúc item, thêm `due_date`, sửa `add()`, và thêm hàm `overdue()`).
### Tasks
- [ ] Import `datetime` module vào đầu file `todo.py`.
- [ ] Sửa constructor: Đảm bảo mỗi item mới có trường `due_date` mặc định là `None`.
- [ ] Sửa hàm `add(self, title, due_date=None)` để chấp nhận và gán giá trị ngày hết hạn.
- [ ] Thêm hàm `overdue()`: Triển khai logic lọc items chưa hoàn thành và so sánh với `datetime.date.today()`.

### Verification Plan
*   `python3 -m unittest todo.py`: Chạy kiểm thử unit để xác minh tất cả các tính năng (thêm, xong, pending, overdue) hoạt động chính xác với trường `due_date`.

## Review Log
(Đã được người dùng phê duyệt và triển khai.)