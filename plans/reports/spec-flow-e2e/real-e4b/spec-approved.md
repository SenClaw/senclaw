# Thêm hạn chót cho công việc và liệt kê việc quá hạn
*   **Status:** Draft
*   **Date:** 2026-10-27

## Overview
Tính năng này nhằm bổ sung trường "Hạn Chót" (Due Date) vào các mục công việc trong ứng dụng danh sách việc cần làm, cho phép người dùng không chỉ quản lý tác vụ mà còn chủ động theo dõi những nhiệm vụ sắp hoặc đã quá hạn. Việc thêm tính năng này sẽ cải thiện khả năng lập kế hoạch và nhắc nhở của người dùng về các cam kết thời gian bằng cách bổ sung cơ chế lọc công việc dựa trên ngày hết hạn.

## The Problem
Hiện tại, ứng dụng danh sách việc cần làm (`todo.py`) chỉ hỗ trợ trạng thái "Hoàn thành" hay "Chưa hoàn thành", thiếu cơ chế xác định thời điểm nhiệm vụ phải được thực hiện. Điều này khiến người dùng khó khăn trong việc phân biệt mức độ ưu tiên dựa trên thời gian và dễ dàng bỏ sót các công việc có hạn chót quan trọng, dẫn đến quản lý tác vụ không hiệu quả về mặt thời gian.

## Goals
1.  **Cấu trúc dữ liệu:** Bổ sung trường `due_date` (Ngày Hạn Chót) vào cấu trúc dữ liệu của mỗi mục công việc (`item`).
2.  **Giao diện người dùng (UI):** Cập nhật khả năng hiển thị ngày hết hạn một cách rõ ràng trong danh sách tác vụ.
3.  **Lọc và Báo cáo:** Phát triển hàm mới (`overdue_tasks`) để truy xuất và liệt kê tất cả các nhiệm vụ mà `due_date` đã trôi qua (và chưa hoàn thành).

## Non-Goals
*   Không bao gồm tính năng nhắc nhở bằng email hoặc thông báo đẩy (push notification) tích hợp bên thứ ba.
*   Không xử lý việc luân chuyển công việc giữa các người dùng khác nhau (chỉ tập trung vào quản lý cá nhân).
*   Chỉ bổ sung ngày hết hạn và logic lọc, không thay đổi cách thức cốt lõi của việc thêm hoặc đánh dấu hoàn thành tác vụ hiện tại.

## Open Questions
1.  **Định dạng ngày:** Định dạng dữ liệu cho `due_date` nên là gì: chuỗi (string) theo định dạng ISO 8601 hay một đối tượng Date Python?
2.  **Hiển thị quá hạn:** Khi một công việc quá hạn, chúng ta nên hiển thị mức độ nổi bật nào để thu hút sự chú ý của người dùng nhất (ví dụ: gắn nhãn đỏ `[OVERDUE]`, thông báo pop-up khi mở ứng dụng)?

## Critical User Journeys
**1. Thêm tác vụ với ngày hết hạn:**
*   **Given:** Người dùng đang ở giao diện thêm tác vụ mới.
*   **When:** Người dùng nhập tiêu đề và chọn một `due_date` cụ thể (ví dụ: 2026-11-15).
*   **Then:** Tác vụ được lưu trữ thành công với trường `due_date` và hiển thị ngày này trong danh sách.

**2. Xác định tác vụ quá hạn:**
*   **Given:** Hệ thống đã có một số tác vụ, trong đó có một tác vụ A hết hạn ngày 2026-10-25 nhưng chưa được đánh dấu hoàn thành.
*   **When:** Người dùng truy vấn danh sách các tác vụ quá hạn (`overdue_tasks()`).
*   **Then:** Hệ thống trả về Tác vụ A và hiển thị rõ ràng trạng thái "Quá hạn" bên cạnh ngày hết hạn.

## Technical Plan
Về cơ bản, cần thực hiện các thay đổi sau:
1.  **Cấu trúc dữ liệu (`todo.py`):** Sửa lớp `TodoList` để mỗi mục từ điển (`item`) phải bao gồm khóa `"due_date"` (và có thể là một giá trị mặc định hoặc null/None).
2.  **Các phương thức:** Cần bổ sung hàm `overdue_tasks()` và sửa đổi các hàm khác như `add()` để chấp nhận tham số ngày hết hạn.
3.  **Xử lý thời gian:** Phải sử dụng thư viện `datetime` của Python (hoặc tương đương) để so sánh ngày hiện tại với giá trị `due_date`.

## Alternatives Considered
*   **Tạo một lớp `Task` riêng biệt:** Tách logic thành một lớp `Task(id, title, due_date, done)` thay vì sử dụng dict. *Ưu điểm:* Cực kỳ sạch sẽ về mặt OOP, dễ kiểm thử đơn vị. *Nhược điểm:* Tăng lượng code cần viết và các file bị ảnh hưởng (ví dụ: cách khởi tạo list).
*   **Sử dụng một kho lưu trữ cơ sở dữ liệu:** Nếu ứng dụng lớn hơn, nên chuyển sang SQLite hoặc tương tự. *Ưu điểm:* Khả năng mở rộng và tính toàn vẹn dữ liệu cao nhất. *Nhược điểm:* Quá mức cần thiết cho phạm vi hiện tại và làm tăng độ phức tạp của dự án.

## Risks
*   **Thiếu định dạng ngày thống nhất:** Nếu không xác định rõ ràng định dạng ngày (ISO 8601 hay đối tượng Python), việc so sánh thời gian trong code sẽ bị sai lệch nghiêm trọng.
*   **Hiệu suất truy vấn:** Việc thêm bộ lọc theo ngày có thể làm chậm quá trình tải danh sách nếu số lượng tác vụ lớn, cần tối ưu hóa hàm `overdue_tasks`.

## Detailed Implementation Plan
### Target Files
#### 1. `todo.py` (Modify)
Thay đổi cấu trúc lớp `TodoList` để chấp nhận và sử dụng ngày hết hạn (`due_date`). Cần sửa đổi các phương thức `__init__`, `add()`, và thêm hàm `overdue_tasks()`.

### Tasks   (- [ ] T1) Cập nhật `__init__` của `TodoList` để hỗ trợ cấu trúc dữ liệu mới.
### Tasks   (- [ ] T2) Sửa `add(title)`: Cho phép nhận tham số ngày hết hạn và lưu trữ nó vào mục `item`.
### Tasks   (- [ ] T3) Viết hàm `overdue_tasks()`: Truy vấn các công việc chưa hoàn thành mà ngày hiện tại đã lớn hơn `due_date`.

## Verification Plan
*   **Command:** `python3 -m unittest test*.py` (Sau khi sửa code, chạy lệnh này để kiểm tra tính đúng đắn của logic).
*   **Expected outcome:** Tất cả các bài kiểm thử đều phải pass.

## Review Log
(Chờ đánh giá)