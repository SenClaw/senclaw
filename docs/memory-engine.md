# Memory engine — một luồng ghi, một luồng đọc

> Trạng thái: **đã thay hoàn toàn luồng cũ** (2026-10-16). Không còn công tắc bật/tắt engine.
> Kế hoạch: [`plans/261016-1441-hindsight-memory-engine/plan.md`](../plans/261016-1441-hindsight-memory-engine/plan.md);
> nghiên cứu gốc: [`plans/reports/research-261016-1441-hindsight-memory-for-senclaw.md`](../plans/reports/research-261016-1441-hindsight-memory-for-senclaw.md).
> Code: [`src/memory/engine/`](../src/memory/engine/).

## Trước và sau

| | Trước | Sau |
|---|---|---|
| Ghi từ hội thoại | cửa sổ reflection → **triplet** vào đồ thị | cửa sổ reflection → **fact** vào kho fact (cùng một lần gọi model) |
| Ghi khi compaction | LLM chưng cất tóm tắt thành ≤ 3 note `memory/*.md` (hoặc lưu nguyên văn) | tóm tắt được **retain thành fact**, cùng bộ trích xuất |
| Đọc mỗi lượt | 3 khối riêng (`<memory>`, `<cognitive_memory>`, `<memory_recall>`), mỗi khối tự xếp hạng, không chung ngân sách, **dán vào tin nhắn người dùng và nằm lại trong lịch sử** | **một** khối `<memory_context>` (RRF + ngân sách), đưa cho model như **ngữ cảnh của lượt**, không lưu vào lịch sử |
| Secret trong chat | đi thẳng vào model trích xuất và đồ thị | bị quét trước khi tới model |
| Xem / xoá | chỉ qua trang Knowledge (đồ thị) | `GET /api/memory/facts`, `DELETE /api/memory/facts/:id`; agent tự tra / quên bằng `memory_search_facts`, `memory_forget_fact` |
| Người dùng đổi ý | không có: hai phiên bản cùng nằm trong kho | fact mới **thay** fact cũ (cũ bị ẩn, giữ lịch sử, xoá fact mới thì cũ sống lại) |

## Ghi: nguồn → retain → kho fact

```text
cửa sổ hội thoại ─┐   (đủ 2000 ký tự hoặc im 2 phút)
                  ├─► quét secret → chunk → 1 lần gọi model/chunk → parse → embed
tóm tắt compaction ┘                         → bỏ cái đã biết (hash / cosine ≥ 0.95) → lưu → liên kết
```

**Fact** là một–hai câu tự đủ nghĩa, có ngày và các thực thể nó nhắc tới: "Emily chuyển đến Huế vào tháng 3 năm 2026 và làm việc
tại FPT." (triplet giữ *ai–gì–gì* và mất *vì sao / khi nào*). Code: [`facts/`](../src/memory/engine/facts/).

| Phần | File | Ghi chú |
|---|---|---|
| Lưu trữ | [`store.rs`](../src/memory/engine/facts/store.rs) | `mem_facts`, `mem_facts_fts`, `mem_fact_entities`, `mem_links` — chỉ thêm (`CREATE … IF NOT EXISTS`), không migrate gì cũ. Vector = BLOB, quét chính xác như `memory_chunks`. Phạm vi `bank` = agent folder |
| Quét secret | [`defense.rs`](../src/memory/engine/facts/defense.rs) | key/token/private key/thẻ (Luhn) → `[REDACTED:<loại>]`. Email, số điện thoại, tên **không** bị xoá |
| Trích xuất | [`extract.rs`](../src/memory/engine/facts/extract.rs) | prompt lấy từ Hindsight (chọn lọc, ngày tuyệt đối, giải đại từ, `world` vs `experience`) + hướng dẫn tiếng Việt; JSON phẳng 4 trường cho model nhỏ; parser chịu được fence/prose/mảng trần |
| Retain | [`retain.rs`](../src/memory/engine/facts/retain.rs) | mọi lỗi chỉ đếm và log (`RetainReport`), không bao giờ ném |
| Nguồn | [`reflection.rs`](../src/agent/agent_pool/reflection.rs), handler `compact:exec` trong `pool.rs` | `retain_text(config, folder, text, source)` với `source` = `reflection` \| `compaction` |

Model trích xuất lấy từ slot `active_cognitive_id` (rơi về model chat đang dùng). Mỗi cửa sổ = **một** lần gọi.

### Đổi ý: fact mới thay fact cũ

Trước khi gọi model, mỗi chunk tìm tối đa 8 fact đã có liên quan (keyword) và đưa vào prompt dưới `KNOWN FACTS:` thành `[F1] …`.
Nếu một fact mới mâu thuẫn một mục trong danh sách, model trả thêm `"replaces": ["F1"]`. Retain **không tin mù**:

- số phải nằm trong danh sách của chunk đó (số tự bịa, `0`, số không thấy → bỏ qua);
- hai fact phải cùng chủ đề: chung một thực thể không chung chung (`user`, `người dùng`, `assistant`… không tính), hoặc chung ≥ 40% từ khoá
  (mỗi bên ≥ 3 từ). Model gạt nhầm "Hà Nội" thành thay "thích React" thì không có gì xảy ra;
- fact cũ chỉ được ẩn **sau khi** fact mới đã được lưu.

Fact bị thay không bị xoá: `superseded_by` trỏ tới fact mới, mọi arm đọc bỏ qua nó, `GET /api/memory/facts?history=true` vẫn thấy. Xoá fact
mới (trigger `mem_facts_restore`) thì fact cũ **sống lại** — sửa nhầm một lần đổi ý chỉ cần quên fact mới. Nhắc lại cùng một điều
(trùng hash hoặc cosine ≥ 0.95) thì không thêm hàng mà tăng `proof_count`; boost nhân `1 + 0.1·(0.5 + ln n / 10 − 0.5)` (tối đa +5%) —
bằng chứng chỉ đảo được thứ tự của các hạng sát nhau, không kéo một kết quả xa lên đầu.

## Đọc: một khối, không gọi model

```text
arms (song song) → RRF (k=60) → gộp bản trùng → boost recency → ngân sách token → <memory_context>
```

| Arm | Đọc | Bật khi |
|---|---|---|
| `curated` | note `memory/*.md` (hybrid FTS/vector; bỏ note của chat khác) | `memoryRecall` (mặc định bật) |
| `cognitive` | đồ thị tri thức (spreading, rơi về keyword khi rỗng) | `preCognitive` (mặc định tắt) |
| `facts_semantic` | fact gần nghĩa | có embedding provider |
| `facts_keyword` | fact chung từ (BM25) | luôn |
| `facts_graph` | fact nối với kết quả tốt nhất qua thực thể chung (`tanh(0.5·n)`) và liên kết | luôn |
| `facts_temporal` | fact thuộc khoảng thời gian nêu trong câu hỏi ("hôm qua", "tháng 3", "31/07") | câu hỏi có thời gian |

Fusion: [`fusion.rs`](../src/memory/engine/fusion.rs) cộng `1/(60+rank)` theo danh tính `(arm, key)` trên mọi danh sách, rồi gộp
hai kết quả chồng nhau ≥ 0.85 (overlap coefficient, ≥ 6 từ). [`boost.rs`](../src/memory/engine/boost.rs): điểm liên quan = hạng rải
trong [0.1, 1.0] × `1 + 0.2·(recency − 0.5)` × `1 + 0.1·(proof − 0.5)`. [`budget.rs`](../src/memory/engine/budget.rs): lấy từ trên xuống, mục quá dài bị
bỏ qua. [`format.rs`](../src/memory/engine/format.rs): khối đánh số, mỗi mục ghi nguồn `(note)` / `(knowledge)` / `(fact)`; chuỗi
`</memory_context>` trong dữ liệu bị vô hiệu. Một arm lỗi hoặc quá 3 s chỉ mất phần của nó.

**`MEMORY.md` không phải arm.** Nó là ngữ cảnh ghim (chỉ dẫn thường trực); khối `<memory>` (env `memory.pre_retrieval`) đọc nguyên
văn như trước, cùng đi vào ngữ cảnh của lượt.

## Memory, profile và context cập nhật thế nào

| | Chứa gì | **Ghi** khi nào, bởi ai | **Đọc** khi nào | Sống bao lâu |
|---|---|---|---|---|
| **Kho fact** (memory tự động) | sự thật có ngày rút ra từ hội thoại | mỗi cửa sổ flush + mỗi lần compaction; model trích xuất; secret bị quét trước | **mỗi lượt**, qua 4 arm, trong ngân sách 600 từ | bền (SQLite); xoá bằng `DELETE /api/memory/facts/:id` |
| **Note curated** + `MEMORY.md` | quyết định, ghi chú có chủ đích | agent gọi `memory_save` hoặc người sửa tay. **Không còn tự sinh từ compaction** | note: mỗi lượt nếu `memoryRecall`; `MEMORY.md`: nguyên văn nếu `memory.pre_retrieval` | bền (file) |
| **Đồ thị tri thức** | tài liệu upload, `cog_add`, SOUL của agent | upload / `cog_add` / ingest SOUL. **Không còn từ chat** | arm `cognitive` nếu `preCognitive` | bền, mờ dần theo decay |
| **Profile** `USER.md` | người dùng là ai + chỉ dẫn thường trực (field, directive) | agent gọi `profile_update` khi người dùng nêu sự thật bền / sở thích; Settings UI. **Engine không tự ghi** | **một lần mỗi phiên** trong ngữ cảnh lượt đầu, lọc tier (chat nhóm chỉ thấy public) | bền (file); directive cũ bị `superseded` |
| **Lịch sử hội thoại** | các lượt của phiên | thêm mỗi lượt; ở 75% cửa sổ thì LLM tóm tắt và thay lịch sử bằng tóm tắt | mọi lần gọi model | trong bộ nhớ của phiên; tin nhắn gốc cũng nằm trong DB |
| **Ngữ cảnh của lượt** (recall) | `<memory_context>` (+ `<memory>`) | **tính lại mỗi lượt** từ câu hỏi mới | gắn vào mỗi lần gọi model *trong lượt đó* | **không lưu vào lịch sử** |

Ví dụ: người dùng nói *"Tôi tên là Sen, từ giờ luôn trả lời bằng tiếng Việt."*

1. **Profile** — agent gọi `profile_update` (`name = Sen`; directive "Luôn trả lời bằng tiếng Việt"). `USER.md` đổi ngay; phiên hiện tại biết nhờ kết quả tool nằm trong lịch sử, các phiên sau nhận qua ngữ cảnh lượt đầu.
2. **Kho fact** — khi cửa sổ flush, model trích xuất ra fact "Người dùng tên là Sen." (không nhờ agent nhớ gọi tool). Từ đó câu hỏi "tôi tên gì" tìm thấy nó.
3. Hai nơi trùng một phần là **chủ ý**: profile nhỏ, luôn có mặt, có tier, ghi đè được (`supersedes`); fact nhiều, có ngày, chỉ hiện khi liên quan.
4. **Ngữ cảnh của lượt** — lượt kế tiếp recall tính lại; kết quả của lượt này không còn trong lịch sử.

Khi compaction chạy, tóm tắt thay lịch sử **và** được retain thành fact: quyết định và việc đã làm vẫn tìm lại được sau khi ngữ cảnh bị cắt.

**Giới hạn hiện tại**: người dùng đổi ý thì fact cũ bị ẩn (xem "Đổi ý") — nhưng chỉ khi model trích xuất nhận ra và chỉ đúng mục; model nhỏ
bỏ sót thì hai phiên bản cùng tồn tại, bản mới hơn được boost nhẹ. Profile **không** được tự cập nhật từ fact (cố ý: profile có tier và quy tắc
chat nhóm chỉ-đọc, ghi tự động từ hội thoại nhiều người là rủi ro chèn lệnh).

## Cấu hình

| | |
|---|---|
| Kho nào được đọc | `memoryRecall` (note, mặc định bật) và `preCognitive` (đồ thị, mặc định tắt) qua `GET`/`POST /api/agent-behavior`; kho fact luôn đọc. Cả hai công tắc được đọc **một lần** mỗi lượt |
| Tắt trích xuất tự động | `SENCLAW_COGNITIVE_REFLECTION=false` (Settings → Knowledge "Auto-reflect"). `SENCLAW_COGNITIVE_ENABLED=false` **không còn** tắt nó — công tắc đó giờ chỉ điều khiển đồ thị |
| Ngân sách | `SENCLAW_MEMORY_ENGINE_MAX_TOKENS` (mặc định 600, tối thiểu 50). **Đơn vị là từ** (`chunker::estimate_tokens`); token thật cao hơn (~1.3× tiếng Anh, ~2× tiếng Việt) |
| Số kết quả mỗi arm | `SENCLAW_SEARCH_MAX_RESULTS` (mặc định 5) |
| Kích thước / nhịp cửa sổ | `SENCLAW_COGNITIVE_REFLECT_{MIN,MAX}_CHARS`, `_COOLDOWN_MS`, `_WINDOW_IDLE_MS` (giữ nguyên tên cũ) |
| Xử lý secret | `SENCLAW_MEMORY_ENGINE_DEFENSE` = `redact` (mặc định) \| `block` (bỏ cả cửa sổ) \| `off`. Giá trị lạ → `redact` |

## Xem và xoá

- `GET /api/memory/facts?bank=<folder>&q=<từ khoá>&limit=&history=` — mới nhất trước, hoặc khớp từ khoá; `limit` 1–200; `history=true` thêm cả fact đã bị thay (kèm `supersededBy`). Mỗi fact có `proofCount`.
- `DELETE /api/memory/facts/:id?bank=<folder>` — quên một fact. Sai `bank` thì 404, không xoá được fact của agent khác.
- Cho agent (server `senclaw-memory`, trong chat là `mcp__core__memory_*`): `memory_search_facts` (`query`, `limit`; không có `query` thì liệt kê mới nhất; câu hỏi có thời gian "hôm qua" cũng tra theo ngày) và `memory_forget_fact` (`id` đầy đủ, chỉ fact của chính agent). MCP chạy tiến trình riêng nên tra bằng từ khoá và ngày, không có vector.

## Chạy eval

```bash
cargo test --lib memory::engine::eval -- --nocapture --test-threads=1
MEMORY_EVAL_OUT=/tmp/mem cargo test --lib memory::engine::eval   # → /tmp/mem.{vectors,keyword-only}.json
```

[`evals/memory/cases.json`](../evals/memory/cases.json): 9 note (một note của chat khác), 8 chunk đồ thị, 11 câu hỏi VN/EN; và cho kho
fact 7 cửa sổ hội thoại chạy qua **retain thật** (chỉ câu trả lời của model trích xuất là dựng sẵn) + 7 câu hỏi chỉ fact trả lời được,
gồm một ca đổi ý (React → Vue: phải thấy Vue và **không** thấy "thích dùng React"; `replaces_matching` trong fixture đóng vai model đọc
`KNOWN FACTS` và chỉ đúng dòng — đổi cụm đó thành cụm không khớp thì eval đỏ).
Luồng cũ giữ lại **chỉ trong test** ([`legacy.rs`](../src/memory/engine/legacy.rs), `#[cfg(test)]`) làm mốc so sánh. Cổng: không rò rỉ;
engine lặp ≤ luồng cũ; có vector thì trả lời ≥ luồng cũ, không vector thì **>**; fact 7/7 so với 0/7; arm graph và temporal mỗi cái trả
lời một câu mà keyword một mình không trả lời được; secret không tới model trích xuất. Số liệu:
[`plans/reports/eval-261016-1535-memory-recall-baseline.md`](../plans/reports/eval-261016-1535-memory-recall-baseline.md).

## Bẫy

- **Spreading cần embedding provider.** Không có provider (cấu hình mặc định) thì `preCognitive` cũ tiêm **rỗng**; `CognitiveArm` rơi về hybrid/keyword.
- **Chat không còn nuôi đồ thị.** Trang Knowledge chỉ còn dữ liệu upload / `cog_add` / SOUL; fact thì xem qua `/api/memory/facts` (chưa có giao diện).
- **Ngân sách đếm từ**, đừng so với context window như thể là token thật.
- **Phạm vi riêng tư của fact = agent folder**, không theo chat (cửa sổ reflection không mang chat jid). Note curated thì có lọc theo chat.
- **Chất lượng model trích xuất nhỏ trên tiếng Việt chưa đo** (eval dùng câu trả lời dựng sẵn), kể cả việc nó có chỉ đúng `replaces` hay không.
- **Số trong `replaces` là số của đúng chunk đó.** `[F1]` của chunk này không phải `[F1]` của chunk khác; code đọc theo danh sách của chunk.
- **Route gộp từ module khác phải có mặt trong `ROUTER_SOURCES`** ([`openapi.rs`](../src/gateway/ui_server/openapi.rs)), nếu không test spec không thấy route và `docs/openapi-daemon.yaml` lệch.
- Ngày của note lấy từ `createdAt` (nửa đêm UTC); thiếu hoặc sai định dạng thì trung tính.
