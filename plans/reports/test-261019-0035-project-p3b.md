# Kiểm chứng P3b: worker kanban đọc và ghi fact của project

Ngày 2026-10-19 · Nền `1079aea` (merge `feat/project`: P1 `59caf13` + P2 `e00e5f7` + P3a `ee6a1c5`) · Nhánh `feat/project-kanban-facts`, worktree `.worktrees/senclaw-project-kanban-facts` · **Chưa commit** (chỉ commit khi người dùng yêu cầu) · 10 file sửa (+1.081/−43 dòng, gồm test), không file mới trong `src/`
Phục vụ: [plan.md](../261018-1618-project-feature/plan.md) P3b, tiêu chí 4 (phần kanban), quyết định #3 · [design.md](../261018-1618-project-feature/design.md) D5, D8, §5.4, §5.5, R4, R9, §10 (e), §11 #3 · [P2](test-261018-2310-project-p2.md) · [P3a](test-261018-2243-project-p3a.md)
Harness + bằng chứng: [project-p3b-e2e/](project-p3b-e2e/) (dùng lại `project-p0-spike/{lib.mjs,env-down.sh}`; mock LLM `mock-llm-p3b.mjs` = mock P2 + log nguyên văn cái extractor nhận + chế độ extractor hỏng).

## 0. Cái GÌ CHƯA đo — đọc trước

- **Chất lượng trích xuất fact từ summary của card trên model thật: CHƯA đo.** Worker và extractor trong mọi test ở đây là **kịch bản**: mock đọc fact ra khỏi khối `¤¤FACTS{…}¤¤` mà driver nhét vào summary của `kanban_complete`; nó không hiểu gì. Kết quả chứng minh **đường ống** (worker nhận gì, card xong gửi gì cho extractor, fact rơi vào đâu), không nói gì về việc một model thật có rút ra fact đúng/đủ từ một câu summary hay không.
- **Độ nhiễu của fact sinh từ summary card: CHƯA đo.** Quyết định #3 của plan: P5 phải đo; nếu rác thì đổi mặc định sang tắt (hiện: bật khi `facts_write=project`).
- **Model thật có gọi `kanban_complete` kèm summary có nghĩa hay không: chưa biết.** Cổng retain chỉ mở khi worker tự gọi `kanban_complete` với summary không rỗng (§4.1). Model nhỏ bỏ qua chỉ dẫn "Never stop without calling one of them" thì không retain gì — đó là điều P5 nên đếm.
- Embedding model thật: không dùng (e2e chạy FTS-only như cài đặt mặc định; eval dùng embedding giả lập như P2).
- Recall quá hạn / lỗi / panic: chỉ test đơn vị (directory giả); e2e không ép được model-store treo.
- Card nhãn `worktree` trong project, worker của app (`assignee` là Space App), Windows: không đổi so với P3a, không thử lại.

## 1. Kết quả

| Việc giao | Kết quả | Test tự động | Chạy thật (daemon cô lập, worker kanban thật, LLM + extractor kịch bản) |
|---|---|---|---|
| (1) Đọc: `prompt` của card trong project = `<memory_context>` + title + description | **đạt** | `kanban::dispatch` 3 (khối dẫn đầu prompt; câu hỏi = title+description của card, gửi cho đúng project; project không có gì để nói / chỉ trắng ⇒ `WorkItem` trùng từng trường (so JSON) với trước; board không project không bao giờ bị hỏi và `WorkItem` trùng JSON; hướng dẫn + thư mục không đổi) | request đầu của worker A: sau hai khối `<system-reminder>` của engine (ngày, gợi ý skill), task mở đầu bằng `<memory_context>` có đúng fact gõ tay `7421` của Alpha, rồi title, rồi description; worker B chỉ thấy fact của Beta |
| Chỉ bank project: không fact project khác, không fact chung | **đạt** | `projects::directory` `a_worker_is_told_the_facts_of_its_own_project_and_no_others` (arm thật trên `FactEngine` thật: bank A, bank B, fact chung origin `''`, fact mức chat; chỉ A lọt vào; project không có fact ⇒ `None` chứ không phải fact của agent); `projects::facts` 2; **eval (e)** 4 ca × 2 chế độ | prompt của A không chứa `9090` (B), `quokkaville` (fact chung bank `shared`), `vault 12` (C), `table 55` (D); tương tự B/C/D/A2 |
| Project `archived`, `facts_write=chat/off` vẫn đọc được | **đạt** | `an_archived_projects_facts_are_still_read` | C (`off`) và D (`chat`) nhận fact gõ tay của chúng; B đã `DELETE` (lưu trữ) vẫn nhận `9090` và fact `03:15` của card trước |
| Recall không làm hỏng/chậm claim: lỗi, panic, quá hạn ⇒ prompt như không có recall + log | **đạt** (test đơn vị) | `facts_that_do_not_answer_in_time…` (recall treo vĩnh viễn, ngân sách 100 ms), `facts_that_fail_outright…` (panic), `cards_claimed_together_wait_for_their_facts_together` (3 card treo, ngân sách 300 ms ⇒ cả tick < 700 ms chứ không 900 ms) | — (không ép được ở e2e) |
| Board không project ⇒ không recall, `WorkItem` trùng từng trường | **đạt** | như trên + test P3a cũ vẫn xanh | worker X (board thường): task bắt đầu bằng title, không có `<memory_context>`, không dấu vết fact nào |
| (2) Ghi: `complete` + `facts_write=project` ⇒ retain `Kanban task "<title>" completed: <summary>`, `source='kanban'`, `origin=''`, qua `projects::fact_target` | **đạt** | `what_a_worker_wrote_when_it_closed_the_card_is_kept_in_the_project` (project, đúng văn bản, source `kanban`); `projects::facts::a_worker_keeps_what_it_finished_in_the_projects_bank…` (cùng đích với chat của project); `projects::directory::what_a_worker_finishes_is_kept_only_where_the_project_keeps_its_facts` (project / chat / off / lạ / id xấu; đọc **live**; lưu trữ vẫn retain) | A: bank `project:<A>` có thêm 1 fact `source=kanban`, `origin=''`, nằm cạnh fact gõ tay (không thay); extractor nhận đúng `Kanban task "Where does the Alpha payment gateway listen" completed: Set up the Falcon blue-green deploy pipeline for Alpha.` — summary của worker, **không** phải `MOCK-DONE KA1`; B và E (project chưa có fact nào) cũng vậy; fact của B không vào bank A |
| Card tiếp theo của project nghe được | **đạt** | — | A2 nhận cả `7421` (gõ tay) lẫn `Falcon` (fact do card A1 để lại) |
| Không retain: `block`, `gave_up`/Failed, timeout, summary rỗng, chuỗi tự đóng | **đạt** | `a_card_that_was_not_closed_by_its_worker_keeps_nothing` (5 tình huống: worker block; worker block sau một lần hoàn thành cũ; đóng không summary; để dispatcher đóng — không lời / có lời); `a_card_that_failed_timed_out_or_was_blocked_keeps_nothing…` (Failed/TimedOut/Blocked dù worker đã `complete`) | A3 (worker `kanban_block`), A4 (không gọi gì, dispatcher tự đóng — comment `dispatcher|complete`), A5 (`kanban_complete` summary rỗng): **không có request nào tới extractor** cho card đó |
| `facts_write=chat/off`, board thường ⇒ không retain | **đạt** | `worker_fact_target` + `a_card_of_a_board_without_a_project_keeps_nothing` | C (`off`), D (`chat`): số fact của bank không đổi, không request tới extractor, không bank mới nào; X (thường): không request, không fact `5150` ở đâu; cả ba card vẫn Done |
| Retain là fire-and-forget; `finalize` không chờ LLM, không fail vì nó | **đạt** | `finishing_a_card_never_waits_for_the_facts_to_be_kept` (retain treo vĩnh viễn, `finalize` vẫn về trong 5 s); `without_a_directory…` | A6: extractor trả HTTP 500 ⇒ daemon log `WARN extraction call failed`, card vẫn Done, không fact, các card sau vẫn chạy |
| (3) Kanban không import kho fact; stdio `kanban-server` vẫn `None` | **đạt** | `grep -n 'memory::' src/kanban/*.rs` không ra gì; trait `ProjectDirectory` thêm `recall`/`retain` (async, mặc định `None`/no-op ⇒ `StaticDirectory` và các impl khác không đổi) | stdio không đổi (không đụng `kanban-server`) |
| (4) Eval (e) "worker kanban nhận fact P" | **đạt** | `evals/memory/cases.json` mục `projects`: 4 ca mới (`worker-of-the-project-is-told-its-facts`, `…hears-the-change-of-mind`, `worker-of-another-project-is-not-told`, `worker-is-not-told-the-common-facts`); `eval.rs` cho `from.worker` dựng truy vấn bằng đúng `projects::worker_recall_query`; xanh ở cả hai chế độ | — |

**Tiêu chí 4, phần kanban** ("fact nói ở chat A của P được nhớ … ở worker kanban của P; không hiện ở project Q"; "lỗi retain/recall của project không làm hỏng"): đạt bằng test tự động và một lượt chạy card thật (tiêu chí 7 cũng giữ cho đường kanban: A6 và các test treo/panic).

## 2. Số liệu

- `cargo check --all-targets`: sạch, 2 cảnh báo có sẵn (`local_models/settings.rs:141`, `tools/write.rs:181`).
- `cargo test --lib`: **3255 passed, 0 failed, 11 ignored** (nền 3235 ⇒ +20: `kanban::dispatch` 13, `projects::directory` 5, `projects::facts` 2). HOME, `SENCLAW_HOME`, cwd riêng; chạy bản sao của binary test. `zen_core::session_store::tests::round_trip_save_load_clear` xanh.
- Eval `cargo test --lib memory::engine::eval`: 11 passed (2 ignored là model thật); 22 câu cũ + 10 ca project cũ không tụt, 4 ca worker xanh cả hai chế độ.
- **Kiểm tra test có cắn**: tạm bỏ cổng `role != "done"` ⇒ `a_card_that_was_not_closed_by_its_worker_keeps_nothing` đỏ (tình huống "block sau một lần hoàn thành cũ"); tạm cho truy vấn worker `folder="main"` + `include_common=true` ⇒ `worker-is-not-told-the-common-facts` LEAKED ở cả hai chế độ eval và `a_worker_reads_the_projects_bank_alone` đỏ. Khôi phục xong, chạy lại xanh.
- e2e: **49/49** check ([run4-final.log](project-p3b-e2e/evidence/run4-final.log), [result-p3b-p3b.json](project-p3b-e2e/evidence/result-p3b-p3b.json)). Binary daemon build từ cây mã cuối (sha256 `5b769b28917ffd53…`, [binary-sha256.txt](project-p3b-e2e/evidence/binary-sha256.txt)). Prompt đầu tiên của từng worker ở `evidence/prompt-K*.txt`; trích log daemon ở `daemon-log-excerpt.txt` (`retained from kanban stored=1` ×4, một `WARN extraction call failed … HTTP 500`).
  Ba lượt đầu **không** sạch, giữ nguyên làm bằng chứng: [run1](project-p3b-e2e/evidence/run1-43of49-driver-data-and-structure-fixed.log) 43/49 (driver: so sánh "bắt đầu bằng title" quên hai khối `<system-reminder>` mà engine luôn chèn trước prompt; và dữ liệu thử sai — xem §6.2), [run2](project-p3b-e2e/evidence/run2-48of49-driver-typo.log) và [run3](project-p3b-e2e/evidence/run3-48of49-same-typo-fix-not-applied.log) 48/49 (một lỗi chính tả trong chuỗi mong đợi của driver; lần sửa đầu bằng `sed -i` trên macOS không áp dụng được). Không lượt nào lộ lỗi của daemon.
- An toàn môi trường: `SENCLAW_HOME`, `HOME`, cwd riêng; cổng 58788/58789/58931; daemon chạy dưới Seatbelt chặn kết nối loopback tới 18788/18789/18790; `SENCLAW_BASE_URL=<cổng UI thử>` + `SENCLAW_APP_TOKEN_MODE=off` đặt trong `env-up.sh` và **driver từ chối chạy** nếu thiếu (chốt của P3a được giữ, thêm điều kiện token). Log daemon: 0 dòng nhắc 18788/18789/18790; `target/debug/senclaw` (daemon thật) mtime vẫn 2026-10-17 22:19, daemon thật vẫn pid 14613 trên 18788. Binary e2e build vào bản COW của `target/debug`, đã xoá.
- OpenAPI: không đổi route ⇒ không cần sinh lại.

## 3. Đã làm

1. **`kanban::ProjectDirectory`** (`kanban/projects.rs`): trait thành `#[async_trait]`, thêm `recall(project_id, query) -> Option<String>` (khối đã bọc, `None` = không có gì) và `retain(project_id, text, source)`; cả hai có mặc định rỗng. Kanban vẫn không import `memory::engine`.
2. **`DbProjectDirectory`** (`projects/directory.rs`): giữ thêm `Config` (tham số thứ 4 của `new`, `lib.rs` truyền `cfg.clone()`). `recall` = `memory::engine::pre_retrieval(ArmSelection{curated:false, cognitive:false, facts:true}, worker_recall_query(..), RecallConfig{max_tokens: engine_max_tokens})`, bỏ dòng cuối `format::DETAIL_HINT` (worker không có tool memory), bọc `<memory_context>`. `retain` đọc project **ngay lúc đó**, hỏi `worker_fact_target`, rồi `facts::retain_text` (tự scrub secret, tự log).
3. **`projects/facts.rs`**: `worker_fact_target` (chỉ `facts_write=project`, đi qua `fact_target`), `worker_recall_scope` (`include_common` luôn `false`), `worker_recall_query` (`folder=""`, `chat_jid=""`). Một chỗ quyết định cho worker, cạnh `fact_target`/`recall_scope` của chat.
4. **`KanbanDispatchProvider`**: `claim_ready` chia hai bước — chọn card chạy được (kiểm root như P3a), rồi hỏi recall **đồng thời** cho mọi card (mỗi lời hỏi là một `tokio::spawn` có `timeout(RECALL_BUDGET = 3 s)`; panic/timeout ⇒ log + `abort`), rồi dựng `WorkItem` (`prompt = khối + "\n\n" + title/description`). `finalize` gọi `retain_completion` sau khi đã xử lý card (§4.1). Hằng `DISPATCHER` thay chuỗi `"dispatcher"` lặp ở 5 chỗ.
5. **`memory/engine/format.rs`**: tách `DETAIL_HINT` thành hằng (không đổi hành vi `render`; test khẳng định `render` kết thúc bằng nó).
6. **Eval**: 4 ca + trường `worker` trong `Asker`; `ask_from` dựng truy vấn worker bằng `worker_recall_query`.
7. **Tài liệu**: `CLAUDE.md` (sửa "later the kanban worker" thành hiện tại; thêm một gạch đầu dòng về đọc/ghi của worker), `plan.md`. `docs/projects.md`, `docs/kanban-guide.md`, `docs/memory-engine.md` là P5.

## 4. Quyết định tôi tự chọn

1. **Văn bản retain là `complete` comment của chính worker, không phải `Outcome::Completed::summary`.** Phát hiện khi đọc `mcp_dispatch::execute`: `Outcome::Completed.summary` là *dòng đầu tiên của câu trả lời cuối* của worker ("Done."), và `Completed` đến cả khi worker đã `kanban_block` rồi kết thúc êm. Summary mà worker tự viết nằm ở comment `kind=complete` do `kanban_complete` ghi. Cổng retain: (a) outcome là `Completed`; (b) card đã ở cột `done` **trước** khi `finalize` đối soát (tức do worker/REST đóng, không phải dispatcher); (c) board có project; (d) tồn tại comment `complete` của tác giả khác `dispatcher`, khác rỗng; (e) directory quyết `facts_write=project`. Hệ quả: card mà worker kết thúc không gọi `kanban_complete` và dispatcher tự đóng **không** retain, kể cả khi dòng đầu của câu trả lời cuối không rỗng (brief chỉ loại trường hợp chuỗi tự đóng / rỗng). Lý do: dòng đầu đó có thể là một câu hỏi ("Which approach do you prefer?") hoặc "Done."; retain nó là nguồn nhiễu lớn nhất. Muốn bao gồm lại: bỏ điều kiện (b) và chuyển sang đọc comment của dispatcher — một thay đổi nhỏ ở `retain_completion`.
2. **Thứ tự prompt**: khối trước, rồi title, rồi description (đúng chữ giao). Trong chat khối là turn context nằm cuối tin nhắn; worker không có turn context nên nó ở trong prompt.
3. **Khối của worker bỏ dòng "call the memory_recall tool …"** vì worker (`skip_mcp_init`, chỉ MCP kanban) không có tool đó; một model nhỏ sẽ phí một lượt thử gọi nó.
4. **Cắt summary ở 4.000 byte** (thêm `…`) trước khi đưa cho extractor — chặn chi phí nhiều lời gọi model cho một summary dài bất thường. Không có trong brief; cắt theo ranh giới ký tự.
5. **Ngân sách 3 s tính cho cả tick**, không phải mỗi card (recall đồng thời). `arm_timeout` của engine (3 s) vẫn nằm trong đó.
6. **Project đã lưu trữ**: đọc như P2 (bất kể `archived`/`facts_write`), và **retain** nếu `facts_write=project` (giống chat của project đã lưu trữ vẫn tiếp tục ghi).
7. `DbProjectDirectory::new` thêm tham số `Config` — cần cho `retain_text` (model trích xuất, `engine_defense`) và giới hạn recall (`search_max_results`, `engine_max_tokens`). Test dùng `Config::from_env()`.
8. Test đơn vị của recall dùng `recall()` + `format::render` với `fact_arms(engine)` truyền vào, vì kho fact của daemon là singleton `OnceLock`; production gọi `pre_retrieval` như giao. Phần dính `default_arms` (chỉ chọn `facts:true`) được e2e phủ.
9. Văn bản retain bằng tiếng Anh như đề xuất (`Kanban task "<title>" completed: <summary>`); chỉ là đầu vào cho model.

## 5. Chưa chạy / chưa kiểm chứng

- Mục §0.
- Chưa chạy với **model thật** làm worker lẫn extractor; chưa đo bao nhiêu card thật kết thúc bằng `kanban_complete` kèm summary dùng được.
- Hai lần retain cho cùng một card (card đưa lại về Ready rồi hoàn thành nhưng worker lần sau gọi `kanban_complete` summary rỗng): `retain_completion` đọc comment `complete` mới nhất của worker, có thể là của lần chạy trước ⇒ cùng một câu được đưa lại cho extractor; `restated` của retain gộp thành củng cố, không sinh bản sao. Không có test e2e.
- `RECALL_BUDGET` cố định 3 s, không cấu hình được (không có yêu cầu).
- Không đo độ trễ recall của worker trên bank lớn (P2 đo arm ≤52 ms p95 ở 5.000 fact; worker dùng đúng bốn arm đó).
- Client web / desktop (P4) chưa hiển thị gì về việc này.

## 6. Phát hiện ngoài phạm vi (chưa sửa)

1. **`finalize` đẩy card mà worker đã `kanban_block` sang Done.** Nhánh `Outcome::Completed` chỉ kiểm `role != "done"`, trong khi `Failed`/`TimedOut` kiểm cả `blocked`. Worker gọi `kanban_block` rồi kết thúc êm ⇒ `Completed` ⇒ card bị chuyển sang Done kèm comment `dispatcher|complete` (dòng đầu câu trả lời cuối). Bằng chứng trong e2e: card A3 có comment `agent|block "needs the staging key"` rồi `dispatcher|complete "MOCK-DONE KA3"` và nằm ở Done (`where the blocked / left cards ended: blocked: done`). Có từ commit đầu (`45e5df1`), không do project hay P3b. P3b không dựa vào nó (cổng retain nhìn `role` trước đối soát). Sửa: `if role != "done" && role != "blocked"` trong nhánh `Completed`.
2. **Fact gõ tay không bị thay thế + gộp gần-trùng ⇒ recall có thể chỉ cho thấy giá trị cũ.** Lượt e2e đầu dùng cặp "gateway listens on port 7421" (gõ tay) và "gateway now listens on port 8123" (từ card): recall chỉ trả 7421 vì `merge_near_duplicates` gấp hai câu gần giống làm một và giữ câu xếp trên, còn `supersede` từ chối thay fact `manual` (P2). Đổi sang hai fact khác chủ đề thì cả hai hiện. Với model thật, fact "đã đổi sang cổng 8123" từ summary card sẽ không đè được fact "cổng 7421" do người gõ, và có thể bị gộp mất. Đúng thiết kế P2 (người gõ thắng), nhưng đáng đo ở P5 vì card là nơi giá trị đổi nhiều nhất.
3. **Kênh tiêm lệnh bền vững**: summary của worker (có thể vừa đọc nội dung web) → extractor → bank project → khối recall của mọi worker/chat sau. Cùng loại với reflection của chat (fact được trích bởi model thành câu khai báo ngắn, `defang` thẻ đóng, scrub secret), không thêm bề mặt mới, nhưng đây là đường ghi tự động duy nhất vào bank project không qua người gõ; tắt bằng `facts_write=chat/off`.

## 7. Chạy lại

```bash
# test (HOME, SENCLAW_HOME, cwd riêng; dùng chung CARGO_TARGET_DIR; sao binary ngay sau khi build)
cd .worktrees/senclaw-project-kanban-facts && export CARGO_TARGET_DIR=/Users/benji/Projects/SenClaw/senclaw/target CARGO_INCREMENTAL=0
cargo check --all-targets && cargo test --lib --no-run     # in đường dẫn binary test (hash trùng giữa các worktree)
S=<scratch>; mkdir -p $S/home $S/cwd; cp <binary test> $S/lib-test; cd $S/cwd && HOME=$S/home SENCLAW_HOME=$S/home/.senclaw $S/lib-test

# e2e: build vào bản COW của target/debug (không ghi đè target/debug/senclaw đang dùng)
cp -c -R $CARGO_TARGET_DIR/debug $S/p3b-target/debug && CARGO_TARGET_DIR=$S/p3b-target cargo build --bin senclaw
export P0_RUNS=$S/runs P0_UI=58788 P0_WS=58789 P0_MOCK=58931       # cổng rảnh, không bao giờ 18788/18789/18790
cd plans/reports/project-p3b-e2e && ./env-up.sh p3b $S/p3b-target/debug/senclaw && node exp-p3b.mjs p3b; ./env-down.sh p3b
rm -rf $S/p3b-target $S/runs                                         # trả đĩa
```

Commit sau khi người dùng yêu cầu — gợi ý: `feat(kanban): workers read and write the facts of their project`.

## Câu hỏi chưa giải quyết

1. Card mà worker kết thúc không gọi `kanban_complete` nhưng dispatcher tự đóng với một dòng không rỗng: có nên retain dòng đó? Hiện: không (§4.1).
2. Sửa lỗi §6.1 (`finalize` đẩy card bị block sang Done) ở nhánh riêng hay gộp vào đây? Nó ảnh hưởng mọi board, độc lập với project.
3. Fact gõ tay thắng fact do card nói sau (§6.2): giữ đúng như P2 hay cho card thay/đứng trước fact gõ tay trong recall?
4. `RECALL_BUDGET` có cần cấu hình được (env) không?
