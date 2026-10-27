# Spec review — docs/designs/261027-them-han-chot-cho-cong-viec.md

version b3b90981 · mode design · 2026-10-27T23:37:50+07:00

| Lens | Status |
|---|---|
| Cảnh | ok |
| Kiểm | ok |
| Hiểu | ok |
| Phản biện | ok |

## Findings (4 blocker, 13 major, 5 nit)

- [BLOCKER] Kiểm · todo.py :: problem :: The plan requires modifying `add()` to accept an optional `due_date`, but does not specify the expected data type or format for this date (e.g., string, datetime object), making implementation impossible without a design decision.
- [BLOCKER] Kiểm · todo.py :: problem :: The logic for determining "overdue" in `get_overdue(self, target_date)` is undefined; it is unclear if the comparison should be against the system clock, or solely against the provided `target_date`, and how to handle cases where no `target_date` is passed.
- [BLOCKER] Kiểm · Verification Plan :: problem :: The plan requires adding 2-3 new test cases for overdue scenarios but does not provide the code for these tests, nor a concrete command that includes them in the testing suite.
- [BLOCKER] Hiểu · Technical Plan :: The condition for an item being overdue requires explicit handling when `item["due_date"]` is `None`. Currently, the logic states: "VÀ (nếu `item["due_date"]` tồn tại, thì `item["due_date"] <= target_date`)". If `item["due_date"]` is `None`, attempting to compare it with a date will cause a runtime error or unpredictable behavior. The implementation must define whether items without due dates should be excluded from the overdue list entirely, or if they should trigger an error/warning.
- [MAJOR] Cảnh · Technical Plan :: `get_overdue(target_date=None)` sử dụng ngày hiện tại (`datetime.date.today()`) làm mặc định :: Phương thức này khiến việc kiểm thử đơn vị (unit testing) trở nên không xác định (non-deterministic), vì kết quả sẽ thay đổi theo thời gian chạy test; nên yêu cầu người dùng luôn truyền vào một `target_date` cụ thể khi gọi hàm, hoặc thiết kế lớp Test để mock ngày hệ thống.
- [MAJOR] Cảnh · Technical Plan :: Sửa đổi signature và logic của hàm `add()` trong `todo.py` để chấp nhận optional `due_date` :: Việc thay đổi chữ ký (signature) của một phương thức cốt lõi như `add()` để thêm thuộc tính thời gian làm tăng sự phụ thuộc (coupling) không cần thiết; nên xem xét việc sử dụng một đối tượng Task riêng biệt hoặc áp dụng mô hình Builder/Factory Pattern để tách logic tạo task khỏi lớp quản lý danh sách.
- [MAJOR] Cảnh · Technical Plan :: Cập nhật cấu trúc dữ liệu trong `todo.py` :: Thiết kế hiện tại đang thao tác trực tiếp với các dictionary nội bộ (item["due_date"]), thiếu một định nghĩa rõ ràng về Task object/interface; nên xác định và sử dụng một lớp `Task` riêng biệt để đảm bảo tính toàn vẹn của dữ liệu, thay vì chỉ là các dict.
- [MAJOR] Hiểu · Technical Plan :: The definition of "overdue" needs clarification regarding boundary conditions. The current logic `item["due_date"] <= target_date` includes equality (meaning a job due *today* is considered overdue). It must be explicitly stated whether the intended definition of "overdue" means the date has passed (`< target_date`) or if it includes the day itself (`<= target_date`).
- [MAJOR] Hiểu · Technical Plan :: The function `get_overdue(target_date: datetime.date = None)` needs a clear rule for when `target_date` is provided versus when it defaults to `datetime.date.today()`. If the user explicitly passes a date, that date must take precedence over any internal logic that might otherwise assume "current day."
- [MAJOR] Hiểu · Overview/Technical Plan :: The structure of the core data object (the "item") is not fully defined. While we know it needs `"due_date"`, we do not know if other required fields like `id` or `title` are mandatory, nor how they interact with the new optional field when initializing a list item.
- [MAJOR] Hiểu · Technical Plan :: The logic for filtering overdue items must explicitly define what happens to an item that is marked as completed (`item["done"] == True`). Although the goal states it should not be counted, this check needs to be placed first and robustly handled in the filter loop to prevent unnecessary date comparisons.
- [MAJOR] Phản biện · Technical Plan :: Lọc các mục thỏa mãn điều kiện: `item["done"] == False` VÀ (nếu `item["due_date"]` tồn tại, thì `item["due_date"] <= target_date`) :: Cần xác định rõ ràng logic so sánh ngày tháng khi `target_date` được truyền vào là một ngày trong tương lai. Nếu mục đích chỉ là tìm việc quá hạn *so với hiện tại*, nên loại bỏ tham số `target_date` và luôn sử dụng `datetime.date.today()` để tránh nhầm lẫn về phạm vi thời gian.
- [MAJOR] Phản biện · Technical Plan :: Phương thức `get_overdue(target_date: datetime.date = None)` :: Không có cơ chế kiểm tra kiểu dữ liệu (type checking) cho tham số `target_date`. Nếu người dùng truyền vào một giá trị không phải là `datetime.date` (ví dụ: chuỗi hoặc integer), chương trình sẽ gặp lỗi runtime thay vì xử lý lỗi an toàn.
- [MAJOR] Phản biện · Technical Plan :: Logic so sánh ngày tháng :: Điều kiện quá hạn chỉ kiểm tra xem `due_date <= target_date`. Cần xác định liệu việc một công việc hết hạn *vào* `target_date` có được coi là quá hạn hay không (ví dụ: nếu hôm nay là 2026-10-27 và hạn chót là 2026-10-27, nó đã quá hạn chưa?). Cần làm rõ ranh giới này.
- [MAJOR] Phản biện · Overview :: Không có cơ chế xử lý lỗi khi truy cập dữ liệu (ví dụ: nếu `item` không chứa key `"due_date"` hoặc `"done"`) :: Mặc dù tài liệu giả định cấu trúc dữ liệu, nhưng việc thiếu các khối try/except hoặc kiểm tra sự tồn tại của khóa (`key in item`) có thể dẫn đến lỗi KeyError khi xử lý danh sách công việc bị hỏng.
- [MAJOR] Phản biện · Risks :: Xung đột với Test hiện tại :: Việc chỉ giảm thiểu bằng cách "Bổ sung tests cho việc khởi tạo và thao tác với `None`" là không đủ; cần phải xem xét toàn bộ các test case cũ để đảm bảo rằng việc thêm trường mới này không làm thay đổi hành vi của bất kỳ hàm nào.
- [MAJOR] Phản biện · Technical Plan :: Tính toán ngày hiện tại :: Khi `target_date` là `None`, sử dụng `datetime.date.today()` có thể gây ra vấn đề về tính nhất quán nếu quá trình chạy (ví dụ: trong một test suite hoặc môi trường CI/CD) yêu cầu khả năng tái lập kết quả. Nên thay thế bằng việc chấp nhận ngày hiện tại như một tham số bắt buộc để đảm bảo kiểm thử được ổn định.
- [NIT] Cảnh · Technical Plan :: Sử dụng ngày hiện tại (`datetime.date.today()`) trong mô tả logic :: Nên làm rõ rằng nếu người dùng không cung cấp `target_date`, hệ thống sẽ sử dụng ngày chạy test/ngày thực thi để đảm bảo tính minh bạch của API.
- [NIT] Hiểu · Technical Plan :: The logic for `get_overdue` must specify how to handle items where `item["due_date"]` is present but invalid (e.g., a malformed date object that somehow bypassed validation).
- [NIT] Hiểu · Overview :: What constitutes the initial state of the `TodoList`? Since the document mentions modifying existing functions (`add()`, `done()`), it assumes the reader knows the current internal structure and class definition of `TodoList` in `todo.py`.
- [NIT] Phản biện · Technical Plan :: `item["due_date"] <= target_date` :: Nếu mục đích là tìm các công việc *đã trôi qua*, nên sử dụng toán tử so sánh nghiêm ngặt hơn (ví dụ: `<`) hoặc làm rõ liệu ngày hết hạn chính xác có được tính là quá hạn hay không.
- [NIT] Phản biện · Goals :: Bảo toàn tính toàn vẹn :: Cần nêu rõ rằng việc thêm trường `due_date` mới sẽ *không* ảnh hưởng đến các hàm hiện tại, thay vì chỉ nói "vẫn hoạt động bình thường", để tránh hiểu lầm về phạm vi kiểm thử.

## What each lens said

### Cảnh

```text
[MAJOR] Technical Plan :: `get_overdue(target_date=None)` sử dụng ngày hiện tại (`datetime.date.today()`) làm mặc định :: Phương thức này khiến việc kiểm thử đơn vị (unit testing) trở nên không xác định (non-deterministic), vì kết quả sẽ thay đổi theo thời gian chạy test; nên yêu cầu người dùng luôn truyền vào một `target_date` cụ thể khi gọi hàm, hoặc thiết kế lớp Test để mock ngày hệ thống.
[MAJOR] Technical Plan :: Sửa đổi signature và logic của hàm `add()` trong `todo.py` để chấp nhận optional `due_date` :: Việc thay đổi chữ ký (signature) của một phương thức cốt lõi như `add()` để thêm thuộc tính thời gian làm tăng sự phụ thuộc (coupling) không cần thiết; nên xem xét việc sử dụng một đối tượng Task riêng biệt hoặc áp dụng mô hình Builder/Factory Pattern để tách logic tạo task khỏi lớp quản lý danh sách.
[MAJOR] Technical Plan :: Cập nhật cấu trúc dữ liệu trong `todo.py` :: Thiết kế hiện tại đang thao tác trực tiếp với các dictionary nội bộ (item["due_date"]), thiếu một định nghĩa rõ ràng về Task object/interface; nên xác định và sử dụng một lớp `Task` riêng biệt để đảm bảo tính toàn vẹn của dữ liệu, thay vì chỉ là các dict.
[NIT] Technical Plan :: Sử dụng ngày hiện tại (`datetime.date.today()`) trong mô tả logic :: Nên làm rõ rằng nếu người dùng không cung cấp `target_date`, hệ thống sẽ sử dụng ngày chạy test/ngày thực thi để đảm bảo tính minh bạch của API.
```

### Kiểm

```text
[BLOCKER] todo.py :: problem :: The plan requires modifying `add()` to accept an optional `due_date`, but does not specify the expected data type or format for this date (e.g., string, datetime object), making implementation impossible without a design decision.
[BLOCKER] todo.py :: problem :: The logic for determining "overdue" in `get_overdue(self, target_date)` is undefined; it is unclear if the comparison should be against the system clock, or solely against the provided `target_date`, and how to handle cases where no `target_date` is passed.
[BLOCKER] Verification Plan :: problem :: The plan requires adding 2-3 new test cases for overdue scenarios but does not provide the code for these tests, nor a concrete command that includes them in the testing suite.
```

### Hiểu

```text
[BLOCKER] Technical Plan :: The condition for an item being overdue requires explicit handling when `item["due_date"]` is `None`. Currently, the logic states: "VÀ (nếu `item["due_date"]` tồn tại, thì `item["due_date"] <= target_date`)". If `item["due_date"]` is `None`, attempting to compare it with a date will cause a runtime error or unpredictable behavior. The implementation must define whether items without due dates should be excluded from the overdue list entirely, or if they should trigger an error/warning.
[MAJOR] Technical Plan :: The definition of "overdue" needs clarification regarding boundary conditions. The current logic `item["due_date"] <= target_date` includes equality (meaning a job due *today* is considered overdue). It must be explicitly stated whether the intended definition of "overdue" means the date has passed (`< target_date`) or if it includes the day itself (`<= target_date`).
[MAJOR] Technical Plan :: The function `get_overdue(target_date: datetime.date = None)` needs a clear rule for when `target_date` is provided versus when it defaults to `datetime.date.today()`. If the user explicitly passes a date, that date must take precedence over any internal logic that might otherwise assume "current day."
[MAJOR] Overview/Technical Plan :: The structure of the core data object (the "item") is not fully defined. While we know it needs `"due_date"`, we do not know if other required fields like `id` or `title` are mandatory, nor how they interact with the new optional field when initializing a list item.
[MAJOR] Technical Plan :: The logic for filtering overdue items must explicitly define what happens to an item that is marked as completed (`item["done"] == True`). Although the goal states it should not be counted, this check needs to be placed first and robustly handled in the filter loop to prevent unnecessary date comparisons.
[NIT] Technical Plan :: The logic for `get_overdue` must specify how to handle items where `item["due_date"]` is present but invalid (e.g., a malformed date object that somehow bypassed validation).
[NIT] Overview :: What constitutes the initial state of the `TodoList`? Since the document mentions modifying existing functions (`add()`, `done()`), it assumes the reader knows the current internal structure and class definition of `TodoList` in `todo.py`.
```

### Phản biện

```text
[MAJOR] Technical Plan :: Lọc các mục thỏa mãn điều kiện: `item["done"] == False` VÀ (nếu `item["due_date"]` tồn tại, thì `item["due_date"] <= target_date`) :: Cần xác định rõ ràng logic so sánh ngày tháng khi `target_date` được truyền vào là một ngày trong tương lai. Nếu mục đích chỉ là tìm việc quá hạn *so với hiện tại*, nên loại bỏ tham số `target_date` và luôn sử dụng `datetime.date.today()` để tránh nhầm lẫn về phạm vi thời gian.
[MAJOR] Technical Plan :: Phương thức `get_overdue(target_date: datetime.date = None)` :: Không có cơ chế kiểm tra kiểu dữ liệu (type checking) cho tham số `target_date`. Nếu người dùng truyền vào một giá trị không phải là `datetime.date` (ví dụ: chuỗi hoặc integer), chương trình sẽ gặp lỗi runtime thay vì xử lý lỗi an toàn.
[MAJOR] Technical Plan :: Logic so sánh ngày tháng :: Điều kiện quá hạn chỉ kiểm tra xem `due_date <= target_date`. Cần xác định liệu việc một công việc hết hạn *vào* `target_date` có được coi là quá hạn hay không (ví dụ: nếu hôm nay là 2026-10-27 và hạn chót là 2026-10-27, nó đã quá hạn chưa?). Cần làm rõ ranh giới này.
[MAJOR] Overview :: Không có cơ chế xử lý lỗi khi truy cập dữ liệu (ví dụ: nếu `item` không chứa key `"due_date"` hoặc `"done"`) :: Mặc dù tài liệu giả định cấu trúc dữ liệu, nhưng việc thiếu các khối try/except hoặc kiểm tra sự tồn tại của khóa (`key in item`) có thể dẫn đến lỗi KeyError khi xử lý danh sách công việc bị hỏng.
[MAJOR] Risks :: Xung đột với Test hiện tại :: Việc chỉ giảm thiểu bằng cách "Bổ sung tests cho việc khởi tạo và thao tác với `None`" là không đủ; cần phải xem xét toàn bộ các test case cũ để đảm bảo rằng việc thêm trường mới này không làm thay đổi hành vi của bất kỳ hàm nào.
[MAJOR] Technical Plan :: Tính toán ngày hiện tại :: Khi `target_date` là `None`, sử dụng `datetime.date.today()` có thể gây ra vấn đề về tính nhất quán nếu quá trình chạy (ví dụ: trong một test suite hoặc môi trường CI/CD) yêu cầu khả năng tái lập kết quả. Nên thay thế bằng việc chấp nhận ngày hiện tại như một tham số bắt buộc để đảm bảo kiểm thử được ổn định.
[NIT] Technical Plan :: `item["due_date"] <= target_date` :: Nếu mục đích là tìm các công việc *đã trôi qua*, nên sử dụng toán tử so sánh nghiêm ngặt hơn (ví dụ: `<`) hoặc làm rõ liệu ngày hết hạn chính xác có được tính là quá hạn hay không.
[NIT] Goals :: Bảo toàn tính toàn vẹn :: Cần nêu rõ rằng việc thêm trường `due_date` mới sẽ *không* ảnh hưởng đến các hàm hiện tại, thay vì chỉ nói "vẫn hoạt động bình thường", để tránh hiểu lầm về phạm vi kiểm thử.
```
