# Research: Hindsight → hệ thống memory cho SenClaw

Ngày: 2026-10-16 · Nguồn: `vectorize-io/hindsight` @ `9a499b1` (2026-10-15, MIT) · Video: YouTube Short `wQXB3H9b6AE`

> Video chỉ đọc được **tiêu đề**: "Hindsight 1.0: Trí nhớ cho AI agent gần 50K sao, nhưng phần nhanh nhất phải trả tiền?".
> Không có transcript/mô tả → câu "phần nhanh nhất trả tiền" **chưa kiểm chứng**. Search web không tìm thấy release "1.0" hay bảng giá.
> Repo là MIT và engine (retain/recall/consolidate/reflect) có đủ trong repo → SenClaw không phụ thuộc phần trả tiền nào để làm lại.

## 1. Kết luận

- SenClaw **đã có 3 hệ nhớ rời nhau** (FTS/vec, curated `.md`, cognitive graph). Thiếu không phải storage mà là **lớp chung phía trên**: đơn vị tri thức tự đủ nghĩa, hợp nhất bằng chứng, truy hồi hợp nhất có budget.
- Hindsight đáng học ở 5 ý (xếp theo giá trị/chi phí):
  1. **Recall hợp nhất**: nhiều arm song song → RRF (k=60) → boost nhân (recency/temporal/proof) → cắt theo **token budget**. Không cần LLM trên request path.
  2. **Fact tự đủ nghĩa** (what/when/where/who/why, world vs experience, bitemporal) thay vì triplet mất ngữ cảnh.
  3. **Observation**: gộp fact thành belief có bằng chứng (`proof_count`, nguồn), UPDATE-over-CREATE, giữ lịch sử khi mâu thuẫn.
  4. **Mental model**: câu hỏi cố định, câu trả lời viết sẵn, tự làm mới khi có tri thức mới. Đọc = 1 DB read.
  5. **Memory defense**: quét secret/PII trước khi lưu. SenClaw hiện **không có** ở phía memory dù auto-reflection đưa mọi chat vào LLM + DB.
- **Không nên** nhúng nguyên Hindsight: Python + Postgres/pgvector, repo 428 MB, ngược triết lý daemon Rust + SQLite + runtime tách rời.
- **Khuyến nghị**: port ý tưởng sang Rust trên SQLite hiện có (kiến trúc A bên dưới), làm **từng bước đều ship được**, bắt đầu bằng recall hợp nhất trên 3 store sẵn có (không schema mới, không LLM mới).

## 2. Hindsight hoạt động thế nào (đã đọc code)

Đường dẫn dưới `hindsight-api-slim/hindsight_api/`.

### Retain (`engine/retain/`)
- Chunk (`fact_extraction.py::iter_chunks`, hỗ trợ hội thoại/JSONL) → **1 LLM call/chunk** ra fact có cấu trúc `what | when | where | who | why` (`Fact`, dòng ~161).
- Prompt `CONCISE_FACT_EXTRACTION_PROMPT` (dòng ~1058–1180): chọn lọc ("hữu ích sau 6 tháng?"), đổi **mọi thời gian tương đối → ngày tuyệt đối**, giải coreference ("my roommate"+"Emily" → "Emily (user's roommate)"), `entities` luôn là mảng string, giữ nguyên ngôn ngữ đầu vào.
- Phân loại `fact_type`: `world` (kể cả sở thích/quy tắc của user) vs `experience` (hành động của chính agent). Quyết định theo **ai đang nói**, truyền qua `context`.
- Hai trục thời gian: `occurred_start/end` (sự kiện xảy ra) và `mentioned_at` (khi được nói tới). Ngày thô (chỉ năm/tháng) phủ **cả kỳ**.
- Entity resolution (`memories/pg/entity_resolver.py`): fuzzy tên (trigram + token ≥ 0.6, quy tắc prefix cho viết tắt) + **đồng xuất hiện có trọng số theo độ hiếm** của partner + gần thời gian. Label entity chỉ khớp exact.
- Link lúc ghi (`memories/pg/links.py`): **semantic** kNN (top-k ≤ 50, ngưỡng cosine, weight ∈ [0.7,1]); **temporal** trong cửa sổ 24h, `weight = max(0.3, 1 − Δh/24)`; **causal** do LLM trả. Entity link tính lúc query bằng self-join `unit_entities`.
- Schema (`models.py`): `memory_units`, `entities`, `unit_entities`, `entity_cooccurrences`, `memory_links(from,to,link_type,entity_id,weight)`, `banks`, `documents`; observation nằm cùng bảng `memory_units` + bảng nguồn riêng.

### Recall (`engine/search/`, `memories/pg/recall.py`)
- 4 arm song song: semantic (vector), keyword (BM25), graph (`link_expansion.py`), temporal (parse cụm thời gian → cửa sổ, chọn **theo độ liên quan rồi rải đều theo bucket thời gian**, không theo recency).
- **RRF** `score = Σ 1/(60+rank)` (`fusion.py`) — chỉ dùng thứ hạng vì điểm các arm không so được.
- Graph score cộng: `tanh(0.5·n_shared_entities) + semantic_w + (causal_w + 1.0)`; cộng (không nhân) vì là các kênh bằng chứng độc lập; `tanh` chặn entity "user" fan-out.
- Cross-encoder rerank trên top 300 RRF; sigmoid chuẩn hoá. **Không có rerank → điểm giả rải [0.1,1.0] theo hạng RRF**, các boost vẫn chạy.
- Boost **nhân**, tâm 1.0: `1 + α(signal − 0.5)`. Recency α=0.2 (tuyến tính 365 ngày, sàn 0.1), temporal proximity α=0.2, proof_count α=0.1 (`0.5 + ln(n)/10`). Biên tổng ≈ +27% / −23% — chỉ "đẩy nhẹ", không lật relevance.
- Token budget (`max_tokens`, mặc định 4096): chọn top-down; mục quá dài thì **bỏ qua và đi tiếp**; không mục nào vừa → vẫn trả top-1. `budget` low/mid/high = 100/300/1000 ứng viên mỗi arm.

### Consolidation → Observation (`engine/consolidation/`)
- Chạy nền sau retain. Với mỗi lô fact mới: recall observation hiện có → **1 LLM call** trả `{creates, updates, deletes}`, mỗi op bắt buộc `reason`.
- Luật cốt lõi trong `prompts.py::_PROCESSING_RULES`: ưu tiên UPDATE hơn CREATE; một observation = một facet; khớp theo entity/facet chứ không theo chủ đề; state change cập nhật ngắn gọn kèm ngày; **cấm tính toán** (không tự cộng/trừ số đếm); không xoá observation ghi sự kiện lịch sử.
- Mâu thuẫn → observation **giữ cả hành trình** ("trước dùng React, nay chuyển Vue"), không ghi đè.
- Dedup near-duplicate: cosine ≥ 0.97 → LLM quyết merge/keep. Fusion riêng `interleave_fusion` cho bước này vì RRF làm rớt "bản sinh đôi" hạng 1 ở một arm.

### Reflect + Mental model (`engine/reflect/`)
- Agent vòng lặp ≤ 10 vòng, công cụ phân tầng: `search_mental_models` → `search_observations` (có cờ stale) → `recall` (fact thô) → `expand` → `done`. Có `directives` (luật cứng) và `disposition` (skepticism/literalism/empathy).
- Mental model: câu hỏi cố định + nội dung viết sẵn, rebuild khi có memory mới **trong scope của nó**, refresh **tăng dần** (delta ops) để văn bản không trôi, giữ lịch sử phiên bản + provenance.

## 3. SenClaw hiện có gì (đã đọc code/docs)

| Hệ | Code | Lưu | Mạnh | Thiếu so với Hindsight |
|---|---|---|---|---|
| FTS memory | `src/memory/{fts_search,manager,chunker}.rs` | `memory_files/chunks/chunks_fts`, `embedding_cache` | hybrid vector 0.7 + FTS 0.3, tokenizer VN, query rewrite | trộn điểm tuyến tính, không RRF, không budget token |
| Curated | `src/memory/curated.rs`, `consolidate.rs`, `mcp/memory_server.rs` | `profiles/<folder>/memory/*.md` + `MEMORY.md` | người đọc được, `memory_save/recall/delete`, consolidate khi compaction (≤3 note) | không bằng chứng/lịch sử, không dedup ngữ nghĩa, chỉ sống từ compaction |
| Cognitive | `src/memory/cognitive/*` (≈11k dòng) | `cog_nodes/edges/vec`, Hebbian, decay, `as_of`, supersede theo cardinality predicate | entity resolution + vector alias merge (0 token), graph k-hop, spreading, tiers LTP | **triplet** mất ngữ cảnh (why/when/who), không fact narrative, recall = 1 LLM tổng hợp |
| Prompt injection | `agent_pool/pool.rs` ~1800–1875 | 3 khối `<memory>`, `<cognitive_memory>`, `<memory_recall>` | không bao giờ làm hỏng lượt | **3 khối độc lập, mỗi khối tự xếp hạng, không chung budget**, dễ trùng nội dung |

Ràng buộc đã đo (từ `CLAUDE.md`): một tool call của chat model tốn **6–11 s**; pre-retrieval phải không LLM, không fail lượt. → ảnh hưởng thiết kế reflect (mục 4.5).

## 4. Thiết kế đề xuất

### 4.1 Kiến trúc (chọn A)

| | Cách làm | Ưu | Nhược |
|---|---|---|---|
| **A (khuyến nghị)** | Port ý tưởng sang Rust, SQLite, mô-đun `src/memory/engine/` | 1 binary, offline, dùng lại LLM slot/embedding/entity sẵn có, đúng quy ước repo | công sức lớn nhất |
| B | Chạy server Hindsight như một runtime | nhanh nhất để thử | Python + Postgres, nặng, lệch kiến trúc, 2 nguồn sự thật |
| C | Connector MCP tới Hindsight đang chạy (`/mcp/{bank}/`) | rẻ, tuỳ chọn | người dùng phải tự vận hành server |

C nên có như tiện ích tuỳ chọn ở phase cuối; không thay A.

### 4.2 Mô hình dữ liệu (SQLite, cùng DB với `memory::schema`)

```
mem_facts(id, bank, kind[world|experience|observation], text, context,
          occurred_start, occurred_end, mentioned_at, created_at, updated_at,
          proof_count, tags_json, doc_id, source_chunk, content_hash,
          consolidated_at, status)
mem_facts_fts   -- FTS5 trên text + tên entity (tokenizer VN sẵn có)
                -- embedding: cột BLOB trong mem_facts, quét chính xác như memory_chunks
                -- (docs/memory.md: chỉ nạp sqlite-vec khi > 50 000 hàng; vec_search đã ưu tiên bảng *_vec nếu có)
mem_fact_entities(fact_id, node_id)   -- node_id → cog_nodes (TÁI DÙNG entity resolution)
mem_links(from_id, to_id, type[semantic|temporal|causal], weight)
mem_obs_sources(obs_id, fact_id, quote)
mem_obs_history(obs_id, version, text, reason, changed_at)
mem_models(id, bank, name, question, content, scope_tags, refreshed_at, history)
```

- `bank` = chuỗi `NodeSet` hiện có (group/persona/cowork/global). Không thêm khái niệm bank mới.
- Entity **không** làm lại: link sang `cog_nodes`, hưởng luôn `merge_alias_entities`. Đây là điểm tiết kiệm lớn nhất so với port nguyên Hindsight.
- Backfill: chunk node của cognitive đã lưu văn bản gốc → chạy lại qua extractor như `re-extract-pending`, không cần dữ liệu mới.

### 4.3 Retain
1. Nguồn: cửa sổ reflection (hook `agent_pool/reflection.rs` đã có size/idle flush), tài liệu upload, tool `memory_retain`.
2. `sanitize_for_cognify` (tái dùng) → **defense**: regex secret/PII → `redact` hoặc `block` (mới, nhỏ).
3. LLM extractor qua `create_cognitive_llm` (slot `active_cognitive_id`, có thể là model local rẻ). Cần helper JSON tổng quát — `parse_triplets` hiện hard-code `triplets`.
4. Embed (kèm tiền tố ngày như Hindsight `augment_texts_with_dates`), dedup hash + cosine ≥ 0.95 → tăng `mentioned_at` thay vì chèn.
5. Link entity → `cog_nodes`; semantic kNN (≥0.7), temporal 24h, causal.
6. Đẩy hàng đợi consolidation (debounce theo bank).
- Prompt VN: giữ guidance tiếng Việt đã tune trong `cognify.rs`; thêm speaker attribution (dòng `X: …` đã có).

### 4.4 Recall hợp nhất (không LLM)
- Trait `RecallArm`; arm: `facts_vec`, `facts_bm25`, `facts_graph` (entity tanh + semantic + causal+1), `facts_temporal`, và **arm bọc store cũ**: `curated`, `cognitive`, `memory_md`.
- Fusion RRF k=60 → boost nhân (recency, temporal, proof) → pack theo `max_tokens` (ước lượng token cho VN/CJK).
- Parser thời gian VN/EN ("hôm qua", "tuần trước", "tháng 3", "last spring") → cửa sổ; `as_of` hiện có giữ nguyên.
- Rerank cross-encoder: **tuỳ chọn**; mặc định điểm giả theo hạng RRF (đúng cách Hindsight suy biến). Cần spike xem slot runtime (llama.cpp rerank) có khả thi.
- Đầu ra: một khối `<memory_context>` có mục *Beliefs* / *Facts* / *Notes*, chung một budget; `memory_recall` MCP nâng cấp tại chỗ (giữ tên, mở rộng schema trả về) + tham số `types`, `max_tokens`, `budget`.
- Toggle mới `memoryEngine` (cạnh `memoryRecall`, `preCognitive`); bật thì **không** phát `<cognitive_memory>`/`<memory_recall>` nữa để khỏi trùng.

### 4.5 Reflect — chủ ý lệch khỏi Hindsight
Hindsight reflect = tối đa 10 vòng LLM lồng nhau. Với SenClaw đó là nhiều chục giây trên đường tool. Thay bằng:
- **Mental model** (đọc sẵn, 0 LLM trên request path) bơm lúc bắt đầu phiên, tương tự `MEMORY.md`/SOUL hiện có.
- `memory_recall` có `types=[observation]` trước, fact thô sau; agent chính tự leo thang (đã là một LLM có tool).
- `memory_reflect` (1 lần: recall + tổng hợp có trích dẫn, như `cog` recall hiện tại) để sau, chỉ khi đo thấy cần.

### 4.6 Observation
- Port `_PROCESSING_RULES` gần nguyên văn (MIT, ghi attribution như precedent Fabric trong `assets/patterns`), nhưng **đổi UUID thành id ngắn `O1,O2…`/`F1,F2…`** để model nhỏ không chép sai.
- Validate op; op sai → bỏ cả phản hồi và rơi về "mỗi fact một observation" (không mất dữ liệu), `consolidated_at` vẫn NULL để thử lại.
- Chạy: debounce sau retain, idle, và trong `maintenance` sweep 24h sẵn có.

## 5. Phases (mỗi phase ship được, đo được)

| Phase | Nội dung | Schema mới | LLM mới | Giá trị |
|---|---|---|---|---|
| P0 | Bộ eval VN/EN + số baseline của stack hiện tại (`evals/cases`) | không | không | có thước đo |
| P1 | Recall hợp nhất trên 3 store cũ: RRF + boost + budget + 1 khối | không | không | sửa phân mảnh/trùng ngay |
| P2 | Fact store + retain + defense + 4 arm thật | `mem_*` | extractor | fact giữ ngữ cảnh, temporal |
| P3 | Observation + lịch sử + proof boost | thêm 2 bảng | consolidator | belief tiến hoá, bớt trùng |
| P4 | Mental model + directives + bơm lúc khởi phiên | `mem_models` | refresh | tri thức ổn định, 0 latency |
| P5 | Reflect 1-lần, rerank runtime, connector Hindsight MCP, UI, docs/openapi | — | tuỳ | hoàn thiện |

## 6. Rủi ro

- **Chất lượng extractor local (gemma-4-e4b…) trên tiếng Việt**: đã thấy model nhỏ arm watch sai; consolidation JSON phức tạp hơn nhiều. Giảm: schema phẳng, id ngắn, parser dung sai, fallback không-merge, **đo ở P0/P2 trước khi cam kết P3**.
- **Chi phí token**: thêm ~1 call/cửa sổ. Hiện ≈1k token/cửa sổ cho triplet. Nếu extractor fact cũng trả entity thì có thể bỏ triplet cho chat (giữ cho tài liệu) → ngang chi phí cũ.
- **Hai nguồn sự thật** (triplet vs fact) nếu chạy song song lâu. Cần quyết định dài hạn.
- **Vector search là quét chính xác có chủ đích** (`docs/memory.md` "Vì sao chưa dùng sqlite-vec": ngưỡng 50 000 hàng, đo 2026-09-12 chỉ 10 vector). Fact nhiều hơn chunk nhiều lần nên cần đo lại sau P2; chuyển `vec0` khi vượt ngưỡng, không đổi call site.
- **Quyền riêng tư**: chat → LLM extractor đã là hành vi hiện tại của reflection; defense chỉ thu hẹp, không thay.
- Working tree `senclaw` đang có thay đổi chưa commit từ việc khác (branch `feat/datagen-import`) — triển khai nên ở worktree riêng.

## 7. Câu hỏi chưa giải quyết

1. Chọn kiến trúc A (port Rust) — đồng ý?
2. Fact extractor có thay triplet cho luồng chat (tiết kiệm token) hay chạy song song một thời gian?
3. Eval: dùng tập hội thoại VN tự dựng + LongMemEval dịch, hay chỉ tự dựng?
4. Rerank cross-encoder: có chấp nhận thêm một runtime/model (bge-reranker GGUF) không, hay dừng ở RRF + boost?
5. Nội dung video "phần nhanh nhất trả tiền" — bạn nhớ đó là phần nào (cloud, rerank, hay tính năng khác)?
