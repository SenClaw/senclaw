# Spec review — docs/designs/261027-them-han-chot-cho-cong-viec.md

version b3b90981 · mode design · 2026-10-27T23:35:22+07:00

| Lens | Status |
|---|---|
| Cảnh | ok |
| Kiểm | ok |
| Hiểu | ok |
| Phản biện | ok |

## Findings (3 blocker, 11 major, 3 nit)

- [BLOCKER] Kiểm · "Sửa đổi signature và logic của hàm `add()`" :: The plan specifies accepting an optional `due_date` but does not define its required data type or format (e.g., string vs. datetime object), which prevents writing the function signature and validation logic in `todo.py`.
- [BLOCKER] Hiểu · Technical Plan :: Logic for `get_overdue` when `item["due_date"]` is missing or null :: The document must explicitly state how items without a defined `due_date` should be treated by `get_overdue()` (e.g., always excluded, or never considered overdue).
- [BLOCKER] Phản biện · Technical Plan :: Phương thức `get_overdue(target_date: datetime.date = None)` :: Logic lọc yêu cầu kiểm tra điều kiện phức tạp (`item["done"] == False` VÀ (nếu `item["due_date"]` tồn tại, thì `item["due_date"] <= target_date`)). Cần xác nhận rõ ràng rằng nếu công việc không có ngày hết hạn, nó sẽ *không bao giờ* được tính là quá hạn.
- [MAJOR] Cảnh · Technical Plan :: `get_overdue(target_date=None)` sử dụng `datetime.date.today()` :: The dependency on real system time makes unit testing non-deterministic; this function signature should accept an explicit `current_date` parameter to allow for mocking and predictable test execution.
- [MAJOR] Cảnh · Data Model :: Cấu trúc dữ liệu của công việc (task) được mô tả là dictionary/map, nhưng không có định nghĩa rõ ràng về schema và cách xử lý các trường bắt buộc (`due_date`) khi đọc hoặc khởi tạo dữ liệu cũ :: The design must formalize the task structure (e.g., using a dataclass or named tuple) to enforce type safety and handle optional fields robustly across all methods, preventing runtime key errors.
- [MAJOR] Kiểm · "Unit Test: ... thêm 2-3 test case mô phỏng các kịch bản về công việc quá hạn" :: The plan instructs adding new test cases for overdue scenarios but does not provide the concrete code or setup for these tests, making verification impossible with a single command run.
- [MAJOR] Hiểu · Detailed Implementation Plan :: Sửa đổi hàm `add(self, title)` :: The current signature of `add()` is assumed to only take `title`, but the implementation must account for other potential parameters that might exist in the existing `TodoList` structure (e.g., description, priority) when adding a new optional parameter like `due_date`.
- [MAJOR] Hiểu · Critical User Journeys :: Output format of `get_overdue()` :: The user journey implies the function returns not just a list of items, but also "thông báo rõ ràng rằng nó đã quá hạn." The technical plan only specifies returning a list; the return type must be defined (e.g., list of dictionaries containing both the item data and an explicit status message).
- [MAJOR] Phản biện · Technical Plan :: Lọc các mục thỏa mãn điều kiện: `item["done"] == False` VÀ (nếu `item["due_date"]` tồn tại, thì `item["due_date"] <= target_date`) :: Cần làm rõ logic so sánh ngày quá hạn. Nếu một công việc không có `due_date`, nó có được coi là "không bao giờ quá hạn" hay cần xử lý riêng?
- [MAJOR] Phản biện · Technical Plan :: Phương thức `get_overdue(target_date: datetime.date = None)` :: Khi `target_date` là `None`, sử dụng ngày hiện tại (`datetime.date.today()`). Điều này tạo ra một điểm lỗi thời gian (time dependency) và không đảm bảo tính tái lập (reproducibility); nên yêu cầu người gọi phải luôn cung cấp `target_date`.
- [MAJOR] Phản biện · Goals :: Triển khai chức năng Overdue Check :: Logic so sánh ngày được mô tả là `item["due_date"] <= target_date` cho việc xác định quá hạn. Điều này có nghĩa là nếu công việc hết hạn *vào* `target_date`, nó vẫn bị coi là chưa quá hạn, điều này mâu thuẫn với mục đích của một danh sách "quá hạn".
- [MAJOR] Phản biện · Technical Plan :: Cập nhật hàm `add()` để chấp nhận tham số ngày hết hạn (`due_date: datetime.date | None`) khi thêm một mục mới vào danh sách :: Không có mô tả về cách xử lý trường hợp người dùng cung cấp giá trị `None` cho `due_date` trong các chức năng hiện tại (ví dụ: nếu nó được coi là không bao giờ hết hạn, hay cần logic khác).
- [MAJOR] Phản biện · Risks :: Lỗi so sánh ngày :: Tài liệu chỉ đề cập đến việc kiểm tra null/none nhưng không cung cấp bất kỳ quy tắc nghiệp vụ nào để xử lý trường hợp `due_date` bị thiếu (null) trong khi vẫn muốn xác định trạng thái quá hạn.
- [MAJOR] Phản biện · Overview :: Tính năng này nhằm mở rộng chức năng của `TodoList` bằng cách hỗ trợ thêm thuộc tính ngày hết hạn (`due_date`) cho mỗi công việc. :: Không xác định giới hạn về số lượng công việc tối đa mà hệ thống có thể quản lý (size limit).
- [NIT] Cảnh · Technical Plan :: Điều kiện lọc quá hạn: `item["due_date"] <= target_date` :: The definition of "overdue" should clarify if the due date *equals* the target date (i.e., is it overdue on the day it is due?) or only strictly before it; this boundary condition needs explicit confirmation to prevent off-by-one errors in logic.
- [NIT] Hiểu · Technical Plan :: Data Model :: The initial structure of the `TodoList` object is not provided, making it unclear if the modification to add `"due_date"` requires changes outside of just updating the dictionary keys within `todo.py`.
- [NIT] Phản biện · Technical Plan :: Nếu `target_date` là `None`, sử dụng ngày hiện tại (`datetime.date.today()`) :: Nên thay thế bằng cách truyền vào một tham số mặc định cố định hoặc yêu cầu người dùng cung cấp giá trị để đảm bảo tính kiểm thử và tái lập (testability).

## What each lens said

### Cảnh

```text
[MAJOR] Technical Plan :: `get_overdue(target_date=None)` sử dụng `datetime.date.today()` :: The dependency on real system time makes unit testing non-deterministic; this function signature should accept an explicit `current_date` parameter to allow for mocking and predictable test execution.
[MAJOR] Data Model :: Cấu trúc dữ liệu của công việc (task) được mô tả là dictionary/map, nhưng không có định nghĩa rõ ràng về schema và cách xử lý các trường bắt buộc (`due_date`) khi đọc hoặc khởi tạo dữ liệu cũ :: The design must formalize the task structure (e.g., using a dataclass or named tuple) to enforce type safety and handle optional fields robustly across all methods, preventing runtime key errors.
[NIT] Technical Plan :: Điều kiện lọc quá hạn: `item["due_date"] <= target_date` :: The definition of "overdue" should clarify if the due date *equals* the target date (i.e., is it overdue on the day it is due?) or only strictly before it; this boundary condition needs explicit confirmation to prevent off-by-one errors in logic.
```

### Kiểm

```text
[BLOCKER] "Sửa đổi signature và logic của hàm `add()`" :: The plan specifies accepting an optional `due_date` but does not define its required data type or format (e.g., string vs. datetime object), which prevents writing the function signature and validation logic in `todo.py`.
[MAJOR] "Unit Test: ... thêm 2-3 test case mô phỏng các kịch bản về công việc quá hạn" :: The plan instructs adding new test cases for overdue scenarios but does not provide the concrete code or setup for these tests, making verification impossible with a single command run.
```

### Hiểu

```text
[BLOCKER] Technical Plan :: Logic for `get_overdue` when `item["due_date"]` is missing or null :: The document must explicitly state how items without a defined `due_date` should be treated by `get_overdue()` (e.g., always excluded, or never considered overdue).
[MAJOR] Detailed Implementation Plan :: Sửa đổi hàm `add(self, title)` :: The current signature of `add()` is assumed to only take `title`, but the implementation must account for other potential parameters that might exist in the existing `TodoList` structure (e.g., description, priority) when adding a new optional parameter like `due_date`.
[MAJOR] Critical User Journeys :: Output format of `get_overdue()` :: The user journey implies the function returns not just a list of items, but also "thông báo rõ ràng rằng nó đã quá hạn." The technical plan only specifies returning a list; the return type must be defined (e.g., list of dictionaries containing both the item data and an explicit status message).
[NIT] Technical Plan :: Data Model :: The initial structure of the `TodoList` object is not provided, making it unclear if the modification to add `"due_date"` requires changes outside of just updating the dictionary keys within `todo.py`.
```

### Phản biện

```text
[MAJOR] Technical Plan :: Lọc các mục thỏa mãn điều kiện: `item["done"] == False` VÀ (nếu `item["due_date"]` tồn tại, thì `item["due_date"] <= target_date`) :: Cần làm rõ logic so sánh ngày quá hạn. Nếu một công việc không có `due_date`, nó có được coi là "không bao giờ quá hạn" hay cần xử lý riêng?
[MAJOR] Technical Plan :: Phương thức `get_overdue(target_date: datetime.date = None)` :: Khi `target_date` là `None`, sử dụng ngày hiện tại (`datetime.date.today()`). Điều này tạo ra một điểm lỗi thời gian (time dependency) và không đảm bảo tính tái lập (reproducibility); nên yêu cầu người gọi phải luôn cung cấp `target_date`.
[MAJOR] Goals :: Triển khai chức năng Overdue Check :: Logic so sánh ngày được mô tả là `item["due_date"] <= target_date` cho việc xác định quá hạn. Điều này có nghĩa là nếu công việc hết hạn *vào* `target_date`, nó vẫn bị coi là chưa quá hạn, điều này mâu thuẫn với mục đích của một danh sách "quá hạn".
[MAJOR] Technical Plan :: Cập nhật hàm `add()` để chấp nhận tham số ngày hết hạn (`due_date: datetime.date | None`) khi thêm một mục mới vào danh sách :: Không có mô tả về cách xử lý trường hợp người dùng cung cấp giá trị `None` cho `due_date` trong các chức năng hiện tại (ví dụ: nếu nó được coi là không bao giờ hết hạn, hay cần logic khác).
[MAJOR] Risks :: Lỗi so sánh ngày :: Tài liệu chỉ đề cập đến việc kiểm tra null/none nhưng không cung cấp bất kỳ quy tắc nghiệp vụ nào để xử lý trường hợp `due_date` bị thiếu (null) trong khi vẫn muốn xác định trạng thái quá hạn.
[BLOCKER] Technical Plan :: Phương thức `get_overdue(target_date: datetime.date = None)` :: Logic lọc yêu cầu kiểm tra điều kiện phức tạp (`item["done"] == False` VÀ (nếu `item["due_date"]` tồn tại, thì `item["due_date"] <= target_date`)). Cần xác nhận rõ ràng rằng nếu công việc không có ngày hết hạn, nó sẽ *không bao giờ* được tính là quá hạn.
[MAJOR] Overview :: Tính năng này nhằm mở rộng chức năng của `TodoList` bằng cách hỗ trợ thêm thuộc tính ngày hết hạn (`due_date`) cho mỗi công việc. :: Không xác định giới hạn về số lượng công việc tối đa mà hệ thống có thể quản lý (size limit).
[NIT] Technical Plan :: Nếu `target_date` là `None`, sử dụng ngày hiện tại (`datetime.date.today()`) :: Nên thay thế bằng cách truyền vào một tham số mặc định cố định hoặc yêu cầu người dùng cung cấp giá trị để đảm bảo tính kiểm thử và tái lập (testability).
```
