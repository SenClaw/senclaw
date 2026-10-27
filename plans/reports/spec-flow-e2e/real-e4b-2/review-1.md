# Spec review — docs/designs/261027-them-han-chot-cho-cong-viec.md

version 3d7e6dc2 · mode design · 2026-10-27T22:51:04+07:00

| Lens | Status |
|---|---|
| Cảnh | ok |
| Kiểm | ok |
| Hiểu | ok |
| Phản biện | ok |

## Findings (3 blocker, 15 major, 1 nit)

- [BLOCKER] Kiểm · Detailed Implementation Plan :: The plan describes high-level tasks but does not provide concrete code changes (e.g., "In `TodoItem.__init__`, add `self.due_date = due_date`") required to write the code from this text :: Specify exact file and line modifications for all structural changes.
- [BLOCKER] Hiểu · Overview/Technical Plan :: problem :: fix
- [BLOCKER] Phản biện · Critical User Journeys / 1. Thêm công việc với deadline :: The process for handling invalid date inputs (e.g., future dates provided as past, or malformed strings) is not defined in the validation step; this could lead to runtime errors or silent data corruption. :: Implement and document robust input validation that raises specific exceptions for unacceptable `due_date` values before item creation.
- [MAJOR] Cảnh · Goals / Data structure :: The design modifies the fundamental data model of a 'task item' by adding `due_date`, but fails to define this structure (e.g., as a dedicated class or named tuple) separate from the `TodoList` logic. :: Define an explicit `TaskItem` component/interface that encapsulates title, status, and due_date, allowing it to be passed independently of the `TodoList`.
- [MAJOR] Cảnh · Critical User Journeys / 2. Lấy danh sách pending :: The requirement states `pending()` should return non-done items, but then adds ambiguity: "(và có thể lọc bỏ các công việc quá hạn nếu logic yêu cầu)". This suggests the function's scope is unclear (should it filter or not?). :: Clarify if `pending()` must *always* include overdue tasks, or if its definition changes based on filtering requirements.
- [MAJOR] Cảnh · Risks / Time Zone Handling :: Relying solely on `datetime.date.today()` without specifying the required timezone context introduces potential bugs when deployed across different environments. :: Explicitly mandate that all date comparisons use a standardized, defined timezone (e.g., UTC) and adjust input handling accordingly to ensure consistency.
- [MAJOR] Kiểm · Tasks :: The signature and return type of the new `overdue()` function are not defined :: Add a concrete definition, e.g., `def overdue(self) -> list[TodoItem]:`.
- [MAJOR] Kiểm · Verification Plan :: The plan lists a general test command but does not provide specific unit tests or assertions for edge cases (e.g., item due today, item completed and overdue) :: Provide at least one example of an edge case test method to be added to `test_todo.py`.
- [MAJOR] Hiểu · Critical User Journeys / 2. Lấy danh sách pending (cần làm) :: problem :: fix
- [MAJOR] Hiểu · Risks/Detailed Implementation Plan :: problem :: fix
- [MAJOR] Hiểu · Detailed Implementation Plan / Tasks :: problem :: fix
- [MAJOR] Phản biện · Technical Plan :: The document fails to specify how `datetime.date` objects will be serialized and deserialized when persisting the `TodoList` state (e.g., saving to a database or file). :: Must define the persistence layer's data type handling for dates, including necessary conversion logic.
- [MAJOR] Phản biện · Critical User Journeys / 3. Lấy công việc quá hạn (Overdue) :: The requirement states that `overdue()` returns items sorted by ID, but this sorting criteria is arbitrary and may not be useful to the user; furthermore, if the list is large, performance implications of filtering and then sorting are unaddressed. :: Specify the required sort order for overdue tasks (e.g., oldest due date first) and provide complexity analysis or constraints on list size/load.
- [MAJOR] Phản biện · Critical User Journeys / 2. Lấy danh sách pending (cần làm) :: The description mentions that `pending()` "có thể lọc bỏ các công việc quá hạn nếu logic yêu cầu" (may filter out overdue tasks if required), but this conditional filtering behavior is not defined, leading to ambiguity in the method's contract. :: Explicitly define whether `pending()` should include or exclude overdue items by default, and document how this exclusion/inclusion can be controlled.
- [MAJOR] Phản biện · Risks / Time Zone Handling :: The assumption that "hệ thống đang hoạt động theo múi giờ cục bộ" (the system operates in the local timezone) is a dangerous assumption for any distributed application; relying on `datetime.date.today()` without explicit UTC handling introduces significant risk of time drift and incorrect deadline calculations. :: All date operations must use UTC timestamps internally, converting to local time only for display purposes, and this requirement must be enforced across all components.
- [MAJOR] Phản biện · Alternatives Considered :: The rejection of State Machine (B) is based solely on "quá phức tạp" (too complex), which is not a concrete technical reason; it fails to analyze if the complexity adds necessary robustness or maintainability that simple state checks cannot provide. :: Provide a detailed comparison showing why the added complexity of a State Machine would be less costly than the potential maintenance burden of handling edge cases with simple boolean flags.
- [MAJOR] Phản biện · Technical Plan :: The document does not specify any limits on the number of items in the `TodoList` (size limit) or the expected rate of additions/updates (load limit), making performance scaling impossible to estimate. :: State explicit constraints for maximum list size and required throughput (e.g., must handle 10,000 items with < 50ms retrieval time).
- [MAJOR] Phản biện · Critical User Journeys / 3. Lấy công việc quá hạn (Overdue) :: The definition of "quá hạn" (`due_date < today`) is insufficient; it does not account for the possibility that a task might be due *today* but hasn't been marked complete, which should also be highlighted to the user. :: Update the overdue condition to include items where `due_date <= today` and are incomplete.
- [NIT] Phản biện · Overview :: The term "TodoList" suggests a simple list structure, but the complexity introduced by deadlines implies potential relational dependencies (e.g., linking tasks to projects), which should be clarified in the scope definition. :: Clarify if the system is strictly limited to an array/list of items or if it will eventually require indexing based on due dates for performance optimization.

## What each lens said

### Cảnh

```text
[MAJOR] Goals / Data structure :: The design modifies the fundamental data model of a 'task item' by adding `due_date`, but fails to define this structure (e.g., as a dedicated class or named tuple) separate from the `TodoList` logic. :: Define an explicit `TaskItem` component/interface that encapsulates title, status, and due_date, allowing it to be passed independently of the `TodoList`.
[MAJOR] Critical User Journeys / 2. Lấy danh sách pending :: The requirement states `pending()` should return non-done items, but then adds ambiguity: "(và có thể lọc bỏ các công việc quá hạn nếu logic yêu cầu)". This suggests the function's scope is unclear (should it filter or not?). :: Clarify if `pending()` must *always* include overdue tasks, or if its definition changes based on filtering requirements.
[MAJOR] Risks / Time Zone Handling :: Relying solely on `datetime.date.today()` without specifying the required timezone context introduces potential bugs when deployed across different environments. :: Explicitly mandate that all date comparisons use a standardized, defined timezone (e.g., UTC) and adjust input handling accordingly to ensure consistency.
```

### Kiểm

```text
[BLOCKER] Detailed Implementation Plan :: The plan describes high-level tasks but does not provide concrete code changes (e.g., "In `TodoItem.__init__`, add `self.due_date = due_date`") required to write the code from this text :: Specify exact file and line modifications for all structural changes.
[MAJOR] Tasks :: The signature and return type of the new `overdue()` function are not defined :: Add a concrete definition, e.g., `def overdue(self) -> list[TodoItem]:`.
[MAJOR] Verification Plan :: The plan lists a general test command but does not provide specific unit tests or assertions for edge cases (e.g., item due today, item completed and overdue) :: Provide at least one example of an edge case test method to be added to `test_todo.py`.
```

### Hiểu

```text
[BLOCKER] Overview/Technical Plan :: problem :: fix
[MAJOR] Critical User Journeys / 2. Lấy danh sách pending (cần làm) :: problem :: fix
[MAJOR] Risks/Detailed Implementation Plan :: problem :: fix
[MAJOR] Detailed Implementation Plan / Tasks :: problem :: fix
```

### Phản biện

```text
[MAJOR] Technical Plan :: The document fails to specify how `datetime.date` objects will be serialized and deserialized when persisting the `TodoList` state (e.g., saving to a database or file). :: Must define the persistence layer's data type handling for dates, including necessary conversion logic.
[MAJOR] Critical User Journeys / 3. Lấy công việc quá hạn (Overdue) :: The requirement states that `overdue()` returns items sorted by ID, but this sorting criteria is arbitrary and may not be useful to the user; furthermore, if the list is large, performance implications of filtering and then sorting are unaddressed. :: Specify the required sort order for overdue tasks (e.g., oldest due date first) and provide complexity analysis or constraints on list size/load.
[MAJOR] Critical User Journeys / 2. Lấy danh sách pending (cần làm) :: The description mentions that `pending()` "có thể lọc bỏ các công việc quá hạn nếu logic yêu cầu" (may filter out overdue tasks if required), but this conditional filtering behavior is not defined, leading to ambiguity in the method's contract. :: Explicitly define whether `pending()` should include or exclude overdue items by default, and document how this exclusion/inclusion can be controlled.
[MAJOR] Risks / Time Zone Handling :: The assumption that "hệ thống đang hoạt động theo múi giờ cục bộ" (the system operates in the local timezone) is a dangerous assumption for any distributed application; relying on `datetime.date.today()` without explicit UTC handling introduces significant risk of time drift and incorrect deadline calculations. :: All date operations must use UTC timestamps internally, converting to local time only for display purposes, and this requirement must be enforced across all components.
[MAJOR] Alternatives Considered :: The rejection of State Machine (B) is based solely on "quá phức tạp" (too complex), which is not a concrete technical reason; it fails to analyze if the complexity adds necessary robustness or maintainability that simple state checks cannot provide. :: Provide a detailed comparison showing why the added complexity of a State Machine would be less costly than the potential maintenance burden of handling edge cases with simple boolean flags.
[MAJOR] Technical Plan :: The document does not specify any limits on the number of items in the `TodoList` (size limit) or the expected rate of additions/updates (load limit), making performance scaling impossible to estimate. :: State explicit constraints for maximum list size and required throughput (e.g., must handle 10,000 items with < 50ms retrieval time).
[MAJOR] Critical User Journeys / 3. Lấy công việc quá hạn (Overdue) :: The definition of "quá hạn" (`due_date < today`) is insufficient; it does not account for the possibility that a task might be due *today* but hasn't been marked complete, which should also be highlighted to the user. :: Update the overdue condition to include items where `due_date <= today` and are incomplete.
[BLOCKER] Critical User Journeys / 1. Thêm công việc với deadline :: The process for handling invalid date inputs (e.g., future dates provided as past, or malformed strings) is not defined in the validation step; this could lead to runtime errors or silent data corruption. :: Implement and document robust input validation that raises specific exceptions for unacceptable `due_date` values before item creation.
[NIT] Overview :: The term "TodoList" suggests a simple list structure, but the complexity introduced by deadlines implies potential relational dependencies (e.g., linking tasks to projects), which should be clarified in the scope definition. :: Clarify if the system is strictly limited to an array/list of items or if it will eventually require indexing based on due dates for performance optimization.
```
