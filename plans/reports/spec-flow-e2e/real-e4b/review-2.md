# Spec review — docs/designs/261027-them-han-chot-cho-cong-viec.md

version 74ef7fda · mode design · 2026-10-27T22:13:00+07:00

| Lens | Status |
|---|---|
| Cảnh | ok |
| Kiểm | ok |
| Hiểu | ok |
| Phản biện | ok |

## Findings (4 blocker, 15 major, 4 nit)

- [BLOCKER] Cảnh · Technical Plan :: "Phải sử dụng thư viện `datetime` của Python... để so sánh ngày hiện tại với giá trị `due_date`." :: Cần định nghĩa rõ ràng và bắt buộc kiểu dữ liệu cho `due_date` là đối tượng `datetime.date` (hoặc `datetime.datetime`) ngay từ bước thiết kế, thay vì chỉ đề cập đến việc sử dụng thư viện này.
- [BLOCKER] Kiểm · Detailed Implementation Plan :: The tasks are described conceptually ("Sửa `add(title)`: Cho phép nhận tham số ngày hết hạn...") but do not specify the required API changes, data types (e.g., should `due_date` be a string or datetime object?), or error handling for missing/invalid dates :: Specify the exact signature change for `__init__` and `add()` including expected input types and how date parsing errors must be handled.
- [BLOCKER] Hiểu · Technical Plan :: When loading existing tasks into `TodoList`, how should the system handle items that do not currently contain the `"due_date"` key? :: The process must define a default value or skip validation for missing keys to prevent runtime errors.
- [BLOCKER] Hiểu · Open Questions :: The document presents two conflicting options for `due_date` format (ISO 8601 string vs. Python Date object), which fundamentally determines the comparison logic in `overdue_tasks()`. :: A definitive choice must be made and documented before implementation begins.
- [MAJOR] Kiểm (machine) · ### Tasks :: missing :: add it under ## Detailed Implementation Plan
- [MAJOR] Kiểm (machine) · ### Verification Plan :: missing :: add it under ## Detailed Implementation Plan
- [MAJOR] Kiểm (machine) · ### Verification Plan :: no command to run :: name the build/test commands in backticks
- [MAJOR] Cảnh · Goals / Technical Plan :: Việc tập trung vào các chi tiết hiển thị như "gắn nhãn đỏ [OVERDUE]" và "thông báo pop-up" là lo lắng về giao diện người dùng (UI) chứ không phải logic nghiệp vụ cốt lõi; chúng nên được tách ra khỏi thiết kế cấu trúc dữ liệu. :: Giới hạn phạm vi của tài liệu này chỉ ở việc cập nhật mô hình dữ liệu (`todo.py`) và các hàm truy vấn, loại bỏ mọi đề cập đến cách thức hiển thị trực quan.
- [MAJOR] Cảnh · Technical Plan / Cấu trúc dữ liệu :: Việc sửa lớp `TodoList` để mỗi mục từ điển (`item`) phải bao gồm khóa `"due_date"` làm cho cấu trúc dữ liệu trở nên cứng nhắc và khó bảo trì; điều này vi phạm nguyên tắc đóng gói (encapsulation). :: Thay vì thay đổi trực tiếp các dictionary bên trong, hãy xem xét việc tạo một lớp `Task` đơn giản chỉ chứa ID, title, due_date, done để quản lý trạng thái của từng mục.
- [MAJOR] Kiểm · Detailed Implementation Plan :: The steps are not concrete enough to write code (e.g., "Cập nhật cấu trúc lớp..." or "Sửa `add(title)`:") :: For every task, provide the precise modification required in the file, such as "Add field `due_date: Optional[datetime]` to `TodoList.__init__` signature" and "Update `add()` signature to accept `due_date: Optional[datetime]`".
- [MAJOR] Kiểm · Verification Plan :: The plan provides a general test command but lacks concrete tests for edge cases (e.g., what if the due date is invalid, or what if no tasks are overdue?) :: Add specific unit tests within the verification plan that cover failure modes and boundary conditions related to `due_date` handling.
- [MAJOR] Hiểu · Technical Plan :: When modifying `add()` to accept a date, the document does not specify how missing or invalid date inputs (e.g., null, malformed string) should be handled by the function. :: The method needs explicit validation logic for the optional date parameter.
- [MAJOR] Phản biện · Alternatives Considered :: The rejection of "Tạo một lớp Task riêng biệt" relies on subjective assessment ("tăng lượng code cần viết và các file bị ảnh hưởng") rather than concrete technical limitations or measurable costs, making the decision potentially flawed :: Re-evaluate the trade-off between OOP purity/testability (Task class) and implementation complexity for this specific scope.
- [MAJOR] Phản biện · Technical Plan :: The plan assumes that simply adding a `due_date` key to the dictionary structure (`item`) will guarantee data integrity and correct type handling across all existing code paths, which is unlikely without explicit schema validation or migration logic :: Implement robust input validation and type coercion (e.g., ensuring `due_date` is always a Python `datetime` object) when adding or loading tasks.
- [MAJOR] Phản biện · Technical Plan :: The plan mentions using the `datetime` library for comparison but fails to address how time zones will be handled, leading to potential discrepancies if the application runs in different geographical locations :: Explicitly define and enforce a single time zone standard (e.g., UTC) for all stored `due_date` values and comparisons.
- [MAJOR] Phản biện · Critical User Journeys :: The "Given" state for Tác vụ A assumes that the system can reliably determine if a task is overdue based solely on comparing today's date to the stored `due_date`, ignoring potential time components (e.g., due at 10 PM vs. checked at 8 AM) :: Clarify whether the comparison should be strictly by day or include time components, and ensure the logic handles tasks due *today* but not yet passed.
- [MAJOR] Phản biện · Technical Plan :: The plan does not specify how `due_date` will be handled for tasks that are currently incomplete but have no assigned date (i.e., when the user skips entering a date) :: Define a clear default state or validation rule for missing `due_date` values to prevent runtime errors in `overdue_tasks()`.
- [MAJOR] Phản biện · Risks :: The risk of "Thiếu định dạng ngày thống nhất" is stated but not mitigated by any concrete technical requirement (e.g., using Pydantic models, enforcing ISO 8601 parsing at the boundary) :: Mandate a single data serialization format (e.g., ISO 8601 string) and enforce strict deserialization/validation upon loading or saving task data.
- [MAJOR] Phản biện · Technical Plan :: The plan does not address how the system will handle date changes or time zone shifts in the future, potentially breaking the current comparison logic :: Document a strategy for versioning the date handling logic to accommodate future requirements (e.g., Daylight Saving Time adjustments).
- [NIT] Cảnh · Open Questions :: Việc đặt câu hỏi về "mức độ nổi bật" khi công việc quá hạn là một quyết định thiết kế UI/UX chứ không phải vấn đề kỹ thuật cốt lõi; nó làm loãng trọng tâm của tài liệu. :: Loại bỏ các câu hỏi liên quan đến giao diện người dùng (UI) và chỉ tập trung vào tính toàn vẹn dữ liệu và logic nghiệp vụ.
- [NIT] Hiểu · Overview/Technical Plan :: The term "item" is used repeatedly to refer to a task dictionary (`dict`) but is never formally defined as the structure or type of data it represents within `todo.py`. :: Define `item` (e.g., `Dict[str, Any]`) in the document for clarity.
- [NIT] Phản biện · Open Questions :: The question regarding "Hiển thị quá hạn" suggests multiple potential UI solutions (red label vs. pop-up) without defining which one is technically feasible or preferred, leading to scope creep :: Select a single, primary method for displaying overdue status and document the necessary front-end/back-end hooks required for that display.
- [NIT] Phản biện · Technical Plan :: The plan mentions modifying existing functions like `add()` but does not specify if these modifications will require updating all calling code paths across the application (e.g., command line interface, API calls) :: Provide an impact analysis detailing every function or module that must be updated to accommodate the new required parameters (`due_date`).

## What each lens said

### Cảnh

```text
[BLOCKER] Technical Plan :: "Phải sử dụng thư viện `datetime` của Python... để so sánh ngày hiện tại với giá trị `due_date`." :: Cần định nghĩa rõ ràng và bắt buộc kiểu dữ liệu cho `due_date` là đối tượng `datetime.date` (hoặc `datetime.datetime`) ngay từ bước thiết kế, thay vì chỉ đề cập đến việc sử dụng thư viện này.
[MAJOR] Goals / Technical Plan :: Việc tập trung vào các chi tiết hiển thị như "gắn nhãn đỏ [OVERDUE]" và "thông báo pop-up" là lo lắng về giao diện người dùng (UI) chứ không phải logic nghiệp vụ cốt lõi; chúng nên được tách ra khỏi thiết kế cấu trúc dữ liệu. :: Giới hạn phạm vi của tài liệu này chỉ ở việc cập nhật mô hình dữ liệu (`todo.py`) và các hàm truy vấn, loại bỏ mọi đề cập đến cách thức hiển thị trực quan.
[MAJOR] Technical Plan / Cấu trúc dữ liệu :: Việc sửa lớp `TodoList` để mỗi mục từ điển (`item`) phải bao gồm khóa `"due_date"` làm cho cấu trúc dữ liệu trở nên cứng nhắc và khó bảo trì; điều này vi phạm nguyên tắc đóng gói (encapsulation). :: Thay vì thay đổi trực tiếp các dictionary bên trong, hãy xem xét việc tạo một lớp `Task` đơn giản chỉ chứa ID, title, due_date, done để quản lý trạng thái của từng mục.
[NIT] Open Questions :: Việc đặt câu hỏi về "mức độ nổi bật" khi công việc quá hạn là một quyết định thiết kế UI/UX chứ không phải vấn đề kỹ thuật cốt lõi; nó làm loãng trọng tâm của tài liệu. :: Loại bỏ các câu hỏi liên quan đến giao diện người dùng (UI) và chỉ tập trung vào tính toàn vẹn dữ liệu và logic nghiệp vụ.
```

### Kiểm

```text
[BLOCKER] Detailed Implementation Plan :: The tasks are described conceptually ("Sửa `add(title)`: Cho phép nhận tham số ngày hết hạn...") but do not specify the required API changes, data types (e.g., should `due_date` be a string or datetime object?), or error handling for missing/invalid dates :: Specify the exact signature change for `__init__` and `add()` including expected input types and how date parsing errors must be handled.
[MAJOR] Detailed Implementation Plan :: The steps are not concrete enough to write code (e.g., "Cập nhật cấu trúc lớp..." or "Sửa `add(title)`:") :: For every task, provide the precise modification required in the file, such as "Add field `due_date: Optional[datetime]` to `TodoList.__init__` signature" and "Update `add()` signature to accept `due_date: Optional[datetime]`".
[MAJOR] Verification Plan :: The plan provides a general test command but lacks concrete tests for edge cases (e.g., what if the due date is invalid, or what if no tasks are overdue?) :: Add specific unit tests within the verification plan that cover failure modes and boundary conditions related to `due_date` handling.
```

### Hiểu

```text
[BLOCKER] Technical Plan :: When loading existing tasks into `TodoList`, how should the system handle items that do not currently contain the `"due_date"` key? :: The process must define a default value or skip validation for missing keys to prevent runtime errors.
[BLOCKER] Open Questions :: The document presents two conflicting options for `due_date` format (ISO 8601 string vs. Python Date object), which fundamentally determines the comparison logic in `overdue_tasks()`. :: A definitive choice must be made and documented before implementation begins.
[MAJOR] Technical Plan :: When modifying `add()` to accept a date, the document does not specify how missing or invalid date inputs (e.g., null, malformed string) should be handled by the function. :: The method needs explicit validation logic for the optional date parameter.
[NIT] Overview/Technical Plan :: The term "item" is used repeatedly to refer to a task dictionary (`dict`) but is never formally defined as the structure or type of data it represents within `todo.py`. :: Define `item` (e.g., `Dict[str, Any]`) in the document for clarity.
```

### Phản biện

```text
[MAJOR] Alternatives Considered :: The rejection of "Tạo một lớp Task riêng biệt" relies on subjective assessment ("tăng lượng code cần viết và các file bị ảnh hưởng") rather than concrete technical limitations or measurable costs, making the decision potentially flawed :: Re-evaluate the trade-off between OOP purity/testability (Task class) and implementation complexity for this specific scope.
[MAJOR] Technical Plan :: The plan assumes that simply adding a `due_date` key to the dictionary structure (`item`) will guarantee data integrity and correct type handling across all existing code paths, which is unlikely without explicit schema validation or migration logic :: Implement robust input validation and type coercion (e.g., ensuring `due_date` is always a Python `datetime` object) when adding or loading tasks.
[MAJOR] Technical Plan :: The plan mentions using the `datetime` library for comparison but fails to address how time zones will be handled, leading to potential discrepancies if the application runs in different geographical locations :: Explicitly define and enforce a single time zone standard (e.g., UTC) for all stored `due_date` values and comparisons.
[MAJOR] Critical User Journeys :: The "Given" state for Tác vụ A assumes that the system can reliably determine if a task is overdue based solely on comparing today's date to the stored `due_date`, ignoring potential time components (e.g., due at 10 PM vs. checked at 8 AM) :: Clarify whether the comparison should be strictly by day or include time components, and ensure the logic handles tasks due *today* but not yet passed.
[MAJOR] Technical Plan :: The plan does not specify how `due_date` will be handled for tasks that are currently incomplete but have no assigned date (i.e., when the user skips entering a date) :: Define a clear default state or validation rule for missing `due_date` values to prevent runtime errors in `overdue_tasks()`.
[MAJOR] Risks :: The risk of "Thiếu định dạng ngày thống nhất" is stated but not mitigated by any concrete technical requirement (e.g., using Pydantic models, enforcing ISO 8601 parsing at the boundary) :: Mandate a single data serialization format (e.g., ISO 8601 string) and enforce strict deserialization/validation upon loading or saving task data.
[MAJOR] Technical Plan :: The plan does not address how the system will handle date changes or time zone shifts in the future, potentially breaking the current comparison logic :: Document a strategy for versioning the date handling logic to accommodate future requirements (e.g., Daylight Saving Time adjustments).
[NIT] Open Questions :: The question regarding "Hiển thị quá hạn" suggests multiple potential UI solutions (red label vs. pop-up) without defining which one is technically feasible or preferred, leading to scope creep :: Select a single, primary method for displaying overdue status and document the necessary front-end/back-end hooks required for that display.
[NIT] Technical Plan :: The plan mentions modifying existing functions like `add()` but does not specify if these modifications will require updating all calling code paths across the application (e.g., command line interface, API calls) :: Provide an impact analysis detailing every function or module that must be updated to accommodate the new required parameters (`due_date`).
```
