# `senclaw create` — dựng Space App, skill, sub-agent từ template

```bash
senclaw create app  "Quản lý Kho" --lang rust     # Space App
senclaw create skill "Báo cáo tuần"                # SKILL.md
senclaw create sub-agent "Data Analyst"            # persona .md
senclaw create list                                # có những template nào
senclaw create update                              # kéo bản template mới nhất
```

Template nằm trên git — sửa một template là mọi người có ngay, không cần release
binary. Đồng thời **mỗi template cũng được nhúng sẵn trong binary**, nên lệnh này
chạy được khi không có mạng, khi proxy chặn git, hoặc khi repo template chưa tồn
tại. Git thắng khi kết nối được; bản nhúng là lưới an toàn, và dòng
`template: git:…` / `template: bundled` in ra luôn nói rõ bạn vừa dùng bản nào.

## Bốn ngôn ngữ

| `--lang` | template | runner | đóng gói |
|---|---|---|---|
| `rust` (mặc định) | `app-rust` | `binary` | `./scripts/pack.sh` build rồi zip |
| `go` | `app-go` | `binary` | `./scripts/pack.sh` build rồi zip |
| `node` | `app-node` | `node` | zip thẳng; daemon chạy `runtime.install` |
| `python` | `app-python` | `python` | zip thẳng; daemon tạo `.venv` nếu có `requirements.txt` |

Cả bốn sinh ra một app **chạy được ngay**: REST `/api/status`, MCP với hai tool,
một trang UI tĩnh, và code gọi ngược lên daemon (model + config KV) đã kèm token.
Không có bước build front-end — UI là một file HTML, để `cargo run` / `go run .`
/ `node server.mjs` / `python3 main.py` là thấy màn hình ngay.

## Cái gì được điền vào template

Tên bạn gõ được tách ra thành mọi dạng chữ mà template có thể cần:

| biến | từ `"Quản lý Kho"` | dùng ở đâu |
|---|---|---|
| `{{id}}` | `quan-ly-kho` | id app, tên thư mục, `mcp.name` |
| `{{name}}` | `Quản lý Kho` | nguyên văn bạn gõ |
| `{{title_name}}` | `Quản lý Kho` | `manifest.name`, tiêu đề trang |
| `{{snake_name}}` | `quan_ly_kho` | tên tool MCP, tên crate |
| `{{pascal_name}}` / `{{camel_name}}` | `QuanLyKho` / `quanLyKho` | tên type, tên biến |
| `{{screaming_name}}` | `QUAN_LY_KHO` | hằng số, env var |
| `{{port}}` | `4801` | cổng trống đầu tiên từ 4800 |
| `{{mcp_name}}` | `quan-ly-kho-mcp` | `mcp.name` |
| `{{description}}` `{{icon}}` `{{author}}` `{{year}}` `{{senclaw_version}}` `{{api_version}}` | | |

**Dấu tiếng Việt chỉ bị khử ở `id`**, không ở tên hiển thị: app trong danh sách
Space Apps vẫn là "Quản lý Kho", chỉ id là `quan-ly-kho`. Khử dấu dùng chung bảng
với `security::replication::fold` (`đ→d`, `ứ→u`, …). Tên không còn ký tự ASCII
nào sau khi khử (ví dụ `"日本語"`) thì lệnh dừng lại và bảo bạn truyền `--id`,
thay vì tạo ra một app tên `-`.

Thêm biến của riêng bạn:

```bash
senclaw create app Kho --var api_base=https://erp.local --var team=logistics
```

`--var` thắng mọi thứ, kể cả `id` và `port`.

## Cổng

Không truyền `--port` thì lệnh tự chọn cổng trống đầu tiên từ **4800**, bỏ qua
hai thứ:

- cổng đã khai trong `senclaw-manifest.json` của **mọi app đã cài** và của các
  app cùng thư mục đang tạo — một app đang dừng vẫn giữ cổng của nó, mà app
  `session` thì phần lớn thời gian là đang dừng;
- cổng **đang có tiến trình lắng nghe** trên `127.0.0.1`.

App SenClaw bán sẵn nằm ở 4300–4799, nên 4800 trở lên là vùng an toàn cho app tự
làm.

## Kiểm tra trước khi ghi

Lệnh render toàn bộ vào bộ nhớ, kiểm tra, rồi mới ghi ra đĩa — hỏng ở file thứ
mười thì không để lại một dự án dở dang. Những gì bị chặn:

| kiểm tra | vì sao |
|---|---|
| `senclaw-manifest.json` phải parse được, có `id` | thiếu là daemon không bao giờ nạp thư mục đó |
| `runtime` phải là object; `mode`/`runner` phải là chuỗi; `port` phải là số | mọi kiểm tra bên dưới đọc field bằng `as_str`/`as_u64`, mà **sai kiểu trả về giống hệt vắng mặt** — một `"port": "4800"` (thừa đúng một cặp nháy, JSON vẫn hợp lệ) sẽ lọt hết mọi kiểm tra rồi tới daemon, nơi nó được đọc thành cổng 0 |
| `runtime.mode` phải là `background` hoặc `session` | **viết sai là im lặng**: daemon rơi về `session`, một app cần chạy nền sẽ lặng lẽ dừng |
| `runtime.runner` phải là `binary`/`node`/`python`/`shell` | sai runner = sai bước cài phụ thuộc |
| không file nào bind `0.0.0.0` | Space App không có xác thực riêng; bind wildcard là mở toàn bộ REST + MCP ra LAN. Template được copy qua lại nhiều nhất trong codebase này, nên một bản copy sai là cả đàn bị lộ |
| skill phải có `name:` trong frontmatter | thiếu là skill không được nạp |
| persona phải có frontmatter YAML | thiếu là `PersonaRegistry` bỏ qua file |

Không có ngoại lệ cho dòng có `SENCLAW_BIND_HOST`. Nghe thì hợp lý — comment
hướng dẫn opt-in cũng nhắc tới `0.0.0.0` — nhưng nó đồng thời tha đúng cái sai mà
kiểm tra này sinh ra để bắt: `env("SENCLAW_BIND_HOST") || "0.0.0.0"`, tức đọc env
var nhưng **mặc định** là wildcard. Comment và văn xuôi (`.md`) đã được bỏ qua từ
trước.

Cảnh báo (không chặn): cổng trong manifest lệch với cổng đã chọn, `id` lệch,
còn `{{placeholder}}` chưa thay.

`--dry-run` chạy hết các bước trên và in ra danh sách file, không ghi gì.

### Giá trị người dùng nhập được escape theo định dạng đích

`--desc`, `--icon`, `--var` là văn bản tuỳ ý, và nó rơi vào bốn loại file khác
nhau với bốn luật khác nhau. Escape chỉ áp cho **giá trị thay vào**, không bao
giờ đụng vào text của chính template:

| đích | escape | vì sao |
|---|---|---|
| `.json` (kể cả `.babelrc`, `.eslintrc`…) | JSON | `--desc 'Quản lý "công việc"'` vẫn ra manifest hợp lệ; `--desc 'x", "id": "evil'` chỉ là mô tả có dấu nháy, không đẻ ra khoá `id` thứ hai (serde lấy khoá trùng **cuối cùng** nên khoá tiêm vào sẽ thắng khoá thật) |
| `.md` — **khối frontmatter** | YAML (cùng luật với JSON) | `description: Trợ lý: quản lý kho` không phải YAML hợp lệ, và khi khối hỏng thì `name:` chết theo — skill không được nạp mà lỗi lại chỉ vào một field đang đúng. Xuống dòng còn tệ hơn: nó chèn được `name: evil-persona` (registry giữ khoá trùng cuối) hoặc một `---` kết thúc sớm khối frontmatter |
| `.md` — **phần body** | không | README không có frontmatter, escape vào đó thì mọi dự án sinh ra đều mở đầu bằng `has \"quotes\"` |
| `.html` | HTML | mô tả hiển thị trên chính UI của app; `<script>` trong đó là một thẻ script thật trên trang daemon proxy |

Template quyết định nửa còn lại: skill và sub-agent **để nháy kép quanh scalar**
(`description: "{{description}}"`), vì escape kiểu YAML chỉ đúng bên trong một
chuỗi có nháy.

## Nơi kết quả rơi vào

| loại | mặc định | vì sao |
|---|---|---|
| app | `./<id>` | cần build trước khi cài, nên nó là một dự án bạn làm việc trên đó |
| skill | `~/.senclaw/managed/skills/<id>/SKILL.md` | markdown thuần, không build — tạo xong là dùng được luôn |
| sub-agent | `~/senclaw/virtual-agents/<id>.md` | như trên |

`--dir <path>` đổi được.

App và skill **sở hữu** thư mục của nó, nên thư mục đích đã có file là bị từ
chối. Sub-agent thì **dùng chung**: `virtual-agents/` chứa mọi persona trên máy,
nên ở đó chỉ trùng đúng file `<id>.md` mới bị chặn — nếu không thì lệnh chỉ chạy
được đúng một lần cho mỗi máy. `--force` bỏ qua rào tương ứng (ghi đè file trùng
tên, không xoá cả thư mục).

## Repo template

Mặc định: `https://github.com/NortonBen/senclaw-templates`, nhánh `main`, clone
vào `~/.senclaw/templates/<tên-repo>-<hash>` — **một thư mục cho mỗi cặp
repo+ref**. Dùng chung một thư mục `repo/` thì lần `--repo` thứ hai thấy clone cũ
đã ở đó, `git pull` kéo từ `origin` cũ, và bạn nhận template của repo trước trong
khi màn hình báo tên repo bạn vừa gõ.

```bash
senclaw create app Kho --repo https://git.cty.vn/senclaw-templates --ref v2
SENCLAW_TEMPLATES_REPO=https://git.cty.vn/senclaw-templates senclaw create app Kho
SENCLAW_TEMPLATES_DIR=/opt/cache senclaw create list     # đổi chỗ cache
senclaw create app Kho --offline                          # chỉ dùng bản nhúng
senclaw create app Kho --refresh                          # clone lại từ đầu
senclaw create app Kho --template ./my-template           # template trên máy
```

### Viết một template

Hai layout đều được nhận: `templates/<tên>/` (repo chính thức, để chừa chỗ cho
README và CI ở gốc) hoặc `<tên>/` ngay gốc repo.

```
templates/app-deno/
  template.json          <- mô tả template; KHÔNG được copy sang dự án mới
  files/                 <- payload; nếu không có thư mục này thì cả thư mục là payload
    senclaw-manifest.json
    main.ts
    web/index.html
```

`template.json` — chỉ `name` là bắt buộc:

```json
{
  "name": "app-deno",
  "kind": "app",
  "lang": "node",
  "description": "Dòng mà `senclaw create list` in ra",
  "variables": [
    { "name": "mcp_name", "default": "{{id}}-mcp" },
    { "name": "api_key", "description": "khoá API", "required": true }
  ],
  "root": "files",
  "ignore": ["fixtures"],
  "postCreate": ["deno task start   # http://127.0.0.1:{{port}}"],
  "minCliVersion": "0.6.0"
}
```

- `kind` thiếu thì suy ra từ payload: có `senclaw-manifest.json` → `app`, có
  `SKILL.md` → `skill`.
- `default` của một biến có thể tham chiếu biến khai trước nó.
- biến `required` mà không có `--var` thì lệnh dừng, không ghi ra một dự án điền
  một nửa.
- `postCreate` được **in ra, không chạy** — một scaffolder tự chạy `cargo build`
  là một scaffolder treo bốn phút không nói gì.
- Không bao giờ copy: `.git`, `template.json`, `node_modules`, `target`,
  `__pycache__`, `.venv`, `dist`, `.senclaw`.

### Cú pháp render

`{{tên_biến}}` (chữ thường, số, gạch dưới) được thay. **Mọi thứ khác trong ngoặc
kép giữ nguyên** — `{{.Name}}` của Go template, `{{ item.title }}` của Vue,
`{{#each}}` của handlebars đều đi qua nguyên vẹn. Cần một `{{` thật thì viết
`{{{{`.

Placeholder trông giống biến nhưng không có biến nào (gõ nhầm `{{app_i}}`) thì
**giữ nguyên trong file và báo cảnh báo** — không xoá trắng, vì im lặng ở đây
nghĩa là một file thiếu một đoạn mà không ai biết.

Tên file cũng được render, từng đoạn một: `files/{{id}}.md` → `data-analyst.md`.
Biến chứa `/` hoặc `..` bị từ chối, nên một giá trị độc không thoát ra ngoài thư
mục đích được.

File không phải UTF-8 (ảnh, font) được copy nguyên bytes. Quyền thực thi của
`scripts/*.sh` được giữ.

## Điều gì được nhúng trong binary

`assets/templates/<tên>/` được [`build.rs`](../build.rs) duyệt và biến thành một
bảng `include_bytes!`. Thêm template nhúng = thả một thư mục vào đó, không phải
sửa danh sách nào — cách còn lại có kiểu hỏng là file nằm trong repo, thiếu trong
binary, và không ai biết cho tới lúc có người offline.

`cargo test` kiểm tra: đủ bốn ngôn ngữ được quảng cáo, mỗi template có
`template.json` hợp lệ với `kind` và `description`, **mỗi template app render ra
rồi phải qua toàn bộ khâu kiểm tra ở trên** (kể cả luật cấm bind `0.0.0.0`), và
hai luật về hợp đồng bridge:

- template nào gọi `/bridge` phải gửi trường **`action`**, không phải
  `capability`. Daemon khai `struct SpaceAppBridgeBody { action, payload }`, nên
  body sai tên bị axum trả **422 trước khi vào handler** — và vì template bọc lỗi
  đó thành một khối `isError` gọn gàng, nó trông y hệt trạng thái "chưa chạy
  daemon". Đã lọt một lần; test này để không lọt lần nữa.
- template phải xử lý `status == "error"`. Completion hỏng được daemon trả về
  **HTTP 200** kèm `{"status":"error","message":…}`, nên chỉ kiểm tra HTTP status
  sẽ biến một sự cố provider thành bản tóm tắt rỗng *thành công*.

## Mã nguồn

| | |
|---|---|
| engine | [`src/scaffold/`](../src/scaffold/) — `render` (`{{…}}`), `spec` (`template.json`), `source` (git/bundled/local), `port`, `create` (render → validate → write) |
| CLI | [`src/cli/commands/create.rs`](../src/cli/commands/create.rs) |
| template nhúng | [`assets/templates/`](../assets/templates/) |
| clone git | dùng lại [`marketplace::git_sync`](../src/marketplace/git_sync.rs) |
