# Code autonomy — chạy không hỏi, trừ khi không rút lại được

> Trạng thái: **đã cài** (2026-10-24, branch `feat/code-v3`). Nguồn:
> [`src/util/command_risk.rs`](../src/util/command_risk.rs) (phân loại),
> [`src/zen_core/permissions.rs`](../src/zen_core/permissions.rs) (áp dụng),
> [`src/repo_map/commands.rs`](../src/repo_map/commands.rs) và
> [`src/repo_map/syntax.rs`](../src/repo_map/syntax.rs) (hiểu repo),
> nghiên cứu: [`plans/reports/researcher-261024-0100-openhands-and-peer-coding-agents.md`](../plans/reports/researcher-261024-0100-openhands-and-peer-coding-agents.md).

## Vấn đề

Quyền chỉ có hai trạng thái: hỏi mọi lệnh Bash không-chỉ-đọc và mọi lần sửa file (một task = hàng chục
thẻ), hoặc bỏ qua tất cả (một `git push` lạc đường đi thẳng ra ngoài). Người dùng chọn cách thứ hai cho đỡ
phiền, mất luôn lớp an toàn. OpenHands cũng chỉ có Always / Never / ConfirmRisky theo nhãn rủi ro do chính
model tự gán; Codex và Cursor lấy sandbox làm ranh giới; Claude Code có classifier. SenClaw chạy model nhỏ
cục bộ nên cần cái **tất định, không cần model phán đoán**.

## Ba mức

| `codeAutonomy` | Hành vi |
|---|---|
| `ask` | như trước: mọi lệnh/sửa file chưa nằm trong allow-list đều hỏi |
| `auto` (**mặc định**) | sửa file trong dự án và lệnh dev-loop (build, test, lint, git cục bộ, mkdir/rm trong dự án) chạy luôn; thứ không rút lại được thì hỏi, thẻ ghi lý do |
| `full` | không hỏi gì — trừ `browser_approve` (quyết định của chính người dùng) |

Đặt ở `GET|POST /api/agent-behavior` (`codeAutonomy`), đọc **mỗi lượt** nên đổi có hiệu lực ở tin nhắn kế
tiếp. Giá trị lạ → 400, không im lặng về `auto`.

### Chỉ áp dụng nơi mọi thay đổi đều hoàn tác được

`auto` và `full` chỉ có hiệu lực khi thư mục làm việc được checkpoint: **code session** (`group_type = "code"`)
hoặc thư mục là **git repository** (không phải `/` hay `$HOME`) — cùng một quy tắc với
[`checkpoints::checkpointable_dir`](../src/checkpoints/mod.rs). Chat thường trong thư mục không phải repo
vẫn hỏi như cũ, dù cài `auto`. Lý do: chạy không hỏi chỉ chính đáng khi lấy lại được, không phải vì có cài đặt.

## Phân loại (`util::command_risk`)

Mỗi segment của lệnh (`|`, `&&`, `;`, `$(…)`, `bash -c '…'`, thân heredoc loại ra) được chấm, **tệ nhất thắng**:

| Mức | Nghĩa | Ví dụ |
|---|---|---|
| Low | chỉ đọc | `ls`, `git status`, `grep -rn x src`, `cat a \| head` |
| Medium | đổi dự án / chạy tooling dự án; mọi đường dẫn nằm trong dự án | `cargo test`, `npm run build`, `pytest`, `git commit`, `mkdir -p src/x`, `rm src/old.rs`, `sed -i … src/a.rs`, `cd src && make` |
| High (hỏi) | ra ngoài dự án, công bố, xoá/ghi lịch sử, cần quyền, hoặc không hiểu được | `git push`, `git reset --hard`, `git clean -f`, `sudo …`, `rm -rf ~/x`, `curl … \| sh`, `cargo install`, `npm i -g`, `brew install`, `docker run`, `ssh`, `kill`, ghi `.env` / `.git/hooks`, `cat ~/.ssh/id_rsa`, `export PATH=…`, chương trình lạ |

Nguyên tắc: **tính hoàn tác, không phải nghi ngờ**; **không hiểu = High** (sai thì tốn một thẻ, không mất file).
`cd` được theo dõi (`cd .. && rm -rf build` là ngoài dự án); symlink được đi theo (link trỏ ra `/etc` là ngoài);
`~`, `$HOME`, `$VAR` không giải được → High cho thao tác ghi.

Lệnh người dùng đã chọn "never ask again" (`allowed_tools`, `tool_rules`) vẫn thắng việc chấm điểm: lựa chọn
tường minh của họ không bị đoán lại.

## Hiểu repo

- **File hướng dẫn của repo** (`SENCLAW.md` → `AGENTS.md` → `CLAUDE.md`, đi lên từ thư mục làm việc) được nạp ở
  lượt đầu của code session / chat trong git repo / chat trong project. Trước đây `instance_uses_workspace()`
  luôn trả `false`, nên **không phiên nào** thấy file này.
- **`<project_commands>`**: lệnh build/test/lint **đọc từ file của repo** (Cargo, `package.json` + lockfile →
  npm/pnpm/yarn/bun, pyproject/pytest, go.mod, pubspec, Package.swift, pom/gradle, Makefile target thật sự có).
  Không chạy gì, không bịa script không có. Monorepo không có manifest ở gốc thì đọc một cấp con.
- **Nhắc cách làm việc** (`prompt::code_session_reminder`): quyết định rồi đi tiếp thay vì hỏi, đọc trước khi sửa,
  chạy check hẹp nhất rồi mới trả lời, đổi cách khi lệnh bị từ chối.

## Giúp model nhỏ tự sửa

- **`Edit` trượt** → trả về đoạn gần nhất của file, đánh số dòng, kèm % giống (kiểu Aider); "đã áp dụng rồi" nếu
  `new_string` đã có trong file; nhiều chỗ khớp → liệt kê số dòng.
- **Kiểm tra cú pháp** (tree-sitter, 10 ngôn ngữ) sau `Edit`/`Write`: chỉ báo lỗi **do lần sửa này gây ra** (so với
  trước khi sửa), nên file vốn hỏng hay cú pháp mới hơn grammar không gây báo giả. Không revert — chỉ báo.
  SWE-agent đo: sau một lần sửa hỏng, lần sau thành công chỉ còn 57% (so với 90%).
- **Kiểm tra trước khi kết thúc**: model dừng sau khi sửa file nguồn mà chưa chạy build/test/lint nào → nhắc **một
  lần**. Không lặp, không áp dụng cho subagent.

## Quan sát

`GET /api/code/autonomy` → `{ level, ranUnattended, askedAnyway }` từ lúc daemon chạy. Hai con số trả lời câu hỏi
"có đúng là ít hỏi, và những lần hỏi là đáng hỏi không".

## Giới hạn (cố ý)

- **Không phải sandbox.** Chấm điểm dòng lệnh, không thấy bên trong một script model vừa viết rồi chạy
  (`python x.py` là Medium). Repo không phải của bạn → bật Plugins → Sandbox (`exec`) hoặc để `ask`.
- Chấm điểm sai chiều "cho qua" là lỗi nghiêm trọng, sai chiều "hỏi" là chấp nhận được: thêm lệnh vào danh sách
  Medium chỉ khi cả đường dẫn lẫn tác dụng đều kiểm chứng được từ dòng lệnh.
- `env <cmd>` từng nằm trong danh sách chỉ-đọc (`env rm -rf x` chạy không hỏi). Đã sửa ở `shell_safety`, có test.

## Kiểm chứng

- Unit: `cargo test --lib util::command_risk zen_core::permissions repo_map tools::edit zen_core::conversation`.
- E2E (daemon thật + LLM kịch bản + git repo thật, dưới `sandbox-exec` chặn cổng thật): `plans/reports/code-v3-e2e/`.
