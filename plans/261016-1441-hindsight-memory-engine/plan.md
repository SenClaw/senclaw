# Plan: memory engine lấy ý từ Hindsight

Status: P0–P2 done, old flow replaced, belief revision + proof boost + agent fact tools done; observations/mental models (P3 rest, P4–P5) pending · Ngày: 2026-10-16 · Nhánh: `feat/memory-engine` (worktree `.worktrees/senclaw-memory-engine`, cắt từ `feat/datagen-import` @ fbe6230)
Nguồn: [research-261016-1441-hindsight-memory-for-senclaw.md](../reports/research-261016-1441-hindsight-memory-for-senclaw.md)

## Outcome

Agent có một lớp nhớ hợp nhất trên 3 hệ sẵn có (FTS, curated, cognitive): ghi được **fact tự đủ nghĩa** có thời gian, gộp thành **belief có bằng chứng**, và truy hồi qua **một** khối `<memory_context>` có chung token budget — không thêm LLM call trên đường request.

## Non-goals

- Không nhúng server Hindsight (Python/Postgres).
- Không thay hay xoá cognitive graph, curated `.md`, `MEMORY.md`; chúng tiếp tục chạy và là arm của recall.
- Không reflect nhiều vòng LLM lồng trong tool.
- Không đổi tên/đổi chữ ký tool MCP đang có (`memory_search/get/save/recall/delete`); chỉ mở rộng.

## Acceptance (tổng)

- Bộ eval P0: recall@5 của engine ≥ stack cũ trên tập VN + EN, mỗi phase không tụt.
- Pre-retrieval p95 không tăng quá 150 ms so với 3 khối cũ; **0 LLM call** trên đường này.
- Mọi lỗi retain/consolidate chỉ log; lượt chat không bao giờ fail.
- Tắt `memoryEngine` → hành vi y như hiện tại.

## Quyết định của người dùng

1. Kiến trúc A (port Rust trên SQLite) — **đã chọn 2026-10-16**.
2. Fact extractor thay triplet cho chat hay chạy song song — **đã chọn thay hẳn (2026-10-16, người dùng: "thay thế hoàn toàn luồng cũ")**. Triplet không còn được nuôi từ chat; đồ thị chỉ còn upload / `cog_add` / SOUL.
3. Chấp nhận rerank runtime hay dừng ở RRF + boost — còn mở, chỉ ảnh hưởng P5.

## Phases

| # | Tên | Trạng thái | Phụ thuộc |
|---|---|---|---|
| P0 | Eval + baseline | done | — |
| P1 | Recall hợp nhất trên store cũ | done | P0 |
| P2 | Fact store + retain + defense | done | P1 |
| P3 | Observation | partly done (belief revision, proof boost); observation layer pending | P2 |
| P4 | Mental model + directives | pending | P3 |
| P5 | Reflect 1-lần, rerank, connector, UI, docs | pending | P1–P4 |

### P0 — Eval + baseline
- Làm: bộ case hội thoại VN/EN (fact đơn, đa lượt, thời gian tương đối, mâu thuẫn/đổi ý, câu hỏi nhiều bước) vào `evals/cases/`; chạy stack hiện tại, ghi recall@k, độ trùng giữa 3 khối, độ trễ pre-retrieval.
- Xong khi: có file số baseline + lệnh chạy lại.
- Rủi ro: tập case lệch → đo sai. Giữ case do người dùng duyệt.
- **Đã làm**: `evals/memory/cases.json` (11 câu VN/EN) + `src/memory/engine/eval.rs` chạy trong `cargo test --lib`, chấm khối cũ và engine trên cùng kho; số liệu ở `plans/reports/eval-261016-1535-memory-recall-baseline.md`. Chạy như test chứ không phải `scripts/evals/run.py` vì đó là eval agent-task cần model thật.
- **Phát hiện**: spreading activation chỉ gieo hạt bằng vector → không có embedding provider (mặc định) thì `preCognitive` tiêm rỗng. Eval vì thế dùng embedder băm từ cho đồ thị.
- **Chưa phủ**: fixture quá dễ để phân biệt tỉ lệ trả lời (cả hai 11/11); chỉ phân biệt được lặp và kích thước. Cần case khó hơn (nhiều nhiễu, ngân sách chặt) trước khi dùng nó để chứng minh P2+.

### P1 — Recall hợp nhất trên store cũ (không schema, không LLM)
- Mới: `src/memory/engine/{mod,arms,fusion,boost,budget,format,recall,legacy,eval}.rs`. Trait `RecallArm`; arm bọc `curated` (qua `MemoryManager::search` source `memory`) và `cognitive` (spreading, rơi về hybrid khi rỗng).
- **Lệch kế hoạch (có chủ đích)**: `MEMORY.md` *không* là arm — là ngữ cảnh ghim, truy hồi theo câu hỏi có thể bỏ sót chỉ dẫn thường trực; khối `<memory>` giữ nguyên. `memory_recall` MCP **chưa** đổi: server MCP chạy ở tiến trình riêng, không với tới `CognitiveSystem` của daemon; làm ở P5 qua REST nội bộ.
- RRF k=60 → boost nhân (recency α .2/365d, sàn .1) → pack token (bỏ mục quá dài rồi đi tiếp; rỗng thì trả top-1).
- Sửa: `agent_pool/pool.rs` phát `<memory_context>` khi `memoryEngine` bật, bỏ hai khối kia; `GlobalConfig.memory_engine` + `agent_behavior_config.rs` + `MemoryConfig.engine_max_tokens`. Hàng toggle web/desktop nằm ở hai repo anh em (`web-app`, `desktop`) — **chưa làm**.
- Test: fusion thuần (ví dụ 1/61+1/65), packing biên, arm lỗi không kéo sập, no-LLM.
- Xong khi: eval ≥ baseline, p95 trong ngân sách.
- **Kết quả**: xem báo cáo eval. Lệnh: `cargo test --lib memory::engine::eval -- --nocapture`.

### P2 — Fact store + retain
- Mới: `engine/{schema,types,defense,extract,retain,links}.rs`; bảng `mem_facts`, `mem_facts_fts`, `mem_fact_entities`, `mem_links` (embedding BLOB, quét chính xác).
- Extractor: prompt port từ Hindsight concise + guidance VN của `cognify.rs`; helper JSON tổng quát cạnh `parse_triplets`; model qua `create_cognitive_llm`.
- Hook: consumer thứ hai của flush trong `agent_pool/reflection.rs`; `memory_retain` MCP; backfill từ chunk node cognitive.
- Defense: regex secret/PII (`redact` | `block`), có test từng mẫu.
- Recall: thêm arm `facts_vec`, `facts_bm25`, `facts_graph`, `facts_temporal` + parser thời gian VN/EN.
- Xong khi: eval các case thời gian/đa lượt cải thiện; secret trong chat không vào DB.
- Rủi ro: chất lượng model nhỏ tiếng Việt → đo trước, có fallback bỏ fact lỗi.
- **Đã làm** (`src/memory/engine/facts/`): `store` (4 bảng `mem_*`), `defense` (quét secret, 3 chế độ), `extract` (prompt + parser chịu lỗi), `retain`, `temporal` (đọc ngày VN/EN), `arms` (semantic / keyword / graph / temporal), `FactEngine` + khởi tạo lúc boot; móc vào `reflect_window_text` (quét secret một lần cho cả triplet lẫn fact). Eval: không fact 0/6 → có fact 6/6 ở cả hai chế độ; arm graph và temporal mỗi cái trả lời một câu mà keyword một mình không trả lời được.
- **Lệch kế hoạch (có chủ đích)**: (1) thực thể lưu dạng khoá chữ thường trong `mem_fact_entities`, **chưa** liên kết sang `cog_nodes` nên chưa hưởng gộp alias của đồ thị; (2) chưa có liên kết nhân quả (cần model trả thêm trường); (3) `memory_retain` MCP chưa làm — cần model của daemon, MCP chạy tiến trình riêng, đi cùng REST ở P5; (4) chưa backfill từ chunk đồ thị; (5) fact giới hạn theo agent folder chứ chưa theo chat.
- **Chưa đo**: chất lượng trích xuất của model nhỏ thật trên tiếng Việt (eval dùng câu trả lời dựng sẵn). Cần chạy thử một lần trên máy thật trước khi bật `memoryEngine` thường trực.

### P3 — Observation
- Mới: `engine/observe.rs`, `mem_obs_sources`, `mem_obs_history`; prompt port `_PROCESSING_RULES` (ghi attribution MIT), id ngắn `O#`/`F#`, validate op, fallback không-merge, `consolidated_at` để thử lại.
- Lịch: debounce sau retain + idle + maintenance 24h.
- Boost `proof_count` (α .1, `0.5+ln n/10`). **Done** (`boost.rs`).
- **Đã làm thay cho tầng observation riêng**: belief revision ngay trong retain — chunk nhìn thấy `KNOWN FACTS` (≤ 8, keyword), model trả `replaces`, retain kiểm số + `same_subject` rồi ẩn fact cũ (`superseded_by`), xoá fact mới thì cũ sống lại; nhắc lại → `proof_count`. Eval có ca đổi ý React → Vue chạy qua retain thật. Lệch kế hoạch: không có bảng `mem_obs_*` / bước consolidate riêng — một lần gọi model/chunk vẫn là một, và lịch sử nằm ngay trên hàng fact. Quyết định còn mở: có cần tổng hợp nhiều fact thành một observation hay không (chỉ làm nếu đo thấy fact rời rạc không đủ).
- Xong khi: case đổi ý cho belief giữ lịch sử; eval không tụt; số observation trùng đo được và thấp.

### P4 — Mental model + directives
- Mới: `engine/models.rs`, `mem_models`; câu hỏi cố định, rebuild khi có memory mới trong scope, refresh tăng dần; bơm đầu phiên cạnh `MEMORY.md`.
- Directives: luật cứng của người dùng, luôn đứng đầu khối.
- Xong khi: đọc model = 0 LLM; model cũ bị đánh dấu stale, không đè retrieval.

### P5 — Hoàn thiện
- `memory_reflect` 1-lần (nếu đo thấy cần), rerank runtime (spike llama.cpp rerank), connector MCP tới Hindsight, UI Settings → Memory (xem fact/observation + bằng chứng) trên web + desktop + channel_app, REST `/api/memory-engine/*` + `docs/openapi-daemon.yaml`, `docs/memory-engine.md`, mục `CLAUDE.md`.

## P6 — Hợp nhất Knowledge vào Facts (đề xuất, 2026-10-17)

Nghiên cứu + luồng update + lộ trình H0–H7: [research-261017-2030-unify-knowledge-into-facts.html](../reports/research-261017-2030-unify-knowledge-into-facts.html). Hướng khuyến nghị: **Facts + Sources** (một kho, một đường ghi; đồ thị chỉ còn là cách nhìn). **H1 đã làm** (2026-10-17): slot đơn trị cho supersession, eval dựng sẵn + extractor thật + `scripts/evals/memory-changed-mind.mjs`; số đo ở `plans/reports/eval-261017-2145-slot-supersession-real-model.md`. Chưa quyết: 6 câu hỏi mở ở cuối tài liệu.

## Quy ước phải giữ

- `snake_case.rs`; `anyhow::Result`; truy cập DB qua `Db::with_conn`; không `env::var` trong thư viện (đi qua `Config`).
- Tên MCP: server `senclaw-memory`, tiền tố `memory_`; trong agent hiện là `mcp__core__memory_*`.
- Route axum 0.7 dùng `:name`.
- Không đặt mã phase/mã audit vào tên test, comment, commit.

## Thay luồng cũ (2026-10-16)

Người dùng chọn thay hoàn toàn luồng cũ và tối ưu. Đã làm:

- **Đọc**: bỏ công tắc `memoryEngine`; `pre_retrieval` luôn là engine. Bỏ `cognitive_pre_retrieval` / `curated_pre_retrieval`; `engine::legacy` chỉ còn trong test làm mốc.
  Khối recall đi vào **ngữ cảnh của lượt** (`set_turn_context` → `outgoing_messages`), không dán vào tin nhắn người dùng nên không nằm lại trong lịch sử.
- **Ghi**: reflection chỉ còn trích xuất fact (một lần gọi/cửa sổ, trước là triplet + fact); compaction retain tóm tắt thành fact, bỏ `memory/consolidate.rs`
  (chưng cất thành note) và phần `mark_dirty` cho daily log đã tắt từ lâu.
- **Hiệu quả**: hai công tắc đọc một lần (trước: mỗi công tắc một lần đọc + parse cả file settings); câu hỏi embed một lần cho cả semantic và graph.
- **Quyền kiểm soát**: `GET /api/memory/facts`, `DELETE /api/memory/facts/:id` — vì fact được rút ra tự động nên phải xem và xoá được.
- **Đổi hành vi cần biết**: chat không còn nuôi đồ thị (trang Knowledge chỉ còn upload / `cog_add` / SOUL); `SENCLAW_COGNITIVE_ENABLED=false` không còn tắt trích xuất hội thoại
  (dùng `SENCLAW_COGNITIVE_REFLECTION=false`); `memoryRecall` giờ chỉ bật/tắt recall note, không còn điều khiển chưng cất khi compaction.
- **Không đổi, có chủ đích**: profile `USER.md` không do engine ghi (xem CLAUDE.md).
