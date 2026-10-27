# Spec review — docs/designs/261027-them-han-chot-cho-cong-viec.md

version 420ba9f6 · mode design · 2026-10-27T22:54:49+07:00

| Lens | Status |
|---|---|
| Cảnh | ok |
| Kiểm | ok |
| Hiểu | ok |
| Phản biện | ok |

## Findings (4 blocker, 13 major, 2 nit)

- [BLOCKER] Cảnh · Technical Plan/Detailed Implementation Plan :: The complex logic for date validation, UTC handling, and comparison is scattered across proposed methods (`add`, `overdue()`) rather than being encapsulated in a dedicated utility or service component. This violates the Single Responsibility Principle and makes the core business logic of `TodoList` brittle and difficult to test independently of time zone arithmetic. :: Extract all date/time validation and comparison logic into a standalone utility module (e.g., `utils.py` or within the `TaskItem` constructor) that accepts standard Python datetime objects, ensuring `TodoList` only calls high-level methods like `is_overdue(date)` rather than performing raw comparisons.
- [BLOCKER] Hiểu · Technical Plan :: The structure/attributes of `TaskItem` are mentioned but not defined. :: Define the required fields, types, and methods for the `TaskItem` class/struct.
- [BLOCKER] Phản biện · Goals :: Phương thức `overdue()` để trả về danh sách các mục thỏa mãn điều kiện: 1) chưa hoàn thành, và 2) ngày hôm nay (theo múi giờ UTC tiêu chuẩn) lớn hơn hoặc bằng `due_date` :: The definition of "ngày hôm nay" must be explicitly defined as the start of the day in UTC (00:00:00.000Z) to prevent partial-day overruns, and the comparison logic needs clarification on whether it should use `< today` or `<= today`.
- [BLOCKER] Phản biện · Alternatives Considered :: The rejection of "Giữ nguyên Dictionary" :: While OOP principles are cited, a technical reason (e.g., difficulty implementing transactional updates or complex validation logic) should be provided instead of relying solely on abstract design patterns.
- [MAJOR] Kiểm (machine) · ### Verification Plan :: no command to run :: name the build/test commands in backticks
- [MAJOR] Cảnh · Goals/Technical Plan :: The document treats `TaskItem` as a mere internal class structure modification, but it is fundamentally a new component with defined responsibilities (data storage + validation). Its interface and dependencies must be formally defined before implementation begins to ensure testability and reusability. :: Define the `TaskItem` component's public interface (methods/properties) in a dedicated section, detailing its required inputs (e.g., `title: str`, `due_date: Optional[datetime]`) and outputs, treating it as an independent module dependency rather than just a class definition within `todo.py`.
- [MAJOR] Kiểm · Detailed Implementation Plan :: "Sửa hàm `add(self, title, due_date=None)`: Thêm validation đầu vào cho ngày tháng và khởi tạo `TaskItem`." :: Specify the required date format (e.g., ISO 8601 string, datetime object) and the expected error handling/exception type for invalid dates.
- [MAJOR] Kiểm · Detailed Implementation Plan :: "Sửa hàm `add(self, title, due_date=None)`: Thêm validation đầu vào cho ngày tháng và khởi tạo `TaskItem`." :: Define the exact API signature or data shape of the `due_date` input when it is provided (e.g., must it be a string? If so, what format?).
- [MAJOR] Kiểm · Detailed Implementation Plan :: "Verification Plan" :: The plan mentions writing test cases for time zones and edge cases but fails to provide a concrete command in backticks that runs these specific new tests.
- [MAJOR] Hiểu · Open Questions / Critical User Journeys 1 :: It is unclear how a user-provided date (which might be local time) is converted into the mandatory UTC format. :: Specify the expected input format for `due_date` and the conversion logic from local time to UTC storage.
- [MAJOR] Hiểu · Goals / Detailed Implementation Plan :: "Validation đầu vào nghiêm ngặt" (strict validation) is mentioned but not detailed. :: Provide concrete rules for date validation, including acceptable formats, required range checks, and error handling mechanisms.
- [MAJOR] Hiểu · Critical User Journeys 3 / Detailed Implementation Plan :: The definition of "ngày hôm nay" (`today`) for overdue calculation is ambiguous regarding time zone and precision (start of day vs. current moment). :: Explicitly define the reference point for `overdue()`: should it check against the start of the UTC day, or the exact moment of execution?
- [MAJOR] Phản biện · Goals :: "Validation đầu vào nghiêm ngặt" cho giá trị ngày tháng :: Specify exactly what constitutes "nghiêm ngặt validation," including acceptable formats (ISO 8601?), required time zone handling during input parsing, and specific error messages/behaviors upon failure.
- [MAJOR] Phản biện · Critical User Journeys :: "ngày hôm nay (theo múi giờ UTC tiêu chuẩn)" :: The exact mechanism for determining "today" must be specified (e.g., `new Date().toISOString().split('T')[0]` in JavaScript, or a specific library call) to ensure consistency across different execution environments and prevent time zone drift issues.
- [MAJOR] Phản biện · Critical User Journeys :: Lấy công việc quá hạn (Overdue) -> "ngày đến hạn phải nhỏ hơn hoặc bằng ngày hôm nay" :: The comparison logic is ambiguous; if `due_date` is 2024-10-27T12:00:00Z and today is 2024-10-28, does it count as overdue? The requirement must be strictly defined (e.g., due date < current UTC day).
- [MAJOR] Phản biện · Technical Plan :: Persistence/Serialization :: How are the `TaskItem` objects serialized when they contain complex UTC datetime values? A concrete plan for handling time zone information during persistence and retrieval is required to prevent data loss or misinterpretation upon loading.
- [MAJOR] Phản biện · Risks :: Time Zone Handling :: If the system interacts with user input from various time zones, what mechanism is in place to convert that local input into the required UTC format *before* storage and comparison? This conversion process must be explicitly defined.
- [NIT] Cảnh · Open Questions :: The decision regarding whether `pending()` should include overdue items is critical for user experience but remains open. This ambiguity affects the core contract of the method. :: Explicitly document the invariant that `pending()` must return all non-completed tasks, regardless of their deadline status, and ensure this requirement is reflected in the test plan to prevent future scope creep or misinterpretation.
- [NIT] Phản biện · Open Questions :: Scope của `pending()` :: The document should clarify if `pending()` needs to filter out items that are technically "overdue" but perhaps marked as archived or otherwise excluded from active view, even if they aren't marked `done`.

## What each lens said

### Cảnh

```text
[BLOCKER] Technical Plan/Detailed Implementation Plan :: The complex logic for date validation, UTC handling, and comparison is scattered across proposed methods (`add`, `overdue()`) rather than being encapsulated in a dedicated utility or service component. This violates the Single Responsibility Principle and makes the core business logic of `TodoList` brittle and difficult to test independently of time zone arithmetic. :: Extract all date/time validation and comparison logic into a standalone utility module (e.g., `utils.py` or within the `TaskItem` constructor) that accepts standard Python datetime objects, ensuring `TodoList` only calls high-level methods like `is_overdue(date)` rather than performing raw comparisons.
[MAJOR] Goals/Technical Plan :: The document treats `TaskItem` as a mere internal class structure modification, but it is fundamentally a new component with defined responsibilities (data storage + validation). Its interface and dependencies must be formally defined before implementation begins to ensure testability and reusability. :: Define the `TaskItem` component's public interface (methods/properties) in a dedicated section, detailing its required inputs (e.g., `title: str`, `due_date: Optional[datetime]`) and outputs, treating it as an independent module dependency rather than just a class definition within `todo.py`.
[NIT] Open Questions :: The decision regarding whether `pending()` should include overdue items is critical for user experience but remains open. This ambiguity affects the core contract of the method. :: Explicitly document the invariant that `pending()` must return all non-completed tasks, regardless of their deadline status, and ensure this requirement is reflected in the test plan to prevent future scope creep or misinterpretation.
```

### Kiểm

```text
[MAJOR] Detailed Implementation Plan :: "Sửa hàm `add(self, title, due_date=None)`: Thêm validation đầu vào cho ngày tháng và khởi tạo `TaskItem`." :: Specify the required date format (e.g., ISO 8601 string, datetime object) and the expected error handling/exception type for invalid dates.
[MAJOR] Detailed Implementation Plan :: "Sửa hàm `add(self, title, due_date=None)`: Thêm validation đầu vào cho ngày tháng và khởi tạo `TaskItem`." :: Define the exact API signature or data shape of the `due_date` input when it is provided (e.g., must it be a string? If so, what format?).
[MAJOR] Detailed Implementation Plan :: "Verification Plan" :: The plan mentions writing test cases for time zones and edge cases but fails to provide a concrete command in backticks that runs these specific new tests.
```

### Hiểu

```text
[BLOCKER] Technical Plan :: The structure/attributes of `TaskItem` are mentioned but not defined. :: Define the required fields, types, and methods for the `TaskItem` class/struct.
[MAJOR] Open Questions / Critical User Journeys 1 :: It is unclear how a user-provided date (which might be local time) is converted into the mandatory UTC format. :: Specify the expected input format for `due_date` and the conversion logic from local time to UTC storage.
[MAJOR] Goals / Detailed Implementation Plan :: "Validation đầu vào nghiêm ngặt" (strict validation) is mentioned but not detailed. :: Provide concrete rules for date validation, including acceptable formats, required range checks, and error handling mechanisms.
[MAJOR] Critical User Journeys 3 / Detailed Implementation Plan :: The definition of "ngày hôm nay" (`today`) for overdue calculation is ambiguous regarding time zone and precision (start of day vs. current moment). :: Explicitly define the reference point for `overdue()`: should it check against the start of the UTC day, or the exact moment of execution?
```

### Phản biện

```text
[MAJOR] Goals :: "Validation đầu vào nghiêm ngặt" cho giá trị ngày tháng :: Specify exactly what constitutes "nghiêm ngặt validation," including acceptable formats (ISO 8601?), required time zone handling during input parsing, and specific error messages/behaviors upon failure.
[BLOCKER] Goals :: Phương thức `overdue()` để trả về danh sách các mục thỏa mãn điều kiện: 1) chưa hoàn thành, và 2) ngày hôm nay (theo múi giờ UTC tiêu chuẩn) lớn hơn hoặc bằng `due_date` :: The definition of "ngày hôm nay" must be explicitly defined as the start of the day in UTC (00:00:00.000Z) to prevent partial-day overruns, and the comparison logic needs clarification on whether it should use `< today` or `<= today`.
[MAJOR] Critical User Journeys :: "ngày hôm nay (theo múi giờ UTC tiêu chuẩn)" :: The exact mechanism for determining "today" must be specified (e.g., `new Date().toISOString().split('T')[0]` in JavaScript, or a specific library call) to ensure consistency across different execution environments and prevent time zone drift issues.
[MAJOR] Critical User Journeys :: Lấy công việc quá hạn (Overdue) -> "ngày đến hạn phải nhỏ hơn hoặc bằng ngày hôm nay" :: The comparison logic is ambiguous; if `due_date` is 2024-10-27T12:00:00Z and today is 2024-10-28, does it count as overdue? The requirement must be strictly defined (e.g., due date < current UTC day).
[MAJOR] Technical Plan :: Persistence/Serialization :: How are the `TaskItem` objects serialized when they contain complex UTC datetime values? A concrete plan for handling time zone information during persistence and retrieval is required to prevent data loss or misinterpretation upon loading.
[BLOCKER] Alternatives Considered :: The rejection of "Giữ nguyên Dictionary" :: While OOP principles are cited, a technical reason (e.g., difficulty implementing transactional updates or complex validation logic) should be provided instead of relying solely on abstract design patterns.
[MAJOR] Risks :: Time Zone Handling :: If the system interacts with user input from various time zones, what mechanism is in place to convert that local input into the required UTC format *before* storage and comparison? This conversion process must be explicitly defined.
[NIT] Open Questions :: Scope của `pending()` :: The document should clarify if `pending()` needs to filter out items that are technically "overdue" but perhaps marked as archived or otherwise excluded from active view, even if they aren't marked `done`.
```
