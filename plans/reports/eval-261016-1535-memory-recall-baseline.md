# Eval: baseline + unified recall (P0/P1)

Ngày 2026-10-16 · nhánh `feat/memory-engine` · fixture `evals/memory/cases.json` · code `src/memory/engine/eval.rs`

Tái hiện:

```bash
cargo test --lib memory::engine::eval -- --nocapture --test-threads=1
MEMORY_EVAL_OUT=/tmp/mem cargo test --lib memory::engine::eval   # → /tmp/mem.{vectors,keyword-only}.json
```

## Cách đo

Cùng một kho (9 note curated, trong đó 1 note của chat khác; 8 chunk đồ thị với triplet dựng sẵn), 11 câu hỏi VN/EN.
Mỗi câu chạy qua **khối cũ** (`<cognitive_memory>` + `<memory_recall>`, mã cũ chuyển nguyên sang `engine::legacy`) và qua **engine**.
Chấm theo chuỗi có trong ngữ cảnh được tiêm: `answered` (đủ chuỗi đáp án), `leaked` (chuỗi bí mật của chat khác), `repeats` (đáp án lặp thêm), `tokens` (ước lượng theo từ).
Hai thế giới riêng cho hai pipeline vì spreading activation ghi ngược Hebbian vào đồ thị.
Không gọi model nào.

## Kết quả

| chế độ | pipeline | trả lời | rò rỉ | lặp | tokens | độ trễ median / p95 |
|---|---|---|---|---|---|---|
| đồ thị có vector (embedder băm từ) | khối cũ | 11/11 | 0 | 2 | 1930 | 3.9 ms / 4.5 ms |
| | engine | 11/11 | 0 | 0 | 1846 (−4%) | 4.1 ms / 4.9 ms |
| không embedding provider (mặc định) | khối cũ | 7/11 | 0 | 0 | 1344 | 0.19 ms / 0.78 ms |
| | engine | **10/11** | 0 | 0 | 1462 (+9%) | 0.55 ms / 1.44 ms |

## Đọc kết quả

1. **Chế độ mặc định có hiệu ứng thật.** Không có embedding provider, spreading activation không có hạt giống (nó gieo bằng vector,
   xem chú thích trong `retrievers.rs::vector_seeds`) nên `<cognitive_memory>` rỗng cho mọi câu: 4/11 câu mà chỉ đồ thị biết
   (`deadline-vi-chat-only`, `compiler-machine-hops`, `port-preference-vi`, `weather-chat-only`) không có đáp án. Arm cognitive của engine
   rơi về hybrid/keyword khi spreading rỗng và lấy lại 3 trong 4. Đây là lợi ích đo được duy nhất của P1 về khả năng trả lời.
2. **Có vector, hai bên ngang nhau về trả lời** (11/11). Fixture bão hoà nên không phân biệt được tỉ lệ trả lời ở chế độ này.
   Thứ phân biệt được là **lặp**: hai fact có ở cả note lẫn đồ thị (`telegram-pairing-both-stores`, `backup-vi-both-stores`)
   khối cũ tiêm hai lần, engine gộp (−35 và −37 token ở hai câu đó).
3. **Chi phí độ trễ nhỏ**: +0.2 ms (có vector), +0.35 ms (keyword-only, vì chạy spreading rồi mới rơi về hybrid). Kho eval bé
   (9 note, 8 chunk, DB trong bộ nhớ) — **không ngoại suy lên kho thật**.
4. **Không rò rỉ** ở cả hai bên: note của chat khác không bao giờ xuất hiện.

## Kho fact (P2)

Cùng harness, thêm 5 cửa sổ hội thoại đi qua **retain thật** (quét secret → parse → embed → dedupe → lưu → liên kết; chỉ câu trả lời
của model trích xuất là dựng sẵn) và 6 câu hỏi mà note với đồ thị không trả lời được. So engine không và có 4 arm fact.

| chế độ | không kho fact | có kho fact |
|---|---|---|
| có vector | 0/6 | **6/6** |
| không embedding provider | 0/6 | **6/6** |

Mỗi arm có đóng góp riêng (chạy từng arm đơn lẻ, `the_graph_and_temporal_arms_each_find_what_keyword_search_cannot`):
- `entity-hop-vi` ("Người phụ trách Falcon đang ở đâu?"): câu trả lời "Lan nghỉ phép…" không chung từ nào với câu hỏi; **graph** trả lời (đi qua thực thể Lan), **keyword không**.
- `yesterday-vi` ("Hôm qua tôi đã làm gì?"): fact ghi "2026-10-15"; **temporal** trả lời, **keyword không**. Với câu không nêu thời gian, arm temporal im lặng.
- Arm **semantic** im lặng khi không có embedding model và trả lời khi có.
- Secret `sk-proj-…` gõ cạnh ngày triển khai Orion: ngày được giữ, khoá **không** xuất hiện trong prompt gửi model trích xuất, trong ngữ cảnh, hay kho.

Giới hạn riêng của phần này: câu trả lời của model trích xuất là dựng sẵn, nên eval chứng minh *đường ống* (lưu, liên kết, tìm, ghép) chứ **không** chứng minh một model nhỏ thật trích xuất đúng từ tiếng Việt. Câu hỏi của 6 case được viết sau khi biết fact nào có trong kho — đây là cổng không-thoái-lui, không phải thước đo tổng quát.

## Giới hạn / điều chưa đo

- `compiler-machine-hops` vẫn trượt ở chế độ keyword-only: hop qua thực thể cần đồ thị đi bộ mà keyword không có. Engine không sửa được cái này.
- Cả hai pipeline **không biết bỏ cuộc**: với câu không có đáp án (`other-chat-secret`) vẫn tiêm 5 hit đồ thị không liên quan
  (68 token cũ, 95 token engine — nhãn `[n] (knowledge)` làm engine dài hơn khi ít hit). FTS chuẩn hoá điểm nên hit đầu luôn 1.0.
  Cần ngưỡng liên quan tuyệt đối — việc của phase sau, không phải P1.
- Embedder băm từ chỉ đo cơ chế (fusion, gộp, ngân sách), **không** nói gì về chất lượng embedding thật. Note curated chỉ tìm bằng keyword.
- 11 câu là quá ít để kết luận thống kê; đây là cổng không-thoái-lui, chưa phải thước đo chất lượng.
- Chưa đo trên dữ liệu thật của người dùng (cố ý: không đụng `~/.senclaw`).

## Cổng test (đang chạy trong `cargo test --lib`)

- không rò rỉ; lặp(engine) ≤ lặp(cũ); gói ≤ ngân sách (trừ khi chỉ còn 1 mục).
- có vector: trả lời(engine) ≥ trả lời(cũ). Không vector: trả lời(engine) **>** trả lời(cũ).

## Câu hỏi chưa giải quyết

1. Thêm ngưỡng liên quan tuyệt đối (abstain) ở P1.x hay đợi P2 khi đã có điểm thật từ fact store?
2. Có muốn một tập eval khó hơn (nhiều nhiễu, ngân sách chặt, câu nhiều bước) trước P2, để P2 có gì chứng minh?
3. Đồ thị của bản cài thật có embedding provider không? Nếu không, `preCognitive` hiện đang tiêm rỗng cho người dùng đó.
4. Chạy extractor thật (model local nhỏ) trên vài cửa sổ hội thoại tiếng Việt thật để biết tỉ lệ fact dùng được trước khi bật `memoryEngine` thường trực?
