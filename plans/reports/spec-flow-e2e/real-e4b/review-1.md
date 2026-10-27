# Spec review — docs/designs/261027-them-han-chot-cho-cong-viec.md

version 74ef7fda · mode design · 2026-10-27T22:10:10+07:00

| Lens | Status |
|---|---|
| Cảnh | ok |
| Kiểm | ok |
| Hiểu | ok |
| Phản biện | ok |

## Findings (6 blocker, 14 major, 4 nit)

- [BLOCKER] Cảnh · Technical Plan :: Định dạng ngày không được xác định rõ ràng và nhất quán :: `todo.py` phải đảm bảo rằng tất cả các giá trị `due_date` được lưu trữ dưới dạng đối tượng `datetime.date` hoặc `datetime.datetime` để việc so sánh thời gian là chính xác, tránh lỗi chuyển đổi chuỗi (string parsing) trong mọi hàm liên quan (`__init__`, `add()`).
- [BLOCKER] Kiểm · todo.py :: The plan requires modifying `add(title)` to accept a `due_date`, but this necessary signature change is not reflected in the provided function outline (todo.py: 8: def add(self, title):) :: Update the outline/signature for `add` to include the date parameter and type hint.
- [BLOCKER] Kiểm · todo.py :: The data shape and expected format of the new `due_date` field are not specified (e.g., Python `datetime.date`, string format YYYY-MM-DD) :: Define the required data type and serialization/deserialization logic for `due_date`.
- [BLOCKER] Hiểu · Technical Plan :: Xử lý thời gian :: Cần xác định rõ ràng việc so sánh ngày hiện tại với `due_date` phải sử dụng múi giờ nào (ví dụ: UTC hay múi giờ cục bộ của người dùng) để đảm bảo tính nhất quán khi xác định trạng thái quá hạn.
- [BLOCKER] Hiểu · Goals/Open Questions :: Định dạng và kiểu dữ liệu cho `due_date` :: Cần quyết định dứt khoát giữa việc lưu trữ dưới dạng chuỗi (string ISO 8601) hay đối tượng Date Python, vì điều này ảnh hưởng trực tiếp đến logic so sánh thời gian trong hàm `overdue_tasks()`.
- [BLOCKER] Phản biện · Technical Plan :: Xử lý thời gian: Phải sử dụng thư viện `datetime` của Python (hoặc tương đương) để so sánh ngày hiện tại với giá trị `due_date` :: Không có cơ chế nào được đề cập để xử lý các múi giờ (time zones), dẫn đến việc so sánh ngày hết hạn sẽ bị sai lệch nghiêm trọng khi ứng dụng chạy ở các khu vực khác nhau.
- [MAJOR] Kiểm (machine) · ### Tasks :: missing :: add it under ## Detailed Implementation Plan
- [MAJOR] Kiểm (machine) · ### Verification Plan :: missing :: add it under ## Detailed Implementation Plan
- [MAJOR] Kiểm (machine) · ### Verification Plan :: no command to run :: name the build/test commands in backticks
- [MAJOR] Cảnh · Goals :: Các mục tiêu về "Giao diện người dùng (UI)" và "Hiển thị" thuộc lớp trình bày (Presentation Layer), không phải lớp mô hình dữ liệu (Data Model) :: Lớp `TodoList` trong `todo.py` chỉ nên chịu trách nhiệm quản lý trạng thái và lọc dữ liệu; việc gắn nhãn đỏ `[OVERDUE]` hoặc thông báo pop-up cần được xử lý ở tầng gọi hàm (calling layer).
- [MAJOR] Cảnh · Detailed Implementation Plan :: Hàm `overdue_tasks()` cần một định nghĩa rõ ràng về "ngày hiện tại" để so sánh :: Cần xác định liệu việc kiểm tra quá hạn chỉ dựa trên ngày (`date`) hay bao gồm cả thời gian (`datetime.time`), và đảm bảo rằng logic này xử lý các trường hợp biên (edge cases) như công việc hết hạn vào cuối ngày hôm nay nhưng được truy vấn vào đầu ngày mai.
- [MAJOR] Kiểm · Detailed Implementation Plan :: The steps listed are high-level tasks ("Sửa `add(title)`...") rather than concrete, actionable code changes (e.g., "Add parameter X to function Y in file Z") :: Rewrite the implementation plan using specific instructions like "Modify signature of `add` in `todo.py` to accept a date object."
- [MAJOR] Kiểm · Verification Plan :: While a command is listed, there are no concrete test cases provided for edge scenarios (e.g., calling `overdue_tasks()` when all tasks are complete, or passing an invalid date format) :: Add specific unit tests covering the normal case and at least two defined edge cases for the new functionality.
- [MAJOR] Hiểu · Technical Plan/Tasks :: Định nghĩa cấu trúc dữ liệu của `item` :: Cần định nghĩa rõ ràng và đầy đủ tất cả các khóa (keys) bắt buộc phải có trong từ điển `item`, bao gồm kiểu dữ liệu mong muốn cho mỗi trường, thay vì chỉ đề cập đến việc bổ sung `"due_date"`.
- [MAJOR] Hiểu · Technical Plan/Tasks :: Xử lý tác vụ không có hạn chót :: Cần xác định logic xử lý khi một mục công việc được tạo mà không cung cấp `due_date` (ví dụ: coi là vô thời hạn, hay buộc phải nhập ngày).
- [MAJOR] Phản biện · Technical Plan :: Cấu trúc dữ liệu (`todo.py`): Sửa lớp `TodoList` để mỗi mục từ điển (`item`) phải bao gồm khóa `"due_date"` (và có thể là một giá trị mặc định hoặc null/None) :: Phải xác định rõ ràng cơ chế xử lý khi người dùng không cung cấp ngày hết hạn, thay vì chỉ nói rằng nó "có thể" bao gồm khóa này.
- [MAJOR] Phản biện · Critical User Journeys :: Xác định tác vụ quá hạn: When: Người dùng truy vấn danh sách các tác vụ quá hạn (`overdue_tasks()`) :: Không có thông tin về cách thức xác định "ngày hiện tại" (current date) được sử dụng để so sánh, điều này là một điểm lỗi nghiêm trọng nếu không được truyền vào hoặc lấy từ nguồn đáng tin cậy.
- [MAJOR] Phản biện · Technical Plan :: Các phương thức: Cần bổ sung hàm `overdue_tasks()` và sửa đổi các hàm khác như `add()` để chấp nhận tham số ngày hết hạn :: Không xác định rõ ràng việc xử lý lỗi (error handling) khi người dùng cung cấp một giá trị ngày không hợp lệ hoặc thiếu định dạng.
- [MAJOR] Phản biện · Overview :: Tính năng này nhằm bổ sung trường "Hạn Chót" (Due Date)... :: Tài liệu bỏ qua các giới hạn về kích thước dữ liệu và số lượng tác vụ tối đa mà hệ thống có thể xử lý hiệu quả khi lọc theo ngày hết hạn, điều này ảnh hưởng trực tiếp đến hiệu suất.
- [MAJOR] Phản biện · Alternatives Considered :: Sử dụng một kho lưu trữ cơ sở dữ liệu: Nếu ứng dụng lớn hơn, nên chuyển sang SQLite hoặc tương tự. :: Mặc dù đã nêu nhược điểm là "Quá mức cần thiết cho phạm vi hiện tại", nhưng việc không đưa ra bất kỳ tiêu chí định lượng nào (ví dụ: số lượng người dùng dự kiến, số tác vụ tối đa) để chứng minh rằng cơ sở dữ liệu trong bộ nhớ là đủ sẽ khiến quyết định này trở nên tùy tiện.
- [NIT] Cảnh · Technical Plan :: Việc sửa đổi `add(title)` nên thay đổi chữ ký hàm để chấp nhận tham số `due_date` một cách rõ ràng hơn, thay vì chỉ nói "chấp nhận tham số ngày hết hạn" :: Cần cập nhật chữ ký của `todo.py: add()` thành `def add(self, title, due_date=None):`.
- [NIT] Hiểu · Critical User Journeys :: Cơ chế hiển thị trạng thái "Quá hạn" :: Nên làm rõ liệu việc hiển thị trạng thái quá hạn chỉ là một nhãn văn bản (`[OVERDUE]`) trong danh sách hay nó yêu cầu thay đổi giao diện người dùng (UI) phức tạp hơn như pop-up.
- [NIT] Phản biện · Open Questions :: Định dạng ngày: Định dạng dữ liệu cho `due_date` nên là gì: chuỗi (string) theo định dạng ISO 8601 hay một đối tượng Date Python? :: Cần xác định rõ ràng rằng việc chọn kiểu dữ liệu sẽ được áp dụng nhất quán trong toàn bộ hệ thống, bao gồm cả khi đọc từ nguồn lưu trữ.
- [NIT] Phản biện · Risks :: Hiệu suất truy vấn: Việc thêm bộ lọc theo ngày có thể làm chậm quá trình tải danh sách nếu số lượng tác vụ lớn, cần tối ưu hóa hàm `overdue_tasks`. :: Đây là một tuyên bố về hiệu năng mà không kèm theo bất kỳ yêu cầu cụ thể nào (ví dụ: độ phức tạp thời gian O(n) hay O(log n)) hoặc các bài kiểm tra hiệu suất để chứng minh tính khả thi.

## What each lens said

### Cảnh

```text
[BLOCKER] Technical Plan :: Định dạng ngày không được xác định rõ ràng và nhất quán :: `todo.py` phải đảm bảo rằng tất cả các giá trị `due_date` được lưu trữ dưới dạng đối tượng `datetime.date` hoặc `datetime.datetime` để việc so sánh thời gian là chính xác, tránh lỗi chuyển đổi chuỗi (string parsing) trong mọi hàm liên quan (`__init__`, `add()`).
[MAJOR] Goals :: Các mục tiêu về "Giao diện người dùng (UI)" và "Hiển thị" thuộc lớp trình bày (Presentation Layer), không phải lớp mô hình dữ liệu (Data Model) :: Lớp `TodoList` trong `todo.py` chỉ nên chịu trách nhiệm quản lý trạng thái và lọc dữ liệu; việc gắn nhãn đỏ `[OVERDUE]` hoặc thông báo pop-up cần được xử lý ở tầng gọi hàm (calling layer).
[MAJOR] Detailed Implementation Plan :: Hàm `overdue_tasks()` cần một định nghĩa rõ ràng về "ngày hiện tại" để so sánh :: Cần xác định liệu việc kiểm tra quá hạn chỉ dựa trên ngày (`date`) hay bao gồm cả thời gian (`datetime.time`), và đảm bảo rằng logic này xử lý các trường hợp biên (edge cases) như công việc hết hạn vào cuối ngày hôm nay nhưng được truy vấn vào đầu ngày mai.
[NIT] Technical Plan :: Việc sửa đổi `add(title)` nên thay đổi chữ ký hàm để chấp nhận tham số `due_date` một cách rõ ràng hơn, thay vì chỉ nói "chấp nhận tham số ngày hết hạn" :: Cần cập nhật chữ ký của `todo.py: add()` thành `def add(self, title, due_date=None):`.
```

### Kiểm

```text
[BLOCKER] todo.py :: The plan requires modifying `add(title)` to accept a `due_date`, but this necessary signature change is not reflected in the provided function outline (todo.py: 8: def add(self, title):) :: Update the outline/signature for `add` to include the date parameter and type hint.
[BLOCKER] todo.py :: The data shape and expected format of the new `due_date` field are not specified (e.g., Python `datetime.date`, string format YYYY-MM-DD) :: Define the required data type and serialization/deserialization logic for `due_date`.
[MAJOR] Detailed Implementation Plan :: The steps listed are high-level tasks ("Sửa `add(title)`...") rather than concrete, actionable code changes (e.g., "Add parameter X to function Y in file Z") :: Rewrite the implementation plan using specific instructions like "Modify signature of `add` in `todo.py` to accept a date object."
[MAJOR] Verification Plan :: While a command is listed, there are no concrete test cases provided for edge scenarios (e.g., calling `overdue_tasks()` when all tasks are complete, or passing an invalid date format) :: Add specific unit tests covering the normal case and at least two defined edge cases for the new functionality.
```

### Hiểu

```text
[BLOCKER] Technical Plan :: Xử lý thời gian :: Cần xác định rõ ràng việc so sánh ngày hiện tại với `due_date` phải sử dụng múi giờ nào (ví dụ: UTC hay múi giờ cục bộ của người dùng) để đảm bảo tính nhất quán khi xác định trạng thái quá hạn.
[BLOCKER] Goals/Open Questions :: Định dạng và kiểu dữ liệu cho `due_date` :: Cần quyết định dứt khoát giữa việc lưu trữ dưới dạng chuỗi (string ISO 8601) hay đối tượng Date Python, vì điều này ảnh hưởng trực tiếp đến logic so sánh thời gian trong hàm `overdue_tasks()`.
[MAJOR] Technical Plan/Tasks :: Định nghĩa cấu trúc dữ liệu của `item` :: Cần định nghĩa rõ ràng và đầy đủ tất cả các khóa (keys) bắt buộc phải có trong từ điển `item`, bao gồm kiểu dữ liệu mong muốn cho mỗi trường, thay vì chỉ đề cập đến việc bổ sung `"due_date"`.
[MAJOR] Technical Plan/Tasks :: Xử lý tác vụ không có hạn chót :: Cần xác định logic xử lý khi một mục công việc được tạo mà không cung cấp `due_date` (ví dụ: coi là vô thời hạn, hay buộc phải nhập ngày).
[NIT] Critical User Journeys :: Cơ chế hiển thị trạng thái "Quá hạn" :: Nên làm rõ liệu việc hiển thị trạng thái quá hạn chỉ là một nhãn văn bản (`[OVERDUE]`) trong danh sách hay nó yêu cầu thay đổi giao diện người dùng (UI) phức tạp hơn như pop-up.
```

### Phản biện

```text
[MAJOR] Technical Plan :: Cấu trúc dữ liệu (`todo.py`): Sửa lớp `TodoList` để mỗi mục từ điển (`item`) phải bao gồm khóa `"due_date"` (và có thể là một giá trị mặc định hoặc null/None) :: Phải xác định rõ ràng cơ chế xử lý khi người dùng không cung cấp ngày hết hạn, thay vì chỉ nói rằng nó "có thể" bao gồm khóa này.
[BLOCKER] Technical Plan :: Xử lý thời gian: Phải sử dụng thư viện `datetime` của Python (hoặc tương đương) để so sánh ngày hiện tại với giá trị `due_date` :: Không có cơ chế nào được đề cập để xử lý các múi giờ (time zones), dẫn đến việc so sánh ngày hết hạn sẽ bị sai lệch nghiêm trọng khi ứng dụng chạy ở các khu vực khác nhau.
[MAJOR] Critical User Journeys :: Xác định tác vụ quá hạn: When: Người dùng truy vấn danh sách các tác vụ quá hạn (`overdue_tasks()`) :: Không có thông tin về cách thức xác định "ngày hiện tại" (current date) được sử dụng để so sánh, điều này là một điểm lỗi nghiêm trọng nếu không được truyền vào hoặc lấy từ nguồn đáng tin cậy.
[MAJOR] Technical Plan :: Các phương thức: Cần bổ sung hàm `overdue_tasks()` và sửa đổi các hàm khác như `add()` để chấp nhận tham số ngày hết hạn :: Không xác định rõ ràng việc xử lý lỗi (error handling) khi người dùng cung cấp một giá trị ngày không hợp lệ hoặc thiếu định dạng.
[MAJOR] Overview :: Tính năng này nhằm bổ sung trường "Hạn Chót" (Due Date)... :: Tài liệu bỏ qua các giới hạn về kích thước dữ liệu và số lượng tác vụ tối đa mà hệ thống có thể xử lý hiệu quả khi lọc theo ngày hết hạn, điều này ảnh hưởng trực tiếp đến hiệu suất.
[MAJOR] Alternatives Considered :: Sử dụng một kho lưu trữ cơ sở dữ liệu: Nếu ứng dụng lớn hơn, nên chuyển sang SQLite hoặc tương tự. :: Mặc dù đã nêu nhược điểm là "Quá mức cần thiết cho phạm vi hiện tại", nhưng việc không đưa ra bất kỳ tiêu chí định lượng nào (ví dụ: số lượng người dùng dự kiến, số tác vụ tối đa) để chứng minh rằng cơ sở dữ liệu trong bộ nhớ là đủ sẽ khiến quyết định này trở nên tùy tiện.
[NIT] Open Questions :: Định dạng ngày: Định dạng dữ liệu cho `due_date` nên là gì: chuỗi (string) theo định dạng ISO 8601 hay một đối tượng Date Python? :: Cần xác định rõ ràng rằng việc chọn kiểu dữ liệu sẽ được áp dụng nhất quán trong toàn bộ hệ thống, bao gồm cả khi đọc từ nguồn lưu trữ.
[NIT] Risks :: Hiệu suất truy vấn: Việc thêm bộ lọc theo ngày có thể làm chậm quá trình tải danh sách nếu số lượng tác vụ lớn, cần tối ưu hóa hàm `overdue_tasks`. :: Đây là một tuyên bố về hiệu năng mà không kèm theo bất kỳ yêu cầu cụ thể nào (ví dụ: độ phức tạp thời gian O(n) hay O(log n)) hoặc các bài kiểm tra hiệu suất để chứng minh tính khả thi.
```
