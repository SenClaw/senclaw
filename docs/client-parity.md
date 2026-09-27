# Parity giữa ba client — Web / Desktop / Mobile

README hứa cùng một sản phẩm trên ba client, nhưng chưa ai đếm xem chúng thật
sự khác nhau ở đâu. Bảng này là kết quả đếm, **soát ngày 2026-09-12** trên
nhánh `main`.

**Cách đọc ô:**

| | |
|---|---|
| ✅ | có màn hình / component thật, tên file ghi ở cột Notes |
| 🟡 | có nhưng mỏng hơn client khác — Notes nói rõ thiếu gì |
| — | không có. Notes phải nói **vì sao** hợp lý, hoặc ghi "candidate work" |
| N/A | không hợp lý trên nền tảng đó, kèm lý do |

Quy tắc duy nhất khiến bảng này còn giá trị: **mọi ✅ phải trỏ được vào một
file đã mở**, không suy từ tên thư mục — một thư mục có thể tồn tại mà bên
trong là stub. Và một ô `—` **không kèm lý do** chính là một việc phải làm; số
ô như vậy là backlog thật, không phải cảm giác.

Soát lại khi thêm một tính năng cho bất kỳ client nào.

**Cập nhật 2026-09-12:** background tasks trên web đã làm (`pages/BackgroundPage.tsx`), còn **sáu** ô.

Seven cells are `—` with no reason found: background tasks on Web; kanban, workflow editor, Telegram pairing, checkpoints, and trajectory replay on Mobile; and trajectory replay on Desktop. Those seven are the actual backlog — every other `—` is a platform constraint, and the 🟡 cells are thinning rather than absence.

| Feature | Web | Desktop | Mobile | Notes |
|---|---|---|---|---|
| chat | ✅ | ✅ | ✅ | `ChatView.tsx` / `chat/conversation_pane.dart` / `screens/chat_screen.dart` |
| sessions list | ✅ | ✅ | ✅ | `Sidebar.tsx` (`SessionList`) / `chat/session_list.dart` / `screens/sessions_screen.dart` |
| new chat | ✅ | ✅ | ✅ | `NewChatScreen.tsx` / `chat/new_chat_dialog.dart` / `screens/new_chat_screen.dart` |
| wiki | ✅ | ✅ | ✅ | `components/wiki/*` / `wiki/wiki_screen.dart` / `wiki/wiki_screen.dart` (tree, search, create, delete) |
| plugins/marketplace | ✅ | ✅ | 🟡 | Mobile has 6 tabs (skills, subagents, plugins, MCP, marketplace, hooks); missing alias, sandbox, widgets, workflows, cowork, space-apps panels |
| settings | ✅ | ✅ | 🟡 | Mobile `more/more_screen.dart` covers connection, notifications/sync, theme, language only — no LLM, channels, profiles, tool rules, TTS/OCR/Whisper |
| space (calendar) | ✅ | ✅ | ✅ | `space/calendar/CalendarView.tsx` / `space/space_screen.dart` `_CalendarTab` / `space/calendar_screen.dart` |
| space (notes) | ✅ | ✅ | ✅ | `space/notes/*` / `space/note_inline_editor.dart` / `space/notes_screen.dart` |
| space (schedules) | ✅ | ✅ | ✅ | `space/schedules/*` / `space/space_screen.dart` `_SchedulesTab` / `space/schedules_screen.dart` |
| space apps | ✅ | ✅ | 🟡 | Mobile `space/apps_screen.dart` lists, restarts, updates and opens apps but cannot install a new one from the hub |
| cowork | ✅ | ✅ | ✅ | `CoworkPage.tsx` + `CoworkTeamDetailPage.tsx` / `cowork/cowork_screen.dart` / `cowork/cowork_workspace_screen.dart` |
| cognitive/knowledge | ✅ | ✅ | ✅ | `CognitivePage.tsx` / `cognitive/cognitive_screen.dart` + `cognitive_graph.dart` / `cognitive/cognitive_screen.dart` |
| kanban | ✅ | ✅ | — | No reason found — candidate work; no `/api/kanban` call anywhere in `channel_app/` |
| usage | ✅ | ✅ | 🟡 | Mobile `usage/usage_screen.dart` is read-only by design comment; no pricing editor (cf. `usage/PricingEditor.tsx`) |
| workflow runs | ✅ | ✅ | ✅ | `WorkflowRunsPage.tsx` / `workflow/workflow_runs_screen.dart` / `workflow/workflow_screen.dart` (start, cancel, rename, delete) |
| workflow editor | ✅ | ✅ | — | No reason found — candidate work; `services/workflow_api.dart` already exposes `draft()` and `create()`, no screen calls them |
| background tasks | ✅ | ✅ | ✅ | `pages/BackgroundPage.tsx` (list, runs, run-now, pause, edit, delete) / `background/background_screen.dart` / `screens/background/background_screen.dart` |
| patterns | ✅ | ✅ | ✅ | `plugins/PatternsPanel.tsx` / `plugins/patterns_panel.dart` / `patterns/{patterns,pattern_run,pattern_sources_tab}` |
| kits | ✅ | ✅ | ✅ | `plugins/KitsPanel.tsx` / `plugins/kits_panel.dart` / `kits/kits_screen.dart` + `kit_install_sheet.dart` |
| workbench | ✅ | ✅ | ✅ | `Workbench.tsx` / `dock/right_dock.dart` `_WorkbenchTab` / `workbench/workbench_screen.dart` (history is device-local there) |
| dispatch view | ✅ | ✅ | ✅ | `DispatchTree.tsx` + `AgentConsole.tsx` / `dock/right_dock.dart` `_ConsoleTab` / `dispatch/dispatch_screen.dart` (has retry) |
| watch strip | ✅ | ✅ | ✅ | `WatchStrip.tsx` / `chat/watch_strip.dart` / `widgets/watch_strip.dart` |
| form UI cards | ✅ | ✅ | ✅ | `FormCard.tsx` / `chat/widgets/form_card.dart` / `widgets/interaction_cards.dart` `FormCard` |
| permission cards | ✅ | ✅ | ✅ | `PermissionCard.tsx` / `chat/widgets/message_widgets.dart` `MessageKind.permission` / `widgets/interaction_cards.dart` |
| inline widgets | ✅ | ✅ | ✅ | `WidgetCard.tsx` / `chat/widgets/widget_card.dart` / `widgets/widget_card.dart` |
| telegram pairing | ✅ | ✅ | — | No reason found — candidate work; mobile `pairing_screen.dart` is relay QR pairing, not `/api/pairings` approval |
| tray screenshot/capture | N/A | ✅ | N/A | Needs a tray and a global hotkey shelling to `screencapture -i`; `capture/{screen_capture,capture_hotkey,capture_review}.dart` |
| code sessions | ✅ | ✅ | ✅ | `NewChatScreen.tsx` `ChatKind` / `chat/new_chat_dialog.dart` `_isCode` / `code/{code_screen,code_session_screen,folder_picker}.dart` |
| checkpoints/changes panel | ✅ | ✅ | — | No reason found — candidate work; mobile only has an enable toggle label in `code/code_screen.dart`, no diff/restore/explain |
| trajectory replay | ✅ | — | — | No reason found — candidate work on both; only `ReplayPanel.tsx` exists, no `trajector` hit in either Flutter client |

## Candidate work, ranked

1. ~~Background tasks on Web~~ — **xong 2026-09-12**.
2. Checkpoints/changes panel on Mobile — mobile already runs code sessions that write files, so there is currently no way to diff or roll back from the phone.
3. Telegram pairing approval on Mobile — pairing codes expire in an hour and the phone is the device most likely to be at hand when one arrives.
4. Trajectory replay on Desktop — desktop is the primary coding client and the replay data is per-turn JSONL the daemon already serves to Web.
5. Kanban on Mobile — Web and Desktop both ship complete boards over `/api/kanban`; the mobile Cowork task list is a different, team-scoped surface.
6. Workflow editor on Mobile — the lowest-cost item on this list: `workflow_api.dart` already has `draft()` and `create()`, only the screen is missing.
7. Settings on Mobile — no way to change models, channels, profiles or tool rules from the phone, which forces a trip to another client for routine changes.
8. Space app install on Mobile — updating an installed app works, but a new app can only be installed from Web or Desktop.
