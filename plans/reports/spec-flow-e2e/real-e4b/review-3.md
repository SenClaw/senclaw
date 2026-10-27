# Spec review — docs/designs/261027-them-han-chot-cho-cong-viec.md

version 74ef7fda · mode design · 2026-10-27T22:16:00+07:00

| Lens | Status |
|---|---|
| Cảnh | ok |
| Kiểm | ok |
| Hiểu | ok |
| Phản biện | ok |

## Findings (4 blocker, 19 major, 3 nit)

- [BLOCKER] Cảnh · Open Questions :: Định dạng dữ liệu cho `due_date` nên là gì: chuỗi hay đối tượng Date Python? :: Phải chọn một định dạng ngày cố định và áp dụng nó nhất quán trong toàn bộ lớp `TodoList`, đồng thời đảm bảo rằng tất cả các hàm so sánh (như `overdue_tasks`) đều sử dụng cùng một kiểu dữ liệu để tránh lỗi logic nghiêm trọng.
- [BLOCKER] Kiểm · Detailed Implementation Plan :: The plan requires changing method signatures and data structures (`due_date` handling) but does not specify the concrete new signature for `add(title)` or how existing methods like `done()` and `pending()` must be updated to handle the new task structure.
- [BLOCKER] Hiểu · Open Questions :: Định dạng ngày cho `due_date` chưa được quyết định (string ISO 8601 hay đối tượng Date Python) và điều này ảnh hưởng trực tiếp đến cách so sánh thời gian trong hàm `overdue_tasks()`. :: Cần chọn một định dạng cố định và xác định rõ ràng logic so sánh: liệu việc so sánh chỉ cần dựa trên ngày (date component) hay phải bao gồm cả giờ/phút (datetime object).
- [BLOCKER] Phản biện · Technical Plan :: Phải sử dụng thư viện `datetime` của Python (hoặc tương đương) để so sánh ngày hiện tại với giá trị `due_date`. :: Không xác định rõ ràng việc đồng bộ hóa múi giờ; nếu ứng dụng chạy trên nhiều khu vực, việc so sánh ngày chỉ bằng `datetime.now()` sẽ dẫn đến sai lệch thời gian nghiêm trọng.
- [MAJOR] Kiểm (machine) · ### Tasks :: missing :: add it under ## Detailed Implementation Plan
- [MAJOR] Kiểm (machine) · ### Verification Plan :: missing :: add it under ## Detailed Implementation Plan
- [MAJOR] Kiểm (machine) · ### Verification Plan :: no command to run :: name the build/test commands in backticks
- [MAJOR] Cảnh · Technical Plan / Cấu trúc dữ liệu :: Thay vì chỉ yêu cầu "sửa lớp `TodoList` để mỗi mục từ điển (`item`) phải bao gồm khóa `"due_date"`", cần định nghĩa rõ ràng một giao diện (interface) hoặc schema cho cấu trúc của một item công việc, thay vì dựa vào việc sửa đổi các dictionary nội bộ.
- [MAJOR] Cảnh · Risks :: Việc lo lắng về "Hiệu suất truy vấn" và tối ưu hóa hàm `overdue_tasks` là quá mức cần thiết (premature optimization) khi ứng dụng đang hoạt động trong phạm vi bộ nhớ đơn giản; nên tập trung hoàn toàn vào việc triển khai logic chính xác trước khi xem xét hiệu năng.
- [MAJOR] Kiểm · Detailed Implementation Plan :: The scope of changes is incomplete; modifying the core data structure with a `due_date` field requires explicit instructions on how this impacts all related methods (e.g., `done(item_id)` or `pending()`) beyond just listing `__init__`, `add()`, and `overdue_tasks()`.
- [MAJOR] Kiểm · Detailed Implementation Plan :: The steps are not concrete enough; they use general directives ("Cập nhật...", "Sửa...") instead of specific instructions like "Add field X to struct Y in file Z" or "Change signature of method Z from A to B".
- [MAJOR] Kiểm · Verification Plan :: While a command is provided, the plan lacks definition for edge case tests (e.g., what happens if no tasks are overdue, or if `due_date` is invalid/missing).
- [MAJOR] Hiểu · Technical Plan / Tasks (- [ ] T2) :: Chữ ký hàm `add(title)` sẽ thay đổi để chấp nhận tham số ngày hết hạn, nhưng không có chữ ký cụ thể nào được cung cấp. :: Cần xác định rõ ràng chữ ký mới của hàm này (ví dụ: `add(self, title: str, due_date: Optional[datetime] = None)`) để các file khác biết cách gọi nó.
- [MAJOR] Hiểu · Technical Plan / Tasks (- [ ] T3) :: Logic so sánh "ngày hiện tại" với `due_date` cần được làm rõ về mặt thời gian. :: Cần xác định xem việc kiểm tra quá hạn chỉ dựa trên ngày (ví dụ: 2026-10-25 đã qua) hay phải tính đến cả múi giờ và thời điểm chạy chương trình, để tránh sai lệch khi tác vụ hết hạn vào cuối ngày.
- [MAJOR] Hiểu · Technical Plan / Cấu trúc dữ liệu :: Việc bổ sung trường `due_date` yêu cầu sửa đổi lớp `TodoList`, nhưng không có định nghĩa rõ ràng về schema đầy đủ của một mục (`item`) sau khi thêm trường này (ví dụ: các khóa bắt buộc và kiểu dữ liệu mặc định cho tất cả các trường). :: Cần cung cấp một ví dụ mẫu hoặc khai báo type hint/schema chi tiết cho cấu trúc `item` mới.
- [MAJOR] Phản biện · Open Questions :: Định dạng dữ liệu cho `due_date` nên là gì: chuỗi (string) theo định dạng ISO 8601 hay một đối tượng Date Python? :: Phải quyết định và ghi rõ ràng trong tài liệu; nếu chọn string, phải xác định cách parse an toàn để tránh lỗi so sánh ngày.
- [MAJOR] Phản biện · Technical Plan :: Cấu trúc dữ liệu (`todo.py`): Sửa lớp `TodoList` để mỗi mục từ điển (`item`) phải bao gồm khóa `"due_date"` (và có thể là một giá trị mặc định hoặc null/None). :: Không xác định rõ ràng cách xử lý trường hợp người dùng không cung cấp ngày hết hạn; cần quy định giá trị mặc định và logic cho các tác vụ không có ngày.
- [MAJOR] Phản biện · Technical Plan :: Cấu trúc dữ liệu (`todo.py`): Sửa lớp `TodoList` để mỗi mục từ điển (`item`) phải bao gồm khóa `"due_date"` (và có thể là một giá trị mặc định hoặc null/None). :: Việc sử dụng dictionary cho cấu trúc dữ liệu công việc làm tăng nguy cơ lỗi kiểu dữ liệu và thiếu tính toàn vẹn, nên xem xét chuyển sang lớp `Task` để đảm bảo các thuộc tính luôn tồn tại.
- [MAJOR] Phản biện · Alternatives Considered :: Tách logic thành một lớp `Task(id, title, due_date, done)` thay vì sử dụng dict. *Ưu điểm:* Cực kỳ sạch sẽ về mặt OOP, dễ kiểm thử đơn vị. *Nhược điểm:* Tăng lượng code cần viết và các file bị ảnh hưởng (ví dụ: cách khởi tạo list). :: Lý do từ chối lớp `Task` quá yếu; nên chấp nhận việc tăng độ phức tạp nhỏ để đảm bảo tính đúng đắn của mô hình dữ liệu, đặc biệt khi xử lý logic thời gian.
- [MAJOR] Phản biện · Alternatives Considered :: Nếu ứng dụng lớn hơn, nên chuyển sang SQLite hoặc tương tự. *Ưu điểm:* Khả năng mở rộng và tính toàn vẹn dữ liệu cao nhất. *Nhược điểm:* Quá mức cần thiết cho phạm vi hiện tại và làm tăng độ phức tạp của dự án. :: Không có đánh giá về chi phí/thời gian phát triển để chuyển sang SQLite, việc loại bỏ giải pháp cơ sở dữ liệu chỉ vì "phạm vi hiện tại" là một rủi ro lớn khi tính năng được sử dụng thực tế.
- [MAJOR] Phản biện · Risks :: Việc thêm bộ lọc theo ngày có thể làm chậm quá trình tải danh sách nếu số lượng tác vụ lớn, cần tối ưu hóa hàm `overdue_tasks`. :: Cần đưa ra giới hạn về kích thước dữ liệu (ví dụ: tối đa 1000 mục) hoặc một giải pháp cụ thể để đảm bảo hiệu suất truy vấn khi vượt qua ngưỡng đó.
- [MAJOR] Phản biện · Critical User Journeys :: Khi người dùng nhập tiêu đề và chọn một `due_date` cụ thể (ví dụ: 2026-11-15). *Then:* Tác vụ được lưu trữ thành công với trường `due_date` và hiển thị ngày này trong danh sách. :: Không nêu rõ cách xử lý khi người dùng nhập định dạng ngày không hợp lệ hoặc vượt quá phạm vi cho phép (ví dụ: năm 9999).
- [MAJOR] Phản biện · Critical User Journeys :: Hệ thống đã có một số tác vụ, trong đó có một tác vụ A hết hạn ngày 2026-10-25 nhưng chưa được đánh dấu hoàn thành. *When:* Người dùng truy vấn danh sách các tác vụ quá hạn (`overdue_tasks()`). *Then:* Hệ thống trả về Tác vụ A và hiển thị rõ ràng trạng thái "Quá hạn" bên cạnh ngày hết hạn. :: Không xác định cơ chế xử lý khi người dùng cố tình thay đổi ngày hết hạn trong quá khứ, liệu hệ thống có cho phép điều này không?
- [NIT] Cảnh · Detailed Implementation Plan / Tasks :: Cần làm rõ rằng hàm `add()` không chỉ nhận tham số ngày hết hạn mà còn phải thực hiện việc *xác thực* (validation) định dạng của ngày đó ngay tại lớp này, để đảm bảo tính toàn vẹn dữ liệu trước khi lưu trữ.
- [NIT] Hiểu · Technical Plan / Tasks (- [ ] T1) :: Khi sửa đổi `__init__`, cần xác nhận rõ ràng cách xử lý giá trị mặc định (default value) của trường `due_date` nếu người dùng không cung cấp nó, và liệu việc thiếu ngày có nên được coi là "không bao giờ hết hạn" hay phải có một giá trị placeholder cụ thể. :: Cần bổ sung logic hoặc ghi chú về hành vi khi `due_date` bị bỏ trống (None).
- [NIT] Phản biện · Overview :: Việc thêm tính năng này sẽ cải thiện khả năng lập kế hoạch và nhắc nhở của người dùng về các cam kết thời gian bằng cách bổ sung cơ chế lọc công việc dựa trên ngày hết hạn. :: Cần làm rõ rằng đây chỉ là *cơ chế lọc* chứ không phải là một giải pháp quản lý lịch trình toàn diện, để tránh kỳ vọng sai lầm từ người dùng.

## What each lens said

### Cảnh

```text
[BLOCKER] Open Questions :: Định dạng dữ liệu cho `due_date` nên là gì: chuỗi hay đối tượng Date Python? :: Phải chọn một định dạng ngày cố định và áp dụng nó nhất quán trong toàn bộ lớp `TodoList`, đồng thời đảm bảo rằng tất cả các hàm so sánh (như `overdue_tasks`) đều sử dụng cùng một kiểu dữ liệu để tránh lỗi logic nghiêm trọng.
[MAJOR] Technical Plan / Cấu trúc dữ liệu :: Thay vì chỉ yêu cầu "sửa lớp `TodoList` để mỗi mục từ điển (`item`) phải bao gồm khóa `"due_date"`", cần định nghĩa rõ ràng một giao diện (interface) hoặc schema cho cấu trúc của một item công việc, thay vì dựa vào việc sửa đổi các dictionary nội bộ.
[MAJOR] Risks :: Việc lo lắng về "Hiệu suất truy vấn" và tối ưu hóa hàm `overdue_tasks` là quá mức cần thiết (premature optimization) khi ứng dụng đang hoạt động trong phạm vi bộ nhớ đơn giản; nên tập trung hoàn toàn vào việc triển khai logic chính xác trước khi xem xét hiệu năng.
[NIT] Detailed Implementation Plan / Tasks :: Cần làm rõ rằng hàm `add()` không chỉ nhận tham số ngày hết hạn mà còn phải thực hiện việc *xác thực* (validation) định dạng của ngày đó ngay tại lớp này, để đảm bảo tính toàn vẹn dữ liệu trước khi lưu trữ.
```

### Kiểm

```text
[BLOCKER] Detailed Implementation Plan :: The plan requires changing method signatures and data structures (`due_date` handling) but does not specify the concrete new signature for `add(title)` or how existing methods like `done()` and `pending()` must be updated to handle the new task structure.
[MAJOR] Detailed Implementation Plan :: The scope of changes is incomplete; modifying the core data structure with a `due_date` field requires explicit instructions on how this impacts all related methods (e.g., `done(item_id)` or `pending()`) beyond just listing `__init__`, `add()`, and `overdue_tasks()`.
[MAJOR] Detailed Implementation Plan :: The steps are not concrete enough; they use general directives ("Cập nhật...", "Sửa...") instead of specific instructions like "Add field X to struct Y in file Z" or "Change signature of method Z from A to B".
[MAJOR] Verification Plan :: While a command is provided, the plan lacks definition for edge case tests (e.g., what happens if no tasks are overdue, or if `due_date` is invalid/missing).
```

### Hiểu

```text
[BLOCKER] Open Questions :: Định dạng ngày cho `due_date` chưa được quyết định (string ISO 8601 hay đối tượng Date Python) và điều này ảnh hưởng trực tiếp đến cách so sánh thời gian trong hàm `overdue_tasks()`. :: Cần chọn một định dạng cố định và xác định rõ ràng logic so sánh: liệu việc so sánh chỉ cần dựa trên ngày (date component) hay phải bao gồm cả giờ/phút (datetime object).
[MAJOR] Technical Plan / Tasks (- [ ] T2) :: Chữ ký hàm `add(title)` sẽ thay đổi để chấp nhận tham số ngày hết hạn, nhưng không có chữ ký cụ thể nào được cung cấp. :: Cần xác định rõ ràng chữ ký mới của hàm này (ví dụ: `add(self, title: str, due_date: Optional[datetime] = None)`) để các file khác biết cách gọi nó.
[MAJOR] Technical Plan / Tasks (- [ ] T3) :: Logic so sánh "ngày hiện tại" với `due_date` cần được làm rõ về mặt thời gian. :: Cần xác định xem việc kiểm tra quá hạn chỉ dựa trên ngày (ví dụ: 2026-10-25 đã qua) hay phải tính đến cả múi giờ và thời điểm chạy chương trình, để tránh sai lệch khi tác vụ hết hạn vào cuối ngày.
[MAJOR] Technical Plan / Cấu trúc dữ liệu :: Việc bổ sung trường `due_date` yêu cầu sửa đổi lớp `TodoList`, nhưng không có định nghĩa rõ ràng về schema đầy đủ của một mục (`item`) sau khi thêm trường này (ví dụ: các khóa bắt buộc và kiểu dữ liệu mặc định cho tất cả các trường). :: Cần cung cấp một ví dụ mẫu hoặc khai báo type hint/schema chi tiết cho cấu trúc `item` mới.
[NIT] Technical Plan / Tasks (- [ ] T1) :: Khi sửa đổi `__init__`, cần xác nhận rõ ràng cách xử lý giá trị mặc định (default value) của trường `due_date` nếu người dùng không cung cấp nó, và liệu việc thiếu ngày có nên được coi là "không bao giờ hết hạn" hay phải có một giá trị placeholder cụ thể. :: Cần bổ sung logic hoặc ghi chú về hành vi khi `due_date` bị bỏ trống (None).
```

### Phản biện

```text
[MAJOR] Open Questions :: Định dạng dữ liệu cho `due_date` nên là gì: chuỗi (string) theo định dạng ISO 8601 hay một đối tượng Date Python? :: Phải quyết định và ghi rõ ràng trong tài liệu; nếu chọn string, phải xác định cách parse an toàn để tránh lỗi so sánh ngày.
[MAJOR] Technical Plan :: Cấu trúc dữ liệu (`todo.py`): Sửa lớp `TodoList` để mỗi mục từ điển (`item`) phải bao gồm khóa `"due_date"` (và có thể là một giá trị mặc định hoặc null/None). :: Không xác định rõ ràng cách xử lý trường hợp người dùng không cung cấp ngày hết hạn; cần quy định giá trị mặc định và logic cho các tác vụ không có ngày.
[MAJOR] Technical Plan :: Cấu trúc dữ liệu (`todo.py`): Sửa lớp `TodoList` để mỗi mục từ điển (`item`) phải bao gồm khóa `"due_date"` (và có thể là một giá trị mặc định hoặc null/None). :: Việc sử dụng dictionary cho cấu trúc dữ liệu công việc làm tăng nguy cơ lỗi kiểu dữ liệu và thiếu tính toàn vẹn, nên xem xét chuyển sang lớp `Task` để đảm bảo các thuộc tính luôn tồn tại.
[MAJOR] Alternatives Considered :: Tách logic thành một lớp `Task(id, title, due_date, done)` thay vì sử dụng dict. *Ưu điểm:* Cực kỳ sạch sẽ về mặt OOP, dễ kiểm thử đơn vị. *Nhược điểm:* Tăng lượng code cần viết và các file bị ảnh hưởng (ví dụ: cách khởi tạo list). :: Lý do từ chối lớp `Task` quá yếu; nên chấp nhận việc tăng độ phức tạp nhỏ để đảm bảo tính đúng đắn của mô hình dữ liệu, đặc biệt khi xử lý logic thời gian.
[MAJOR] Alternatives Considered :: Nếu ứng dụng lớn hơn, nên chuyển sang SQLite hoặc tương tự. *Ưu điểm:* Khả năng mở rộng và tính toàn vẹn dữ liệu cao nhất. *Nhược điểm:* Quá mức cần thiết cho phạm vi hiện tại và làm tăng độ phức tạp của dự án. :: Không có đánh giá về chi phí/thời gian phát triển để chuyển sang SQLite, việc loại bỏ giải pháp cơ sở dữ liệu chỉ vì "phạm vi hiện tại" là một rủi ro lớn khi tính năng được sử dụng thực tế.
[MAJOR] Risks :: Việc thêm bộ lọc theo ngày có thể làm chậm quá trình tải danh sách nếu số lượng tác vụ lớn, cần tối ưu hóa hàm `overdue_tasks`. :: Cần đưa ra giới hạn về kích thước dữ liệu (ví dụ: tối đa 1000 mục) hoặc một giải pháp cụ thể để đảm bảo hiệu suất truy vấn khi vượt qua ngưỡng đó.
[MAJOR] Critical User Journeys :: Khi người dùng nhập tiêu đề và chọn một `due_date` cụ thể (ví dụ: 2026-11-15). *Then:* Tác vụ được lưu trữ thành công với trường `due_date` và hiển thị ngày này trong danh sách. :: Không nêu rõ cách xử lý khi người dùng nhập định dạng ngày không hợp lệ hoặc vượt quá phạm vi cho phép (ví dụ: năm 9999).
[MAJOR] Critical User Journeys :: Hệ thống đã có một số tác vụ, trong đó có một tác vụ A hết hạn ngày 2026-10-25 nhưng chưa được đánh dấu hoàn thành. *When:* Người dùng truy vấn danh sách các tác vụ quá hạn (`overdue_tasks()`). *Then:* Hệ thống trả về Tác vụ A và hiển thị rõ ràng trạng thái "Quá hạn" bên cạnh ngày hết hạn. :: Không xác định cơ chế xử lý khi người dùng cố tình thay đổi ngày hết hạn trong quá khứ, liệu hệ thống có cho phép điều này không?
[BLOCKER] Technical Plan :: Phải sử dụng thư viện `datetime` của Python (hoặc tương đương) để so sánh ngày hiện tại với giá trị `due_date`. :: Không xác định rõ ràng việc đồng bộ hóa múi giờ; nếu ứng dụng chạy trên nhiều khu vực, việc so sánh ngày chỉ bằng `datetime.now()` sẽ dẫn đến sai lệch thời gian nghiêm trọng.
[NIT] Overview :: Việc thêm tính năng này sẽ cải thiện khả năng lập kế hoạch và nhắc nhở của người dùng về các cam kết thời gian bằng cách bổ sung cơ chế lọc công việc dựa trên ngày hết hạn. :: Cần làm rõ rằng đây chỉ là *cơ chế lọc* chứ không phải là một giải pháp quản lý lịch trình toàn diện, để tránh kỳ vọng sai lầm từ người dùng.
```
