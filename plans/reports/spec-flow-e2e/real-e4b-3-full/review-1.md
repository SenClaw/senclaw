# Spec review — docs/designs/261027-them-han-chot-cho-cong-viec.md

version b3b90981 · mode design · 2026-10-27T23:30:10+07:00

| Lens | Status |
|---|---|
| Cảnh | ok |
| Kiểm | ok |
| Hiểu | ok |
| Phản biện | ok |

## Findings (3 blocker, 14 major, 3 nit)

- [BLOCKER] Kiểm ·  :: In "Sửa đổi signature và logic của hàm add()", the data type and expected input format for `due_date` are not specified, making it impossible to write the function signature or validation logic.
- [BLOCKER] Hiểu · Technical Plan :: Khi cập nhật cấu trúc dữ liệu để thêm trường `"due_date"`, tài liệu không mô tả cách xử lý hoặc di chuyển dữ liệu cho các mục công việc hiện có chưa có thuộc tính này. :: Cần bổ sung bước về cơ chế khởi tạo (initialization) hoặc migration data để đảm bảo các bản ghi cũ vẫn hoạt động khi được đọc vào hệ thống mới.
- [BLOCKER] Phản biện · Goals :: Cập nhật cấu trúc dữ liệu :: Không có giới hạn nào được nêu về số lượng công việc tối đa mà hệ thống phải xử lý (ví dụ: 1000 items), điều này ảnh hưởng đến hiệu suất của hàm `get_overdue()`.
- [MAJOR] Cảnh · Technical Plan :: Lỗi so sánh ngày trong `get_overdue` logic :: Cần làm rõ điều kiện quá hạn: liệu công việc phải hết hạn *trước* ngày mục tiêu (`< target_date`) hay bao gồm cả ngày đó (`<= target_date`)? Hiện tại, nếu sử dụng `<=`, một công việc có due date bằng target_date vẫn được coi là quá hạn, điều này cần xác nhận với người dùng.
- [MAJOR] Cảnh · Detailed Implementation Plan :: Sử dụng `datetime.date.today()` bên trong phương thức `get_overdue` tạo ra sự phụ thuộc vào thời gian hệ thống (side effect) và làm giảm khả năng kiểm thử đơn vị (unit testability) :: Nên truyền ngày hiện tại (`target_date=None`) từ lớp gọi hoặc sử dụng Dependency Injection để cung cấp ngày, thay vì gọi trực tiếp `datetime.date.today()` bên trong phương thức.
- [MAJOR] Kiểm ·  :: In "Sửa đổi signature và logic của hàm add()", the plan does not specify if the existing `add()` method should be overloaded or replaced when accepting an optional `due_date`, which is necessary for modifying its API.
- [MAJOR] Kiểm ·  :: In "Verification Plan", while new test cases are requested, there is no concrete command provided to run these newly written overdue unit tests.
- [MAJOR] Hiểu · Technical Plan :: Logic so sánh ngày tháng trong `get_overdue` (`item["due_date"] <= target_date`) không làm rõ liệu một công việc có hạn chót *chính xác* bằng `target_date` (ngày hôm nay) có được tính là quá hạn hay không. :: Cần định nghĩa rõ ràng: "Quá hạn" có bao gồm cả ngày hết hạn của ngày gọi hàm (`target_date`) hay chỉ những ngày đã trôi qua trước đó?
- [MAJOR] Hiểu · Detailed Implementation Plan :: Việc thay đổi signature của hàm `add(self, title)` thành chấp nhận tham số tùy chọn `due_date` sẽ gây ra lỗi tương thích (breaking change) cho bất kỳ đoạn mã nào đang gọi hàm này mà không truyền thêm đối số. :: Cần cung cấp hướng dẫn về việc cập nhật tất cả các điểm gọi (call sites) đến hàm `add()` để tránh lỗi runtime.
- [MAJOR] Phản biện · Technical Plan :: Lọc các mục thỏa mãn điều kiện: `item["done"] == False` VÀ (nếu `item["due_date"]` tồn tại, thì `item["due_date"] <= target_date`) :: Cần làm rõ logic so sánh ngày quá hạn. Nếu một công việc không có `due_date`, nó có được coi là "không bao giờ quá hạn" hay cần xử lý riêng?
- [MAJOR] Phản biện · Technical Plan :: Phương thức `get_overdue(target_date: datetime.date = None)` :: Khi `target_date` là `None`, sử dụng ngày hiện tại (`datetime.date.today()`). Điều này tạo ra một phụ thuộc vào thời gian hệ thống (system clock) và có thể gây khó khăn cho việc kiểm thử đơn vị (unit testing). Nên yêu cầu người gọi cung cấp rõ ràng ngày mục tiêu thay vì mặc định là `None`.
- [MAJOR] Phản biện · Goals :: Triển khai chức năng Overdue Check :: Logic so sánh ngày (`item["due_date"] <= target_date`) có vẻ sai. Để một công việc bị quá hạn, nó phải được hoàn thành *sau* ngày hết hạn. Điều kiện đúng phải là: `item["due_date"] < target_date`.
- [MAJOR] Phản biện · Technical Plan :: Lọc các mục thỏa mãn điều kiện :: Logic so sánh ngày cần sửa lại để xác định chính xác trạng thái "quá hạn". Một công việc quá hạn khi nó chưa hoàn thành VÀ (ngày hết hạn đã trôi qua). Điều này yêu cầu `item["due_date"]` phải tồn tại và phải nhỏ hơn ngày mục tiêu.
- [MAJOR] Phản biện · Risks :: Lỗi so sánh ngày :: Tài liệu chỉ đề cập đến rủi ro chung mà không cung cấp một quy tắc kinh doanh rõ ràng về việc xử lý các trường hợp biên (edge cases) như: 1) Công việc hết hạn vào ngày hôm nay và chưa hoàn thành; 2) Công việc không có `due_date`.
- [MAJOR] Phản biện · Alternatives Considered :: Sử dụng trường "priority" thay vì "due_date" :: : Mặc dù tài liệu tuyên bố rằng việc sử dụng `datetime.date` là đủ, nhưng nó bỏ qua khả năng người dùng muốn kết hợp cả mức độ ưu tiên và ngày hết hạn (ví dụ: P1 *và* quá hạn).
- [MAJOR] Phản biện · Alternatives Considered :: Lưu trữ ngày dưới dạng chuỗi ISO 8601 :: Tài liệu bác bỏ phương án này vì "làm tăng độ phức tạp và dễ xảy ra lỗi type casting", nhưng không đề xuất một giải pháp thay thế nào để xử lý việc nhập dữ liệu từ các nguồn bên ngoài mà vẫn đảm bảo tính toàn vẹn của kiểu dữ liệu.
- [MAJOR] Phản biện · Technical Plan :: Mô hình hóa thay đổi (Data Model) :: Tài liệu không xác định cách thức xử lý trường hợp `due_date` là `None` trong logic so sánh quá hạn, dẫn đến nguy cơ lỗi khi truy cập thuộc tính trên giá trị null/none.
- [NIT] Cảnh · Technical Plan / Data Model :: Việc cập nhật cấu trúc dữ liệu chỉ được mô tả là "thêm trường" mà không xác định rõ cách các hàm hiện tại (`add`, `done`) sẽ xử lý việc thiếu giá trị ngày hết hạn (None) khi đọc hoặc ghi dữ liệu vào item dictionary :: Cần bổ sung logic mặc định hóa (defaulting) trong `add()` để đảm bảo rằng nếu người dùng không cung cấp `due_date`, nó được lưu trữ là `None` một cách rõ ràng và nhất quán.
- [NIT] Hiểu · Technical Plan :: Logic lọc trong `get_overdue` cần xử lý rõ ràng trường hợp `item["due_date"]` là `None`. Hiện tại, điều kiện chỉ kiểm tra sự tồn tại (`if item["due_date"]` tồn tại) nhưng không xác định cách thức so sánh nếu nó bị bỏ qua. :: Cần làm rõ rằng việc thiếu `due_date` (là `None`) sẽ luôn khiến công việc đó được coi là chưa quá hạn, bất kể trạng thái hoàn thành của nó.
- [NIT] Phản biện · Technical Plan :: Phương thức `get_overdue(target_date: datetime.date = None)` :: Nếu người dùng gọi hàm với `target_date=None`, tài liệu nói nó sẽ sử dụng ngày hiện tại, nhưng không nêu rõ nếu việc này xảy ra trong môi trường kiểm thử (test environment) thì kết quả có thể bị thay đổi theo thời gian thực hay không.

## What each lens said

### Cảnh

```text
[MAJOR] Technical Plan :: Lỗi so sánh ngày trong `get_overdue` logic :: Cần làm rõ điều kiện quá hạn: liệu công việc phải hết hạn *trước* ngày mục tiêu (`< target_date`) hay bao gồm cả ngày đó (`<= target_date`)? Hiện tại, nếu sử dụng `<=`, một công việc có due date bằng target_date vẫn được coi là quá hạn, điều này cần xác nhận với người dùng.
[MAJOR] Detailed Implementation Plan :: Sử dụng `datetime.date.today()` bên trong phương thức `get_overdue` tạo ra sự phụ thuộc vào thời gian hệ thống (side effect) và làm giảm khả năng kiểm thử đơn vị (unit testability) :: Nên truyền ngày hiện tại (`target_date=None`) từ lớp gọi hoặc sử dụng Dependency Injection để cung cấp ngày, thay vì gọi trực tiếp `datetime.date.today()` bên trong phương thức.
[NIT] Technical Plan / Data Model :: Việc cập nhật cấu trúc dữ liệu chỉ được mô tả là "thêm trường" mà không xác định rõ cách các hàm hiện tại (`add`, `done`) sẽ xử lý việc thiếu giá trị ngày hết hạn (None) khi đọc hoặc ghi dữ liệu vào item dictionary :: Cần bổ sung logic mặc định hóa (defaulting) trong `add()` để đảm bảo rằng nếu người dùng không cung cấp `due_date`, nó được lưu trữ là `None` một cách rõ ràng và nhất quán.
```

### Kiểm

```text
[BLOCKER] In "Sửa đổi signature và logic của hàm add()", the data type and expected input format for `due_date` are not specified, making it impossible to write the function signature or validation logic.
[MAJOR] In "Sửa đổi signature và logic của hàm add()", the plan does not specify if the existing `add()` method should be overloaded or replaced when accepting an optional `due_date`, which is necessary for modifying its API.
[MAJOR] In "Verification Plan", while new test cases are requested, there is no concrete command provided to run these newly written overdue unit tests.
```

### Hiểu

```text
[BLOCKER] Technical Plan :: Khi cập nhật cấu trúc dữ liệu để thêm trường `"due_date"`, tài liệu không mô tả cách xử lý hoặc di chuyển dữ liệu cho các mục công việc hiện có chưa có thuộc tính này. :: Cần bổ sung bước về cơ chế khởi tạo (initialization) hoặc migration data để đảm bảo các bản ghi cũ vẫn hoạt động khi được đọc vào hệ thống mới.
[MAJOR] Technical Plan :: Logic so sánh ngày tháng trong `get_overdue` (`item["due_date"] <= target_date`) không làm rõ liệu một công việc có hạn chót *chính xác* bằng `target_date` (ngày hôm nay) có được tính là quá hạn hay không. :: Cần định nghĩa rõ ràng: "Quá hạn" có bao gồm cả ngày hết hạn của ngày gọi hàm (`target_date`) hay chỉ những ngày đã trôi qua trước đó?
[MAJOR] Detailed Implementation Plan :: Việc thay đổi signature của hàm `add(self, title)` thành chấp nhận tham số tùy chọn `due_date` sẽ gây ra lỗi tương thích (breaking change) cho bất kỳ đoạn mã nào đang gọi hàm này mà không truyền thêm đối số. :: Cần cung cấp hướng dẫn về việc cập nhật tất cả các điểm gọi (call sites) đến hàm `add()` để tránh lỗi runtime.
[NIT] Technical Plan :: Logic lọc trong `get_overdue` cần xử lý rõ ràng trường hợp `item["due_date"]` là `None`. Hiện tại, điều kiện chỉ kiểm tra sự tồn tại (`if item["due_date"]` tồn tại) nhưng không xác định cách thức so sánh nếu nó bị bỏ qua. :: Cần làm rõ rằng việc thiếu `due_date` (là `None`) sẽ luôn khiến công việc đó được coi là chưa quá hạn, bất kể trạng thái hoàn thành của nó.
```

### Phản biện

```text
[MAJOR] Technical Plan :: Lọc các mục thỏa mãn điều kiện: `item["done"] == False` VÀ (nếu `item["due_date"]` tồn tại, thì `item["due_date"] <= target_date`) :: Cần làm rõ logic so sánh ngày quá hạn. Nếu một công việc không có `due_date`, nó có được coi là "không bao giờ quá hạn" hay cần xử lý riêng?
[MAJOR] Technical Plan :: Phương thức `get_overdue(target_date: datetime.date = None)` :: Khi `target_date` là `None`, sử dụng ngày hiện tại (`datetime.date.today()`). Điều này tạo ra một phụ thuộc vào thời gian hệ thống (system clock) và có thể gây khó khăn cho việc kiểm thử đơn vị (unit testing). Nên yêu cầu người gọi cung cấp rõ ràng ngày mục tiêu thay vì mặc định là `None`.
[MAJOR] Goals :: Triển khai chức năng Overdue Check :: Logic so sánh ngày (`item["due_date"] <= target_date`) có vẻ sai. Để một công việc bị quá hạn, nó phải được hoàn thành *sau* ngày hết hạn. Điều kiện đúng phải là: `item["due_date"] < target_date`.
[MAJOR] Technical Plan :: Lọc các mục thỏa mãn điều kiện :: Logic so sánh ngày cần sửa lại để xác định chính xác trạng thái "quá hạn". Một công việc quá hạn khi nó chưa hoàn thành VÀ (ngày hết hạn đã trôi qua). Điều này yêu cầu `item["due_date"]` phải tồn tại và phải nhỏ hơn ngày mục tiêu.
[MAJOR] Risks :: Lỗi so sánh ngày :: Tài liệu chỉ đề cập đến rủi ro chung mà không cung cấp một quy tắc kinh doanh rõ ràng về việc xử lý các trường hợp biên (edge cases) như: 1) Công việc hết hạn vào ngày hôm nay và chưa hoàn thành; 2) Công việc không có `due_date`.
[MAJOR] Alternatives Considered :: Sử dụng trường "priority" thay vì "due_date"::: Mặc dù tài liệu tuyên bố rằng việc sử dụng `datetime.date` là đủ, nhưng nó bỏ qua khả năng người dùng muốn kết hợp cả mức độ ưu tiên và ngày hết hạn (ví dụ: P1 *và* quá hạn).
[MAJOR] Alternatives Considered :: Lưu trữ ngày dưới dạng chuỗi ISO 8601:: Tài liệu bác bỏ phương án này vì "làm tăng độ phức tạp và dễ xảy ra lỗi type casting", nhưng không đề xuất một giải pháp thay thế nào để xử lý việc nhập dữ liệu từ các nguồn bên ngoài mà vẫn đảm bảo tính toàn vẹn của kiểu dữ liệu.
[MAJOR] Technical Plan :: Mô hình hóa thay đổi (Data Model) :: Tài liệu không xác định cách thức xử lý trường hợp `due_date` là `None` trong logic so sánh quá hạn, dẫn đến nguy cơ lỗi khi truy cập thuộc tính trên giá trị null/none.
[BLOCKER] Goals :: Cập nhật cấu trúc dữ liệu :: Không có giới hạn nào được nêu về số lượng công việc tối đa mà hệ thống phải xử lý (ví dụ: 1000 items), điều này ảnh hưởng đến hiệu suất của hàm `get_overdue()`.
[NIT] Technical Plan :: Phương thức `get_overdue(target_date: datetime.date = None)` :: Nếu người dùng gọi hàm với `target_date=None`, tài liệu nói nó sẽ sử dụng ngày hiện tại, nhưng không nêu rõ nếu việc này xảy ra trong môi trường kiểm thử (test environment) thì kết quả có thể bị thay đổi theo thời gian thực hay không.
```
