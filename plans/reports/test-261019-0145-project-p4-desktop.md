# Kiểm chứng P4 desktop: giao diện Project cho SenClaw Desktop (Flutter)

Ngày 2026-10-19 · Nền desktop `3c70b29` · Nhánh `feat/project`, worktree `.worktrees/desktop-project` · **Chưa commit** · 15 file sửa (+613/−84 dòng) · 25 file mới: 11 trong `lib/` (≈3.200 dòng), 12 trong `test/` (≈2.700 dòng, gồm 3 file hỗ trợ), 1 trong `tool/live/` (564 dòng) — xem §3
Phục vụ: [plan.md](../261018-1618-project-feature/plan.md) P4 (phần desktop) · [design.md](../261018-1618-project-feature/design.md) D7, D10, §5.2, §5.6, §6, §7 · API: daemon `feat/project` @ `e8b5264` (chỉ đọc)
Bằng chứng: [project-p4-desktop-e2e/evidence/](project-p4-desktop-e2e/evidence/) (20 ảnh, `notes.txt`, khối `<project>` trong system prompt, log build/khởi chạy app).

## 0. Cái GÌ CHƯA chạy thật — đọc trước

- **Không lái được cửa sổ app macOS bằng chuột/phím.** Xin quyền computer-use cho bản test của app bị người dùng **từ chối** (`user_denied`); tôi không xin lại. Cái đã làm thay:
  1. `flutter build macos --debug` của nhánh **build được**; bản sao ký lại bundle id riêng (`com.senclaw.senclawDesktop.p4e2e`, giữ entitlements) **khởi chạy** với `HOME` scratch, **kết nối WS vào daemon cô lập** (log daemon: client tự xác thực; stderr app chỉ có dòng "merged UI and platform thread"), rồi tôi tắt nó. Không có thao tác nào trong cửa sổ.
  2. `tool/live/projects_live_test.dart`: **`ApiClient` + `WsClient` thật, widget thật, daemon cô lập thật** (binary `senclaw` feat/project @ `e8b5264`, sha256 `00f50dca…`), lái bằng `WidgetTester`. Cửa sổ là của engine test, **không phải** cửa sổ macOS; ảnh PNG do engine render (Roboto thay font hệ thống). Còn ô vuông ở nhãn một số nút và chữ trong `DropdownButton` là **giả tạo của engine test** (style không nêu font ⇒ Ahem), không phải lỗi app.
- Chưa thử: native file picker (nút Browse chỉ thử với picker giả; plugin thật chưa bấm), làm mới khi cửa sổ lấy lại focus trên cửa sổ thật (`AppLifecycleState.resumed` chỉ test bằng invalidate), hai cửa sổ cùng mở, cửa sổ mini-chat (dùng chung `NewChatScreen`), Windows/Linux, LLM và extractor thật (mock; không đo gì về chất lượng), `/api/kanban/generate` thật (chỉ kiểm body gửi `project_id`), purge bank 20.000 fact (live: 1 fact, 277 ms; con số 22–23 s là của P2/web), timeout 5 phút của purge (kiểm bằng đồng hồ giả, chưa chờ thật).

## 1. Kết quả theo hạng mục giao

| # | Hạng mục | Kết quả | Test tự động | Chạy thật (daemon cô lập) |
|---|---|---|---|---|
| 1 | `features/projects/`: provider + trang `/projects` + mục nav cạnh Kanban | **đạt** | `projects_screen_test` 19, `project_tabs_test` 25, layout 28 | danh sách trống → tạo → hai project → lưu trữ/khôi phục → xoá (ảnh 01, 04, 17, 18, 20) |
| 1a | Danh sách + bật/tắt đã lưu trữ | đạt | tile, "Thiếu thư mục", trống; một request `archived=all` rồi lọc | 17 |
| 1b | Hộp tạo: tên, root + Browse, `validate-root` khi đổi, lý do bị từ chối, mô tả, `facts_write`, `inherit_common` | đạt | debounce 350 ms (3 phím ⇒ 1 lần hỏi), `/`, `/Users/me`, không tồn tại, root trùng nêu tên project, Browse điền tên, gửi đúng body | root `/` ⇒ "Thư mục gốc của hệ thống tệp…" (02); root hợp lệ (03); root của Alpha ⇒ `“Alpha”` đang dùng (05); Gamma có CLAUDE.md ⇒ gợi ý; `factsWrite=chat` lưu đúng ở daemon |
| 1c | Tab Tổng quan: sửa tên/mô tả/root/facts_write/inherit_common; lưu trữ/khôi phục; xoá thật `?purge=true` | đạt | chỉ gửi trường đổi; root mới phải qua kiểm tra (root hiện tại thì không); `archived` true/false; hộp xoá nêu số phiên/board/fact + cảnh báo 20.000 fact ≈ 23 s; Huỷ không gọi gì | 04, 18, 19; purge 277 ms: phiên về `projectId=null`, board `project_id=null` và `workspace_dir` = root cũ |
| 1d | Tab Context: markdown, đếm ký tự, giới hạn 8.000, cảnh báo từ 5.000, 409 | đạt | 4.999/5.000/8.001; đếm bỏ khoảng trắng đầu cuối như daemon; PUT mang `updatedAt`; 409 hiện bản daemon cạnh bản mình, 3 lựa chọn (dùng bản daemon / ghi đè theo version vừa thấy / tiếp tục sửa); bản mới tới khi chưa sửa thì thay lặng lẽ, đang sửa thì chỉ báo | lưu (06); PUT chen ngang ⇒ hộp 409 với **body thật của daemon** (07), "Ghi đè" lưu được. **Khối `<project name="Alpha">` với đúng context vừa sửa nằm trong system prompt của phiên tạo từ `NewChatScreen`** ([system-prompt-project-block.txt](project-p4-desktop-e2e/evidence/system-prompt-project-block.txt)) |
| 1e | Tab Facts: liệt kê, thêm tay, xoá, ghi rõ fact cá nhân ở lại bank project | đạt | chip "nhập tay"/kanban; ẩn fact bị thay; tìm; <8 ký tự bị chặn ở client; `secret_in_fact`; trùng chữ ⇒ "đã có"; xoá có xác nhận; ghi chú R4 + cảnh báo khi `factsWrite≠project` / `inheritCommon=false` | thêm tay (08); khoá API `sk-ant-…` bị daemon từ chối, hiện "…chứa bí mật (api_key) nên không được lưu" |
| 1f | Tab Boards (kèm `effective_workspace_dir`), Chats (mở được chat) | đạt | thư mục chạy của board có/không `workspace_dir`; bấm board ⇒ `/kanban`; bấm chat ⇒ `/chat` + chọn jid; "Phiên mới ở đây" gài `newChatProjectProvider`; project lưu trữ khoá cả hai nút | 10 (board tạo từ hộp, `effective_workspace_dir` = root) |
| 2 | `group.dart`, `groups_provider.dart` (`projectId`); `NewChatScreen`: chọn Project, khoá thư mục, chat/code, gửi `projectId`; cowork/schedule/workflow không gán | đạt | picker chỉ project đang mở; chọn ⇒ root hiện + ổ khoá; chat: `allowedWorkDirs=null`; code: `[root]`, bấm pill thư mục **không** mở chooser; bỏ chọn ⇒ trả thư mục; kind Cowork ⇒ mất picker; preselect dùng một lần; preselect đã lưu trữ ⇒ không gửi | **đăng ký qua WS thật**: `group:registered` mang `projectId`, `allowedWorkDirs` = root (11); `update:group` đổi sang project khác ⇒ **phiên vẫn ở Alpha**; đăng ký vào project đã lưu trữ ⇒ daemon từ chối, desktop hiện lý do và về form |
| 3 | `session_list.dart`: nhóm theo project (thu gọn được) + "Không project" | đạt | nhóm theo project, "Không có dự án" luôn cuối; đếm; thu gọn từng nhóm; id khi project đã mất; sort theo tên; chọn từ menu & lưu pref; trục agent cũ giữ nguyên + icon project ở dòng | 12 (nhóm "ALPHA" thu gọn được) |
| 4 | Kanban: hộp tạo/AI chọn Project, Workspace chỉ đọc "theo project", lọc danh sách, AI board gửi `project_id` | đạt | không project ⇒ ô folder như cũ và gửi `workspace_dir`; có project ⇒ hiện root khoá, **không gửi** `workspace_dir`; gõ folder trước rồi chọn project ⇒ bỏ folder; mở từ trang project ⇒ khoá project; lọc Tất cả / Không / một project; header board "Dự án: … · Thư mục làm việc: …"; `/generate` gửi `project_id` | 09, 14, 15, 16 |
| 5 | `facts_panel.dart`: bank `project:<id>` hiện tên | đạt | "Dự án: Alpha · 2 (+1)", id khi project đã mất, xoá qua `?bank=project%3A…` | 13: dropdown "Dự án: Alpha", xoá fact qua route chung của Knowledge **chạy** (bank project) |
| 6 | Tiếng Việt cho mọi chuỗi mới | đạt | `projects_i18n_test` đọc **mã nguồn** `lib/features/projects/` và fail nếu thiếu khoá; giữ placeholder; 130 khoá mới trong `vi/projects.dart`, không trùng khoá có sẵn | toàn bộ ảnh chạy bằng tiếng Việt |
| 7 | Mục trong `desktop/CLAUDE.md` | đạt | — | mục "Projects" (gán lúc tạo, 409, purge chậm, hai trục nhóm, bank project, test i18n/layout) |

Layout với **font thật**, en + vi (`projects_layout_test`, 28 test, 0 overflow): danh sách ở 1100/800 px, năm tab ở 900 px, Tổng quan cuộn tới nút xoá, hộp tạo có lý do bị từ chối, hộp xoá, hộp xung đột context, `SessionList` nhóm theo project trong sidebar 264 px, New Session có project ở 1000/520 px, hộp tạo board có project, **`AppShell` với mục Projects mới** ở cửa sổ 900×420. Test hiện có `vietnamese_layout_test` vẫn xanh.

## 2. Số liệu

- `flutter analyze`: **No issues found** (toàn repo, gồm `test/` và `tool/`).
- `flutter test`: **504 passed, 1 skipped** (nền 379 + 1 skipped có sẵn ⇒ +125 test mới: màn hình 19, tab 25, session 13, danh sách session 9, kanban 14, facts panel 5, api_client 8, i18n 4, layout 28).
- **Kiểm tra test có cắn**: 6 đột biến tạm thời, cả 6 bị bắt rồi khôi phục — bỏ tra zone timeout (`api_client_test` đỏ 2), xoá một khoá vi (`projects_i18n_test`), bỏ "Không project xuống cuối" (`session_list_test`), bỏ `projectId` khỏi `createChat` (`session_test` đỏ 2), bỏ nhánh 409 (`project_tabs_test` đỏ 4), bỏ debounce (`projects_screen_test`).
- Live: 1 test, ~22 s, 20 ảnh + [notes.txt](project-p4-desktop-e2e/evidence/notes.txt). Mất ≈12 vòng mới xanh (kể cả lỗi biên dịch của chính script); các lần đỏ đều là **lỗi của chính driver** (chờ trang cũ, `pumpAndSettle` làm đồng hồ giả nhảy 10 phút khiến timeout 30 s của `ApiClient` nổ, bấm trúng snackbar, thiếu router/plugin ghi âm) — không lần nào lộ lỗi của app hay daemon.
- Build `flutter build macos --debug`: ≈3 phút, `build/` ≈570 MB lúc đo; **đã xoá `build/`** và bản sao `.app`, binary daemon scratch. Đĩa lúc bắt đầu 9,6 GB, thấp nhất 8,4 GB, sau dọn 9,8 GB.
- An toàn môi trường: daemon dưới Seatbelt chặn connect tới 18788/18789/18790, `HOME`, `SENCLAW_HOME`, cwd riêng, cổng 52788/52789/52931; log daemon 0 dòng nhắc cổng thật; script live từ chối cổng mặc định. Tiến trình tôi khởi động (daemon pid 96137, mock 96136, app 97625) đã dừng, 3 cổng trống; không đụng daemon thật (pid 14613) hay `~/.senclaw`. App test đọc `HOME` scratch nên không đọc `~/.senclaw/api_token` thật.

## 3. Đã làm

1. **Nền** (`api_client.dart`): `ApiException.body` (thân JSON lỗi: lý do root bị từ chối, bản context hiện tại ở 409); `ApiClient.withTimeout(limit, body)` bằng giá trị zone — **không** thêm tham số cho `delete`, vì 17 test fake `implements ApiClient` sẽ vỡ hết; chỉ `purge` dùng (`kProjectPurgeTimeout` = 5 phút).
2. **`lib/models/project.dart`**, **`lib/features/projects/`** (9 file): provider + `ProjectsApi` (mọi thao tác làm mới đúng chỗ), `projectsFocusRefreshProvider` (làm mới khi cửa sổ lấy lại focus), trang list/detail, tab Tổng quan/Context/Facts/Boards/Chats, hộp tạo, hộp xoá, `ProjectPicker`, `ProjectRootField` (kiểm tra ngay khi đổi, bỏ kết quả cũ), `FactsWriteField`.
3. **Sửa file có sẵn**: `nav.dart`, `router.dart` (`/projects`); `group.dart` + `groups_provider.dart` (`projectId`, `createChat(projectId:)`, `GroupMode.agent|project`, `registerRefusalProvider`); `new_chat_dialog.dart`; `chat_screen.dart` (về form khi daemon từ chối đăng ký); `session_list.dart`; `kanban_models.dart` (`projectId`, `effectiveWorkspaceDir`), `kanban_providers.dart`, `kanban_dialogs.dart` (`BoardPlacementFields`), `kanban_screen.dart` (lọc, chip, header; card `worktree` dùng thư mục chạy thật); `facts_panel.dart` (`bankLabel`, `FactRow` công khai, `MemoryFact.source`); `l10n.dart` + `vi/projects.dart`; `CLAUDE.md`.
4. **Test + công cụ**: 9 file test mới, `test/support/{fake_projects_api,fake_ws,projects_harness}.dart`, `tool/live/projects_live_test.dart` (cần daemon scratch, tự bỏ qua nếu thiếu `LIVE_WORK_DIR`).

## 4. Quyết định tôi tự chọn

1. **Hai trục nhóm session.** Trục cũ "Project" thực chất nhóm theo **thư mục agent** (`g.folder`); đổi tên thành `GroupMode.agent` (nhãn menu "Agent", tiêu đề "Agents" thay "Projects"), **vẫn lưu `'project'`** để cài đặt cũ giữ hành vi; trục project thật là `GroupMode.project` lưu `'projects'`. **Mặc định vẫn là agent** (không đổi giao diện của ai khi chưa dùng project); ở các chế độ khác, phiên trong project có icon + tooltip tên project. Web chọn cùng thiết kế nhưng gọi trục cũ là "Thư mục profile".
2. Tab Facts của project đặt tên **"Project facts"** (khoá `'Facts'` toàn cục dịch thành "Fact (sự thật đã nhớ)", quá dài cho tab); tab thứ năm là **"Sessions"** (từ vựng của app) chứ không phải "Chats".
3. Picker project ở New Session **chỉ hiện khi có ≥1 project đang mở**; không có thì trang y như cũ.
4. Không có hộp "sửa board" ở desktop (chưa từng có; daemon cũng không có API đổi project của board): "tạo/sửa" = hộp tạo + hộp AI. Ô thư mục trong hộp tạo hiện **root của project** (board chưa tồn tại nên chưa có `effective_workspace_dir`); danh sách/header board dùng `effective_workspace_dir` do daemon trả.
5. Lỗi `register:group` không có chỗ nào ở desktop xử lý WS `error`; thêm `registerRefusalProvider` (chỉ nhận tiền tố `register:group failed:`) và `ChatScreen` bỏ phiên vừa mở, về form, kèm lý do. Lý do là câu **tiếng Anh của daemon** (xem §5).
6. Context 409: "Ghi đè" ghi theo **version vừa thấy**, nên thay đổi thứ ba chen vào vẫn bị từ chối chứ không mất; không bao giờ thử lại PUT lặng lẽ.
7. Lưu trữ/khôi phục **không** hỏi xác nhận (hoàn tác được); xoá thật có hộp xác nhận nhưng **không** bắt gõ lại tên.
8. `ApiTimeout` khi purge không bị coi là lỗi: daemon vẫn xoá, hộp báo "chưa trả lời, có thể vẫn đang xoá" và các màn hình tải lại.
9. Đếm ký tự context bằng `runes` của chuỗi đã `trim` (khớp `chars().count()` của daemon, đúng cả với emoji); fact kiểm 8–600 ký tự ở client bằng hằng số của daemon.
10. Từ vựng: "dự án" cho Project (khớp bản dịch có sẵn), "bảng" cho board, "phiên" cho session, "Không có dự án" cho nhóm không project.

## 5. Phát hiện ở daemon (không sửa; cách đã đi vòng)

- **Lý do từ chối `register:group` là câu tiếng Anh** (ví dụ "the project is archived and takes no new sessions") và đi qua WS `error`, không có `code`. Desktop hiện nguyên văn sau tiền tố đã dịch. Nếu muốn dịch hẳn, daemon cần thêm `code`.
- Purge là **một request chờ**, không có tiến độ: desktop dùng timeout dài + hộp "Đang xoá…". Bank lớn (20.000 fact ≈ 23 s) chỉ đo ở P2/web, không đo lại ở đây.
- Không có `project:updated` (đã biết, đã chốt): làm mới theo thao tác của mình + khi lấy focus.
- Xác nhận ngược chiều (tốt): `update:group` đổi project bị từ chối thật (phiên ở nguyên Alpha); route chung `DELETE /api/memory/facts/:id?bank=project:<id>` xoá được fact của bank project; `GET /api/memory/facts/banks` có bank `project:*` khi có fact; `validate-root` trả `inUseBy`; 409 context trả đủ `content/updatedAt/chars/maxChars`.

## 6. Hạn chế / cần theo dõi

- Chưa có bằng chứng từ **cửa sổ app thật** (xem §0). Ảnh là từ engine test.
- Làm mới khi lấy focus chỉ là observer vòng đời; nếu macOS không phát `resumed` khi cửa sổ lấy focus trong một số trường hợp (vd. nhiều cửa sổ), danh sách chỉ đổi ở lần làm mới tay / chuyển trang.
- Phiên **đang chạy** giữ thư mục cũ sau khi sửa root hoặc xoá project (hạn chế P1) — trang Tổng quan nói rõ khi sửa root.
- Fact cá nhân trong chat project ở lại bank project (R4) — tab Facts nói rõ; chưa đo.
- Cột `Facts` của trang Knowledge phụ thuộc `GET /api/projects` để dịch tên bank: daemon cũ không có route ⇒ bank hiện bằng id (không lỗi).
- `README`/`docs/projects.md` của desktop không có (tài liệu daemon là P5); mục CLAUDE.md đã đủ cho người bảo trì.

## 7. Chạy lại

```bash
cd .worktrees/desktop-project
flutter analyze && flutter test                     # 504 test, không cần daemon
# kiểm chứng thật (daemon cô lập, không bao giờ 18788/18789):
#   dựng daemon scratch như CLAUDE.md "Run against a dev daemon" (HOME, SENCLAW_HOME, cwd, cổng riêng, Seatbelt)
LIVE_WORK_DIR=<thư mục có alpha/ beta/ gamma/> LIVE_EVIDENCE_DIR=<thư mục ảnh> \
  flutter test tool/live/projects_live_test.dart \
  --dart-define=SENCLAW_UI_PORT=52788 --dart-define=SENCLAW_WS_PORT=52789
rm -rf build                                         # trả đĩa
```

Gợi ý commit (khi người dùng yêu cầu): `feat(projects): projects page, project in new sessions, session list and kanban`.

## Câu hỏi chưa giải quyết

1. Có cấp quyền computer-use để chạy lại các ảnh từ **cửa sổ macOS thật** (bản build đã chứng minh chạy được; script `tool/live` chỉ thay thế cho phần lái)?
2. Nhóm session mặc định có nên chuyển sang "theo project" khi người dùng đã có phiên trong project (hiện: giữ agent để không đổi cài đặt cũ)?
3. Nhãn trục cũ: desktop "Agent", web "Thư mục profile" — thống nhất một chữ?
4. Daemon có thể thêm `code` cho lỗi `register:group` để client dịch được không (hiện lý do là tiếng Anh)?
5. Giữ `tool/live/` trong repo hay chỉ giữ ảnh/bằng chứng?
6. Cần gõ lại tên project để xác nhận xoá thật không?
