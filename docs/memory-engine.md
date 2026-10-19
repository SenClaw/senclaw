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
| Trích xuất | [`extract.rs`](../src/memory/engine/facts/extract.rs) | prompt lấy từ Hindsight (chọn lọc, ngày tuyệt đối, giải đại từ, `world` vs `experience`) + hướng dẫn tiếng Việt; JSON phẳng 4 trường cho model nhỏ; parser chịu được fence/prose/mảng trần, đọc **giá trị JSON hoàn chỉnh đầu tiên** (thừa `}` ở cuối thì bỏ qua) và bù `]`/`}` model quên đóng (ở cuối, hoặc trước dấu đóng của vòng ngoài khi `]` của mảng bị rơi) — trừ khi câu trả lời dừng giữa một chuỗi (câu bị cắt nửa chừng không được lưu). Một phần tử hỏng trong mảng (khoá lặp `{"text{"text":…`) chỉ làm mất chính nó: mảng không đọc được thì từng phần tử được parse riêng, phần tử hỏng bị bỏ và ghi một cảnh báo `kept`/`dropped`; câu trả lời đó **không** tính vào `failed_chunks` (chỉ câu không đọc được phần tử nào, hoặc mảng không được đóng vì bị cắt, mới tính) |
| Retain | [`retain.rs`](../src/memory/engine/facts/retain.rs), [`restated.rs`](../src/memory/engine/facts/restated.rs) | mọi lỗi chỉ đếm và log (`RetainReport`), không bao giờ ném; `restated.rs` nhận ra fact model chỉ nhắc lại (xem dưới) |
| Nguồn | [`reflection.rs`](../src/agent/agent_pool/reflection.rs), handler `compact:exec` trong `pool.rs` | `retain_text(config, &FactTarget { bank, origin }, text, source)` với `source` = `reflection` \| `compaction` \| `kanban` (summary của card, xem "Project"); đích do `projects::fact_target` chọn (bank của profile gắn với chat, hoặc `project:<id>` không gắn chat) |

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

### Slot: thay bằng luật, không cần model chỉ số thứ tự

Model nhỏ hiếm khi trỏ đúng "F1" trên đường chat thật (đo: 0/2 lượt đổi ý, trong khi trên cửa sổ dựng sẵn được 15/18). Vì vậy mỗi fact có thể
mang thêm **slot** = `(subject, predicate)` — thuộc tính nào của ai ("user · lives_in", "user · favorite_frontend_framework"). Model trả
`subject` và `predicate` tuỳ chọn trong cùng một lần gọi; [`slots.rs`](../src/memory/engine/facts/slots.rs) chuẩn hoá (mọi cách gọi người dùng →
`user`; `Lives in`/`resides_in` → `lives_in`; `favourite x` → `favorite_x`).

- **Chỉ predicate đơn trị mới thay thế**: danh sách hạt giống mang từ bảng `cog_predicate_meta` của đồ thị (`name`, `lives_in`, `works_at`,
  `job_title`, `status`, `price`…) và các lựa chọn đặt tên `favorite_*` / `preferred_*` / `current_*` / `primary_*` / `main_*` / `default_*` (số nhiều như
  `favorite_foods` coi là đa trị). `likes`, `knows`, `uses`… và predicate lạ coi là **đa trị**: giữ một fact cũ thì cứu được, ẩn nhầm một fact đúng thì không.
- Fact mới thay mọi fact **hiện hành cùng slot** mà chat đó được thấy (cùng `bank`, cùng phạm vi chat — không bao giờ thay fact của chat khác), sau khi fact mới đã được lưu.
- **Câu về thời điểm sớm hơn không thay**: "năm 2025 tôi làm ở FPT" nói sau "tôi đang làm ở VNG" là lịch sử, không phải đổi việc (so mốc thời gian của fact mới với fact cũ).
- Prompt cho model thấy slot của từng `KNOWN FACTS` (`[F1] <user · lives_in> …`) và dặn dùng **đúng** predicate đó khi nêu giá trị mới; có ví dụ đổi ý và ví dụ chuyển nhà
  không nêu `replaces`. `replaces` vẫn dùng, hai đường cùng chỉ một fact thì tính một lần. Log retain có `superseded_by_slot`.
- Model không nêu slot thì không có gì thay đổi so với trước (không đoán slot từ câu chữ).

### Model nhắc lại fact đã biết

Prompt cấm nhắc lại `KNOWN FACTS`, nhưng model nhỏ vẫn làm, thường bằng câu chữ khác ("User" thay "Người dùng", thêm "đã"). Có embedding thì vector bắt được;
cài đặt mặc định (không embedding) chỉ bắt được trùng nguyên văn, nên bản diễn đạt lại thành một fact mới. Trước khi lưu, [`restated.rs`](../src/memory/engine/facts/restated.rs)
so fact mới với đúng các dòng `KNOWN FACTS` của chunk nó: nếu nó **không thêm gì**, dòng cũ chỉ được `touch` (tính vào `reinforced`) và fact mới bị bỏ. Phải đủ cả năm điều kiện:

- fact mới không nêu `replaces` (nêu `replaces` là tuyên bố có đổi);
- ≥ 3 từ và ≥ 80% từ của fact mới đã có trong fact cũ. Không dùng `overlap_coefficient`: nó chia cho vế *ngắn*, nên một fact dài chứa fact ngắn đã lưu được 1.0 dù thêm ý;
- mọi số/ngày, từ phủ định (`không`, `not`…) và tên riêng (từ viết hoa không phải đầu câu sau) của fact mới đều có trong fact cũ, và fact mới giữ các từ phủ định của fact cũ. Lệch một số, một "không" hay một tên là khẳng định khác; bản nhắc lại bỏ bớt một tên hay một số thì chỉ nói ít hơn, còn bỏ mất một "không" thì nói ngược;
- entity model liệt kê (trừ `user`/`assistant`) vẫn còn quá nửa số từ ở phía kia;
- `when` của fact mới, nếu có, trùng fact cũ hoặc fact cũ chưa có ngày — khi đó fact cũ nhận ngày (`FactStore::date_if_missing`).

"user", "người dùng", "assistant", "trợ lý" không tính là từ. Điều **không** bắt được: đổi một từ thường, không phải tên và không ai liệt kê làm entity
("dark mode" → "light mode" trong câu dài); ngưỡng 80% chỉ là lề để phần lớn ca như vậy nằm dưới vạch. Model nhận ra có đổi thì phải nêu `replaces`.

**Hai fact trong cùng một câu trả lời.** Model nhỏ hay nói một chuyện hai lần trong một câu trả lời: một dòng ngắn ("Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán.") và một bản
đầy đủ ("Hôm qua, … bằng cách xóa cookie cũ …, mất khoảng hai tiếng."). Mọi fact của câu trả lời chỉ được lưu sau khi parse xong, nên khi xét fact thứ hai thì fact thứ nhất chưa phải
`StoredFact`; và nếu bản ngắn đứng trước thì nó đã bị lưu trước khi bản dài cho thấy nó không thêm gì. Vì vậy `restated::without_restated_siblings` so các fact **như model đã nói**, ngay sau khi
parse và trước khi embed/lưu: bản ngắn bị bỏ dù đứng trước hay sau, bản đầy đủ giữ nguyên (chữ, ngày, entity). Cùng các điều kiện như trên (số, phủ định, tên, entity hai phía, `when`, `replaces`), cộng ba điều khác:

- **chặt hơn**: *mọi* từ của fact bị bỏ phải có ở fact kia (không phải ≥ 80%). Không có câu chữ đã lưu nào để model diễn đạt lại, và đổi một từ thường ("buổi tối" → "buổi sáng") là điều không gì khác bắt được, nên ở đây nó không được phép làm mất một fact;
- fact có `replaces` luôn ở lại và được xét trước (bản sao không `replaces` của nó mới là bản bị bỏ);
- fact **có ngày** không bị bỏ vào fact không có ngày (không ai chuyển ngày sang cho nó); hai fact khác ngày là hai sự kiện. Ngày không bị đổi ở bất cứ đâu.

Fact bị bỏ tính vào `RetainReport::folded`, **không** vào `reinforced`: `reinforced` nghĩa là một fact đã lưu vừa được nhắc lại (`touch`, `proof_count + 1`), còn một câu trả lời là một lần nhắc, nên hai
bản trong cùng câu trả lời không phải bằng chứng thêm. `extracted = stored + reinforced + folded`. Fact còn lại vẫn đi tiếp qua kiểm tra với `KNOWN FACTS` như mọi fact khác (nên bản ngắn và bản dài cùng nhắc lại một fact
đã lưu chỉ `touch` nó một lần, không phải hai). Chưa làm: so giữa các chunk của cùng một lần `retain` (mỗi chunk có một câu trả lời riêng).

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

**`MEMORY.md` không phải arm.** Nó là ngữ cảnh ghim (chỉ dẫn thường trực), **bật mặc định** (`SENCLAW_PRE_RETRIEVAL=false` để tắt): mỗi lượt đi nguyên văn trong khối `<memory>` cùng ngữ cảnh của lượt, cắt ở 8.000 ký tự (kèm dòng báo "đọc file để xem phần còn lại") để một file phình to không ăn hết ngữ cảnh mỗi lượt.

## Project: bank thứ hai

Chat tạo trong một **project** ([`projects.md`](projects.md)) có thêm một bank fact `project:<id>` ngoài bank của profile. Đích **ghi** do `facts_write` của project, được quyết ở **một** chỗ —
`projects::fact_target` —, mọi nơi ghi (cửa sổ reflection, compaction, worker kanban) đi qua nó dưới dạng `FactTarget { bank, origin }`:

| Chat | Bank | `origin` |
|---|---|---|
| ngoài project, hoặc project có `facts_write=chat` | bank của profile | jid của chat (như trước) |
| trong project, `facts_write=project` (mặc định) | `project:<id>` | rỗng ⇒ mọi chat của project cùng đọc, xác nhận và thay thế fact của nhau |
| trong project, `facts_write=off` | không ghi | — |

**Đọc**: `RecallQuery.project` do `projects::recall_scope` dựng mỗi lượt. Bốn arm `ProjectFacts` chạy đúng bốn chiến lược trên bank project (`Scope::Agent`, `folder` đổi thành bank) và **không trả gì** khi câu hỏi không đến từ project;
nhãn arm vẫn là `facts` (luật fusion). `inherit_common=false` làm bốn arm thường trả rỗng nên chỉ còn fact project. Cửa sổ reflection vẫn **theo từng chat** dù bank chung: extractor đọc một hội thoại mỗi lần.

**Worker kanban** của board gắn project không có profile và không có chat: đầu mỗi card nó nhận khối `<memory_context>` chỉ gồm bank project (`projects::worker_recall_query`, không fact chung, không dòng gợi ý
`memory_recall` vì worker không có tool đó), chờ tối đa 3 giây cho cả tick. Lưu summary của card thành fact là **tuỳ chọn, mặc định tắt** (`SENCLAW_DISPATCH_CARD_FACTS=true`, `projects::worker_fact_target(project, enabled)` là chỗ duy nhất quyết). Khi bật và worker tự `kanban_complete` kèm summary,
`Kanban task "<tiêu đề>" completed: <summary>` (cắt 4.000 byte) được retain nền vào bank project với `source=kanban`. Summary là comment `complete` của chính worker — **không** phải dòng đầu câu trả lời cuối — và card để dispatcher tự đóng, bị block, hết giờ hay summary rỗng thì không retain gì.

Fact người gõ (`source=manual`) không bao giờ bị hội thoại thay (`supersede` từ chối cả `replaces` lẫn slot). Xoá bank theo lô 500 hàng, nhả khoá và nghỉ 10 ms giữa lô — một `DELETE` đơn là bậc hai vì trigger FTS (20.000 fact ≈ 18–24 s chặn mọi truy vấn).

**Đã đo bằng extractor kịch bản**: đường ống, cách ly và đổi ý xuyên chat (eval: 10 ca `projects` + 4 ca `worker`, cả hai chế độ embedding; 22 câu cũ không tụt; arm project p95 ≤ 52 ms trên bank 5.000 fact có vector).
**Đã đo trên model thật nhỏ** (gemma-4-e2b qua sen-mlx, **không phải** e4b của người dùng, 3 lần mỗi phép đo; [`projects.md`](projects.md) mục 9): 41–50% fact lưu vào bank project từ cửa sổ hỗn hợp là cá nhân/lẫn; fact cũ **không** bị thay khi đổi ý (3/3); 28 summary card cho 36–48 fact, 31–44% là rác (câu "đã hoàn thành" trống rỗng, bản chép lại fact của card khác bằng tiếng khác hoặc gán nhầm card) — nên mặc định tắt. Model chép lại `KNOWN FACTS` thành fact mới mỗi khi có thể: `restated` không bắt được bản dịch sang tiếng khác.
**Chưa đo trên e4b thật.**

## Memory, profile và context cập nhật thế nào

| | Chứa gì | **Ghi** khi nào, bởi ai | **Đọc** khi nào | Sống bao lâu |
|---|---|---|---|---|
| **Kho fact** (memory tự động) | sự thật có ngày rút ra từ hội thoại | mỗi cửa sổ flush + mỗi lần compaction; model trích xuất; secret bị quét trước | **mỗi lượt**, qua 4 arm, trong ngân sách 600 từ | bền (SQLite); xoá bằng `DELETE /api/memory/facts/:id` |
| **Note curated** + `MEMORY.md` | quyết định, ghi chú có chủ đích | agent gọi `memory_save` hoặc người sửa tay. **Không còn tự sinh từ compaction** | note: mỗi lượt nếu `memoryRecall`; `MEMORY.md`: nguyên văn (mặc định bật, cắt 8.000 ký tự) | bền (file) |
| **Đồ thị tri thức** | tài liệu upload, `cog_add`, SOUL của agent | upload / `cog_add` / ingest SOUL. **Không còn từ chat** | arm `cognitive` nếu `preCognitive` | bền, mờ dần theo decay |
| **Profile** `USER.md` | người dùng là ai + chỉ dẫn thường trực (field, directive) | agent gọi `profile_update` khi người dùng nêu sự thật bền / sở thích; Settings UI. **Engine không tự ghi** | **một lần mỗi phiên** trong ngữ cảnh lượt đầu, lọc tier (chat nhóm chỉ thấy public) | bền (file); directive cũ bị `superseded` |
| **Kho fact của project** | sự thật rút ra từ các chat của một project; fact người gõ tay | cùng đường retain với kho fact nhưng vào bank `project:<id>`, không gắn chat, khi `facts_write=project` (mặc định); người gõ qua `POST /api/projects/:id/facts` (không model, `source=manual`) | **mỗi lượt** của chat trong project (thêm bốn arm trên bank này, chung ngân sách 600 từ) và **đầu mỗi card** của board gắn project (chỉ bank này) | bền; xoá bằng `DELETE /api/projects/:id/facts/:factId` hoặc purge project |
| **Fact từ card kanban** (mặc định **tắt**: `SENCLAW_DISPATCH_CARD_FACTS=true`) | một dòng `Kanban task "<tiêu đề>" completed: <summary>` qua extractor | khi bật: worker tự `kanban_complete` kèm summary, board có project, `facts_write=project`; nền, `source=kanban`, không chờ | như kho fact của project | bền |
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
| Phạm vi fact | `SENCLAW_MEMORY_FACTS_SCOPE` = `chat` (mặc định: mỗi cuộc trò chuyện chỉ nhớ lại fact của chính nó và fact chung của agent) \| `agent` (mọi chat của một agent dùng chung một kho) |
| `MEMORY.md` mỗi lượt | `SENCLAW_PRE_RETRIEVAL` (mặc định `true`; `false` để tắt) |
| Xử lý secret | `SENCLAW_MEMORY_ENGINE_DEFENSE` = `redact` (mặc định) \| `block` (bỏ cả cửa sổ) \| `off`. Giá trị lạ → `redact` |

## Xem và xoá

- `GET /api/memory/facts?bank=<folder>&q=<từ khoá>&limit=&history=` — mới nhất trước, hoặc khớp từ khoá; `limit` 1–200; `history=true` thêm cả fact đã bị thay (kèm `supersededBy`). Mỗi fact có `proofCount`.
- `GET /api/memory/facts/banks` — các agent đang có fact: `{bank, current, replaced, lastMentionedAt}`, agent được nhắc gần nhất trước (để giao diện có danh sách chọn).
  Bank của project (`project:<id>`) cũng nằm trong danh sách; xem, nhập tay và xoá fact của chúng qua `/api/projects/:id/facts` ([`projects.md`](projects.md) mục 4).
- `DELETE /api/memory/facts/:id?bank=<folder>` — quên một fact. Sai `bank` thì 404, không xoá được fact của agent khác.
- **Giao diện**: tab **Facts** trong trang Knowledge — web ([`FactsView.tsx`](../../web-app/src/components/cognitive/FactsView.tsx), `lib/memoryFactsApi.ts`) và desktop (`lib/features/cognitive/facts_panel.dart`): chọn agent, tìm từ khoá, bật "hiện fact đã bị thay", quên một fact (có hỏi lại). Ngày hiển thị theo độ chính xác đã lưu (`2026`, `2026-03`, `2026-03-09`; đọc theo UTC), `×n` là số lần được nhắc.
- Log `[facts] retained from …` ghi `stored`, `reinforced`, `folded`, `replaces_claimed`, `superseded`, `failed_chunks`: `replaces_claimed=0` nghĩa là model không nêu fact nào bị thay, còn `claimed > superseded` nghĩa là retain từ chối (số lạ hoặc khác chủ đề).
- Cho agent (server `senclaw-memory`, trong chat là `mcp__core__memory_*`): `memory_search_facts` (`query`, `limit`; không có `query` thì liệt kê mới nhất; câu hỏi có thời gian "hôm qua" cũng tra theo ngày) và `memory_forget_fact` (`id` đầy đủ, chỉ fact của chính agent). MCP chạy tiến trình riêng nên tra bằng từ khoá và ngày, không có vector.

## Chạy eval

```bash
cargo test --lib memory::engine::eval -- --nocapture --test-threads=1
MEMORY_EVAL_OUT=/tmp/mem cargo test --lib memory::engine::eval   # → /tmp/mem.{vectors,keyword-only}.json
```

[`evals/memory/cases.json`](../evals/memory/cases.json): 9 note (một note của chat khác), 8 chunk đồ thị, 11 câu hỏi VN/EN; và cho kho
fact 15 cửa sổ hội thoại chạy qua **retain thật** (chỉ câu trả lời của model trích xuất là dựng sẵn) + 11 câu hỏi chỉ fact trả lời được,
gồm một ca đổi ý (React → Vue: phải thấy Vue và **không** thấy "thích dùng React"; `replaces_matching` trong fixture đóng vai model đọc
`KNOWN FACTS` và chỉ đúng dòng — đổi cụm đó thành cụm không khớp thì eval đỏ) và bốn ca **slot** không nêu `replaces`: chuyển thành phố, đổi
framework backend, hai giá trị của `likes` đều còn, và một câu "năm 2025…" không được thay chỗ làm hiện tại.
Luồng cũ giữ lại **chỉ trong test** ([`legacy.rs`](../src/memory/engine/legacy.rs), `#[cfg(test)]`) làm mốc so sánh. Cổng: không rò rỉ;
engine lặp ≤ luồng cũ; có vector thì trả lời ≥ luồng cũ, không vector thì **>**; fact 11/11 so với 0/11; arm graph và temporal mỗi cái trả
lời một câu mà keyword một mình không trả lời được; secret không tới model trích xuất. Số liệu:
[`plans/reports/eval-261016-1535-memory-recall-baseline.md`](../plans/reports/eval-261016-1535-memory-recall-baseline.md).

### Eval đổi ý qua đường chat thật

`node scripts/evals/memory-changed-mind.mjs --runs 3` chạy trên một daemon cô lập (đầu file có cách dựng): mỗi lần một agent folder riêng, một lượt nói thích React, chờ fact
được lưu, một lượt đổi ý sang Vue, rồi kiểm — không còn fact **hiện hành** nào nêu sở thích cũ, và fact cũ nằm trong lịch sử với `supersededBy`. Khác harness ở chỗ nó đi qua
toàn bộ đường thật: lượt chat dài, cửa sổ reflection, lời assistant lọt vào cửa sổ, flush khi im. Từ chối cổng 18788/18789. `--old-turn` / `--new-turn` đổi kịch bản
(kịch bản khó: một câu nêu tên, nơi làm và React). Số đo (model 2B, mẫu nhỏ):
[`plans/reports/eval-261017-2145-slot-supersession-real-model.md`](../plans/reports/eval-261017-2145-slot-supersession-real-model.md).

### Eval với model trích xuất thật (`#[ignore]`)

`real_extractor_keyword_only` và `real_extractor_with_vectors` chạy 15 cửa sổ của `fact_windows` qua **model thật** (client production
`OpenAiCompatLlm`, kể cả timeout 60 s) thay cho `ScriptedFacts`, rồi hỏi 11 `fact_queries` và chấm y như trên. Không bao giờ chạy mặc định
và không đo gì khi chạy mặc định — đây là thước đo, không phải cổng: model đổi câu trả lời giữa các lần chạy, nên chạy vài lần.

Điểm cuối được đọc từ env, **một lần trong test** (không đọc trong code thư viện): `SENCLAW_EVAL_LLM_BASE_URL` (endpoint kiểu OpenAI, có hoặc không
có `/v1`), `SENCLAW_EVAL_LLM_MODEL`, `SENCLAW_EVAL_LLM_API_KEY` (tuỳ chọn). Test từ chối cổng 18788/18789 — đó là daemon thật. Dựng một
runtime riêng trên cổng trống, cwd và `HOME`/`SENCLAW_HOME` đều là thư mục tạm (CLAUDE.md "Never develop against a live install"):

```bash
S=$(mktemp -d); mkdir -p $S/home $S/root $S/models
cd $S/root && HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_LOCAL_MODELS_DIR=$S/models \
  ~/.senclaw/runtimes/sen-mlx/<phiên bản>/bin/sen-mlx serve --host 127.0.0.1 --port 38931 \
  --model ~/.senclaw/local-models/<thư mục model> &
curl -s http://127.0.0.1:38931/v1/models      # "id" → SENCLAW_EVAL_LLM_MODEL
```

```bash
SENCLAW_EVAL_LLM_BASE_URL=http://127.0.0.1:38931/v1 SENCLAW_EVAL_LLM_MODEL=<id> \
  cargo test --lib memory::engine::eval::real_extractor -- --ignored --nocapture --test-threads=1
```

Trên đĩa chật: dùng chung thư mục target của checkout chính, `cargo test --lib --no-run`, rồi chạy thẳng file test (không có `--` trước tên lọc):
`<target>/debug/deps/senclaw-<hash> memory::engine::eval::real_extractor_keyword_only --ignored --nocapture`.

In ra theo từng cửa sổ: các dòng `KNOWN FACTS` model được xem (`shown [Fn]`), từng fact nó trích (loại, `when`, entities, `replaces`) và bộ đếm của
retain; câu trả lời hỏng thì in nguyên văn (`CALL FAILED` / `UNUSABLE REPLY`). Sau đó: số `replaces` model nêu / được chấp nhận
(`RetainReport.superseded`; `superseded_by_slot` là phần thay bằng luật) / bị bỏ qua (nêu − chấp nhận; trong đó bao nhiêu số nằm ngoài danh sách đã xem), kho cuối cùng (`live` / `retired`),
và với ca đổi ý thì fact cũ **theo câu chữ của model** (fact model trích từ cửa sổ cũ, cùng thực thể với fixture) đã `retired` hay còn sống. Còn sống thì câu hỏi
bị chấm `LEAKED` dù câu chữ khác cụm trong fixture ("thích sử dụng React" ≠ "thích dùng React"). Không trích được fact cũ thì ca đó không thử gì về `replaces`.
Mỗi câu hỏi in `answered` / `MISSED` / `LEAKED` kèm ngữ cảnh đã tiêm khi trượt. Một dòng `summary` gom lại. Chấm chuỗi như cũ, kèm thêm số câu đúng
**khi bỏ dấu tiếng Việt** ("xóa" ≠ "xoá" là cách đặt dấu, không phải quên). Chỉ hai khẳng định: ít nhất một cuộc gọi tới được model, và secret không bao giờ nằm trong
prompt gửi đi. Số đo trên một model 2B: [`plans/reports/eval-261017-0005-memory-real-extractor.md`](../plans/reports/eval-261017-0005-memory-real-extractor.md).

Cho project có thêm hai test `#[ignore]` trong `eval/real_project.rs` (dữ liệu: [`evals/memory/project-measure.json`](../evals/memory/project-measure.json)), cùng biến môi trường và cách chạy:
`memory::engine::eval::real_project`.
- `real_extractor_personal_facts_in_a_project_chat`: 12 cửa sổ của một chat project (5 về dự án, 4 cá nhân, 3 lẫn). In từng fact được lưu vào bank project kèm nhãn `project`/`personal`/`both`/`other`
  (theo marker), rồi hỏi 7 câu cá nhân và 5 câu dự án từ chat đó, từ chat khác của project và từ chat **ngoài** project, so với cùng câu trả lời của model được lưu theo kiểu trước khi có project
  (bank profile gắn chat). Đo R4 / quyết định #2.
- `real_extractor_card_summaries`: 28 summary kiểu worker viết (tốt, chung chung "Done.", có số liệu, đường dẫn, dài, ngắn, tiếng Việt, câu hỏi, đổi giá trị, có secret, gần trùng) qua `retain` y như dispatcher.
  In fact sinh ra của từng card (`useful` nếu chứa marker của card, `JUNK?` nếu không hoặc card vốn không có gì đáng lưu), bank cuối, và 7 câu hỏi "worker kế tiếp được nghe gì". Đo quyết định #3.
Hai test chỉ in và không chấm: người chạy đọc output. Số đo: `plans/reports/test-*-project-p5-measure.md`.

## Bẫy

- **Spreading cần embedding provider.** Không có provider (cấu hình mặc định) thì `preCognitive` cũ tiêm **rỗng**; `CognitiveArm` rơi về hybrid/keyword.
- **Chat không còn nuôi đồ thị.** Trang Knowledge chỉ còn dữ liệu upload / `cog_add` / SOUL; fact thì xem qua `/api/memory/facts` (giao diện: tab Facts, xem trên).
- **Ngân sách đếm từ**, đừng so với context window như thể là token thật.
- **Fact theo chat.** Mỗi fact mang `origin` (JID của chat đã nói ra) và một chat chỉ thấy fact của mình cộng fact chung của agent — luật của note curated (`memory_session_rank`): fact của chat khác không bao giờ hiện, kể cả qua arm graph hay `memory_search_facts`. Cửa sổ reflection nay theo từng chat, cùng câu ở hai chat là hai fact (mỗi chat giữ bản của mình), và model trích xuất chỉ được cho xem — và chỉ có thể "thay" — fact mà chat đó thấy. Giao diện Facts và `GET /api/memory/facts` là góc nhìn của chủ máy: thấy fact của mọi chat, mỗi fact có `origin`. Fact đã có trước thay đổi này không có `origin` và vẫn hiện ở mọi chat. Đặt `SENCLAW_MEMORY_FACTS_SCOPE=agent` để dùng chung như trước.
- **Chunk ghi chú chưa có vector** (lập chỉ mục lúc chưa có model embedding) được bù vector ở lần sync sau khi model chạy được, tối đa 640 chunk mỗi lần sync; trước đây sync theo hash file nên không bao giờ quay lại chúng.
- **Eval mặc định không đo model trích xuất** (câu trả lời dựng sẵn). Đo thật mới có trên một model 2B (`gemma-4-e2b`, 28 lần chạy): chỉ đúng `replaces` mọi lần có nêu (15/15, rồi 9/9),
  nhưng đôi lúc sai năm, và còn hai điểm yếu của model nhỏ. (1) Câu trả lời hỏng JSON ~3%: thiếu/thừa/lệch dấu đóng đã được `extract.rs` chữa; kiểu khoá bị lặp
  (`{"text{"text":"…"`, ~1,4%) chỉ mất đúng phần tử hỏng (các fact còn lại của cửa sổ vẫn được lưu), nhưng fact đó mất hẳn. (2) Model hay đọc lại `KNOWN FACTS`: `restated.rs` chặn bản diễn đạt lại, không chặn được hai fact trong *cùng* một câu trả lời
  ("Người dùng đã sửa lỗi…" ngắn và bản dài có "Hôm qua"). Chưa đo trên `gemma-4-e4b` đang cấu hình.
- **Số trong `replaces` là số của đúng chunk đó.** `[F1]` của chunk này không phải `[F1]` của chunk khác; code đọc theo danh sách của chunk.
- **Route gộp từ module khác phải có mặt trong `ROUTER_SOURCES`** ([`openapi.rs`](../src/gateway/ui_server/openapi.rs)), nếu không test spec không thấy route và `docs/openapi-daemon.yaml` lệch.
- Ngày của note lấy từ `createdAt` (nửa đêm UTC); thiếu hoặc sai định dạng thì trung tính.
