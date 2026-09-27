# BA Studio — Space App Business Analyst (apps/ba)

Thiết kế dựa trên nghiên cứu bộ **BA-Kit của AI4BA** (https://ai4ba.com/ba-kit, khảo sát 2026-08-02):
55 skills chia 9 giai đoạn làm việc của IT Business Analyst / Product Owner, chạy trên AI coding
agent, xuất tài liệu URD/BRD/PRD/SRS/use case/user story/wireframe/diagram theo quy trình BA chuẩn.

App này **không sao chép nội dung bán** của BA-Kit; nó tái hiện *quy trình + cấu trúc tài liệu chuẩn
BA* (vốn là kiến thức ngành: IEEE 830 SRS, Cockburn use case, Gherkin AC, BPMN…) thành một Space App
có UI + MCP, để agent SenClaw làm việc như một trợ lý BA: **chia theo giai đoạn, chạy theo workflow,
mọi bước đều xuất ra tài liệu có cấu trúc, có truy vết**.

- Port: **4740** · MCP server: **`ba-mcp`** (tool đầy đủ `mcp__ba-mcp__ba_*`) · DB:
  `~/.senclaw/space-app-data/ba/app.sqlite` (ngoài thư mục cài — install zip xoá app dir, override `BA_DATA_DIR`)
- Đối tượng: IT BA, PO/PM, người viết URD/BRD/PRD/SRS thường xuyên, người vẽ wireframe/diagram/use case/user story hàng ngày.

## 1. Nghiên cứu BA-Kit — những gì rút ra được

### 1.1 Cấu trúc 9 giai đoạn × 55 skills (nguyên văn từ trang)

| # | Giai đoạn | Skills |
|---|---|---|
| 1 | Lập kế hoạch sản phẩm (3) | `/prd`, `/roadmap`, `/discover` |
| 2 | Thu thập & đặc tả (7) | `/brainstorm`, `/urd`, `/brd`, `/prd-epic`, `/srs`, `/reverse-doc`, `/code-to-srs` |
| 3 | Sơ đồ nghiệp vụ (11) | `/sequence`, `/activity`, `/activity-swimlane`, `/bpmn`, `/state`, `/erd`, `/d2-erd`, `/d2-activity`, `/d2-architect`, `/dbdiagram`, `/usecase-diagram` |
| 4 | Use case & user story (3) | `/usecase`, `/userstory`, `/ac` |
| 5 | Thiết kế màn hình (6) | `/user-flow`, `/wireframe-ascii`, `/wireframe-html`, `/prototype-html`, `/prototype-next`, `/figma` |
| 6 | Tích hợp API (7) | `/api-assess`, `/api-doc`, `/api-design`, `/api-map`, `/api-checklist`, `/api-test`, `/api-readiness` |
| 7 | Kiểm thử (3) | `/test-checklist`, `/test-cases`, `/playwright-gen` |
| 8 | Kiểm soát chất lượng (6) | `/gap`, `/doc-drift`, `/ask`, `/cr`, `/dashboard`, `/kg` |
| 9 | Bàn giao & vận hành (9) | `/jira`, `/confluence`, `/export`, `/preview`, `/reverse-preview`, `/userguide`, `/meet`, `/update-overview`, `/delegate` |

### 1.2 Ba workflow mẫu (trang gốc gọi là "không ép quy trình cứng")

1. **Mặc định (trọn vòng đời)**: `/prd → /roadmap → /brainstorm → /srs → /wireframe-html → /userstory → /jira → /test-checklist → /test-cases`
2. **User story trước, chi tiết sau**: `/brainstorm → /userstory → /ac → /srs → /wireframe-html → /test-checklist → /test-cases`
3. **Prototype demo trước**: `/user-flow → /prototype-html → /brainstorm → /srs → /userstory → /test-checklist → /test-cases`

### 1.3 Phân tích output mẫu thật (skills-pack-examples)

Đã đọc 2 output mẫu công khai của kit — đây là "DNA" tài liệu mà app phải tái hiện:

**`srs-preview.html`** (feature *authentication*, 31 FR) cho thấy:

- Tài liệu tổ chức **theo feature** (slug: `authentication`, `payment`, `vocabulary-flashcard`…),
  file kiểu vault: `brainstorms/email-and-google-auth.md`, `srs/authentication-spec.md`,
  `srs/authentication-erd.md`, `ascii-wireframe/{flow}.md`, `_index.md`.
- **ID convention thống nhất, prefix theo feature**: `FR-authentication-001`, `NFR-…-001`,
  `BR-…-001`, `E-…-001` (error), `SC-…-01` (success criteria), `uc-login-email`, `US-…`, `TC-…`,
  `CR-YYYYMMDD-NNN`, `OQ-n` (open question, có trạng thái resolved).
- **SRS spec 11 mục**: 1 Scope (cover + KHÔNG cover) · 2 Actors & Stakeholders (bảng: loại
  người/hệ thống/ngoài, mục tiêu, trong scope?) · 3 FR (bảng: ID, Title, Description dạng
  "Khi/Nếu… hệ thống phải…", Priority P0/P1, **Verify by** demo/test, **Source** trỏ về mục
  brainstorm) · 4 NFR (Category: performance/availability/security/privacy/usability/compliance +
  Acceptance đo được) · 5 Business Rules (Rule, Trigger, **Implements FR**, Source) · 6 **Error
  Matrix** (Trigger, Severity, Related FR, Screen state — nguyên văn thông báo tiếng Việt,
  Recovery) · 7 Success Criteria (Outcome, Đo bằng, Mốc đạt %) · 8 Data Entities (tóm tắt, chi
  tiết ở erd.md) · 9 Flows (sequence diagram từng flow, header "Liên quan: FR… | Error: E… |
  Related UC: uc-…") · 10 Screens (tóm tắt, chi tiết ở wireframe) · 11 Constraints/Dependencies/
  Assumptions (kèm Source/Owner).
- Kèm phần **Giới thiệu chung**: Glossary (bảng: Thuật ngữ, Định nghĩa, Xuất hiện ở feature nào,
  Aliases "tránh dùng"), Operating Environment, Conventions.
- **Truy vết xuyên suốt**: FR → nguồn brainstorm; BR → FR; Error → FR; Flow → FR/E/UC; OQ resolved
  ghi ngay tại nguồn.

**`dashboard.html`** cho thấy hệ đo lường của kit:

- 4 KPI: **Truy vết (coverage %)** — FR có US phủ; **Tiến độ pipeline** — mỗi feature đi qua 8
  chặng `URD → BRD → PRD → SRS → UseCase → Story → AC → Test`; **Độ tươi tài liệu** (điểm, doc
  stale khi upstream đổi sau nó — "stale chain" lan truyền brainstorm → srs → usecase → test);
  **Rủi ro/việc gấp** (CR treo, review quá hạn, OQ tồn đọng).
- **Kanban lifecycle tài liệu**: `DRAFT → IN REVIEW → REVISIONS → APPROVED → SHIPPED`.
- Lỗ hổng truy vết: FR chưa phủ US, FR chưa có test, US mồ côi (không trỏ FR), UC chưa test.
- CR: `CR-YYYYMMDD-NNN`, status applied nhưng **artifact pending** (tài liệu liên quan chưa đồng
  bộ) → việc gấp.
- Coverage tính bằng **engine deterministic** (script), không phải AI đoán.

### 1.4 Nguyên tắc vận hành rút ra (để app làm theo)

1. **Phỏng vấn trước khi viết** — `/brainstorm` ghi ý tưởng thô rồi *phỏng vấn làm rõ* scope,
   luồng, ràng buộc; OQ được đánh số và chốt dần. → App cần *interview mode*: generate có thể trả
   về **câu hỏi làm rõ** thay vì tài liệu khi đầu vào mỏng.
2. **Tài liệu sinh theo chuỗi, cái sau đọc cái trước** — SRS đọc brainstorm, user story đọc SRS,
   test đọc use case… → App truyền *ngữ cảnh tài liệu upstream* vào prompt sinh.
3. **Một thay đổi = một CR cập nhật đồng bộ** — `/cr` phân tích tác động rồi sửa các tài liệu liên
   quan, tránh sửa tay từng file. → App có bảng CR + impacts + apply draft-first.
4. **Đo được, truy vết được** — coverage/staleness deterministic. → App parse ID từ nội dung
   markdown bằng code Rust, không nhờ AI.
5. **Xuất được cho stakeholder** — `/export` PDF/Word/HTML, `/preview` trang HTML gộp toàn bộ tài
   liệu 1 feature. → App có trang Preview + export Markdown/HTML.

## 2. Map 55 skill → tính năng app

Ký hiệu: ✅ = doc generator trong app (AI sinh, lưu DB, có template riêng) · ⚙️ = tính năng
tính toán/deterministic · 🔌 = ngoài phạm vi (cần hệ thống ngoài), ghi rõ lý do.

| Skill | Trong app | Ghi chú |
|---|---|---|
| `/prd` | ✅ `prd` | PRD toàn sản phẩm, mục 6 "Danh sách tính năng" bóc thành bảng — app cho phép **import bảng này thành features** của project |
| `/roadmap` | ✅ `roadmap` | Now/Next/Later + lý do ưu tiên (impact/effort) |
| `/discover` | ✅ `discover` | Điều tra ý tưởng: nhu cầu, đối thủ, Go/No-Go |
| `/brainstorm` | ✅ `brainstorm` | Interview mode: sinh câu hỏi làm rõ theo 6 nhóm, chốt OQ |
| `/urd` | ✅ `urd` | Persona, nhu cầu UR-xx, hành trình |
| `/brd` | ✅ `brd` | Mục tiêu, phạm vi, stakeholder, rủi ro, ROI |
| `/prd-epic` | ✅ `prd_epic` | Đặc tả 1 tính năng: capability P0/P1/P2, release plan |
| `/srs` | ✅ `srs` | 11 mục theo đúng mẫu srs-preview (xem §3) |
| `/reverse-doc` | ✅ `reverse_doc` | Tái lập SRS từ **văn bản dán vào** (app không đọc Word/PDF/ảnh — daemon có OCR riêng); từng mục kèm mức tin cậy cao/vừa/thấp |
| `/code-to-srs` | ✅ `reverse_doc` (nguồn `code`) | Dán source code → SRS tái lập, mỗi mục trích dẫn file:dòng, kèm mức tin cậy |
| `/sequence` | ✅ `diagram/sequence` | Mermaid `sequenceDiagram`, header truy vết FR/E/UC |
| `/activity` | ✅ `diagram/activity` | Mermaid `flowchart` có nhánh quyết định |
| `/activity-swimlane` | ✅ `diagram/activity_swimlane` | Mermaid flowchart + `subgraph` theo vai trò (kit dùng PlantUML; app dùng Mermaid để render tại chỗ, kèm khối PlantUML cho ai cần) |
| `/bpmn` | ✅ `diagram/bpmn` | Xuất **BPMN 2.0 XML** trong code block (import Camunda/Bizagi được) + Mermaid mô phỏng để xem nhanh |
| `/state` | ✅ `diagram/state` | Mermaid `stateDiagram-v2` cho 1 đối tượng |
| `/erd` | ✅ `diagram/erd` | Mermaid `erDiagram` |
| `/d2-erd`, `/d2-activity`, `/d2-architect` | ✅ khối D2 trong `diagram/erd`, `diagram/activity`, `diagram/architecture` | App render Mermaid; mỗi doc kèm fence ```d2 tương đương cho ai dùng D2 CLI |
| `/dbdiagram` | ✅ `diagram/dbml` | Khối **DBML** (dbdiagram.io) + bảng giải thích; không render hình |
| `/usecase-diagram` | ✅ `diagram/usecase` | Mermaid flowchart actor–usecase (Mermaid không có UC chuẩn; kèm khối PlantUML) |
| `/usecase` | ✅ `usecase` | Chuẩn Cockburn đầy đủ (xem §3) |
| `/userstory` | ✅ `userstory` | Bảng story US-xx trỏ FR, MoSCoW, sẵn sàng vào backlog |
| `/ac` | ✅ `ac` | Given/When/Then cho từng story, kèm edge/negative |
| `/user-flow` | ✅ `user_flow` | Happy/error/edge path + Mermaid flowchart |
| `/wireframe-ascii` | ✅ `wireframe_ascii` | Khung ký tự box-drawing + bảng mô tả control |
| `/wireframe-html` | ✅ `wireframe_html` | HTML đen trắng render element thật, xem trong iframe sandbox |
| `/prototype-html` | ✅ `prototype_html` | 1 file HTML bấm được, điều hướng + lưu trạng thái localStorage |
| `/prototype-next` | 🔌 | Cần scaffold dự án Next.js ngoài app — dùng agent code IDE; app dừng ở prototype_html |
| `/figma` | 🔌 | Cần MCP Figma + tài khoản Figma; ngoài phạm vi app |
| `/api-assess` | ✅ `api_assess` | Đánh giá đối tác API, build-vs-buy |
| `/api-doc` | ✅ `api_doc` | Dán tài liệu API đối tác → tóm tắt nghiệp vụ |
| `/api-design` | ✅ `api_design` | Thiết kế tích hợp: hệ thống phối hợp thế nào (kèm sequence) |
| `/api-map` | ✅ `api_map` | Bảng mapping field 3 tầng: API ↔ dữ liệu hệ thống ↔ màn hình |
| `/api-checklist` | ✅ `api_checklist` | Nhóm: auth, happy, validation, error, timeout/retry, idempotency |
| `/api-test` | ✅ `api_test` | Sinh **Bruno collection** chạy được (fence .bru từng request + env + hướng dẫn `bru run`); app không tự chạy — dán vào repo/AutoTest |
| `/api-readiness` | ✅ `api_readiness` | Cổng kiểm tra go-live, checklist có trạng thái |
| `/test-checklist` | ✅ `test_checklist` | Outline kịch bản theo luồng, ưu tiên, để review trước |
| `/test-cases` | ✅ `test_cases` | Bảng TC-xx chạy được, sinh từ checklist, trỏ US/UC |
| `/playwright-gen` | ✅ `playwright` | Sinh file `.spec.ts` trong code block (không chạy trong app; app AutoTest 4640 chạy được nếu cần) |
| `/gap` | ✅ `gap_report` (AI) | Soi thiếu luồng/màn/rule trên bộ tài liệu feature |
| `/doc-drift` | ✅ `doc_drift` (AI) | Dán code/ghi chú dev → đối chiếu với tài liệu, chỗ nào lệch |
| `/ask` | ⚙️+AI `ba_ask` | Q&A trên toàn bộ tài liệu project, trả lời kèm trích dẫn doc; lưu `qa_log` |
| `/cr` | ✅⚙️ CR engine | CR-YYYYMMDD-NNN: AI phân tích impact → danh sách tài liệu ảnh hưởng → apply từng cái (draft-first) |
| `/dashboard` | ⚙️ dashboard | 4 KPI + kanban + việc gấp, tính deterministic (§5) |
| `/kg` | ⚙️✅ `ba_kg` + tab KG | Knowledge Graph deterministic: node=doc, cạnh upstream + cạnh tham chiếu ID (đếm), mermaid + bảng edges trong UI |
| `/jira`, `/confluence` | 🔌 | Cần credential Jira/Confluence Cloud; điểm nối tương lai qua MCP ngoài |
| `/export` | ⚙️ export | Gói Markdown (.md bundle) + HTML standalone; PDF/Word để daemon/hệ khác convert |
| `/preview` | ⚙️ preview | Trang HTML gộp toàn bộ tài liệu 1 feature (giống srs-preview.html) |
| `/reverse-preview` | ⚙️ preview | Cùng trang preview, hiển thị badge mức tin cậy khi doc thuộc loại reverse |
| `/userguide` | ✅ `userguide` | Cẩm nang vận hành theo vai trò admin/CSKH |
| `/meet` | ✅ `meeting` | Ghi chú họp thô → biên bản: quyết định + action item (ai, hạn) |
| `/update-overview` | ✅ `overview` | Tài liệu dùng chung project (glossary, môi trường, convention — phần "Giới thiệu chung" của mẫu SRS) |
| `/delegate` | 🔌 | San tải sang AI khác = việc của daemon/dispatch, không thuộc app |

Tổng: **31 loại tài liệu sinh được trong app** (9 subtype diagram tính theo subtype), 6 tính năng
deterministic (dashboard, trace, preview, export, search, ask), 6 skill ngoài phạm vi có lý do.

## 3. Template tài liệu (contract với AI)

Mỗi loại tài liệu có: *mô tả*, *giai đoạn*, *upstream types* (tài liệu đọc trước khi sinh),
*sections bắt buộc*, *câu hỏi phỏng vấn* khi thiếu đầu vào, *quy tắc ID*. Template sống ở
`apps/ba/src/templates.rs`, là **hợp đồng đầu ra**: prompt yêu cầu AI trả đúng khung markdown, code
Rust kiểm mục nào thiếu (soft-check, ghi cảnh báo vào doc meta).

Điểm chung mọi tài liệu:

```markdown
# <Tiêu đề> — <tên feature/project>
<!-- ba:meta type=<doc_type> feature=<slug> version=<n> generated=<ISO> -->

… các section theo loại …

## Open Questions
| OQ | Câu hỏi | Trạng thái | Chốt |
```

- ID quy ước: `FR-<feature>-NNN`, `NFR-`, `BR-`, `E-`, `SC-`, `US-`, `AC-`, `TC-`, `UC-<feature>-NNN`, `UR-`, `PER-` (persona), `CR-YYYYMMDD-NNN`, `OQ-n`.
- Bảng dùng đúng cột như mẫu ở §1.3 (FR có *Verify by* + *Source*; BR có *Implements FR*; Error matrix có *Screen state* nguyên văn thông báo; SC có *Mốc đạt* đo được).
- Diagram: code fence ```mermaid``` là bản render chính; PlantUML/D2/DBML/BPMN-XML kèm theo fence riêng khi loại đó yêu cầu.
- Ngôn ngữ mặc định tiếng Việt, thuật ngữ BA giữ tiếng Anh (FR, NFR, backlog…).

(Chi tiết đầy đủ từng template — sections, câu hỏi phỏng vấn, ví dụ — nằm trong `templates.rs`,
mỗi entry là source of truth; doc này không lặp lại để tránh drift.)

## 4. Workflow engine

- **Template workflow** (seed 3 cái từ trang + tuỳ biến):
  1. `full-lifecycle` — prd → roadmap → brainstorm → srs → wireframe_html → userstory → test_checklist → test_cases *(bỏ bước /jira — ngoài phạm vi)*
  2. `story-first` — brainstorm → userstory → ac → srs → wireframe_html → test_checklist → test_cases
  3. `prototype-first` — user_flow → prototype_html → brainstorm → srs → userstory → test_checklist → test_cases
- Workflow gắn với **feature** (riêng `prd`/`roadmap`/`discover` cấp project — bước cấp project
  trong workflow feature sẽ sinh doc cấp project nếu chưa có).
- Mỗi step: `{doc_type, subtype?, status: pending|done|skipped, doc_id?}`. `ba_workflow_advance`
  hành động `run` (sinh doc bằng AI rồi đánh done), `done` (gắn doc có sẵn), `skip`.
- App gợi ý **bước kế tiếp** + cho đảo thứ tự (đúng tinh thần "không ép quy trình cứng").

## 5. Truy vết, độ tươi, dashboard (deterministic — code Rust, không AI)

- **Parser ID**: regex quét markdown lấy mọi ID theo quy ước → bảng `doc_ids(document_id, kind, id)`
  (đánh lại mỗi lần doc đổi). Quan hệ suy ra: US phủ FR khi bảng story có cột FR ref chứa
  `FR-<feature>-NNN`; AC phủ US; TC phủ US/UC; BR implements FR; E related FR.
- **Coverage**: %FR có ≥1 US; FR chưa có TC; US mồ côi; UC chưa có test — như dashboard mẫu.
- **Pipeline per feature**: 8 chặng `urd, brd, prd_epic, srs, usecase, userstory, ac, test_cases`
  (đúng 8 cột dashboard mẫu; prd_epic thay vị trí "PRD" cấp feature) — chặng đạt khi có doc loại đó
  không ở trạng thái draft-stale.
- **Staleness**: đồ thị upstream tĩnh theo loại (brainstorm → urd/brd/prd_epic/srs; srs → usecase/
  userstory/diagram/wireframe; usecase/userstory → ac/test_checklist; test_checklist → test_cases;
  user_flow → wireframe_*/prototype). Doc stale nếu upstream cùng feature có `updated_at` mới hơn.
  "Stale chain" liệt kê cạnh lan truyền gần nhất.
- **Lifecycle**: `draft → in_review → revisions → approved → shipped` (kanban). Review quá hạn:
  in_review > 7 ngày.
- **Việc gấp**: CR applied còn impact pending; doc stale điểm thấp; OQ chưa resolve; review quá hạn.

## 6. CR engine (change request đồng bộ)

1. `ba_cr_create(title, mô tả thay đổi)` → mã `CR-YYYYMMDD-NNN`, AI đọc toàn bộ doc của feature →
   **bảng impact**: từng doc bị ảnh hưởng, mục nào, sửa gì, mức độ.
2. `ba_cr_apply(cr, impact)` → AI viết lại doc đó (giữ khung template, chỉ đổi phần liên quan,
   thêm ghi chú `> CR-…: <tóm tắt>`), lưu **version mới**, doc quay về `draft` chờ review
   (draft-first như Moltbook/Shopee).
3. CR `closed` khi mọi impact applied/skipped. Dashboard nhắc CR treo theo số ngày.

## 7. Kiến trúc & dữ liệu

Rust axum app theo pattern thinking (khung) + study (chi tiết): `main.rs` (SPA fallback kiểu
study), `config.rs` (`SENCLAW_BIND_HOST`, `PORT` 4740, `BA_DATA_DIR`), `state.rs`, `schema.sql` +
`db.rs` (rusqlite **0.32** + Mutex + WAL, FTS5 cho documents), `templates.rs`, `llm.rs` (reqwest
gọi bridge `POST /api/space/apps/ba/bridge` action `llm.request` — chỉ system/prompt/maxTokens/
profile, `MAX_OUT=32000`, `finish=="length"` là lỗi, retry 3, lọc prompt-injection kiểu
`sanitize_retrieved` của study), `engine.rs` (generate + interview + context assembly + jobs),
`trace.rs` (parser ID + coverage + staleness + dashboard), `cr.rs`, `export.rs`, `api.rs` (mọi
logic ở `*_value` dùng chung REST/MCP), `mcp.rs` (JSON-RPC + SSE, khung y hệt thinking), web UI.

Bảng chính: `projects`, `features`, `documents` (+ `doc_versions`, `doc_ids`, FTS5
`documents_fts`), `workflows`, `change_requests`, `cr_impacts`, `qa_log`, `jobs` (generate chạy
nền — UI poll; pattern rewrite-story).

Ngữ cảnh sinh doc = project context + feature description + **upstream docs** (theo đồ thị §5,
cắt theo ngân sách ký tự, doc mới hơn ưu tiên) + answers phỏng vấn.

## 8. REST + MCP (27 tool `ba_*`)

REST `/api/*` phục vụ UI; MCP `ba-mcp` cho agent — cùng engine:

`ba_project_create/list/get/update` · `ba_feature_add/list/update` ·
`ba_doc_list/get/write/update_status/search/versions` · `ba_generate` (interview: trả
`questions[]` khi thiếu input; `answers` để trả lời) · `ba_workflow_templates/start/status/advance`
· `ba_cr_create/list/get/apply` · `ba_gap_check` · `ba_ask` · `ba_trace` · `ba_dashboard` ·
`ba_export`.

## 9. Web UI (React + Vite, giống app gần đây)

- **Projects** → tạo/chọn project.
- **Project home**: dashboard 4 KPI + việc gấp + kanban doc + tiến độ 8 chặng per feature.
- **Feature detail**: stepper workflow, tài liệu nhóm theo 9 giai đoạn (đúng thứ tự BA-Kit),
  viewer markdown (render mermaid, bảng), iframe sandbox cho wireframe/prototype HTML, nút
  Generate (modal phỏng vấn khi AI hỏi lại), version history, đổi lifecycle status.
- **Catalog giai đoạn**: trang liệt kê 9 nhóm × các "skill" trong app (mỗi cái 1 nút sinh) — giữ
  đúng tinh thần "55 skills chia theo giai đoạn".
- **CR page**: danh sách CR, bảng impact, nút apply từng impact.
- **Preview** `/preview?feature=…`: trang gộp toàn bộ doc 1 feature giống srs-preview.html, có
  mục lục, badge tin cậy cho reverse doc. **Export** tải .md bundle / .html.

## 10. Ngoài phạm vi (ghi rõ để không hiểu nhầm)

Jira/Confluence sync (cần credential + API ngoài), Figma MCP, prototype Next.js scaffold, chạy
Playwright/Bruno thật (app AutoTest lo phần chạy), đọc Word/PDF/ảnh trực tiếp (dán text hoặc dùng
OCR app), `/delegate` (thuộc dispatch daemon). Các điểm nối này để mở rộng sau qua MCP ngoài.
