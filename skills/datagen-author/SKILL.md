---
name: datagen-author
description: Tự soạn một bộ dữ liệu nhỏ nhưng chuẩn — taxonomy nhãn có định nghĩa, câu mẫu đa dạng, câu đối lập ở ranh giới, đáp án đúng — rồi lưu thành file JSONL trong thư mục làm việc và nhập vào Datagen kèm nguồn gốc. Dùng làm seed cho Datagen mở rộng, hoặc làm bộ đo vàng (gold set). Dùng khi chưa có dataset nào phù hợp và cần một điểm xuất phát do chính agent viết, có kiểm, thay vì để model sinh hàng loạt từ con số không.
version: 1.0.0
when-to-use: khi người dùng muốn "soạn bộ câu mẫu cho flow X", "viết 30 câu test cho chatbot", "tạo bộ eval vàng cho prompt này", "chuẩn bị seed để sinh dữ liệu", "định nghĩa các nhãn rồi cho ví dụ", "gắn nhãn danh sách câu này", "viết bài toán có đáp án để kiểm thử" — hoặc khi skill `datagen` cần seed mà không có dataset nào khớp. KHÔNG dùng khi catalog đã có bộ phù hợp (nhập thẳng bằng skill `datagen`), khi cần hàng trăm bản ghi (soạn 5–10 seed rồi giao `expand_seeds`), hay khi người dùng đã có file dữ liệu (nhập file).
triggers:
  - soạn bộ câu mẫu
  - viết câu mẫu
  - tạo câu mẫu
  - bộ câu mẫu
  - tạo seed
  - viết seed
  - bộ seed
  - tạo bộ seed
  - bộ đo vàng
  - gold set
  - golden set
  - tự tạo dataset
  - tự viết dataset
  - soạn dataset
  - gắn nhãn
  - định nghĩa nhãn
  - taxonomy
  - câu đối lập
  - contrastive examples
  - bộ test cho chatbot
  - test case cho prompt
  - eval cho prompt
allowed-tools:
  - Write
  - Read
  - mcp__senclaw-datagen__datagen_start
  - mcp__senclaw-datagen__datagen_status
  - mcp__senclaw-schedule__schedule_watch
---

# Tự soạn dữ liệu cho Datagen

Bạn là người viết dữ liệu. Một bộ nhỏ viết kỹ (5–10 câu mỗi nhãn) làm seed tốt hơn hàng trăm câu sinh từ hai câu mẫu
vội. Còn một bộ đo vàng thì phải do người duyệt. Mục tiêu của skill này: ra một file sạch, có định nghĩa, nằm trong
`datasets/` kèm nguồn gốc, để Datagen và người dùng dùng tiếp.

Tool Datagen (`mcp__senclaw-datagen__datagen_start`, `mcp__senclaw-datagen__datagen_status`) được nạp cùng skill này;
chỉ `ToolSearch` nếu chúng **không** có trong danh sách tool của bạn.

## 1. Chốt mục đích (trước khi viết dòng nào)

Hỏi, hoặc suy ra từ ngữ cảnh rồi nói ra, ba điều:

1. **Dữ liệu dùng để làm gì?** Seed cho `expand_seeds`, bộ đo vàng, hay bài có đáp án cho `reasoning_traces`. Mục đích
   quyết định dạng bản ghi và số lượng.
2. **Phân loại cái gì, thành những nhãn nào?** Ví dụ: skill nào được nạp, ý định khách hàng, mức rủi ro.
3. **Ngôn ngữ và giọng của người dùng thật:** tiếng Việt có dấu / không dấu, xưng hô, độ dài, có lẫn tiếng Anh không.

Trước khi soạn, gọi `datagen_status` (không `job_id`) và xem `catalog`. Nếu đã có bộ khớp (intent tiếng Việt, toán có
đáp án, hỏi đáp có đoạn văn), đề xuất nhập bộ đó thay vì tự viết.

## 2. Taxonomy nhãn

- **Loại trừ nhau.** Mỗi câu thuộc đúng một nhãn. Hai nhãn hay lẫn thì viết luật phân xử ngay trong định nghĩa. Ví dụ:
  `calendar` là *một* sự kiện hoặc lời nhắc; `schedule` là *agent tự chạy* một việc theo chu kỳ.
- **Luôn có một nhãn "không thuộc"** (`none` / `khac`): chuyện phiếm, câu hỏi chung, việc ngoài phạm vi. Thiếu nhãn này
  thì bộ đo không bắt được lỗi dương tính giả.
- **Định nghĩa một câu cho mỗi nhãn**, viết sao cho một người lạ đọc là gắn được nhãn. Chính câu này đi vào tham số
  `labels` của `expand_seeds`, nơi bộ phân loại mù dựa vào nó.
- Mỗi lần 3–6 nhãn. Nhiều hơn thì tách thành nhiều bộ.

## 3. Viết ví dụ

Mỗi nhãn 5–10 câu nếu làm seed; 20–50 câu nếu làm bộ đo vàng. Trong mỗi nhãn, rải đều:

- **Độ dài:** 3 từ ("im lặng"), một câu, hai câu có ngữ cảnh.
- **Cách nói:** ra lệnh, hỏi, nói vòng ("không biết chiều nay mình có rảnh không"), cộc lốc, lịch sự.
- **Bề mặt:** có dấu / không dấu / gõ sai nhẹ, lẫn tiếng Anh ("book giúp cái meeting"), số và ngày viết nhiều kiểu.
- **Thực thể khác nhau:** tên, địa điểm, sản phẩm, thời gian. Không lặp một khuôn và chỉ thay mỗi tên.
- **Câu đối lập:** với mỗi cặp nhãn hay lẫn, viết vài cặp câu gần giống nhau nhưng khác nhãn ("nhắc tôi 8 giờ sáng mai"
  → `calendar`; "mỗi sáng 8 giờ tự tổng hợp email gửi tôi" → `schedule`). Đây là phần giá trị nhất của bộ dữ liệu.

Không bao giờ đưa vào: API key, mật khẩu, số tài khoản, số điện thoại hay email thật, nội dung chat riêng của người dùng
mà họ chưa cho phép.

Với các dạng khác:
- **`problem_answer`:** đáp án tính tay và kiểm lại; mỗi bài một đáp án duy nhất, ghi số không đơn vị nếu được.
- **`qa_context`:** trích `context` nguyên văn; mỗi câu hỏi trả lời được chỉ từ đoạn đó.

## 4. Tự kiểm trước khi lưu

1. Đọc lại từng câu, che nhãn đi: theo định nghĩa thì gắn nhãn nào? Lệch hoặc mơ hồ thì sửa câu hoặc bỏ câu.
2. Đếm từng nhãn: lệch nhau quá 2 lần thì viết bù.
3. Quét trùng và gần trùng (cùng khuôn, chỉ đổi một từ): giữ câu hay hơn.
4. Bộ đo vàng: **đưa cho người dùng duyệt.** Hiện bảng nhãn + 3 câu mỗi nhãn, và hỏi những câu bạn còn phân vân. Bộ đo do
   chính model tự viết tự chấm thì mang theo thiên lệch của chính model đó.

## 5. Lưu và nhập vào Datagen

Ghi file bằng `Write` vào thư mục làm việc của chat, vd. `datasets-src/<tên>.jsonl`, một JSON mỗi dòng:

```jsonl
{"text": "cho mình chặn lịch chiều mai từ 2 đến 4 giờ để viết báo cáo", "label": "calendar"}
{"text": "mỗi sáng 8 giờ tự tổng hợp email chưa đọc rồi gửi tôi", "label": "schedule"}
{"text": "chào bạn, hôm nay thế nào", "label": "none"}
```

Các dạng khác: `{"problem", "answer"}`, `{"question", "answer", "context"}`, `{"user", "assistant"}`.

Rồi nhập. Bản ghi được chuẩn hoá, manifest ghi nguồn và license:

```json
mcp__senclaw-datagen__datagen_start {
  "pipeline": "import", "name": "<tên>",
  "source": {"file": "datasets-src/<tên>.jsonl"},
  "kind": "labeled_text",
  "license": "private",
  "attribution": "Soạn bởi agent SenClaw trong chat; người dùng đã duyệt: có/không"
}
```

Import xong gần như ngay. Gọi `datagen_status {"job_id": …}` một lần để xác nhận số dòng giữ lại và lý do loại (dòng
trống, dòng trùng).

## 6. Mở rộng (nếu cần nhiều hơn)

Bộ vừa nhập dùng làm seed cho `expand_seeds`, với đúng các định nghĩa nhãn ở bước 2:

```json
mcp__senclaw-datagen__datagen_start {
  "pipeline": "expand_seeds", "name": "<tên>-mo-rong",
  "seeds_from": "<tên>", "per_label": 5,
  "labels": {"calendar": "…", "schedule": "…", "none": "…"},
  "target": 20, "language": "Vietnamese"
}
```

Chạy thử `target` khoảng 20 trước. Đặt watch theo skill `datagen` (mục "Sau khi bắt đầu"), không chờ trong lượt. Kết quả
tốt thì chạy lớn hơn.

**Giữ hai bộ tách biệt.** Bộ tự soạn và đã duyệt là bộ **đo**. Bộ sinh ra từ nó dùng để **huấn luyện / tinh chỉnh**,
hoặc làm bộ đo phụ. Đo trên dữ liệu sinh từ chính seed của bộ đo sẽ cho điểm đẹp hơn thực tế.

## 7. Báo lại

- Taxonomy (nhãn + định nghĩa) và số câu mỗi nhãn.
- Đường dẫn file nguồn trong thư mục làm việc, và tên dataset trong Datagen.
- Những câu còn phân vân, cần người dùng quyết.
- Bước tiếp: mở rộng bằng `expand_seeds`, dùng làm bộ đo, hoặc xem ở trang **Datagen → Dataset**.
