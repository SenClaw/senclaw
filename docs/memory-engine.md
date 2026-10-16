# Memory engine — truy hồi hợp nhất

> Trạng thái: **P1 (truy hồi hợp nhất) và P2 (kho fact) đã cài** (2026-10-16), mặc định **tắt**. Kế hoạch đầy đủ:
> [`plans/261016-1441-hindsight-memory-engine/plan.md`](../plans/261016-1441-hindsight-memory-engine/plan.md);
> nghiên cứu gốc: [`plans/reports/research-261016-1441-hindsight-memory-for-senclaw.md`](../plans/reports/research-261016-1441-hindsight-memory-for-senclaw.md).
> Code: [`src/memory/engine/`](../src/memory/engine/).

## Vấn đề

Tri thức của agent nằm ở ba chỗ mọc riêng (xem [memory.md](memory.md),
[curated-memory-design.md](curated-memory-design.md), [knowledge-cognitive-flow.md](knowledge-cognitive-flow.md)):
note curated `memory/*.md`, đồ thị cognitive, và `MEMORY.md`. Trước mỗi lượt, `AgentPool`
tiêm **mỗi kho một khối riêng** (`<memory_recall>`, `<cognitive_memory>`), mỗi khối tự xếp hạng,
tự cắt, **không chung ngân sách** — nên cùng một fact có thể xuất hiện hai lần.

## Cách hoạt động

```text
arms (song song) → RRF (k=60) → gộp bản trùng → boost recency → ngân sách token → <memory_context>
```

| Bước | File | Ghi chú |
|---|---|---|
| Arm | [`arms.rs`](../src/memory/engine/arms.rs) | `CuratedArm` (hybrid FTS/vector trên note), `CognitiveArm` (spreading; rơi về keyword khi spreading rỗng) |
| Fusion | [`fusion.rs`](../src/memory/engine/fusion.rs) | `Σ 1/(60+rank)`; chỉ dùng thứ hạng vì điểm các kho không so được. Hai kết quả chồng nhau ≥ 0.85 (overlap coefficient, ≥ 6 từ mỗi bên) là một, cộng điểm |
| Boost | [`boost.rs`](../src/memory/engine/boost.rs) | điểm liên quan thay bằng hạng rải trong [0.1, 1.0] (chưa có cross-encoder) × `1 + 0.2·(recency − 0.5)`; recency giảm tuyến tính 365 ngày, không rõ ngày = trung tính |
| Ngân sách | [`budget.rs`](../src/memory/engine/budget.rs) | lấy từ trên xuống; mục quá dài bị bỏ qua và đi tiếp; không mục nào vừa thì vẫn trả mục đầu |
| Hiển thị | [`format.rs`](../src/memory/engine/format.rs) | một khối đánh số, mỗi mục ghi nguồn `(note)` / `(knowledge)`; chuỗi `</memory_context>` trong dữ liệu bị vô hiệu |

Không có lời gọi model nào trên đường này. Một arm lỗi hoặc quá 3 s chỉ mất phần của nó
(`RecallConfig::arm_timeout`); lượt chat không bao giờ fail vì memory.

**`MEMORY.md` cố ý không phải arm.** Nó là ngữ cảnh ghim (chỉ dẫn thường trực); truy hồi theo
câu hỏi có thể bỏ sót chúng. Khối `<memory>` (env `memory.pre_retrieval`) giữ nguyên.

## Kho fact (P2)

Note curated do người viết, đồ thị giữ triplet (mất *vì sao / khi nào / ai*). **Fact** là một–hai câu tự đủ nghĩa, có ngày và
các thực thể nó nhắc tới: "Emily chuyển đến Huế vào tháng 3 năm 2026 và làm việc tại FPT." Code:
[`src/memory/engine/facts/`](../src/memory/engine/facts/).

```text
cửa sổ hội thoại (reflection) → quét secret → chunk → 1 lần gọi model/chunk → parse → embed
                              → bỏ cái đã biết (hash / cosine ≥ 0.95) → lưu → liên kết (ngữ nghĩa ≥ 0.7, cùng ngày ±24h)
```

| Phần | File | Ghi chú |
|---|---|---|
| Lưu trữ | [`store.rs`](../src/memory/engine/facts/store.rs) | bảng `mem_facts`, `mem_facts_fts`, `mem_fact_entities`, `mem_links` (chỉ thêm, `CREATE … IF NOT EXISTS`, không migrate gì cũ). Vector = BLOB, quét chính xác như `memory_chunks`. Phạm vi `bank` = agent folder |
| Quét secret | [`defense.rs`](../src/memory/engine/facts/defense.rs) | key/token/private key/thẻ (Luhn) → `[REDACTED:<loại>]`, chạy **trước** model và trước cả đường triplet. Email, số điện thoại, tên **không** bị xoá |
| Trích xuất | [`extract.rs`](../src/memory/engine/facts/extract.rs) | prompt lấy từ Hindsight (chọn lọc, ngày tuyệt đối, giải đại từ, `world` vs `experience`) + hướng dẫn tiếng Việt; JSON phẳng 4 trường cho model nhỏ; parser chịu được fence/prose/mảng trần |
| Retain | [`retain.rs`](../src/memory/engine/facts/retain.rs) | mọi lỗi chỉ đếm và log (`RetainReport`), không bao giờ ném |
| Đọc ngày trong câu hỏi | [`temporal.rs`](../src/memory/engine/facts/temporal.rs) | luật VN/EN ("hôm qua", "tuần trước", "tháng 3", "31/07", "in 2025") → khoảng `[start,end)`; không dùng model |
| 4 arm | [`arms.rs`](../src/memory/engine/facts/arms.rs) | `facts_semantic` (cần embedding), `facts_keyword` (BM25), `facts_graph` (hạt giống từ 2 arm trên → thực thể chung `tanh(0.5·n)` + liên kết), `facts_temporal` (rải đều theo 4 khoảng của cửa sổ) |

Cùng danh tính `(facts, id)` ở cả bốn arm nên RRF cộng phiếu khi một fact được nhiều arm tìm thấy.

- **Chạy song song với triplet, không thay.** Thay triplet bằng fact cho chat là quyết định để sau khi đo trên hội thoại thật.
- **Bật khi nào**: retain chạy trong cùng cửa sổ reflection (`reflect_window_text`), nên cần `cognitive.enabled`, `memory.cognitive_reflection` **và** `memoryEngine`.
  Mỗi cửa sổ thêm **một** lần gọi model trích xuất (qua slot `active_cognitive_id`), tuần tự sau cognify.
- **Phạm vi riêng tư = agent folder**, như đồ thị: cửa sổ reflection không mang chat jid nên fact của một chat có thể xuất hiện ở chat khác dùng chung folder.
  Note curated thì có lọc theo chat; fact **chưa**.
- **Embedding**: dùng chung provider của đồ thị. Không có provider thì fact vẫn lưu và vẫn tìm bằng từ khoá, thực thể, ngày.
- Câu hỏi được embed **một lần** dù semantic và graph chạy đồng thời (`FactEngine::embed_query`).

## Bật / cấu hình

| | |
|---|---|
| Công tắc | `memoryEngine` — `GET`/`POST /api/agent-behavior` (cạnh `memoryRecall`, `preCognitive`). **Mặc định tắt.** Chưa có hàng trên web/desktop (hai repo anh em) |
| Kho nào được đọc | vẫn theo `memoryRecall` (note) và `preCognitive` (đồ thị). Bật engine chỉ gộp và cắt ngân sách phần đã được tiêm, không đọc thêm kho nào người dùng đã tắt |
| Ngân sách | `SENCLAW_MEMORY_ENGINE_MAX_TOKENS` (mặc định 600, tối thiểu 50). **Đơn vị là từ** (`chunker::estimate_tokens`), token thật của model cao hơn (~1.3× tiếng Anh, ~2× tiếng Việt) |
| Số kết quả mỗi arm | `SENCLAW_SEARCH_MAX_RESULTS` (mặc định 5) |
| Xử lý secret trong cửa sổ | `SENCLAW_MEMORY_ENGINE_DEFENSE` = `redact` (mặc định) \| `block` (bỏ cả cửa sổ) \| `off`. Giá trị lạ → `redact` |
| Tắt engine | khối cũ chạy lại y như trước — [`legacy.rs`](../src/memory/engine/legacy.rs) là chính đoạn mã cũ, chuyển sang nhận kho làm tham số |

## Chạy eval

```bash
cargo test --lib memory::engine::eval -- --nocapture
MEMORY_EVAL_OUT=/tmp/memory-eval.json cargo test --lib memory::engine::eval   # thêm JSON
```

[`evals/memory/cases.json`](../evals/memory/cases.json): 9 note (một note của chat khác), 8 chunk đồ thị với
triplet dựng sẵn, 11 câu hỏi VN/EN, và cho kho fact: 5 cửa sổ hội thoại với câu trả lời của model trích xuất dựng sẵn
(chạy qua **retain thật**: quét secret, parse, embed, lưu, liên kết) + 6 câu hỏi chỉ fact trả lời được. Mỗi câu chấm: **answered** (ngữ cảnh chứa mọi chuỗi `must_include`),
**leaked** (chứa chuỗi `must_exclude` của chat khác), **repeats** (chuỗi đáp án lặp thêm lần nữa), **tokens**.
Chạy hai chế độ: đồ thị có vector (embedder băm từ `HashedWords` thay model thật) và **không embedding provider**
(cấu hình mặc định). Kho fact: không có fact trả lời 0/6, có fact 6/6 ở cả hai chế độ; riêng arm graph và temporal mỗi cái trả lời một câu mà
keyword một mình không trả lời được (kiểm bằng cách chạy từng arm đơn lẻ); secret gõ trong chat không tới model trích xuất.
Cổng test: không rò rỉ; engine lặp ≤ khối cũ; có vector thì trả lời ≥ khối cũ; không vector thì
trả lời **>** khối cũ. Không gọi model. Số liệu (có vector 11/11 = 11/11, lặp 2 → 0; không vector 7/11 → 10/11): xem
[`plans/reports/eval-261016-1535-memory-recall-baseline.md`](../plans/reports/eval-261016-1535-memory-recall-baseline.md).

## Bẫy

- **Spreading cần embedding provider.** Không có provider (cấu hình mặc định, `SENCLAW_EMBEDDING_PROVIDER=none`)
  thì `preCognitive` tiêm **rỗng** vì hạt giống chỉ đến từ vector (đo trên fixture: 4/11 câu mất đáp án). `CognitiveArm`
  rơi về hybrid (= keyword khi không có vector) nên engine lấy lại 3/4; khối cũ thì không.
- **Hit không có chữ bị bỏ ở arm cognitive.** Một node entity chỉ có tên không phải tri thức; khối cũ vẫn in nó.
- **Ngân sách đếm từ.** Đừng so với context window như thể là token thật.
- Ngày của note lấy từ `createdAt` trong frontmatter (nửa đêm UTC); thiếu hoặc sai định dạng thì trung tính,
  không phải lỗi.
