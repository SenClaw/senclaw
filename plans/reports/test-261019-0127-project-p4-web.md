# Kiểm chứng P4 (web-app): giao diện Project

Ngày 2026-10-19 · web-app nền `f12b16b` · nhánh `feat/project`, worktree `.worktrees/web-app-project` · **chưa commit** · daemon dùng để chạy thật: `feat/project` @ e8b5264, sha256 `00f50dcab5edbfd3…` (đã kiểm khớp trước và sau khi chép vào thư mục tạm)
Phục vụ: [plan.md](../261018-1618-project-feature/plan.md) P4 (chỉ phần web) · [design.md](../261018-1618-project-feature/design.md) §5.2, §6, §7 · [P1](test-261018-2159-project-p1.md) [P2](test-261018-2310-project-p2.md) [P3a](test-261018-2243-project-p3a.md) [P3b](test-261019-0035-project-p3b.md)
Ảnh chụp: [project-p4-web/screenshots/](project-p4-web/screenshots/) (35 ảnh, đánh số theo thứ tự chạy) · bằng chứng văn bản: [project-p4-web/evidence/](project-p4-web/evidence/) · script dựng daemon cô lập: [p4-up.sh](project-p4-web/p4-up.sh), [p4-down.sh](project-p4-web/p4-down.sh)

## 0. Cái GÌ CHƯA làm / chưa đo — đọc trước

- **Model thật: không dùng.** LLM là mock phát lại kịch bản; fact "trích tự động" trong ảnh 15 là do extractor giả (mock), không nói gì về chất lượng trích xuất. Chỉ chứng minh đường ống: khối `<project>` và fact tay có mặt trong request tới model.
- **Card kanban chạy thật: không.** Chỉ tạo board gắn project và kiểm payload/thư mục (`effective_workspace_dir`). Board AI: request thật tới daemon mang `project_id`, prompt tới LLM mở đầu bằng "About the project this board belongs to…" (mock request #10); mock trả `[]` nên sinh board thất bại (502) — không đo chất lượng board sinh ra.
- **Không có test runner mới.** Chỉ `tsc -b` + `npm run build` + chạy thật. Hàm thuần (`contextChars`, `contextLevel`, `projectIdOfBank`) chỉ được kiểm gián tiếp qua giao diện (đếm 353 / 5.053 / 8.293 ký tự đúng).
- **Hai cửa sổ cùng lúc: chỉ mô phỏng** (sửa bằng REST rồi bắn sự kiện `focus`; ảnh không có, kiểm bằng JS: tên project đổi sau focus, 409 context thật từ PUT chen ngang).
- **Cửa sổ hẹp: kiểm 640 px (nội dung ≈380 px) và 1024 px; 375 px không kiểm được có ý nghĩa** — sidebar cố định 260 px (có sẵn, không responsive) chừa ~80 px cho nội dung ở mọi trang, không riêng project.
- Chỉ Chrome trong app (pane trình duyệt). Không thử Safari/Firefox, Windows, hay daemon thật.
- Desktop (Flutter) thuộc session khác; không đụng.
- Bản dịch hai thông điệp từ chối `register:group` (§3.6) chưa chạy trong giao diện tiếng Việt; chỉ kiểm bằng tiếng Anh (ảnh 17).
- Bash trong chat code ở ảnh nằm ngoài mục tiêu: bước `Bash` lỗi "Missing required field: description" là do kịch bản mock của tôi thiếu tham số, không phải giao diện.

## 1. Kết quả theo mục giao việc

| # | Việc | Kết quả | Chạy thật (daemon cô lập + LLM giả) |
|---|---|---|---|
| 1 | `lib/projectsApi.ts` typed client; `projectId` trong `types.ts` và `registerGroup` | **đạt** | toàn bộ luồng dưới đây đi qua nó; `groups` WS mang `projectId` (sidebar nhóm đúng); `register:group` mang `projectId` (chat vào đúng project, log mock) |
| 2a | `/projects`: danh sách + bật/tắt đã lưu trữ; nút Sidebar cạnh Kanban | **đạt** | ảnh 01, 18, 22, 23 |
| 2b | Hộp tạo: tên, root + kiểm `validate-root` khi gõ + nêu lý do, mô tả, facts_write, inherit_common | **đạt** | root `/` bị từ chối kèm lý do (02); root thật hợp lệ + Git + hint CLAUDE.md (03); root đã có project (27, vi); tạo xong vào trang chi tiết (04) |
| 2c | Tổng quan: sửa tên/mô tả/root/facts_write/inherit_common; lưu trữ/khôi phục; xoá thật có hộp xác nhận nêu số chat/board/fact, cảnh báo 20.000 fact ~23 s | **đạt** | lưu trữ qua Popconfirm, khôi phục (19, 20); hộp xoá nêu 7 fact / 2 chat / 1 board (21, 33); kho lớn: cảnh báo + "đang xoá, đừng đóng" (34, 35) |
| 2d | Context: soạn markdown, đếm, giới hạn 8.000, cảnh báo 5.000, 409 | **đạt** | lưu (05); chen ngang bằng PUT ngoài ⇒ hộp 409 hiện bản đang lưu cạnh bản của tôi, "Ghi đè" lưu được (06); 5.053 ký tự ⇒ cảnh báo vàng (24); 8.293 ⇒ đỏ + "agent chỉ đọc 8.000 đầu" (25) |
| 2e | Facts: liệt kê, thêm tay, xoá; ghi rõ fact cá nhân ở lại bank project | **đạt** (xoá từng fact chưa bấm trong run) | thêm tay → gắn "nhập tay" (07); khung thông tin R4 ở đầu tab (07); xoá từng fact có Popconfirm — chỉ kiểm bằng code, chưa bấm; xoá cả bank đã đi qua purge |
| 2f | Boards: kèm `effective_workspace_dir`, link sang /kanban | **đạt** | tab Boards → "Board mới trong project" mở hộp tạo đã chọn project; click hàng mở `/kanban?board=<id>` |
| 2g | Chats: mở được chat | **đạt** | nút "Mở" → `/chat/<jid>`, lịch sử hiện đủ |
| 3 | `NewChatScreen`: chọn Project, khoá thư mục = root, chat/code, gửi `projectId`; ẩn khi kind không hợp lệ | **đạt** | chat: "Chat in orion" + ổ khoá root (08); code: "What should we build in orion?" (16); tạo được cả chat lẫn code trong project (REST `/chats` thấy `groupType` chat và code); kind Workflow không có bộ chọn; project vừa lưu trữ ở phía sau ⇒ daemon từ chối, hiện toast, giữ nguyên màn hình và chữ đã gõ (17) |
| 4 | `SessionList`: nhóm theo project (thu gọn được) + "Không project" | **đạt** | ảnh 09, 10, 29; thu gọn/mở rộng 4→3→4 dòng; sau khi xoá project, chat về nhóm thường |
| 5 | Kanban: Project khi tạo/sửa, Workspace chỉ đọc "theo project", lọc theo project, AI board gửi `project_id` | **đạt** (không có "sửa project", xem §3.8) | 11, 12, 13, 14, 30; body `POST /api/kanban/generate` = `{"goal":…,"project_id":"prj_…"}`, không có `workspace_dir` |
| 6 | `FactsView`: bank `project:<id>` hiện tên project | **đạt** | "Project: orion · 4", khung thông tin + link sang tab Facts của project (15) |
| 7 | Chuỗi mới có tiếng Việt | **đạt** | 153 khoá thêm vào `vi.web.json` (không đổi/xoá khoá nào có sẵn); script đối chiếu: 0 chuỗi `t()` thiếu, 0 khoá thừa; đã duyệt tay các ảnh tiếng Việt (23–35) |
| 8 | CLAUDE.md của web-app | **đạt** | mục "Projects": gán lúc tạo, 409 context, purge chậm, root chỉ đọc, `by-project`/`folder` |

**Chứng minh đường ống** ([evidence/chat-request-project-block-and-memory-context.txt](project-p4-web/evidence/chat-request-project-block-and-memory-context.txt)): trong request model nhận ở lượt đầu của chat tạo từ giao diện — khối `<project name="orion" root="…">` có trong system prompt, chứa **bản context đã lưu** (có dòng "Money is always integer cents" thêm qua bản ghi đè ở bước 409), nằm trước `<user_operating_rules>`; `<memory_context>` ở tin người dùng cuối chứa **fact gõ tay** "The Orion API listens on port 8080 in development.". Chat thứ hai tạo ngoài project (cùng câu hỏi) không nhận khối `<project>`, không nhận marker, không nhận fact ([evidence/leak-check-chat-outside-project.txt](project-p4-web/evidence/leak-check-chat-outside-project.txt)).

**Sau khi xoá thật** ([evidence/after-purge-state.txt](project-p4-web/evidence/after-purge-state.txt)): bảng project rỗng; board "Orion roadmap" `project_id=null`, `workspace_dir` = root cũ; bank `project:*` biến mất khỏi `/api/memory/facts/banks`; thư mục `projects/<id>` (context) bị xoá; **thư mục root nguyên vẹn** (CLAUDE.md, README.md, src).

**Purge 20.000 fact từ giao diện** ([evidence/purge-20000-facts-ui.txt](project-p4-web/evidence/purge-20000-facts-ui.txt)): 22,0 s từ lúc bấm đến lúc về `/projects` (daemon đo ~23 s ở P2); không còn hàng mồ côi ở `mem_facts`, `mem_facts_fts`, `mem_fact_entities`, `mem_links`. Fact tổng hợp (cách seed y harness P2), không phải fact thật.

## 2. Số liệu

- `npx tsc -b`: sạch. `npm run build`: xanh. Một cảnh báo esbuild `css-syntax-error` **có sẵn** (build `f12b16b` trong thư mục tạm cho đúng cảnh báo đó).
- Thay đổi: 11 tệp sửa (+591/−69) và 14 tệp mới (~2.180 dòng) trong `src/`, cộng CLAUDE.md. Không thêm phụ thuộc (không `npm install`).
- Log daemon cô lập: 0 dòng nhắc 18788/18789, 0 dòng `MCP-external HTTP connect`, 0 dòng ERROR (WARN chỉ là `GATEWAY_TOKEN not set` và `cognify` của mock).

## 3. Quyết định tôi tự chọn

1. **Tiếng Việt dùng "dự án"** (khoá `Project` → "Dự án" đã có trong `vi.json` do desktop sinh); `No project` → "Không có dự án". Brief ghi "Không project" — chưa biết desktop chọn gì; nếu khác, một dòng trong `vi.web.json` đổi được, nhưng hai client nên thống nhất.
2. **"Group by → Project" của `SessionList` trước đây là nhóm theo thư mục profile** (`g.folder`), gọi là "Projects". Đổi tên nội bộ thành `folder` ("Thư mục profile") và thêm `by-project` cho project thật. Giá trị lưu cũ `project` đọc thành `folder` (người dùng giữ nguyên cách nhóm). **Chưa chọn gì ⇒ tự động**: `by-project` khi có chat thuộc project, ngược lại `folder` (người dùng không dùng project không thấy gì đổi).
3. Hộp tạo project cho `facts_write` / `inherit_common` vào mục thu gọn "Fact và bộ nhớ" (mặc định hợp lý; không thì hộp cao hơn khung 768 px, nút Tạo nằm ngoài màn hình).
4. Chọn project ở màn tạo chat: gửi `projectId` và `allowedWorkDirs: null` (daemon tự đặt `[root]`); nút gửi bị khoá khi `rootExists=false`; project đã lưu trữ không có trong danh sách.
5. Liên kết sâu: `/projects/:id?tab=…`, `/chats?newProject=<id>`, `/kanban?board=<id>`, `/kanban?project=<id>&new=1`.
6. **Client trước đây bỏ qua mọi tin WS `error`.** `register:group` bị từ chối (project vừa lưu trữ) khiến chat chờ vô hạn. `useWebSocket` giờ giữ `registerError` (chỉ cho tiền tố `register:group failed`) và `ChatPage` hiện toast rồi trả về màn hình tạo chat. Hai thông điệp biết trước được dịch.
7. `api()` của `KanbanPage` trích `error` từ JSON thay vì hiện nguyên chuỗi JSON (toast của **mọi** lời gọi kanban giờ gọn hơn; ý nghĩa không đổi).
8. **Kanban không có "sửa project"**: daemon chỉ nhận `project_id` lúc `POST /boards`/`/generate` (không có route đổi, và thiết kế chốt tại lúc tạo); trang cũng chưa có hộp sửa board (chỉ đổi tên). Chỉ chọn Project ở hai hộp tạo.
9. Lọc board theo project làm phía client (daemon trả hết); đang lọc một project thì nó là mặc định của hộp tạo.
10. Context vượt 8.000 ký tự vẫn **cho lưu** (daemon cắt lúc đưa vào prompt); chỉ cảnh báo đỏ. Có tab xem trước markdown.
11. Chat nằm trong tab Chats lấy "hoạt động gần nhất" từ danh sách group qua WS (REST `lastActive` rỗng với chat web, xem §4).
12. Số nhiều: dùng dạng trung tính "{n} chat(s)", "{n} fact(s)" thay vì sinh hai khoá. Số lớn định dạng theo ngôn ngữ giao diện (`8.293 / 8.000`).
13. Trang `/projects` dùng `AppLayout sidebar={null}` như Kanban: cột trái trống ngoài thanh điều hướng (cùng mẫu có sẵn).

## 4. Thiếu sót API daemon (không sửa daemon) và cách đi vòng

| Thiếu sót | Hệ quả | Đi vòng |
|---|---|---|
| `GET /api/projects/:id/chats` trả `lastActive: null` cho chat web | cột "hoạt động gần nhất" sẽ trống/sai | lấy `lastActivity` từ `groups` WS theo `jid`, rơi về `addedAt` |
| Từ chối đăng ký qua WS là chuỗi tiếng Anh `register:group failed: <lý do>`, không có mã | không phân loại chắc chắn được | khớp tiền tố; dịch hai thông điệp biết trước, còn lại hiện nguyên văn |
| Không có route đổi project của board | không có "chuyển board sang project" | không làm (khớp thiết kế); ghi ở CLAUDE.md |
| `DELETE ?purge=true` trả lời đồng bộ sau khi xoá bank (~23 s/20.000 fact) | trình duyệt phải chờ lâu | timeout 10 phút, hộp xoá không đóng được lúc chạy, nêu trước ở hộp xác nhận |
| Bản build e8b5264 vẫn gọi AI kanban về `127.0.0.1:18788` (đã sửa ở nhánh khác, chưa gộp) | board AI từ daemon cô lập có thể đụng daemon thật | chạy daemon cô lập với `SENCLAW_BASE_URL` = cổng mình, `SENCLAW_APP_TOKEN_MODE=off`, bọc `sandbox-exec` chặn 18788/18789/18790 (log: 0 lần nhắc 18788) |

## 5. Hạn chế / cần theo dõi

- Mọi chuỗi của `KanbanPage` ngoài phần mới ("Title", "Template", "Workspace directory", "New Board"…) và `NewChatScreen` vẫn tiếng Anh: hai tệp có sẵn không dùng `t()`; theo CLAUDE.md không retrofit lặt vặt.
- Danh sách project ở trang chi tiết đọc lại khi lấy focus nhưng một tab Facts/Chats đang mở không tự tải lại ngoài nút "Làm mới".
- `useProjects('all')` được gọi ở nhiều chỗ (sidebar, Kanban, FactsView, danh sách): mỗi nơi tự tải và tự nghe `focus` — vài lời gọi nhỏ trùng nhau mỗi lần focus; chưa có bộ nhớ đệm chung.
- Sửa root project không làm chat đang chạy đổi thư mục (hạn chế P1); hộp Tổng quan nói rõ điều này khi sửa root.
- Ảnh chụp là khung 800×600 của pane (viewport 1024×768) và có độ trễ so với thao tác; vài ảnh được chụp lần hai để thấy hộp thoại sau hoạt hình.

## 6. Chạy lại

```bash
# web: worktree .worktrees/web-app-project (node_modules là symlink tới web-app/node_modules, KHÔNG commit)
npx tsc -b && npm run build
# daemon cô lập + LLM giả (cổng 51788/51789/51931), cwd/HOME/SENCLAW_HOME trong thư mục tạm
P4_RUNS=/tmp/senclaw-p4-runs senclaw/plans/reports/project-p4-web/p4-up.sh <senclaw binary e8b5264> <web dist>
# mở http://127.0.0.1:51788/projects ; LLM giả nhận chỉ thị §§STEPS{"tag":"X","steps":[]}§§ trong tin nhắn và ghi mọi request vào <run>/mock
P4_RUNS=/tmp/senclaw-p4-runs senclaw/plans/reports/project-p4-web/p4-down.sh
```

Đã dọn: dừng daemon (pid 86295) và mock (pid 86294) do tôi khởi động, cổng 51788/51789/51931 trống; xoá thư mục chạy tạm, `dist/` và symlink `node_modules` trong worktree. Daemon thật 18788/18789 và `~/.senclaw` không bị đụng.

## Câu hỏi chưa giải quyết

1. Wording "Không có dự án" (web) vs "Không project": thống nhất với desktop?
2. Có cần cache chung cho `useProjects` (hoặc một sự kiện WS `project:updated` ở v2) không, hay mỗi nơi tự tải là đủ?
3. Trang Kanban có nên có hộp "sửa board" (đổi tên + thư mục) để project hiện đầy đủ ở đó không? Hiện chỉ có ở hộp tạo.
4. `lastActive` của `/api/projects/:id/chats` có nên được daemon điền không (client desktop cũng sẽ gặp)?
