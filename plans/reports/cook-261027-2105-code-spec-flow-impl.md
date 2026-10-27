# Triển khai luồng spec cho phiên Code (brainstorm → draft → review 4 hướng → approve)

**Ngày:** 2026-10-27 · **Spec đã duyệt:** [plans/261027-1505-code-spec-flow/design.md](../261027-1505-code-spec-flow/design.md)
· **Nhánh:** `feat/spec-flow` ở `senclaw`, `web-app`, `desktop` — **chưa commit, chưa push, chưa cài**.

## Đã làm

| Phần | Ở đâu |
|---|---|
| Kho state ngoài repo, hash thiết kế, giai đoạn, slug, khối `<spec>` | `src/specs/mod.rs` |
| Cổng chặn + receipt đọc + `/spec skip` | `src/specs/gate.rs`, gọi trong `zen_core/run_tools.rs` trước tầng quyền |
| Review 4 hướng goldfish (Cảnh, Kiểm, Hiểu, Phản biện) × 2 chế độ, kiểm máy, bộ đọc kết quả, báo cáo | `src/specs/review.rs` |
| 5 lệnh: `/brainstorm` `/draft` `/review` `/approve` `/spec` | `src/specs/commands.rs`; gateway WS, `message_router`, REST chat phiên Code; pool viết lại lượt |
| Flow theo từng chat (cả subagent `Task`), quên receipt khi xoá lịch sử / compaction | `agent_pool/{engine,pool,traits}.rs`, `zen_core/{engine,mod,conversation}.rs`, `tools/task.rs`, `command_dispatcher.rs` |
| Setting `specFlow` (`required` mặc định / `on` / `off`) | `group_manager/{types,llm,mod}.rs`, `/api/agent-behavior` |
| Sửa kèm: `tree -o FILE` không còn bị coi là chỉ-đọc | `util/shell_safety.rs` |
| Tài liệu | `docs/spec-flow.md`, `CLAUDE.md` (mục "Spec flow"), `skills/code/SKILL.md`, help của lệnh |
| Web: 5 lệnh trong menu `/`, công tắc Spec flow | `web-app` (`specCommands.ts`, `SpecFlowControl.tsx`, …) — báo cáo [fullstack-261027-2050](fullstack-261027-2050-spec-flow-clients.md) |
| Desktop: như web + 12 widget test | `desktop` (`spec_flow_field.dart`, `slash_mention_input.dart`, …) |

## Code review sau triển khai (reviewer ngữ cảnh trống)

9 finding, đã xử lý (chi tiết trong `## Review Log` của spec): lịch hẹn `/draft <path>` mở lại spec cũ đã duyệt → chỉ gateway đổi spec đang
mở; phiên Code dùng chung `workspace-state-main.json` → dùng `allowed_work_dirs[0]`; retry chạy lại `/review` → retry bằng lượt đã viết lại;
subagent worktree bị `required` chặn → miễn dưới `SENCLAW_HOME`; `..` sau symlink; `**Status:**` ở dòng task; parser ("BLOCKER: None",
gạch dài, `### Blockers`); cursor kênh; lời từ chối không còn vào sổ lỗi. Ghi giới hạn: `Bash` xét theo thư mục chạy; provider không báo
`finish` (Antigravity) thì không nhận ra bị cắt; Stop không huỷ review đang chạy.

## Đã kiểm chứng

| Việc | Kết quả |
|---|---|
| `cargo test --lib` toàn bộ (trước review code) | **3766 qua, 0 lỗi**, 13 bỏ qua có sẵn (gồm test ghim OpenAPI) |
| `cargo test --lib` toàn bộ (sau sửa) | 3769 qua, **1 lỗi không liên quan**: `tools::bash::tests::a_foreground_timeout_ends_the_whole_command` (đòi < 10 s, máy load ~9 khi chạy cả bộ); chạy riêng 3/3 qua, 1,01 s |
| Test tập trung sau sửa (`specs::`, `run_tools`, `conversation`, `shell_safety`, `agent_behavior`, `command_dispatcher`) | 144 qua |
| E2E chạy lại trên code đã sửa | **41/41** |
| Test mới (lần đầu): `specs::` (hash/NFC/Review Log giữa file, stage, slug, find_root, capsule, 13 test cổng gồm `..`/symlink/store/alias/receipt/required, parser 4 dạng, kiểm máy, 4 lens với model kịch bản, lệnh approve/spec/skip/review) + `shell_safety` + `agent_behavior` | 65 qua |
| E2E daemon nháp + mock LLM ([spec-flow-e2e/](spec-flow-e2e/), `up.sh`/`drive.mjs`) — CUJ 1-4 + kho state + REST + `off` | **41/41**; đối chiếu log: 9 lời từ chối của cổng, 16 lệnh gọi lens, khối `<spec>` trong request của phiên thứ hai |
| web-app `npm run build` | xanh |
| desktop `flutter analyze` + `flutter test` | sạch; 638 qua, 1 skip có sẵn |

## Chưa kiểm chứng

- **Model thật** (gemma-4-e4b / LAN): hỏi từng câu, viết đúng heading, xử lý finding, đọc spec khi bị từ chối — chỉ mock. Thời gian một lượt `/review` trên model local chưa đo.
- **Bấm thật trên UI** web/desktop (chỉ build + widget test với daemon giả).
- **Kênh Telegram**: đường `run_agent` mang nguyên văn lệnh — chỉ biên dịch, chưa chạy.
- Chưa cài vào app thật; chưa commit.

## Giới hạn đã chấp nhận (ghi trong spec + docs)

`Bash` ghi `state.json` khi `codeAutonomy = full`; tool MCP ghi file và lịch `script` không bị chặn; subagent trong worktree riêng không bị chặn
(nhánh riêng, người merge); cổng ép "đã duyệt" + "đã đọc", không ép "chỉ sửa file trong Target Files".

## Câu hỏi còn mở

- Web `NewChatScreen` có nên gợi ý 5 lệnh cho tin nhắn đầu tiên không (desktop đã có vì dùng chung widget)?
- Drift report (so spec đã duyệt với code theo thời gian, như `/egm-drift-report`) — làm đợt sau?
