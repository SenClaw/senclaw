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
- **Phiên code không có `EnterPlanMode`** (người dùng vẫn tự chuyển sang Plan được): model nhỏ vào Plan mode cho mọi việc "đụng 3 file" và mất 100–500 giây lập kế hoạch,
  rồi dừng ở thẻ duyệt `ExitPlanMode`.
- **Model tự host (localhost, mạng LAN, `.local`) không bị cắt theo giờ như host công cộng**: e4b ở LAN chạy 7–10 token/giây nên ghi một file 100 dòng đã quá 2 phút. Ba giới hạn
  cùng gây ra "Network error: operation timed out" hoặc "LLM request timed out" đều được nâng cho loại endpoint này: hạn tổng 120 giây mỗi request (bỏ), hạn 180 giây mỗi lượt
  của vòng lặp (thành 900 giây như engine local), và hạn im lặng 120 giây (thành 900 giây; request không stream thì im lặng tới khi có cả câu trả lời). Host công cộng giữ nguyên.
  Lưu ý: `LLM_TIMEOUT` làm pool **reset phiên** (mất ngữ cảnh), khác `NETWORK_ERROR` được giữ ngữ cảnh.
- **Kiểm tra trước khi kết thúc**: model dừng sau khi sửa file nguồn mà chưa chạy build/test/lint nào → nhắc tối đa
  **hai lần**, nêu đúng lệnh cần chạy; vẫn bỏ qua thì câu trả lời cuối mang cảnh báo "chưa kiểm chứng". Không áp dụng cho subagent.

## Vòng lặp không kẹt, sửa file an toàn, có trần, có kiểm chứng (round 2)

Đối chiếu với OpenHands (xem báo cáo nghiên cứu) cho thấy bốn chỗ SenClaw còn kém; bốn chỗ đó, và hai lỗi tìm thấy khi đọc mã, đã sửa.

### Phát hiện kẹt theo **kết quả** (`zen_core/stuck.rs`)

| Mẫu | Điều kiện | Ghi chú |
|---|---|---|
| `SameResult` | cùng lệnh, **cùng kết quả**, 4 lần liên tiếp | `cargo test` lần 2 sau khi sửa file cho kết quả khác → không phải kẹt |
| `Alternating` | hai lệnh A, B xen kẽ, cùng kết quả, 3 vòng | hai lần sửa hoàn tác nhau |
| `SameFailure` | cùng lệnh thất bại với cùng output 4 lần, dù giữa chừng có sửa | sửa mà không chạm tới nguyên nhân |

Lần đầu: nhắc một lần (nêu tên lệnh), xoá lịch sử để mẫu phải hình thành lại. Lần hai trong 25 lượt: dừng, tắt tool, ép trả lời trung thực
("đừng nói đã xong"). Chỉ cho phiên code; chat thường giữ bộ đếm theo tên tool như cũ.

Hai lỗi cũ đã sửa:

- **Chặn lệnh trùng** chặn mọi lệnh giống hệt trong cả một lượt, nên `cargo test` → sửa → `cargo test` bị trả "viết câu trả lời cuối ngay". Giờ chỉ chặn khi
  *không có gì đổi từ lần chạy trước* (epoch: tăng mỗi khi `Edit`/`Write`/`NotebookEdit`/`Task` hoặc `Bash` không-chỉ-đọc chạy). Đọc lại file khi chưa
  sửa gì vẫn bị chặn.
- **Bộ đếm "cùng tool, không có chữ"** dừng một phiên code đọc 8 file liên tiếp. Phiên code không dùng nó nữa.

### Sửa file (`tools/file_io.rs`)

- Ghi **nguyên tử**: file tạm cùng thư mục → `rename` (giữ quyền, đi theo symlink). Không bao giờ lộ nửa file.
- **Khoá theo file** quanh cả đọc-sửa-ghi: subagent / DAG worker cùng sửa một file không mất lần sửa nào.
- **`Edit {file_path, undo: true}`** hoàn tác lần sửa gần nhất (nhiều lần được; tối đa 8 bản, 64 file, bỏ file > 1 MB). Chỉ hoàn tác khi file còn đúng như
  lần sửa để lại; ai đó đã sửa sau đó thì từ chối, không đè mất. Ghi tạo file mới thì undo xoá file.
- Bash cho biết nó kết thúc ra sao: `[exit code N]` khi khác 0, `(no output; exit code 0)` khi rỗng. Trước đây model chỉ thấy phần đuôi của một lần test đỏ.

### Giới hạn một yêu cầu code

| | Mặc định | Đổi ở |
|---|---|---|
| Số lượt gọi tool | **500** (chat giữ 30; `SENCLAW_MAX_AGENT_TURNS` luôn thắng) | `POST /api/agent-behavior {"codeMaxTurns": 10..5000}` |
| Tổng token (vào + ra, mọi lần gọi) | **10 triệu**, `0` = tắt | `{"maxRunTokens": n}` |
| Hỏi model "xong chưa?" | tắt | `{"finishJudge": true}` |

Chạm trần = ép một câu trả lời cuối với tool bị tắt (không vứt công việc). **Không** phát `SessionError` trước câu trả lời đó: pool coi nó là kết thúc yêu
cầu và câu trả lời mất. Nén ngữ cảnh chủ động giờ lặp lại được (cách ≥ 8 lượt) thay vì một lần mỗi yêu cầu — cần khi một yêu cầu dài 500 lượt.

### Kiểm chứng trước khi kết thúc (`zen_core/finish_check.rs`)

Thứ tự khi model dừng ở phiên code (mỗi bước chỉ một lần, trừ phán quan và PreStop):

0. Câu trả lời **kết thúc bằng dấu hai chấm** mà không gọi công cụ nào ("Đây là nội dung cho file X:" rồi dừng) → nhắc "bạn mới chỉ mô tả, hãy gọi công cụ ngay",
   **tối đa 2 lần**. Chỉ xét dấu `:` (kể cả `：`, bỏ qua `*`/`_` cuối), không có danh sách cụm từ nên dùng được cho mọi ngôn ngữ. Đo trên e4b: dự án 7 file dừng sau 2 file;
   nhắc xong nó gọi `Write` ba lần liền.
   Cùng ngân sách 2 lần áp cho một yêu cầu **chưa sửa file hay chạy lệnh nào** (chỉ đọc/`repo_map` không tính là đã làm) mà câu trả lời kết thúc bằng `?` hoặc khối code
   (` ``` `), **hoặc** yêu cầu có nêu tên file mà chưa có lời gọi nào chạm tới file đó: "Bạn đồng ý với kế hoạch này chứ?", lệnh viết thành chữ (`read_file "models.py"`),
   "Xin vui lòng xác nhận kế hoạch", hoặc một lần `repo_map` rồi dừng. Nhắc: đừng hỏi xin duyệt, hãy gọi công cụ (kèm danh sách file chưa mở); chỉ khi thiếu thông tin
   thật sự thì nói thiếu gì trong một câu. Sau khi đã sửa/chạy thì câu hỏi/khối code ở cuối là kết thúc bình thường và không bị nhắc.
1. Sửa mã nguồn nhưng chưa chạy build/test/lint nào → nhắc, **tối đa 2 lần**. Lần 1 nêu đúng lệnh và nói rằng các lần sửa nằm *sau* lần chạy
   cuối (model từng lập luận "yêu cầu trước đã chạy rồi" rồi báo test pass khi chưa chạy). Lần 2 chỉ bảo "gọi Bash ngay với `<lệnh>`". Model vẫn trả
   lời không chạy → câu trả lời cuối được gắn dòng `⚠️ Chưa kiểm chứng …` (tiếng Việt nếu yêu cầu có dấu tiếng Việt, ngược lại tiếng Anh), nêu lệnh nên chạy.
   Cảnh báo chỉ nằm ở tin gửi người dùng, không nằm trong lịch sử gửi lại cho model (model sẽ bắt chước). Lệnh được chọn theo thứ tự: lệnh kiểm tra
   chính model đã chạy trong hội thoại → lệnh test của stack chứa file vừa sửa (`repo_map::commands`; thư mục chỉ có `test_*.py` đọc runner từ file:
   `unittest` hoặc `pytest`) → không nêu lệnh nào.
2. Lần build/test/lint cuối **thất bại** (`[exit code N]`) và chưa có lần nào pass sau đó → nhắc, không cho báo "xong".
3. (tuỳ chọn, `finishJudge`) một lần gọi model chấm "xong chưa?" → `{complete, missing}`; thiếu thì gửi lại danh sách, tối đa 2 vòng. Lỗi hoặc JSON
   không đọc được = cho kết thúc (chỉ là tư vấn).
4. Hook **`PreStop`** do người dùng tự viết trong `hooks.json`: `blocking: true` + `{"blocked": true, "reason": "…"}` gửi `reason` về làm tin nhắn kế
   tiếp, tối đa 3 lần. Cố ý là sự kiện riêng, **không** nằm trong danh sách của marketplace plugin: `Stop` vốn không chặn được, vì một gói không đáng tin
   không được giữ agent chạy hay đặt chữ vào ngữ cảnh của nó.

Prompt của phiên code có thêm quy trình 8 bước gọn (hiểu → baseline → định vị → tái hiện lỗi → sửa nhỏ nhất → kiểm → đọc lại diff → báo cáo).

## Quan sát

`GET /api/code/autonomy` → `{ level, ranUnattended, askedAnyway }` từ lúc daemon chạy. Hai con số trả lời câu hỏi
"có đúng là ít hỏi, và những lần hỏi là đáng hỏi không".

## Giới hạn (cố ý)

- **Không phải sandbox.** Chấm điểm dòng lệnh, không thấy bên trong một script model vừa viết rồi chạy
  (`python x.py` là Medium). Repo không phải của bạn → bật Plugins → Sandbox (`exec`) hoặc để `ask`.
- Chưa có: sandbox làm ranh giới phê duyệt, phân tích shell bằng AST (`tree-sitter-bash` đã có trong `Cargo.toml`), luật "chỉ dẫn từ file repo mà dẫn tới
  `curl | sh` thì nâng lên High", `AGENTS.md` lồng nhau theo đường dẫn, che bớt kết quả tool cũ, tool `think`, terminal bền.
- Chấm điểm sai chiều "cho qua" là lỗi nghiêm trọng, sai chiều "hỏi" là chấp nhận được: thêm lệnh vào danh sách
  Medium chỉ khi cả đường dẫn lẫn tác dụng đều kiểm chứng được từ dòng lệnh.
- `env <cmd>` từng nằm trong danh sách chỉ-đọc (`env rm -rf x` chạy không hỏi). Đã sửa ở `shell_safety`, có test.

## Kiểm chứng

- Unit: `cargo test --lib util::command_risk zen_core::permissions repo_map tools::edit zen_core::conversation`.
- E2E (daemon thật + LLM kịch bản + git repo thật, dưới `sandbox-exec` chặn cổng thật): `plans/reports/code-v3-e2e/`.
