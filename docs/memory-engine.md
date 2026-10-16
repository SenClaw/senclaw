# Memory engine — truy hồi hợp nhất

> Trạng thái: **P1 đã cài** (2026-10-16), mặc định **tắt**. Kế hoạch đầy đủ:
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

## Bật / cấu hình

| | |
|---|---|
| Công tắc | `memoryEngine` — `GET`/`POST /api/agent-behavior` (cạnh `memoryRecall`, `preCognitive`). **Mặc định tắt.** Chưa có hàng trên web/desktop (hai repo anh em) |
| Kho nào được đọc | vẫn theo `memoryRecall` (note) và `preCognitive` (đồ thị). Bật engine chỉ gộp và cắt ngân sách phần đã được tiêm, không đọc thêm kho nào người dùng đã tắt |
| Ngân sách | `SENCLAW_MEMORY_ENGINE_MAX_TOKENS` (mặc định 600, tối thiểu 50). **Đơn vị là từ** (`chunker::estimate_tokens`), token thật của model cao hơn (~1.3× tiếng Anh, ~2× tiếng Việt) |
| Số kết quả mỗi arm | `SENCLAW_SEARCH_MAX_RESULTS` (mặc định 5) |
| Tắt engine | khối cũ chạy lại y như trước — [`legacy.rs`](../src/memory/engine/legacy.rs) là chính đoạn mã cũ, chuyển sang nhận kho làm tham số |

## Chạy eval

```bash
cargo test --lib memory::engine::eval -- --nocapture
MEMORY_EVAL_OUT=/tmp/memory-eval.json cargo test --lib memory::engine::eval   # thêm JSON
```

[`evals/memory/cases.json`](../evals/memory/cases.json): 9 note (một note của chat khác), 8 chunk đồ thị với
triplet dựng sẵn, 11 câu hỏi VN/EN. Mỗi câu chấm: **answered** (ngữ cảnh chứa mọi chuỗi `must_include`),
**leaked** (chứa chuỗi `must_exclude` của chat khác), **repeats** (chuỗi đáp án lặp thêm lần nữa), **tokens**.
Chạy hai chế độ: đồ thị có vector (embedder băm từ `HashedWords` thay model thật) và **không embedding provider**
(cấu hình mặc định). Cổng test: không rò rỉ; engine lặp ≤ khối cũ; có vector thì trả lời ≥ khối cũ; không vector thì
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
