# Spec review — docs/designs/261027-them-han-chot-cho-cong-viec.md

version 420ba9f6 · mode design · 2026-10-27T22:59:22+07:00

| Lens | Status |
|---|---|
| Cảnh | ok |
| Kiểm | ok |
| Hiểu | ok |
| Phản biện | ok |

## Findings (5 blocker, 15 major, 3 nit)

- [BLOCKER] Cảnh · Technical Plan :: The requirement to handle all time comparisons using UTC without specifying the exact Python standard library mechanism for creating timezone-aware datetime objects introduces a high risk of time zone bugs. :: Must specify precisely how `datetime.date.today()` and input parsing must be performed (e.g., explicitly defining the use of `datetime.timezone.utc` or similar constructs) to ensure all date objects are consistently aware UTC timestamps, not naive local times.
- [BLOCKER] Kiểm · Verification Plan :: The plan requires testing edge cases (e.g., item overdue today) but does not provide concrete test code or a specific, runnable command for these tests :: Add explicit unit test methods within `test*.py` and update the project commands to run them.
- [BLOCKER] Hiểu · Goals :: Validation đầu vào nghiêm ngặt cho ngày tháng :: Cần định nghĩa rõ ràng các quy tắc validation: Định dạng chấp nhận được là gì? Xử lý trường hợp `due_date` bị thiếu hoặc không phải kiểu ngày tháng như thế nào (raise exception hay trả về lỗi)?
- [BLOCKER] Phản biện · Open Questions :: "Hàm `pending()` nên trả về TẤT CẢ các công việc chưa hoàn thành..." :: Việc quyết định này là một giả định hành vi quan trọng; cần xác nhận với người dùng cuối hoặc cung cấp bằng chứng tại sao việc bao gồm mục quá hạn trong danh sách pending lại phù hợp.
- [BLOCKER] Phản biện · Risks :: "Persistence/Serialization: Cần xác định cách lưu trữ và tải các đối tượng `TaskItem` (bao gồm các giá trị UTC)" :: Đây là rủi ro kiến trúc lớn nhất; cần cung cấp chi tiết về cơ chế serialization (ví dụ: JSON, database type) để đảm bảo tính toàn vẹn của dữ liệu ngày tháng UTC.
- [MAJOR] Kiểm (machine) · ### Verification Plan :: no command to run :: name the build/test commands in backticks
- [MAJOR] Cảnh · Critical User Journeys / 2. Lấy danh sách pending (cần làm) :: The definition that `pending()` must return *all* incomplete items, including overdue ones, conflicts with the goal of helping users identify missed deadlines. :: Re-evaluate if `pending()` should only return non-overdue tasks, or if a separate method/view is needed to list all incomplete items while highlighting the overdue subset.
- [MAJOR] Cảnh · Detailed Implementation Plan / Sửa hàm add(self, title, due_date=None) :: The proposed change modifies the signature of `add()` from `add(self, title)` to include an optional `due_date` parameter. :: Explicitly document how this breaking change affects existing usage and ensure that the implementation handles the absence of a `due_date` gracefully without requiring mandatory input or causing runtime errors for backward compatibility.
- [MAJOR] Kiểm · Detailed Implementation Plan :: The required data shape/format for date inputs and storage (e.g., string format, datetime object) is not specified in relation to validation or class definition :: Define the exact structure of `TaskItem` fields and the expected input type/format for dates used by `add()` and stored within `TaskItem`.
- [MAJOR] Kiểm · Detailed Implementation Plan :: The tasks are described conceptually ("Sửa hàm...", "Thêm/sửa hàm...") rather than providing concrete, actionable code modifications (e.g., "In `todo.py`, modify method `add` to include X validation")... :: Provide precise instructions for modifying the signature and body of all methods in `todo.py`.
- [MAJOR] Hiểu · Technical Plan :: Việc chuẩn hóa mọi hoạt động ngày tháng về UTC :: Cần chỉ định rõ ràng hàm/thư viện cụ thể sẽ được sử dụng để lấy "ngày hôm nay" theo múi giờ UTC, vì việc này phụ thuộc vào ngôn ngữ lập trình và môi trường runtime.
- [MAJOR] Hiểu · Critical User Journeys (1) :: Giả định về ngày hết hạn hợp lệ :: Cần cung cấp logic hoặc quy tắc cho việc xác thực rằng `due_date` không phải là giá trị trong quá khứ, thay vì chỉ giả định điều này xảy ra khi người dùng nhập liệu.
- [MAJOR] Hiểu · Detailed Implementation Plan / Tasks (1) :: Định nghĩa lớp TaskItem :: Mặc dù các thuộc tính được liệt kê (title, due_date, done), cần xác định rõ ràng kiểu dữ liệu và cấu trúc của `TaskItem` (ví dụ: `due_date` phải là đối tượng datetime UTC hay chuỗi ngày tháng?).
- [MAJOR] Hiểu · Critical User Journeys (3) :: Điều kiện so sánh quá hạn (`due_date <= today`) :: Cần làm rõ xem việc so sánh này chỉ xét đến Ngày (Date component) hay bao gồm cả Giờ/Phút/Giây. Nếu nó phải là "ngày hôm nay", thì cần đảm bảo rằng thời gian được sử dụng cho `today` là 00:00:00 UTC để tránh lỗi biên giới múi giờ.
- [MAJOR] Phản biện · Goals :: "Validation đầu vào nghiêm ngặt" cho giá trị ngày tháng :: Cần mô tả chi tiết các quy tắc validation (ví dụ: định dạng chấp nhận, phạm vi ngày hợp lệ) và cách xử lý khi validation thất bại (ném exception hay trả về lỗi cụ thể).
- [MAJOR] Phản biện · Critical User Journeys 3 :: "ngày đến hạn phải nhỏ hơn hoặc bằng ngày hôm nay" (trong `overdue()`): :: Cần làm rõ logic so sánh ngày tháng: liệu nó chỉ so sánh ngày (date part) hay bao gồm cả thời gian (time component), đặc biệt khi sử dụng UTC.
- [MAJOR] Phản biện · Technical Plan :: "tái định nghĩa logic của `pending()` và `overdue()`" :: Không có mô tả về cách thức tái định nghĩa này sẽ được thực hiện trong mã nguồn, chỉ là một tuyên bố ý định.
- [MAJOR] Phản biện · Alternatives Considered :: "Phương án này được chọn vì nó tách rời mô hình dữ liệu khỏi logic quản lý (TodoList), tăng tính tái sử dụng và bảo trì." :: Việc tuyên bố lợi ích kiến trúc ("tăng tính tái sử dụng") cần được hỗ trợ bằng ví dụ cụ thể về nơi mà sự tách biệt này sẽ giúp giảm thiểu công sức hoặc cải thiện khả năng mở rộng.
- [MAJOR] Phản biện · Goals :: "Lọc Overdue: Bổ sung phương thức mới (`overdue()`) để trả về danh sách các mục thỏa mãn điều kiện: 1) chưa hoàn thành, và 2) ngày hôm nay (theo múi giờ UTC tiêu chuẩn) lớn hơn hoặc bằng `due_date`." :: Không có cơ chế xử lý khi hệ thống không thể xác định được "ngày hôm nay" (ví dụ: môi trường test bị cô lập thời gian).
- [MAJOR] Phản biện · Technical Plan :: "Các thay đổi chính bao gồm việc chuẩn hóa mọi hoạt động ngày tháng về UTC, thêm validation đầu vào cho các hàm thao tác với deadline, và tái định nghĩa logic của `pending()` và `overdue()`." :: Thiếu xác định rõ ràng về nguồn gốc dữ liệu thời gian (ví dụ: API call hay input người dùng) để đảm bảo tất cả đều được xử lý qua lớp chuẩn hóa UTC.
- [NIT] Hiểu · Open Questions :: Scope của pending() :: Cần xác nhận liệu việc sắp xếp các mục trong hàm `pending()` có nên ưu tiên hiển thị các mục quá hạn ở vị trí nổi bật (ví dụ: đầu danh sách) hay chỉ cần sắp xếp theo ID tăng dần như đã nêu.
- [NIT] Phản biện · Overview :: "Việc mở rộng hệ thống quản lý danh sách việc cần làm (`TodoList`) để cho phép gán ngày hết hạn (deadline) cho mỗi công việc." :: Nên nêu rõ liệu deadline có thể được thiết lập là null/không bắt buộc hay không.
- [NIT] Phản biện · Goals :: "Cho phép người dùng thiết lập hoặc cập nhật hạn chót khi thêm/sửa công việc, đồng thời phải thực hiện Validation đầu vào nghiêm ngặt cho giá trị ngày tháng." :: Cần làm rõ liệu validation có cần kiểm tra xem `due_date` có nằm trong tương lai hay không (ngăn chặn người dùng thiết lập deadline quá xa).

## What each lens said

### Cảnh

```text
[BLOCKER] Technical Plan :: The requirement to handle all time comparisons using UTC without specifying the exact Python standard library mechanism for creating timezone-aware datetime objects introduces a high risk of time zone bugs. :: Must specify precisely how `datetime.date.today()` and input parsing must be performed (e.g., explicitly defining the use of `datetime.timezone.utc` or similar constructs) to ensure all date objects are consistently aware UTC timestamps, not naive local times.
[MAJOR] Critical User Journeys / 2. Lấy danh sách pending (cần làm) :: The definition that `pending()` must return *all* incomplete items, including overdue ones, conflicts with the goal of helping users identify missed deadlines. :: Re-evaluate if `pending()` should only return non-overdue tasks, or if a separate method/view is needed to list all incomplete items while highlighting the overdue subset.
[MAJOR] Detailed Implementation Plan / Sửa hàm add(self, title, due_date=None) :: The proposed change modifies the signature of `add()` from `add(self, title)` to include an optional `due_date` parameter. :: Explicitly document how this breaking change affects existing usage and ensure that the implementation handles the absence of a `due_date` gracefully without requiring mandatory input or causing runtime errors for backward compatibility.
```

### Kiểm

```text
[BLOCKER] Verification Plan :: The plan requires testing edge cases (e.g., item overdue today) but does not provide concrete test code or a specific, runnable command for these tests :: Add explicit unit test methods within `test*.py` and update the project commands to run them.
[MAJOR] Detailed Implementation Plan :: The required data shape/format for date inputs and storage (e.g., string format, datetime object) is not specified in relation to validation or class definition :: Define the exact structure of `TaskItem` fields and the expected input type/format for dates used by `add()` and stored within `TaskItem`.
[MAJOR] Detailed Implementation Plan :: The tasks are described conceptually ("Sửa hàm...", "Thêm/sửa hàm...") rather than providing concrete, actionable code modifications (e.g., "In `todo.py`, modify method `add` to include X validation")... :: Provide precise instructions for modifying the signature and body of all methods in `todo.py`.
```

### Hiểu

```text
[BLOCKER] Goals :: Validation đầu vào nghiêm ngặt cho ngày tháng :: Cần định nghĩa rõ ràng các quy tắc validation: Định dạng chấp nhận được là gì? Xử lý trường hợp `due_date` bị thiếu hoặc không phải kiểu ngày tháng như thế nào (raise exception hay trả về lỗi)?
[MAJOR] Technical Plan :: Việc chuẩn hóa mọi hoạt động ngày tháng về UTC :: Cần chỉ định rõ ràng hàm/thư viện cụ thể sẽ được sử dụng để lấy "ngày hôm nay" theo múi giờ UTC, vì việc này phụ thuộc vào ngôn ngữ lập trình và môi trường runtime.
[MAJOR] Critical User Journeys (1) :: Giả định về ngày hết hạn hợp lệ :: Cần cung cấp logic hoặc quy tắc cho việc xác thực rằng `due_date` không phải là giá trị trong quá khứ, thay vì chỉ giả định điều này xảy ra khi người dùng nhập liệu.
[MAJOR] Detailed Implementation Plan / Tasks (1) :: Định nghĩa lớp TaskItem :: Mặc dù các thuộc tính được liệt kê (title, due_date, done), cần xác định rõ ràng kiểu dữ liệu và cấu trúc của `TaskItem` (ví dụ: `due_date` phải là đối tượng datetime UTC hay chuỗi ngày tháng?).
[MAJOR] Critical User Journeys (3) :: Điều kiện so sánh quá hạn (`due_date <= today`) :: Cần làm rõ xem việc so sánh này chỉ xét đến Ngày (Date component) hay bao gồm cả Giờ/Phút/Giây. Nếu nó phải là "ngày hôm nay", thì cần đảm bảo rằng thời gian được sử dụng cho `today` là 00:00:00 UTC để tránh lỗi biên giới múi giờ.
[NIT] Open Questions :: Scope của pending() :: Cần xác nhận liệu việc sắp xếp các mục trong hàm `pending()` có nên ưu tiên hiển thị các mục quá hạn ở vị trí nổi bật (ví dụ: đầu danh sách) hay chỉ cần sắp xếp theo ID tăng dần như đã nêu.
```

### Phản biện

```text
[MAJOR] Goals :: "Validation đầu vào nghiêm ngặt" cho giá trị ngày tháng :: Cần mô tả chi tiết các quy tắc validation (ví dụ: định dạng chấp nhận, phạm vi ngày hợp lệ) và cách xử lý khi validation thất bại (ném exception hay trả về lỗi cụ thể).
[BLOCKER] Open Questions :: "Hàm `pending()` nên trả về TẤT CẢ các công việc chưa hoàn thành..." :: Việc quyết định này là một giả định hành vi quan trọng; cần xác nhận với người dùng cuối hoặc cung cấp bằng chứng tại sao việc bao gồm mục quá hạn trong danh sách pending lại phù hợp.
[MAJOR] Critical User Journeys 3 :: "ngày đến hạn phải nhỏ hơn hoặc bằng ngày hôm nay" (trong `overdue()`): :: Cần làm rõ logic so sánh ngày tháng: liệu nó chỉ so sánh ngày (date part) hay bao gồm cả thời gian (time component), đặc biệt khi sử dụng UTC.
[MAJOR] Technical Plan :: "tái định nghĩa logic của `pending()` và `overdue()`" :: Không có mô tả về cách thức tái định nghĩa này sẽ được thực hiện trong mã nguồn, chỉ là một tuyên bố ý định.
[BLOCKER] Risks :: "Persistence/Serialization: Cần xác định cách lưu trữ và tải các đối tượng `TaskItem` (bao gồm các giá trị UTC)" :: Đây là rủi ro kiến trúc lớn nhất; cần cung cấp chi tiết về cơ chế serialization (ví dụ: JSON, database type) để đảm bảo tính toàn vẹn của dữ liệu ngày tháng UTC.
[MAJOR] Alternatives Considered :: "Phương án này được chọn vì nó tách rời mô hình dữ liệu khỏi logic quản lý (TodoList), tăng tính tái sử dụng và bảo trì." :: Việc tuyên bố lợi ích kiến trúc ("tăng tính tái sử dụng") cần được hỗ trợ bằng ví dụ cụ thể về nơi mà sự tách biệt này sẽ giúp giảm thiểu công sức hoặc cải thiện khả năng mở rộng.
[MAJOR] Goals :: "Lọc Overdue: Bổ sung phương thức mới (`overdue()`) để trả về danh sách các mục thỏa mãn điều kiện: 1) chưa hoàn thành, và 2) ngày hôm nay (theo múi giờ UTC tiêu chuẩn) lớn hơn hoặc bằng `due_date`." :: Không có cơ chế xử lý khi hệ thống không thể xác định được "ngày hôm nay" (ví dụ: môi trường test bị cô lập thời gian).
[NIT] Overview :: "Việc mở rộng hệ thống quản lý danh sách việc cần làm (`TodoList`) để cho phép gán ngày hết hạn (deadline) cho mỗi công việc." :: Nên nêu rõ liệu deadline có thể được thiết lập là null/không bắt buộc hay không.
[MAJOR] Technical Plan :: "Các thay đổi chính bao gồm việc chuẩn hóa mọi hoạt động ngày tháng về UTC, thêm validation đầu vào cho các hàm thao tác với deadline, và tái định nghĩa logic của `pending()` và `overdue()`." :: Thiếu xác định rõ ràng về nguồn gốc dữ liệu thời gian (ví dụ: API call hay input người dùng) để đảm bảo tất cả đều được xử lý qua lớp chuẩn hóa UTC.
[NIT] Goals :: "Cho phép người dùng thiết lập hoặc cập nhật hạn chót khi thêm/sửa công việc, đồng thời phải thực hiện **Validation đầu vào nghiêm ngặt** cho giá trị ngày tháng." :: Cần làm rõ liệu validation có cần kiểm tra xem `due_date` có nằm trong tương lai hay không (ngăn chặn người dùng thiết lập deadline quá xa).
```
