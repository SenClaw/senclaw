# Kiểm chứng P0: Project cho session chat và Kanban

Ngày 2026-10-18 · Nền `main` @ ede1e16 · Phương pháp: **chạy** daemon cô lập + LLM giả có kịch bản (không đo chất lượng model)
Phục vụ: [plan.md](../261018-1618-project-feature/plan.md) P0 · [design.md](../261018-1618-project-feature/design.md) D6/D7 · [explore](explore-261018-1618-project-feature-current-state.md)
Harness + bằng chứng: [project-p0-spike/](project-p0-spike/) (chạy lại: xem mục 7).

## 1. Kết quả

"FAIL" = hành vi **hiện tại** của daemon sai theo tiêu chí P0 (suy luận của explore/design đứng vững, tính năng Project phải sửa). "PASS" = cơ chế dùng được như design giả định.

| # | Câu hỏi | Kết quả | Suy luận trong design | Ảnh hưởng |
|---|---|---|---|---|
| a | 2 code session cùng profile, `allowedWorkDirs` khác nhau: có thấy nhầm thư mục? | **FAIL** (đã dựng lại F1): `workspace_info`, `datagen_start` (đường tương đối), `doc_write` thấy thư mục của session kia. `Bash`/`Write`/`Read` thì đúng | Đúng (F1) | D6 cần thêm phần dưới |
| b | DAG dispatch trong session khác chạy ở thư mục nào? | **FAIL**: chạy ở thư mục **của session tạo core sau cùng** của profile, không phải của session gọi. Parent xếp hàng còn đọc ở tiến trình daemon, không qua env | Đúng, nhưng thiếu 1 đường (parent queued) | Sửa 5.2/D6 |
| c | `OneShotOptions.custom_rules` có vào system prompt worker? | **FAIL**: không. Persona `system_prompt` vào, guidance (→ `custom_rules`) không có mặt ở đâu trong request. Worker kanban thật: system prompt = guidance của provider, cwd = `workspace_dir`, không recall (mục c′) | Đúng (F2, F3) | Giữ D8: nối vào `guidance`/`prompt` |
| d | `destroy_agent_full(jid)` rồi nhắn tiếp: lịch sử còn? MCP env dựng lại? | **PASS có điều kiện**: lịch sử còn nguyên, cwd và env MCP dựng lại theo binding mới. Nhưng: không có đường gọi; **rò tiến trình MCP cũ**; **ghi đè file state của folder làm lệch session anh em** | Đúng 2/2, thiếu 3 điều | Sửa D7 |
| e | Xoá cả bank `mem_facts` (FK ON): `mem_fact_entities`, `mem_links`, `mem_facts_fts` đi theo? | **PASS**: cả ba đi theo, bank khác nguyên vẹn. Phụ thuộc `foreign_keys=ON`; xoá hàng loạt **O(n²)** do trigger FTS | Đúng | 5.6: xoá theo lô |

Phát hiện ngoài 5 câu: E1–E6 ở mục 5.

## 2. Cách chạy (cô lập)

- **Binary**: `main` ede1e16 + 1 hook 33 dòng ([spike-hook.patch](project-p0-spike/evidence/spike-hook.patch), chỉ để gọi `destroy_agent_full` từ ngoài — mục d). Build ở worktree detached `.worktrees/senclaw-project-p0`, sha256 `8f66d956…29da0` ([binary-sha256.txt](project-p0-spike/evidence/binary-sha256.txt)). Bản cài đặt (79b40f8) **không** dùng, cũng không đụng.
- **Đĩa**: ổ chỉ còn 2.5 GB. Đã xoá `senclaw/target/debug/incremental` (12 GB, cache build cũ từ 16/10, tái sinh được). Build vào bản nhân bản COW của `target/` (`cp -c`) để **không ghi đè `target/debug/senclaw`** dùng chung; sau khi xong xoá bản nhân bản và worktree.
- **Cô lập**: `HOME`, `SENCLAW_HOME`, **cwd** đều là thư mục tạm trong scratchpad; UI/WS `38788/38789`, LLM giả `38931`; không chạm `18788/18789` hay `~/.senclaw`. Sau mỗi lần boot: `grep -c "MCP-external HTTP connect"` = **0**. Chỉ dừng tiến trình do harness ghi pid.
- **LLM giả** ([mock-llm.mjs](project-p0-spike/mock-llm.mjs)): OpenAI-compatible (stream + không), phát lại tool call do test đặt trong tin nhắn (`§§STEPS{"tag":..,"steps":[..]}§§`), ghi mọi request (system prompt, danh sách tool, kết quả tool) để grep. Trả fact cố định cho cuộc gọi trích fact. Không đo model.
- **Ghi chú harness**: tool MCP không nằm sẵn trong roster (34 tool), phải `ToolSearch select:` trước; `Bash` bắt buộc `description`; tiền tố `@@` bị daemon coi là chỉ thị đính kèm file nên không dùng.

## 3. Chi tiết (a) và (b)

Hai code session cùng `folder=shared`: `web:shared:a` (`allowedWorkDirs=[proj/A]`), `web:shared:b` (`[proj/B]`); mỗi thư mục có `seeds.jsonl` khác nội dung. Mỗi lượt gọi: `Bash pwd`, `workspace_info`, `Write` đường dẫn tương đối, `datagen_start {pipeline:import, source:{file:"seeds.jsonl"}}` (đường tương đối). Lệnh: `node exp-ab.mjs <label> bundled` ([kết quả](project-p0-spike/evidence/result-ab-bundled.json)).

Diễn biến file `workspace-state-shared.json`: A1 tạo core → `proj/A`; B1 tạo core → **ghi đè** `proj/B` (pool.rs:1015-1021); A2 không tạo lại core nên file vẫn `proj/B`.

Lượt A2 (A đang làm, file state = B):

| Công cụ | Thấy | Đúng? |
|---|---|---|
| `Bash pwd` | `proj/A` | đúng (engine giữ cwd theo jid) |
| `Write rel-A2.txt` | `proj/A/rel-A2.txt` | đúng |
| `workspace_info` | `currentDir: proj/B`, `isAtDefault: false` (`allowedWorkDirs`, `defaultWorkspace` vẫn `proj/A`) | **sai** |
| `datagen_start` import `seeds.jsonl` | dataset `imp-a2` = `seed-from-B one \| seed-from-B two` | **sai**: đọc file của B |
| `doc_write` (chỉ có khi `SENCLAW_MCP_BUNDLED=false`, [kết quả](project-p0-spike/evidence/result-ab-separate.json)) | `proj/B/docs/p0-A2.md` | **sai**: ghi tài liệu vào repo của B |

Đối xứng được chứng minh thêm ở mục (d): tạo lại core của một session làm session anh em thấy thư mục mới (lượt `D22` của `exp-d.mjs`).

**(b) DAG.** `create_parent_and_run` với 2 task `persona:p0-worker` (worker chạy `Bash pwd` + `Write` tương đối), gọi từ A rồi từ B, file state đang là B:

- Parent `p-20261018-0001` (chat `web:shared:a`): `sharedWorkspace = proj/B`; cả hai worker `pwd` = `proj/B`, ghi `dag-A3-1.txt`, `dag-A3-2.txt` vào **proj/B** (proj/A không có).
- Parent `p-20261018-0004` (chat `web:shared:b`): `proj/B`: đúng chỉ vì B tạo core sau.
- Cơ chế: `dispatch_server.rs:212` (lúc tạo parent) và `bridge.rs:1183` (`read_admin_workspace`) ghép `workspace-state-{admin_folder}.json`; worker persona lấy cwd = `shared_workspace` (`bridge.rs:1560`).
- **Parent xếp hàng**: tạo khi DAG khác của cùng folder đang chạy thì `shared_workspace: None` (`dispatch_server.rs:515`) và `status=queued`; khi tới lượt, `activate_next_queued` (`bridge.rs:1145`, **trong daemon**, không có env của session nào) đọc lại file state theo folder. Quan sát: `p-20261018-0010` ghi `sharedWorkspace=''` lúc xếp hàng.

## 4. Chi tiết (c), (d), (e)

**(c) `custom_rules`.** Đường dùng được ngoài kanban và đi cùng `run_one_shot`: bước agent của workflow (`step_runners.rs:134`: `guidance → custom_rules`, `persona.system_prompt → system_prompt`). Workflow `p0-wf` có guidance mức workflow (`GUIDANCE-DEF-MARKER-91c2`) và mức bước (`GUIDANCE-STEP-MARKER-b84d`), persona có `PERSONA-SYSTEM-MARKER-7f3a`. `node exp-c.mjs <label>` ([kết quả](project-p0-spike/evidence/result-c.json)):

```
requestBodyBytes 7445, systemPromptChars 4011
personaMarkerInSystemPrompt          true
guidanceDefMarkerInSystemPrompt      false
guidanceStepMarkerInSystemPrompt     false
anyGuidanceMarkerAnywhereInRequestBody false
```

Bản ghi run vẫn có `guidanceSnapshot` chứa cả hai marker, tức daemon **tin** là đã giao. Đọc code khớp: `custom_rules` chỉ có khai báo (`zen_core/mod.rs:1156`, `:1210`), gán (`isolated_runner.rs:224`) và không ai đọc. Chạy lại độc lập từ bản copy harness trong repo cho cùng kết quả (request 7453 B, cùng 3 marker `false`).

**(c′) Worker kanban thật** (bổ sung sau, binary `main` ede1e16 không hook, dispatcher bật bằng `POST /api/dispatch-config {"enabled":true}`; `node exp-kanban.mjs <label>`, [kết quả](project-p0-spike/evidence/result-kanban.json)). Hai board mẫu `standard`, mỗi board một card ở Ready; board 1 có `workspace_dir=proj/A`, board 2 không có.

| | Card board có workspace (K1) | Card board không có workspace (K2) |
|---|---|---|
| System prompt | 5.106 ký tự, **mở đầu bằng guidance** `You are running Kanban task #1…`; **không có** `You are a helpful AI assistant` (prompt mặc định bị thay, đúng F3/explore §4) | cùng dạng (5.128 ký tự, `#2`) |
| Phần còn lại của system prompt | env (cwd, OS), danh sách 21 skill, khối AGENTS.md | như K1 |
| Tin nhắn user | có tiêu đề card (`P0 card with workspace`) | có tiêu đề card |
| Recall / hồ sơ | **không** có `<memory_context>`, `<user_profile>` hay `MEMORY.md` (F3 xác nhận) | như K1 |
| `Bash pwd` | `proj/A` = `workspace_dir` của board | `home/.senclaw/mcp-dispatch/2` (thư mục tạm theo id card, dưới `SENCLAW_HOME`) |
| Tool kanban | 33 tool trong roster, **không** có tool kanban; phải `ToolSearch select:mcp__senclaw-kanban__…` rồi gọi được; `kanban_complete` đưa card sang cột Done | như K1 |

Kết luận cho P3: đường D8 (nối khối project vào `guidance`, recall vào `prompt`) là đường duy nhất worker đọc được; `custom_rules` thì không (mục c). Cwd lấy từ `board.workspace_dir` đúng như design 5.5; không có workspace thì rơi vào scratch dưới `SENCLAW_HOME`, không phải root nào.

**(d) `destroy_agent_full`.** Hai session cùng `folder=dshare` (`web:dshare:1` → A, `web:dshare:2` → B). Sau 2 lượt của D1 và 1 lượt của D2: `update:group` đổi `allowedWorkDirs` của D1 sang `proj/A2`, ghi `web:dshare:1` vào `spike-destroy-full` (hook gọi `destroy_agent_full`), rồi nhắn tiếp. `node exp-d.mjs <label>` ([kết quả](project-p0-spike/evidence/result-d.json)). Log: `[SPIKE] destroy_agent_full(web:dshare:1)` … `returned`.

| Quan sát | Kết quả |
|---|---|
| Lịch sử | Request đầu của lượt sau destroy mang **18 message** (cả D11, D12); lượt sau nữa 26 → còn nguyên, được nạp lại từ lịch sử LLM đã lưu |
| cwd | `Bash pwd` = `proj/A2` (binding mới) |
| Env MCP | `core-server` mới: `SENCLAW_DEFAULT_WORKSPACE=proj/A2`, `SENCLAW_ALLOWED_WORK_DIRS=["proj/A2"]`; `workspace_info` = `proj/A2` |
| Tiến trình MCP cũ | **Không bị dừng**: pid 23645 (env `proj/A`, tạo 10:15:00) vẫn sống khi kiểm tra lần cuối, ~1 phút 50 giây sau destroy (10:15:07); ngay sau dựng lại đã có **2 core-server** cho `web:dshare:1` (`ps eww`). `ZenEngine::dispose` (`engine.rs:1279`) không đóng MCP con; `McpManager::dispose` chỉ chạy lúc tắt |
| File state folder | Dựng lại ghi đè `workspace-state-dshare.json` = `proj/A2`; session anh em D2 sau đó: `workspace_info` → `currentDir: proj/A2`, `isAtDefault: false` (cwd của Bash vẫn `proj/B`) |
| Watcher bộ nhớ | `destroy_agent_full` gọi `MemoryManager::destroy_agent(folder)` → huỷ watcher của cả folder (log `watcher cancelled`); `init_agent(dshare)` dựng lại ở lần tạo core kế tiếp (~2.5 s sau) |
| Đường gọi | `destroy_agent_full` chỉ được `destroy_all` (tắt daemon) gọi. WS `agent:control stop` → `stop_agent` chỉ `create_session` lại (`pool.rs:2602-2690`) và **không** dựng lại MCP/cwd — đã chạy (`exp-g.mjs`, [kết quả](project-p0-spike/evidence/result-g.json)): `update:group` đổi `allowedWorkDirs` sang `proj/A2` rồi Stop, lượt sau vẫn `Bash pwd`=`proj/A`, `workspace_info`=`proj/A`, vẫn 1 `core-server` (lịch sử còn, 10 message). Gọi cho jid không tồn tại: êm, không hỏng gì (lượt `D14` của `exp-d.mjs` vẫn chạy) |

**(e) Xoá bank.** Daemon bật `foreign_keys=ON` ở cả 2 kết nối (`db/mod.rs:145,155`); MCP memory server dùng cùng `Db::open`. Bảng `mem_*` nằm trong `senclaw.db`. Dữ liệu do chính pipeline tạo (mock trả fact, `when` cùng ngày → có link `temporal`; không có embedding nên không có link `semantic`). Bản sao DB, `sqlite3` 3.54 có FTS5, [exp-e.sh](project-p0-spike/exp-e.sh) ([số liệu](project-p0-spike/evidence/e-cascade.txt)): `DELETE FROM mem_facts WHERE bank='shared'` (11 fact):

| | FK ON (như daemon) | FK OFF |
|---|---|---|
| `mem_fact_entities` mồ côi | **0** (77 → 48) | 29 |
| `mem_links` mồ côi | **0** (100 → 60) | 40 |
| `mem_facts_fts` mồ côi | **0** (trigger `mem_facts_ad`) | 0 (trigger không cần FK) |
| bank `dshare` | 12 fact / 32 entity / 45 link / 12 fts, **không đổi** | không đổi |

Xác nhận trên kết nối sống của daemon: `DELETE /api/memory/facts/:id` một fact có 3 entity + 5 link + 1 fts → cả ba về 0.

Hiệu năng (cùng cách, bank tổng hợp, FK ON, **một lệnh DELETE**): 2.000 fact → 0,23 s; 8.000 → 2,88 s; 20.000 → **17,8 s** — bậc hai vì trigger `DELETE FROM mem_facts_fts WHERE fact_id = old.id` quét FTS cho từng hàng (`fact_id UNINDEXED`). `Db` là một kết nối sau `Mutex`, nên xoá 20k fact chặn mọi truy vấn khác gần 18 s.

## 5. Phát hiện ngoài 5 câu

- **E1. Stop của một session huỷ DAG của session khác cùng profile.** `agent:control stop` ở `web:fshare:2` → `stop_agent` → `cancel_admin_parents(folder)` (`bridge_api.rs:77`): task của DAG do `web:fshare:1` tạo báo `Cancelled: admin agent stopped` ([result-f.json](project-p0-spike/evidence/result-f.json)).
- **E2. DAG xếp hàng theo profile, không theo session**: DAG của F2 tạo khi DAG của F1 đang chạy trả `QUEUED (another dispatch is active…)`.
- **E3. `doc_write` không có trong cấu hình mặc định.** `senclaw-docs` có `build_child!` trong `core_server.rs:215` nhưng không nằm trong `DEFAULT_CORE_SERVERS` (`helper.rs:548`); `ToolSearch` báo `No tool is named mcp__core__doc_write`. Chỉ có khi `SENCLAW_MCP_BUNDLED=false` (`mcp__docs__doc_write`). Lỗi có sẵn, ngoài phạm vi.
- **E4. Registry persona nạp trước khi thư mục `virtual-agents/` tồn tại** (data home mới): log `Primary directory not found`; `list_agents` vẫn liệt kê `persona:*` (đọc đĩa) nhưng task DAG báo `Never started: its agent or persona could not be resolved` sau 60 s. Thấy ở harness với home trống; chưa kiểm trên cài đặt thật. Harness tạo sẵn persona trước khi boot.
- **E5. `custom_rules` còn hai nơi tin vào nó**: guidance của workflow (đã chạy) và `continuity_context` của tác vụ nền (`background/runner.rs:238`, chỉ đọc code, chưa chạy).
- **E6.** `ToolSearch` bắt buộc trước mọi tool MCP trong chat; `Bash` bắt buộc `description` (chỉ ảnh hưởng harness).

## 6. D6/D7 trong design.md cần sửa gì

**D6 — đúng hướng, chưa đủ.**
1. Giữ: file state riêng theo chat cho session thuộc project. Phần workspace/docs/datagen đọc đường dẫn qua env `SENCLAW_WORKSPACE_STATE_FILE` do pool đặt lúc spawn (`core_mcp_config`, `pool.rs:1085`) nên `workspace_state_key` là đủ cho 3 tool này. (`bridge_servers.rs:47` ghép tên riêng cho các server cầu nối — phải đổi cùng.)
2. **DAG không đọc env**: `dispatch_server.rs:212`, `virtual_server.rs:95`, `bridge.rs:1183` ghép `workspace-state-{admin_folder}.json`. 5.2 đã có `SENCLAW_PROJECT_ROOT` cho 2 server MCP; thêm: **(i) parent xếp hàng** — đặt `shared_workspace` lúc tạo kể cả khi `queued` (bỏ nhánh `if has_active {None}` ở `dispatch_server.rs:515`), hoặc cho `activate_next_queued` tra root theo `parent.chat_jid` (đã có trong parent). Không làm thì DAG thứ hai của một project chat vẫn chạy ở thư mục của folder.
3. Ghi vào phạm vi: **session không thuộc project vẫn mang F1** (hai code chat cùng profile). Tiêu chí 2 chỉ hứa cho session trong project; nếu muốn sửa cho mọi code chat thì khoá file state theo chat cho tất cả — cần quyết định của người dùng.
4. Ghi vào phạm vi: **E1/E2** — hàng đợi và Stop của DAG vẫn khoá theo `admin_folder`. Hai chat thuộc hai project khác nhau của một profile vẫn xếp hàng DAG và Stop của chat này huỷ DAG của chat kia. Hoặc chấp nhận và nêu ở tài liệu, hoặc khoá DAG theo (folder + project).

**D7 — viết lại.**
1. "Dựng lại core bằng `destroy_agent_full`" chạy được: lịch sử còn, cwd và env MCP theo binding mới. Nhưng phải làm thêm: **(i) thêm đường gọi** (hiện chỉ shutdown gọi); **(ii) đóng MCP con của jid** trước khi tạo lại (rò tiến trình, mỗi lần đổi project thêm một `core-server`); **(iii) chỉ an toàn sau D6**: nếu không, dựng lại ghi đè file state của folder và làm lệch mọi session anh em (lượt `D22` ở mục d); **(iv)** `destroy_agent_full` huỷ watcher bộ nhớ của cả folder trong vài giây — chấp nhận được.
2. Hệ quả cho câu hỏi mở #4 (đổi project cho session đang chạy): chi phí (i)+(ii)+(iii) không nhỏ; phương án "chỉ gán lúc tạo session" rẻ hơn. P0 cho phép cả hai, **không** cho phép giữ nguyên D7 như đã viết.

**Khác.**
- 5.6 (xoá project → xoá bank): cascade đủ với FK ON; ghi thêm **xoá theo lô (≤500 hàng/lệnh, nhả khoá DB giữa các lô)** vì `DELETE` một lệnh là O(n²) (mục e). Xoá phải đi qua `Db` (FK ON), không qua kết nối tự mở.
- R3 / D8: giữ — dùng `guidance` (mục c′: đúng là system prompt của worker kanban) và `prompt` cho recall; thêm test kiểm khối project **có mặt** trong request (harness (c)/(c′) làm được việc này, tái dùng ở P3/P5). Lưu ý: board không có `workspace_dir` chạy ở scratch dưới `SENCLAW_HOME`, nên "board gắn project" phải đặt `workspace_dir` = root (design 5.5 đã nêu).
- Thứ tự P1: **D6 trước D7**; đừng làm D7 trước.

## 7. Chạy lại

```bash
cd senclaw/plans/reports/project-p0-spike
export P0_RUNS=/tmp/senclaw-p0-runs         # nơi đặt run-<label>/ (cũng cô lập HOME, SENCLAW_HOME, cwd)
./env-up.sh a <senclaw binary>                 # LLM giả :38931 + daemon :38788/:38789; in số dòng "MCP-external HTTP connect" (phải 0)
node exp-ab.mjs a bundled                      # (a)(b); thêm SENCLAW_MCP_BUNDLED=false ở env-up rồi "separate" để thử doc_write
node exp-c.mjs a                               # (c)
node exp-d.mjs a                               # (d): cần binary có spike-hook.patch
node exp-f.mjs a                               # E1/E2
node exp-g.mjs a                               # Stop có dựng lại cwd/MCP không
node exp-kanban.mjs a                          # worker kanban thật (dispatcher bật bằng REST)
./exp-e.sh <bản sao senclaw.db> ON             # (e); dùng "sqlite3 .backup" để tạo bản sao
./env-down.sh a
```

`env-up.sh` từ chối khi cổng đang có người nghe (lúc kiểm lại, một session khác đã giữ 38788/38789): đặt `P0_UI`, `P0_WS`, `P0_MOCK` sang cổng rảnh (rerun E1/E2, Stop dùng 48788/48789/48931; sau đó session P1 dùng chính các cổng đó nên kanban chạy ở 51788/51789/51931; đặt `LOG_TOOLS=1` trước `env-up` để mock ghi roster tool của mọi request). Không bao giờ dùng 18788/18789 (`lib.mjs` từ chối). Mọi tiến trình harness đã dừng khi xong.

## Chưa kiểm chứng

- Card kanban có nhãn `worktree` (`Workspace::Worktree`), WIP limit, card nhiều phụ thuộc: chưa chạy. Worker thật đã chạy cho card thường (mục c′).
- `continuity_context` của tác vụ nền (E5): chỉ đọc code.
- `update:group` một mình (không Stop) đổi cwd/MCP không: coi như không (explore + `exp-g` cho thấy cả Stop cũng không); không chạy riêng.
- Chat thường (`group_type=chat`): không chạy; cwd mặc định theo profile nên hai chat thường cùng profile dùng cùng thư mục, không có cặp "khác thư mục" để lẫn.
- (e): xoá bằng `sqlite3` CLI 3.54 trên bản sao, không phải SQLite đóng gói của rusqlite; cascade qua kết nối daemon chỉ kiểm cho 1 fact (REST). Chưa thử bank có fact bị thay (`superseded_by`) cùng bị xoá (trigger `mem_facts_restore`).
- Chỉ macOS. Không đo model thật, không đo extractor (LLM giả).
- Chưa chạy lại trên bản cài đặt 79b40f8 (khác ede1e16 ở browser/prompt/web_fetch, không đụng code liên quan).

## Câu hỏi chưa giải quyết

1. Sửa F1 cho **mọi** code chat (khoá file state theo chat) hay chỉ session trong project?
2. DAG: chấp nhận hàng đợi/Stop theo profile (E1/E2) như hạn chế đã biết, hay khoá theo (folder + project)?
3. Câu hỏi mở #4: bỏ "đổi project cho session đang chạy" khỏi v1 (gán lúc tạo) hay làm D7 đủ (đường gọi + đóng MCP con)?
4. E3 (`doc_write` thiếu ở cấu hình mặc định) và E4 (persona registry trên home mới): ai xử lý, và có ảnh hưởng tới người dùng thật không?
