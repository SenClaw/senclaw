# Tách thư mục SenClaw qua biến môi trường (`SENCLAW_HOME`, `SENCLAW_DATA_HOME`)

Daemon dùng hai thư mục gốc. Mỗi thư mục có một biến môi trường để dời đi, nên một ứng dụng có thể
chạy SenClaw làm runtime core với state riêng, không đụng tới bản cài của người dùng.

| Biến | Thay cho | Chứa |
|---|---|---|
| `SENCLAW_HOME` | `~/.senclaw` | `config.json`, `senclaw.db`, `api_token`, logs, runtimes, local-models, sandbox, dữ liệu Space App |
| `SENCLAW_DATA_HOME` | `~/senclaw` | `profiles/`, `workspace/` (kể cả Space App đã cài), `wiki/`, `workflows/`, `quicknotes/`, `virtual-agents/` |

Resolver duy nhất: [`src/util/paths.rs`](../src/util/paths.rs) `senclaw_home()` / `senclaw_data_home()`. Mọi module
lấy đường dẫn mặc định từ đây; `Config::from_env()` cũng vậy.

## Quy tắc phân giải

- Không đặt biến nào → `~/.senclaw` và `~/senclaw`, như trước.
- Chỉ đặt `SENCLAW_HOME` → dữ liệu người dùng mặc định là **`$SENCLAW_HOME/data`**. Chỉ một biến là đủ để tách
  hẳn; nếu dữ liệu vẫn ở `~/senclaw` thì hai daemon sẽ ghi chung profiles, sessions và workspace.
- Đặt `SENCLAW_HOME` đúng bằng `~/.senclaw` → dữ liệu vẫn ở `~/senclaw`. Đặt biến trùng mặc định thì không đổi gì.
- `SENCLAW_DATA_HOME` luôn thắng khi được đặt.
- Giá trị có thể bắt đầu bằng `~/` hoặc là đường dẫn tương đối (tính từ thư mục hiện tại). Khi khởi động, daemon
  ghi lại cả hai biến thành đường dẫn tuyệt đối trong env của chính nó (`pin_senclaw_dirs_in_env`), nên MCP server,
  runtime và Space App con (mỗi thứ chạy ở cwd khác) đều thấy đúng một thư mục.
- Các biến riêng từng đường dẫn cũ (`DB_PATH`, `SENCLAW_CONFIG_PATH`, `WORKSPACE_DIR`, `SENCLAW_LOCAL_MODELS_DIR`, …)
  vẫn thắng giá trị suy ra từ hai thư mục gốc.
- Runtime (`sen-*`) nhận `SENCLAW_HOME` = thư mục state thực tế của daemon (trước đây là thư mục cha của
  `config.json`).

## Chạy một ứng dụng độc lập với SenClaw làm runtime core

```bash
SENCLAW_HOME="$PWD/.senclaw-dev" \
SENCLAW_UI_PORT=28788 SENCLAW_WS_PORT=28789 \
senclaw start
```

Hoặc đặt cùng các dòng đó trong `.env` của ứng dụng (daemon đọc `.env` ở thư mục chạy). Luôn đổi cả cổng:
`18788`/`18789` là của daemon chính. Client nào nói chuyện với daemon này phải được chỉ đúng cổng
(`senclaw acp --gateway ws://127.0.0.1:28789`).

Cách này thay cho mẹo `HOME=<thư mục tạm>` khi phát triển. `HOME` vẫn dùng được, nhưng nó còn dời cả
`~/.claude`, cấu hình git, keychain… còn `SENCLAW_HOME` chỉ dời những gì SenClaw sở hữu.

## Sandbox

Thư mục state chứa DB và token, nên sandbox chặn nó ở **bất cứ đâu** `SENCLAW_HOME` trỏ tới, ngoài `~/.senclaw`
(vẫn bị chặn, vì bản cài của người dùng trên cùng máy cũng riêng tư như vậy):

- macOS Seatbelt, chế độ đọc `open`: thư mục state nằm trong danh sách `deny file-read*`; thư mục làm việc của sandbox
  bên trong nó được cho phép lại sau lệnh deny (`direct::mac_denied_roots`).
- Linux bubblewrap: nếu thư mục state nằm ngoài `$HOME` thì có thêm một `--tmpfs` che nó, đặt trước các bind
  (`direct::bwrap_state_mask`).
- Gắn thư mục (`sandbox::mounts::validate`) và quyền đọc toolchain theo PATH đều không bao giờ cấp thứ gì
  nằm trong thư mục state. `$SENCLAW_HOME/bin` trên PATH sẽ không kéo theo thư mục cha.

Hệ quả của mặc định `$SENCLAW_HOME/data`: workspace nằm trong thư mục state, nên không gắn được thư mục con của
nó vào sandbox. Muốn gắn thì đặt `SENCLAW_DATA_HOME` ra ngoài.

## Không thay đổi

- Thư mục `.senclaw/` **theo dự án** hoặc **theo app** (`<workspace>/.senclaw/hooks.json`, `<project>/.senclaw/mcp.json`,
  `<app>/.senclaw/mcp-tools.json`, `runtime.log`) không phải thư mục gốc, nên không đi theo các biến này.
- `scripts/install.sh` vẫn cài binary vào `~/.senclaw/bin` (đổi bằng `SENCLAW_INSTALL_DIR`).
- `SENCLAW_DATA_DIR` (tên cũ, chỉ Kanban đọc) vẫn ghi đè gốc dữ liệu Space App của Kanban.
