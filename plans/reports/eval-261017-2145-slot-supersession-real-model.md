# Slot đơn trị cho supersession — đo trên model thật (H1)

Ngày 2026-10-17 · `gemma-4-e2b-it-OptiQ-4bit` qua `sen-mlx 0.1.1` · `temperature 0.1` · daemon cô lập (HOME/cwd tạm, cổng 28788/28789).
Thay đổi: fact có thêm `slot = (subject, predicate)` tuỳ chọn; predicate đơn trị thì fact mới thay fact cũ **cùng slot** bằng luật
(xem [memory-engine.md](../../docs/memory-engine.md) "Slot"). Câu hỏi: đổi ý có còn lọt không khi model **không** nêu `replaces`?

## Ba thước đo

| Thước đo | Cách chạy | Kết quả |
|---|---|---|
| **Dựng sẵn** (cổng, mặc định) | `cargo test --lib memory::engine::eval` — 15 cửa sổ, 11 câu hỏi, 4 ca slot không `replaces` | qua; chuyển thành phố, đổi backend, `likes` hai giá trị, "năm 2025" không thay chỗ làm hiện tại |
| **Extractor thật** (cửa sổ cố định) | `real_extractor_keyword_only --ignored`, 5 lần | xem bảng 1 |
| **Đường chat thật** (daemon + WS) | `node scripts/evals/memory-changed-mind.mjs --runs 3` | xem bảng 2 |

### 1. Extractor thật, 15 cửa sổ, 3 ca đổi ý (React→Vue, Quy Nhơn→Vũng Tàu, Express→Fastify)

| Lần | Prompt | Slot model nêu | Ca đổi ý đã thử / bị ẩn | Bị ẩn bởi luật (không có `replaces`) |
|---|---|---|---|---|
| 1 | trước khi chỉnh | 10/20 fact | 2 / 1 — **thành phố còn sống** (model không nêu slot cũng không nêu `replaces`) | 0 |
| 2 | sau khi chỉnh | 11/18 | 3 / 3 | 0 |
| 3 | sau | 15/21 | 3 / 3 | **2** |
| 4 | sau | 12/18 | 3 / 3 | 0 |
| 5 | sau | 11/20 | 2 / 2 (1 ca model bỏ mất fact cũ nên không thử) | **1** |

Chỉnh prompt = schema có sẵn `"subject":"","predicate":""`, câu "dùng ĐÚNG predicate đó, chỉ vậy là đủ", và một ví dụ chuyển nhà không `replaces`.
Sau chỉnh: 11/11 ca đã thử đều bị ẩn; **3 lần ẩn chỉ nhờ luật** (model không nêu `replaces`). Model nêu slot cho 55–71% số fact.

### 2. Đường chat thật (script mới)

| Kịch bản | Build trước slot | Build có slot |
|---|---|---|
| Dễ: "thích React" → "đổi ý, giờ dùng Vue" | 2/2 xong (1 lượt hết giờ chat, không liên quan memory) | **3/3** |
| Khó: "tên là Lan, làm ở Orion, thích React" (một câu nhiều ý) → "chuyển sang Vue" | **1/3** (2 lần fact React còn sống) | **2/3** |

Kịch bản khó là kịch bản đã làm `replaces_claimed=0` hai lần trên bản cài. Mẫu nhỏ (3 lần/ô): chỉ đủ để thấy chiều, không đủ cho tỉ lệ.

## Lỗi khác tìm ra khi viết eval (đã sửa)

1. **Gộp trùng nhầm hai fact ngắn.** `fusion::terms_of` đếm cả header của fact ("noted 2026-10-16") — giống hệt nhau trong một ngày — nên "thích uống kombucha"
   và "thích uống matcha" bị coi là gần trùng và chỉ một cái được nhớ lại. Nay fact chỉ so phần thân.
2. **Arm graph lan qua thực thể `user`.** Mọi fact đều có `user`, nên arm graph kéo ra mười mấy fact không liên quan và đè mất đường nối thật. Nay bỏ qua chủ thể chung
   (`user`, `assistant`…) khi nối theo thực thể.

## Điểm yếu còn lại

- **Fact nhiều thuộc tính.** Model hay gộp "tên Lan, làm ở Orion, thích React" thành một fact rồi gán slot theo một thuộc tính (có lần `works_at` cho chủ thể "lan"). Khi bị
  thay, fact gộp kéo theo thông tin còn đúng (tên, nơi làm) ra khỏi recall (vẫn còn trong lịch sử). Hướng sửa: dặn "mỗi fact một thuộc tính" và đo lại; chưa làm.
- **Slot sai tên.** Có lần model đặt `favorite_kombucha` (giá trị thành thuộc tính). Vô hại vì mỗi giá trị một slot riêng, nhưng không thay được gì.
- **Truy hồi sau khi đổi câu chữ.** Fact "đã chuyển vào Vũng Tàu" không còn chữ "sống" nên câu hỏi "sống ở thành phố nào?" không tìm ra nó bằng từ khoá (ca `slot-moved-city` trượt ở extractor thật,
  không phải do supersession).
- Model nhỏ nêu slot cho khoảng nửa số fact; fact không có slot không bao giờ bị thay bằng luật.
- Chưa đo trên `gemma-4-e4b` (máy LM Studio không trả lời lúc đo) hay model đám mây.

## Câu hỏi chưa giải quyết

- Có nên dặn model "một fact một thuộc tính" ngay (H1b), hay thử trước trên e4b?
- Có nên suy slot từ `KNOWN FACTS` khi model bỏ sót mà fact mới nhắc lại giá trị cũ ("không còn ở Quy Nhơn nữa")? Đã cố ý không làm (đoán từ câu chữ dễ ẩn nhầm).
