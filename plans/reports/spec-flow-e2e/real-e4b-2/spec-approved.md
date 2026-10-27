# Thêm tính năng hạn chót và hiển thị công việc quá hạn

*   **Status:** Needs Clarification (Cần làm rõ)
*   **Date:** 2026-10-27

## Overview
Việc mở rộng hệ thống quản lý danh sách việc cần làm (`TodoList`) để cho phép gán ngày hết hạn (deadline) cho mỗi công việc. Đồng thời, bổ sung chức năng để lọc và liệt kê các mục công việc đã quá ngày hết hạn (overdue).

## The Problem
Hiện tại, `TodoList` chỉ theo dõi trạng thái hoàn thành (`done: True/False`) nhưng không có cơ chế nào quản lý tính thời gian hay nhắc nhở về deadline. Điều này làm mất khả năng nhận biết và hiển thị các công việc quan trọng đã bị lỡ hẹn, gây khó khăn cho người dùng trong việc lập kế hoạch và theo dõi tiến độ dự án thực tế.

## Goals
1.  **Cập nhật cấu trúc dữ liệu:** Tách biệt logic Task Item thành một component rõ ràng (`TaskItem`) để quản lý các thuộc tính (title, due_date, done).
2.  **Thao tác với Deadline:** Cho phép người dùng thiết lập hoặc cập nhật hạn chót khi thêm/sửa công việc, đồng thời phải thực hiện **Validation đầu vào nghiêm ngặt** cho giá trị ngày tháng.
3.  **Lọc Overdue:** Bổ sung phương thức mới (`overdue()`) để trả về danh sách các mục thỏa mãn điều kiện: 1) chưa hoàn thành, và 2) ngày hôm nay (theo múi giờ UTC tiêu chuẩn) lớn hơn hoặc bằng `due_date`.

## Non-Goals
*   Thiết lập hệ thống nhắc nhở tự động (notifications) cho người dùng.
*   Quản lý nhiều dự án/danh sách công việc độc lập; chỉ tập trung vào logic của một `TodoList` instance.
*   Cho phép sửa ngày hết hạn sau khi công việc đã được đánh dấu hoàn thành.

## Open Questions
1.  **Định dạng và Xử lý Ngày tháng:** Tất cả các thao tác so sánh ngày tháng phải sử dụng **UTC (Coordinated Universal Time)** để đảm bảo tính nhất quán toàn cầu.
2.  **Scope của `pending()`:** Hàm `pending()` nên trả về TẤT CẢ các công việc chưa hoàn thành, bao gồm cả những mục đã quá hạn hay không? Việc này ảnh hưởng đến cách người dùng nhận biết vấn đề.

## Critical User Journeys
### 1. Thêm công việc với deadline
*   **Given:** `TodoList` instance đang hoạt động và ngày hết hạn hợp lệ (không phải là giá trị trong quá khứ hoặc định dạng sai).
*   **When:** Người dùng gọi phương thức `add()` và cung cấp giá trị cho ngày hết hạn (`due_date`).
*   **Then:** Một item mới được tạo với cấu trúc `TaskItem` riêng biệt, bao gồm trường `due_date`, và hệ thống ghi nhận trạng thái hợp lệ.

### 2. Lấy danh sách pending (cần làm)
*   **Given:** `TodoList` instance chứa các mục đã hoàn thành và chưa hoàn thành.
*   **When:** Người dùng gọi phương thức `pending()`.
*   **Then:** Hệ thống trả về một danh sách tất cả các item **chưa được đánh dấu hoàn thành**, được sắp xếp theo ID tăng dần (và *có bao gồm* các mục quá hạn).

### 3. Lấy công việc quá hạn (Overdue)
*   **Given:** `TodoList` instance chứa ít nhất một mục đã vượt qua ngày hết hạn (`due_date <= today`).
*   **When:** Người dùng gọi phương thức mới `overdue()`.
*   **Then:** Hệ thống trả về danh sách các item thỏa mãn điều kiện: chưa hoàn thành và quá hạn (tức là ngày đến hạn phải nhỏ hơn hoặc bằng ngày hôm nay), được sắp xếp theo ID.

## Technical Plan
Triển khai sẽ yêu cầu cấu trúc lại `TodoList` để chứa một lớp/struct `TaskItem`. Các thay đổi chính bao gồm việc chuẩn hóa mọi hoạt động ngày tháng về UTC, thêm validation đầu vào cho các hàm thao tác với deadline, và tái định nghĩa logic của `pending()` và `overdue()`.

## Alternatives Considered
- **A. Tách TaskItem:** Thiết lập một lớp/struct riêng biệt (`TaskItem`) để đóng gói dữ liệu công việc. Phương án này được chọn vì nó tách rời mô hình dữ liệu khỏi logic quản lý (TodoList), tăng tính tái sử dụng và bảo trì.
- **B. Giữ nguyên Dictionary:** Không nên làm điều này vì nó vi phạm nguyên tắc lập trình hướng đối tượng, khiến mã khó đọc và khó mở rộng khi thêm các validation phức tạp hơn.

## Risks
1.  **Time Zone Handling:** Bắt buộc phải sử dụng UTC cho mọi thao tác logic về thời gian. Điều này đòi hỏi cả việc xử lý đầu vào (Input) cũng như ngày hệ thống (`date.today()`).
2.  **Persistence/Serialization:** Cần xác định cách lưu trữ và tải các đối tượng `TaskItem` (bao gồm các giá trị UTC) khi cần lưu trạng thái của danh sách công việc.

## Detailed Implementation Plan
### Target Files
*   `todo.py`: Modify (Thêm class `TaskItem`, sửa cấu trúc item, cập nhật hàm `add()`, và thêm/sửa `overdue()` và `pending()`).
### Tasks
- [ ] Định nghĩa lớp `TaskItem` trong file `todo.py`.
- [ ] Cập nhật constructor của `TodoList` để sử dụng `TaskItem`.
- [ ] Sửa hàm `add(self, title, due_date=None)`: Thêm validation đầu vào cho ngày tháng và khởi tạo `TaskItem`.
- [ ] Sửa hàm `pending()`: Đảm bảo nó luôn trả về tất cả các item chưa hoàn thành.
- [ ] Thêm/sửa hàm `overdue()`: Triển khai logic so sánh ngày UTC hiện tại với `due_date` của `TaskItem`.

### Verification Plan
*   **Kiểm thử Unit:** Viết thêm test case cho việc xử lý múi giờ và các trường hợp biên (Edge cases) như item quá hạn *hôm nay*.

## Review Log
(Đã được người dùng phê duyệt và tiến hành sửa đổi Spec.)