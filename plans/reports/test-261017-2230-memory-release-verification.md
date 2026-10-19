# Kiểm chứng bản memory đã cài (2026-10-17)

Phạm vi: bản đang nằm trong `/Applications` (cài 21:58) và ba commit tương ứng. Hai phần: bộ test hồi quy của cả ba repo, và thử nghiệm đầu-cuối
**bằng đúng binary đã cài** trên daemon cô lập (HOME/cwd tạm, cổng 28788/28789, không chạm daemon thật 18788/18789) với model cục bộ
`gemma-4-e2b-it-OptiQ-4bit` (`sen-mlx 0.1.1`), qua đường chat thật (WS gateway → reflection → trích xuất → recall).

| Repo | Commit |
|---|---|
| senclaw (worktree `feat/memory-engine`) | `2a510af feat(memory): per-chat facts, slot supersession and a pinned MEMORY.md` |
| web-app (`feat/datagen-import`) | `f12b16b feat(cognitive): facts tab in the Knowledge page` |
| desktop (`feat/datagen-import`) | `3c70b29 feat(cognitive): facts tab in the Knowledge screen` |

Lưu ý: chuỗi phiên bản của bản cài vẫn là `0.1.6 (e92c23f569df)` vì lúc build chưa commit; các build sau commit mới mang hash mới.

## 1. Bộ test hồi quy

| Bộ | Kết quả |
|---|---|
| senclaw `cargo test --lib --tests` | **3.173 qua, 0 hỏng**, 11 bỏ qua (`#[ignore]`: harness model thật). 3 test `runtime_manager_lifecycle` hỏng lần đầu chỉ vì tìm binary phụ `echo_runtime` ở `<repo>/target`, trong khi dùng thư mục target chung; đặt đúng chỗ thì qua cả 3 |
| web-app `npm run build` (`tsc -b && vite build`) | xanh, không lỗi TypeScript |
| desktop `flutter analyze` / `flutter test` | không lỗi / **379 qua**, 1 bỏ qua |

## 2. Đầu-cuối trên binary đã cài

| # | Kịch bản | Kỳ vọng | Kết quả |
|---|---|---|---|
| 1 | **Phạm vi chat** (mặc định): chat A nói "dự án tên mã Zorbax-436", chat B của cùng agent hỏi | B không biết; fact mang `origin` của A | **Đạt** — B trả lời "không có thông tin"; `origin = web:verify-…-a` |
| 2 | **Phạm vi agent** (`SENCLAW_MEMORY_FACTS_SCOPE=agent`), cùng kịch bản | B nhớ lại | **Đạt** — B trả lời "Zorbax-106"; `origin` rỗng |
| 3 | **MEMORY.md ghim mặc định**: file ghi "Tên mã dự án chuẩn là Pelican-9", chat mới hỏi | trả lời từ file, không cần recall | **Đạt 2/2** (cấu hình mặc định, không biến môi trường) |
| 4 | **Secret**: gõ khoá `sk-proj-…` giả trong chat | khoá không nằm trong fact nào | **Đạt** — 2 fact được lưu, 0 chứa khoá, 0 dấu `REDACTED` thừa |
| 5 | **Đổi ý** (kịch bản khó: một câu nêu tên, nơi làm, React → "chuyển sang Vue") ×2 | không còn fact hiện hành nêu React; fact cũ còn trong lịch sử | **Đạt 2/2**; fact "tên Lan, làm ở Orion" được tách riêng nên vẫn còn hiện hành |
| 6 | **Chuyển nhà** rồi **quên fact thay thế** | fact cũ ẩn; xoá fact mới thì fact cũ sống lại | **Đạt** — Quy Nhơn bị ẩn sau "chuyển vào Vũng Tàu"; `DELETE` fact mới → 200, fact cũ hiện lại, fact mới biến mất |
| 7 | `GET /api/memory/facts/banks` | liệt kê agent có fact kèm số hiện hành / đã thay | **Đạt** — 3 agent, `replaced` đúng |

Ghi chú thử nghiệm: lần đầu kịch bản 3 "trượt" vì tôi đặt `MEMORY.md` sai thư mục (dữ liệu của daemon nằm ở `~/senclaw/profiles`, không ở
`$SENCLAW_HOME/data`); đặt đúng chỗ thì đạt. Đó là lỗi của bài thử, không phải của sản phẩm.

## 3. So với các số đo trước

| Thước đo | Trước | Nay |
|---|---|---|
| Đổi ý qua đường chat thật, kịch bản khó (model 2B) | 1/3 trước slot; `replaces_claimed=0` ở bản cài cũ | 2/3 trên build gỡ lỗi, **2/2** trên binary đã cài |
| Phạm vi giữa các chat của cùng một agent | fact của chat này hiện ra ở chat kia (70/84 nhóm dùng chung `main`) | cô lập theo chat, có công tắc quay về dùng chung |
| `MEMORY.md` | không vào ngữ cảnh mặc định (24 agent có file) | vào mọi lượt, cắt 8.000 ký tự |

## 4. Điểm còn mở

- **Mẫu nhỏ** (2–3 lần mỗi ô, một model 2B): đủ thấy chiều, không đủ cho tỉ lệ. Chưa đo trên `gemma-4-e4b` (model người dùng cấu hình) hay model đám mây.
- **Fact nhiều thuộc tính**: ở build gỡ lỗi có một lần model gộp "tên Lan, làm ở Orion, thích React" thành một fact rồi gán slot sai; trên binary đã cài hai lần đều tách riêng nên không gặp,
  nhưng điểm yếu chưa được sửa (hướng: dặn "mỗi fact một thuộc tính").
- **Tìm theo nghĩa vẫn tắt** (không có runtime llama.cpp lẫn model embedding); recall dùng từ khoá, thực thể và ngày. Bước bù vector sẽ tự chạy khi có model.
- **Đĩa chật**: còn ~3,5 GB; thư mục target chung đang 29 GB. Nên dọn trước lần build kế tiếp.
- **Chưa kiểm** bằng giao diện: tab Facts trên desktop mới chỉ được kiểm bằng test widget (người dùng từ chối cấp quyền điều khiển app); web đã kiểm trực tiếp trên daemon thử.

## Câu hỏi chưa giải quyết

- Có chạy lại bộ đo trên `gemma-4-e4b` khi máy LM Studio `192.168.0.93` lên không?
- Có làm bước "mỗi fact một thuộc tính" (và đo lại kịch bản khó) trước khi bàn tiếp việc hợp nhất Knowledge vào Facts không?
