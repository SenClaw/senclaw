# Kiểm chứng P2: fact riêng của project — ghi, đọc, nhập tay, xoá, tool agent, eval

Ngày 2026-10-18 · Nền `59caf13` (P1, `feat/project`) · Nhánh `feat/project-facts`, worktree `.worktrees/senclaw-project-facts` · Commit `e00e5f7` (một commit, chưa push; commit sau khi người dùng yêu cầu) · 22 file sửa + 1 file mới (`src/projects/facts.rs`), +2.182/−103 dòng gồm test
Phục vụ: [plan.md](../261018-1618-project-feature/plan.md) P2, tiêu chí 4 · [design.md](../261018-1618-project-feature/design.md) D5, D9, §4, §5.4, §5.6, §10, R4, R9 · [P1](test-261018-2159-project-p1.md)
Harness + bằng chứng: [project-p2-e2e/](project-p2-e2e/) (dùng lại `project-p0-spike/lib.mjs`; mock LLM riêng `mock-llm-p2.mjs`).

## 0. Cái GÌ CHƯA đo — đọc trước

- **Chất lượng trích xuất fact trên model thật: CHƯA đo.** Mọi chỗ cần "extractor" ở đây là **kịch bản**: eval dùng `ScriptedFacts`, e2e dùng mock đọc các khối `¤¤FACTS{…}¤¤` do chính driver nhét vào tin nhắn. Kết quả chứng minh **đường ống** (fact ghi vào đâu, chat nào đọc được gì, tool, REST, xoá), không nói gì về việc một model thật có tách đúng "fact của dự án" ra khỏi hội thoại hay không, hay `replaces` có trỏ đúng không. R4 (fact cá nhân bị nhốt trong bank project) vẫn là rủi ro chưa đo; P5 phải đo trên model người dùng thực dùng.
- **Embedding model thật: chưa dùng.** Eval dùng `HashedWords` (giả lập), e2e chạy không có embedding provider (mặc định cài đặt). Nhánh semantic của `ProjectFacts` chỉ được chạy với giả lập.
- Không thử client web/desktop (P4), không thử worker kanban (P3b), không thử Windows, không thử `mcp.bundled=false` (mỗi server một tiến trình) cho `memory_search_facts`.
- **Đường nén ngữ cảnh (`compact:exec`) → fact project**: logic chọn đích đã đổi sang cùng `fact_target`, được test ở mức pool (`what_a_session_remembers_goes_where…`), nhưng closure `compact:exec` **không được chạy** trong e2e (cần hội thoại đủ dài để nén).

## 1. Kết quả theo tiêu chí

| Tiêu chí (plan.md) | Kết quả | Test tự động | Chạy thật (daemon cô lập + LLM giả, **extractor là kịch bản**) |
|---|---|---|---|
| **4. Fact nói ở chat A của P được nhớ ở chat B của P; không hiện ở chat ngoài project hay project Q; fact chung vẫn hiện trừ khi tắt `inherit_common`** (phần chat) | **đạt** | eval `projects` 10 ca × 2 chế độ (có/không embedding), `a_chat_outside_every_project_is_told_what_it_always_was`, `a_project_fact_found_by_several_arms_…`; `projects::facts` 5; `project_tests` 2; `reflection` 2 | A1 nói ⇒ bank `project:<A>` có 2 fact (origin rỗng, source `reflection`), bank profile không có gì thêm. Khối `<memory_context>` mà **model nhận** (đọc từ body request của mock): A2 thấy `7421` + fact chung; chat ngoài project và chat của B thấy fact chung, **không** thấy `7421`. PATCH `inheritCommon=false` ⇒ lượt kế A2 mất fact chung, giữ fact project; chat ngoài project không đổi; bật lại ⇒ có lại |
| Ca (a) fact project không lọt sang chat ngoài / project Q (`must_exclude`) | **đạt** | eval `leak-outside-a-project`, `leak-into-another-project`, `leak-into-the-other-direction` + control `the-project-itself-answers`; 22 câu cũ hỏi từ ngoài project trong world có fact project: tập fact tìm được **giống hệt** world không có, và không chứa chuỗi nào của project (5 marker) | như dòng trên |
| Ca (b) fact chung hiện trong P, mất khi `inherit_common=false` | **đạt** | `common-facts-reach-the-project`, `common-facts-off`, `own-facts-when-common-off` | như trên |
| Ca (c) chat B của P trả lời được điều nói ở chat A | **đạt** | `said-in-another-chat` | A2 nhận `7421` do A1 nói |
| Ca (d) đổi ý ở chat B thay fact của chat A | **đạt** | `changed-mind-in-another-chat-{a,b}` | A2 nói "dời sang thứ Năm" (`replaces_matching`) ⇒ fact cũ có `supersededBy`, danh sách hiện chỉ giá trị mới; A1 (chat đã nói giá trị cũ) lượt sau nhận giá trị mới, không nhận giá trị cũ |
| `facts_write` = `chat` / `off` (live, đọc mỗi lần) | **đạt** | `project_tests::what_a_session_remembers_goes_where_its_project_says_and_is_read_live`, `projects::facts` | `chat`: fact vào bank profile gắn jid của chat, không vào project, chỉ chat đó nhận được; `off`: sau 8 s không có fact mới ở đâu cả |
| Nhập tay `POST /api/projects/:id/facts` (không LLM; `source='manual'`; retain không thay) | **đạt** | REST 7 (tạo/liệt kê/xoá, trùng chữ, embed khi có model, từ chối, không chạm project khác, purge); `store` 2; `retain` 1 | POST ⇒ 201 `manual`, origin rỗng; cùng chữ ⇒ 200 `existing`; mảnh vụn và secret bị 400; sau đó hội thoại nói MySQL với `replaces` trỏ vào fact tay ⇒ fact tay **còn nguyên**, fact MySQL nằm cạnh |
| `memory_search_facts` đọc `SENCLAW_PROJECT_BANK` | **đạt** | `tools` 3 | chat project thấy fact project + fact chung; chat ngoài project và chat project khác: "No matching facts."; `memory_forget_fact` từ ngoài project: "No fact with id"; từ trong project: "Forgot fact" |
| Xoá project: bank `project:<id>` xoá theo lô ≤500, qua `Db` | **đạt** (xem §5 về độ trễ) | `store` (lô 50/50/20, nhả khoá, 537 fact qua 2 lượt), REST purge (2) | purge 42 fact ⇒ `deletedFacts=42`, không còn hàng mồ côi ở `mem_fact_entities` / `mem_links` / `mem_facts_fts`; bank khác và fact chung nguyên vẹn; chat của project bị xoá tiếp tục nhớ như chat thường |

Cổng của phase:

| Cổng | Kết quả |
|---|---|
| 22 câu eval hiện có không tụt | **giữ nguyên**: 11 note/graph + 11 fact, cả hai chế độ; `run_engine` giờ chạy bộ arm thật của daemon (8 arm fact) ⇒ đường sản xuất đã nằm trong số đo cũ; cũ vs mới ở world có fact project: tập fact giống hệt |
| Arm project ≤150 ms p95 | **đạt**, bank 5.000 fact có vector, 30 lượt mỗi arm, build debug: graph p95 52 ms, semantic 45, keyword 5, temporal 6 (max 52 ms) |
| Recall không gọi model | **giữ**: `ProjectFacts` chỉ đổi `folder`/`facts_by_chat`/`project` của query rồi gọi arm cũ |

## 2. Số liệu

- `cargo check --all-targets`: sạch; 2 cảnh báo có sẵn (`local_models/settings.rs:141`, `tools/write.rs:181`).
- `cargo test --lib`: **3207 passed, 0 failed, 11 ignored** (nền P1 3179 ⇒ +28 test mới) — HOME, `SENCLAW_HOME`, cwd riêng; chạy trực tiếp binary test.
- Eval: `cargo test --lib memory::engine::eval` 11 passed (2 ignored là model thật), chạy 3 lần liên tiếp đều xanh.
- **Kiểm tra eval có cắn**: tạm làm hỏng (a) `hides_common` luôn false và (b) `fact_target` ghi vào bank profile gắn chat ⇒ `common-facts-off` LEAKED, `own-facts-when-common-off` / `said-in-another-chat` MISSED, `changed-mind-…-a` LEAKED; khôi phục xong. Tương tự bỏ nghỉ giữa các lô ⇒ `a_reader_that_arrives_during_a_deletion_is_served_before_it_ends` đỏ 3/3.
- e2e: **53/53** check ([result-p2.json](project-p2-e2e/evidence/result-p2.json), [run.log](project-p2-e2e/evidence/run.log)), chạy 3 lần từ daemon mới (hai lần trên binary đầu, một lần trên binary cuối), đều 53/53. Binary cuối sha256 `52c7c3e4…1fd0` ([binary-sha256.txt](project-p2-e2e/evidence/binary-sha256.txt)). Mẫu khối recall và kết quả tool: [recall-samples.txt](project-p2-e2e/evidence/recall-samples.txt).
- **Harness P1 chạy lại trên binary cuối: 96/96** ([p1-rerun.log](project-p2-e2e/evidence/p1-rerun.log)); bằng chứng P1 gốc không bị ghi đè. (Harness P0 không chạy lại: nó dựng trên binary có `spike-hook.patch`, không áp dụng cho P2.)
- `docs/openapi-daemon.yaml` sinh lại: +2 path (`/api/projects/{id}/facts`, `/api/projects/{id}/facts/{fact_id}`).
- Đĩa: dùng chung `CARGO_TARGET_DIR` của checkout chính với `CARGO_INCREMENTAL=0`; binary daemon e2e build vào bản COW của `target/debug` rồi **xoá**; `target/debug/senclaw` (daemon thật đang dùng) mtime nguyên (Oct 17 22:19); daemon thật 18788 không bị đụng, e2e dùng 49788/49789/49931, dừng xong.

## 3. Đã làm

1. **`FactTarget { bank, origin }`** (`memory/engine/facts/mod.rs`) thay tham số `(bank, chat_jid)` của `retain_text`. `projects::fact_target` (file mới `projects/facts.rs`) là **một chỗ duy nhất** chọn đích theo bảng §5.4: không project hoặc `facts_write=chat` ⇒ (profile, jid) như cũ; `project` ⇒ (`project:<id>`, ''); `off` ⇒ `None`. Pool đọc project **mỗi lần** (đổi `facts_write` có hiệu lực ở cửa sổ/tóm tắt kế).
2. **Reflection**: `reflect_push` nhận `FactTarget`; cửa sổ mang `target`; khoá cửa sổ **vẫn theo chat** khi bank là của project (extractor đọc từng hội thoại), không đổi cho chat ngoài project. **Compaction** (`compact:exec`) dùng cùng đích.
3. **Recall**: `RecallQuery.project: Option<ProjectRecall{bank, include_common}>` dựng từ binding mỗi lượt (`projects::recall_scope`); `ProjectFacts<A>` bọc 4 arm (`fact_arms()` trả 8 arm), nhãn `facts` giữ nguyên, tên chiến lược `facts_project_*`; 4 arm cũ trả rỗng khi `include_common=false`. Hệ số ưu tiên fact project = 1.0.
4. **Store**: `supersede` từ chối fact `manual` (cả `replaces` lẫn slot); `insert_manual` (chữ trùng một fact đã có ⇒ fact đó thành `manual`); `delete_bank` theo lô 500 qua `Db`, nghỉ 10 ms giữa lô; `StoredFact.source` (REST `/api/memory/facts` thêm trường `source`, cộng thêm, không phá).
5. **REST** (cuối `ui_server/projects.rs`): `GET|POST /api/projects/:id/facts`, `DELETE /api/projects/:id/facts/:factId`; POST không gọi LLM, embed khi có model, 8–600 ký tự, `kind`/`when` kiểm, **từ chối secret** (`defense::scrub` chế độ Block); purge xoá bank **trước** rồi mới xoá hàng project (lỗi giữa chừng còn project để thử lại), trả `deletedFacts`.
6. **MCP**: `memory_search_facts` đọc `SENCLAW_PROJECT_BANK` (project trước, rồi fact của agent trừ khi project `inherit_common=false` — đọc **live** từ DB nên khớp với recall); `memory_forget_fact` thử bank project khi không thấy trong bank agent. Project đã xoá ⇒ bỏ qua env, hành xử như chat thường.
7. **Eval**: `evals/memory/cases.json` mục `projects` (4 cửa sổ, 10 câu); `eval.rs` 5 test; test thêm ở `store`, `tools`, `projects::facts`, `reflection`, `project_tests`, REST, `retain`.
8. Tài liệu: 3 gạch đầu dòng quy tắc trong mục Memory engine của `CLAUDE.md`; sửa dòng `retain_text` trong `docs/memory-engine.md`. Phần tài liệu đầy đủ (`docs/projects.md`, bảng "ai đọc/ghi") là P5.

## 4. Quyết định tôi tự chọn

1. **Khoá cửa sổ reflection vẫn theo chat** với bank project (design nói "giữ khoá theo chat, thêm bank/origin vào cửa sổ"). `window_key` thêm tham số `bank`; chat ngoài project cho khoá y hệt trước.
2. **`facts_write=off` không đẩy gì vào cửa sổ** (không đệm rồi bỏ lúc flush).
3. **Recall đọc bank project bất kể `facts_write`** (`off` tắt ghi, không tắt đọc), kể cả project đã lưu trữ.
4. `include_common=false` tắt **cả** fact mức chat của bank profile (chỉ còn bank project) vì bốn arm cũ trả rỗng đúng như giao. Hệ quả cho tổ hợp `facts_write=chat` + `inherit_common=false`: chat không còn thấy chính fact mức chat của nó (xem §6).
5. `POST` trùng chữ với fact đã có ⇒ **200 + `existing:true`** và fact đó chuyển thành `manual` (nếu không, retain sau này vẫn thay được nó).
6. `memory_forget_fact` từ chat project xoá được cả fact project (kể cả fact tay): cùng quyền với fact của agent, theo cùng luật "chủ máy dặn quên".
7. Fact tay không có entity ⇒ `GraphArm` không nối chúng qua entity; keyword/semantic/temporal vẫn tìm thấy. Chưa làm liên kết tự động khi nhập tay (cần model/embedding, ngoài phạm vi "không gọi LLM").
8. Giữ lô **500** như giao; thêm nghỉ 10 ms vì phát hiện ở §5.
9. Hai thay đổi nhỏ phục vụ test/đúng đắn: `extract::{MIN,MAX}_FACT_CHARS` thành `pub` (REST dùng đúng giới hạn của extractor); `FactTarget: Default` (cửa sổ `#[derive(Default)]`).

## 5. Phát hiện khi đo: nhả khoá `Mutex` không đủ để nhường chỗ

P0 ghi "nhả khoá `Db` giữa các lô". Làm đúng chữ đó **chưa đủ**: `Db` là `std::sync::Mutex`, thread vừa nhả lại giành lại ngay trước khi thread chờ kịp thức, nên mọi truy vấn khác vẫn đợi hết cuộc xoá. Đo trên daemon cô lập, 20.000 fact tổng hợp (có entity, link, text index), một thăm dò `GET /api/projects` mỗi 20 ms trong lúc purge:

| | purge | yêu cầu khác phải đợi (max / p95) |
|---|---|---|
| một lệnh `DELETE` (đo trên bản sao DB, cùng máy) | 22,5–24,0 s | 22,5–24,0 s (chặn hết) |
| lô 500, **không nghỉ** | 21,3 s | **21,3 s** — thăm dò đầu tiên chờ hết cuộc xoá |
| lô 500 + nghỉ 10 ms (bản giao) | 22,7 s | **1,13 s / 1,03 s** |

Tổng thời gian không giảm (vẫn bậc hai vì trigger FTS quét cả bảng `mem_facts_fts` cho từng hàng, `fact_id UNINDEXED`); cái đổi là các truy vấn khác không còn bị chặn tới 20+ s. Bank thường gặp (vài trăm đến vài nghìn fact) purge dưới một giây. Chi phí mỗi hàng ∝ **kích thước toàn bảng FTS** (mọi bank), không chỉ bank bị xoá — đây là suy luận từ cơ chế, **chưa đo** với DB có nhiều bank. Sửa tận gốc (đổi FTS sang contentless theo rowid hoặc bỏ trigger quét) là thay đổi schema, ngoài phạm vi P2; nếu cần, tách riêng. Đáng cân nhắc: 100 hàng/lô sẽ đưa đợi tối đa xuống ~0,25 s với giá ~2 s thêm cho bank 20k.

Phản hồi `DELETE ?purge=true` của bank rất lớn sẽ mất tương ứng (20k ≈ 23 s): client cần timeout đủ dài hoặc P4 hiển thị tiến trình.

## 6. Hạn chế / cần theo dõi

- `facts_write=chat` + `inherit_common=false`: bốn arm cũ rỗng ⇒ chat mất cả fact mức chat của chính nó. Đúng chữ giao, nhưng có thể không phải điều người dùng mong; xem câu hỏi 1.
- Một cửa sổ reflection đang chờ flush lúc project bị purge có thể tạo lại vài fact trong bank vừa xoá (không project nào sở hữu, hiện trong `GET /api/memory/facts/banks`). Không có test; hiếm (cửa sổ ≤ vài chục giây). Cách chữa nếu cần: kiểm tra project còn tồn tại lúc flush.
- Core đang chạy giữ `SENCLAW_PROJECT_BANK` từ lúc dựng (tool `memory_search_facts`); `inherit_common` thì đọc live. Đổi root/purge vẫn không dựng lại core (hạn chế P1).
- `retain` đếm `replaces_claimed` cả khi bị từ chối vì fact `manual` (chỉ số, không ảnh hưởng dữ liệu).
- Xung đột merge dự kiến với `feat/project-kanban` (P3a, cùng nền `59caf13`): `ui_server/projects.rs` (`routes()`, `projects_delete`, khối `use`, cuối module test), `src/projects/mod.rs` (`pub mod`/`pub use`), `docs/openapi-daemon.yaml` (sinh lại sau khi gộp), `plan.md`. Cả hai nhánh đã `rustfmt` sạch các file đó nên hunk gộp được bằng tay; không có hunk nào của tôi trong `src/lib.rs`.

## 7. Chạy lại

```bash
# test (HOME, SENCLAW_HOME, cwd riêng; dùng chung CARGO_TARGET_DIR của checkout chính)
cd .worktrees/senclaw-project-facts && export CARGO_TARGET_DIR=/Users/benji/Projects/SenClaw/senclaw/target CARGO_INCREMENTAL=0
cargo check --all-targets && cargo test --lib --no-run               # in đường dẫn binary test
find $CARGO_TARGET_DIR/debug/deps -maxdepth 1 -name 'senclaw-<hash>.*.rcgu.o' -delete
S=<scratch>; mkdir -p $S/home $S/cwd; cd $S/cwd && HOME=$S/home SENCLAW_HOME=$S/home/.senclaw <binary test> memory::engine::eval --nocapture

# e2e: binary daemon vào bản COW của target/debug, không ghi đè target/debug/senclaw
cp -c -R $CARGO_TARGET_DIR/debug $S/p2-target/debug && CARGO_TARGET_DIR=$S/p2-target cargo build --bin senclaw
export P0_RUNS=$S/runs P0_UI=49788 P0_WS=49789 P0_MOCK=49931          # cổng rảnh, không bao giờ 18788/18789
cd plans/reports/project-p2-e2e
./env-up.sh p2 $S/p2-target/debug/senclaw && node exp-p2.mjs p2 && ./env-down.sh p2
./env-up.sh scale $S/p2-target/debug/senclaw && node scale-purge.mjs scale 20000 && ./env-down.sh scale
# P1 trên cùng binary: ../project-p0-spike/env-up.sh p1 <binary> && (cd ../project-p1-e2e && node exp-p1.mjs p1 <binary>)
rm -rf $S/p2-target $S/runs                                           # trả đĩa
```

Đã commit: `e00e5f7 feat(projects): facts that belong to a project` (23 file, +2.300/−103; không có thay đổi nào ngoài cây này).

## Câu hỏi chưa giải quyết

1. `facts_write=chat` + `inherit_common=false`: chat có nên vẫn thấy fact mức chat của chính nó không? Hiện: không (đúng chữ giao). Đổi cần thêm một `Scope` "chỉ chat này" vào `FactStore`.
2. Lô 500 hay nhỏ hơn (100)? Hay tách một thay đổi riêng để gỡ O(n²) của trigger FTS (ảnh hưởng mọi lần xoá fact, không riêng purge)?
3. `memory_forget_fact` có được phép xoá fact `manual` của project không (hiện được, như fact của agent)?
4. Có chữa khe "cửa sổ đang chờ flush lúc purge" ngay không, hay để P5 đo trước?
5. Có cần đo extractor thật cho ca đổi ý xuyên chat (d) và R4 ngay trước P3b/P4 không, hay để P5 như kế hoạch?
