# Ghép Soul Core vào SenClaw — bản đồ điểm nối

Tài liệu thứ ba của loạt Soul Core. Hai tài liệu trước trả lời *cái gì* và *tại
sao*; tài liệu này trả lời **ghép vào đâu, sửa file nào, theo thứ tự nào**.

- [soul-core-user-profile.md](soul-core-user-profile.md) — hồ sơ người dùng (`USER.md`)
- [soul-core-encryption.md](soul-core-encryption.md) — vault mã hoá bằng mật khẩu

> **Trạng thái: ĐÃ TRIỂN KHAI (15/08/2026)** cho toàn bộ nhánh `USER.md` /
> `TOOLS.md` / `AGENTS.md` (§1–§9). Nhánh **vault** vẫn là thiết kế, trừ GĐ 0
> (quyền file 0600) đã làm — xem §10 để biết cái gì đã chạy và verify thế nào.

Mọi vị trí dưới đây đã đọc và kiểm chứng trên cây nguồn hiện tại (15/08/2026).

---

## 1. Bốn điều đã kiểm chứng — chốt trước vì thiết kế dựa vào chúng

### 1.1 JID mang sẵn loại chat, **nhất quán trên mọi kênh** ✅

Doc 1 §5.4 để ngỏ "cần kiểm chứng Feishu/QQ/WeChat". Đã kiểm:

| Kênh | Riêng tư | Nhóm | Nguồn |
|---|---|---|---|
| Telegram | `tg:<bot>:user:<id>` | `tg:<bot>:group:<id>` | [`telegram.rs:30`](../src/channels/telegram.rs) |
| Feishu | `feishu:user:<id>` | `feishu:group:<id>` | [`feishu/helpers.rs:167`](../src/channels/feishu/helpers.rs) |
| WeChat | `wx:user:<id>` | *(chỉ hỗ trợ p2p)* | [`wechat/helpers.rs:131`](../src/channels/wechat/helpers.rs) |
| App relay | `app:<cid>:user:<sender>` | — | [`channels/app.rs:109`](../src/channels/app.rs) |
| Web UI | `web:*` | — | UI của chính chủ |

Kết luận: **quy ước `:user:` / `:group:` dùng được**, không cần luồn `ChatType`
xuống engine. Nhưng vẫn phải **fail-closed**: jid không khớp mẫu nào → `PublicOnly`,
không phải `Full`.

### 1.2 Engine đã biết jid — không phải luồn tham số ✅

[`agent_pool/engine.rs:185`](../src/agent/agent_pool/engine.rs) đặt
`instance_id: jid.to_string()`. Nên `ZenEngine` tự suy được `ProfileScope` từ
`self.instance_id`, y hệt cách `instance_uses_workspace(_instance_id)`
([`zen_core/engine.rs:2330`](../src/zen_core/engine.rs)) đang làm cho `SENCLAW.md`.

Đó là chỗ đã dành sẵn cho đúng loại quyết định này — không phải thêm tham số
xuyên bốn tầng.

### 1.3 `GroupBinding` **không** mang `chat_type` — và không cần

[`types.rs:60`](../src/types.rs) có `jid`, `folder`, `channel`, `group_type`
(`chat`/`cowork`/`code`)… nhưng không có `ChatType`. Không sao: jid đã đủ (§1.1).
Ghi lại ở đây để người sau không đi tìm.

### 1.4 Soul watcher **nằm trong nhánh cognitive** — đừng bắt chước ⚠️

`spawn_soul_watcher` ở [`lib.rs:1368`](../src/lib.rs) nằm **bên trong**
`match memory::cognitive::init_daemon(…) { Some(sys) => … }`. Tức là:

> **Không cấu hình embedding provider ⇒ không có soul watcher.**

Với `SOUL.md` thì hợp lý (watcher tồn tại để re-ingest vào graph). Với `USER.md`
thì **sai hoàn toàn** — hồ sơ người dùng không liên quan gì tới cognitive. Watcher
của user profile phải nằm **ngoài** khối đó.

## 2. Sơ đồ ghép nối

```
                      ┌──────────────────────────────┐
                      │  src/user_profile/  (mới)    │
                      │  parse · tier · cache · save │
                      └──────────────┬───────────────┘
             ┌───────────────┬───────┴────────┬──────────────────┐
             ▼               ▼                ▼                  ▼
   zen_core/engine.rs   ui_server/       mcp/user_profile   lib.rs (boot)
   collect_first_turn   user_profile.rs   _server.rs        ensure + watcher
   _context(scope)      REST + WS         (qua senclaw-core)
             │               │                │
             ▼               ▼                ▼
      prompt lượt đầu    Web + Desktop    agent gọi tool
                          Settings

                      ┌──────────────────────────────┐
                      │  src/vault/  (mới, doc 2)    │
                      │  argon2id · envelope · AEAD  │
                      └──────────────┬───────────────┘
                                     │ DEK trong RAM của **daemon**
                      ┌──────────────┴───────────────┐
                      ▼                              ▼
            ui_server/vault.rs                mcp/vault_server.rs
            REST + capability token   ──HTTP loopback──►  (subprocess senclaw-core)
```

**Điểm khác biệt cốt lõi giữa hai nhánh**: `user_profile` đọc **file**, nên MCP
server đọc trực tiếp được. `vault` cần **DEK trong RAM của daemon**, mà MCP chạy ở
process khác — nên bắt buộc đi loopback. Xem §4.

## 3. Từng điểm nối

### 3.1 `src/user_profile/` — module mới

```
src/user_profile/
├── mod.rs      UserProfile, ProfileScope, load/save, cache (RwLock<Option<…>>)
├── parse.rs    front-matter YAML + directive (active/superseded) + tier
├── render.rs   render(scope) -> Option<String>  ← LUẬT TIER NẰM Ở ĐÂY
└── watcher.rs  mtime-poll 30s, phát WS event khi đổi
```

Khai báo trong [`src/lib.rs`](../src/lib.rs) cạnh các `pub mod` khác.

`render(scope)` là **cửa duy nhất** ra ngoài. Mọi đầu đọc (inject, MCP, REST,
bridge) đều gọi nó; không ai được tự parse file. Đây là điều kiện để luật tier ở
doc 1 §5.4 không bị đi vòng.

### 3.2 `src/config.rs` — hai dòng

Thêm vào `PathsConfig` ([`config.rs:109`](../src/config.rs)) và khối khởi tạo
([`config.rs:518`](../src/config.rs)):

```rust
pub user_profile_path: PathBuf,
pub tools_notes_path: PathBuf,   // TOOLS.md, xem doc 1 §3.5
```

```rust
user_profile_path: env_path("SENCLAW_USER_PROFILE_PATH", senclaw_home.join("USER.md")),
tools_notes_path:  env_path("SENCLAW_TOOLS_NOTES_PATH", senclaw_home.join("TOOLS.md")),
```

Chú ý dùng **`senclaw_home`** (`~/.senclaw`), **không** phải `senclaw_data`
(`~/senclaw`) — xem doc 1 §5.1.

### 3.3 `src/lib.rs` — boot sequence

Đặt **sau** khối "1c. Ensure main agent directory" ([`lib.rs:1396`](../src/lib.rs))
và **ngoài** khối cognitive (§1.4):

```rust
// ===== 1d. Soul core (hồ sơ người dùng) =====
user_profile::ensure_exists(&cfg);          // ghi template nếu chưa có
user_profile::load_into_cache(&cfg);        // nạp lần đầu
user_profile::spawn_watcher(
    cfg.paths.user_profile_path.clone(),
    std::time::Duration::from_secs(30),
);
```

`ensure_exists` viết template rỗng có bình luận, theo mẫu `ensure_agent_dirs`
([`group_manager/dirs.rs:9`](../src/gateway/group_manager/dirs.rs)) — file luôn tồn
tại thì Settings UI không phải xử lý trạng thái "chưa có".

### 3.4 `src/zen_core/engine.rs` — điểm inject

Ba sửa đổi, đều nhỏ:

**a)** Thêm enum cạnh `instance_uses_workspace` ([`engine.rs:2330`](../src/zen_core/engine.rs)):

```rust
pub enum ProfileScope { None, PublicOnly, Full }

fn profile_scope_for(instance_id: &str) -> ProfileScope {
    if instance_id.starts_with("web:") || instance_id.starts_with("app:") {
        ProfileScope::Full
    } else if instance_id.contains(":group:") {
        ProfileScope::PublicOnly
    } else if instance_id.contains(":user:") {
        ProfileScope::Full
    } else {
        ProfileScope::PublicOnly   // fail-closed: jid lạ chỉ được public
    }
}
```

**b)** `collect_first_turn_context` ([`engine.rs:2343`](../src/zen_core/engine.rs))
nhận thêm `scope` và chèn block `<user_profile>` cạnh dòng ngày tháng.

**c)** Chỗ gọi ([`engine.rs:1630`](../src/zen_core/engine.rs)) truyền
`Self::profile_scope_for(&self.instance_id)`.

Giữ nguyên tinh thần comment đã có ở `engine.rs:1617`: đây là `<system-reminder>`
ổn định, chỉ ở lượt đầu, **không phá prompt cache**.

**Đồng thời nên vá luôn** (doc 1 §6.2 mục 3): thêm trần `20_000` ký tự cho phần
`SENCLAW.md` trong chính hàm đó. Hiện đọc không giới hạn.

### 3.5 `src/gateway/ui_server/` — REST

Tạo `user_profile.rs` theo khuôn [`profile_files.rs`](../src/gateway/ui_server/profile_files.rs)
(cùng dạng: đọc/ghi file, state `Arc<UiState>`), rồi thêm route vào chuỗi
`build_router` ([`core.rs:246`](../src/gateway/ui_server/core.rs)):

```rust
.route("/api/user-profile", get(user_profile_get).put(user_profile_put))
.route("/api/user-profile/enabled", get(up_enabled_get).put(up_enabled_put))
.route("/api/tools-notes", get(tools_notes_get).put(tools_notes_put))
```

`build_router` là chuỗi `.route()` phẳng dùng chung `UiState` — thêm route không
cần đụng gì khác. Không đặt dưới `/api/space/apps/` (nơi `app_auth` gác theo app id).

**Công tắc bật/tắt** theo mẫu `get_memory_recall_enabled` /
`save_memory_recall_enabled` ([`group_manager/llm.rs:101`](../src/gateway/group_manager/llm.rs))
— lưu trong global config JSON, đọc theo request, không cần restart.

### 3.6 WebSocket — đẩy thay đổi lên UI

`gateway.broadcast_to_admins(&json)` ([`websocket_gateway/gateway.rs:218`](../src/gateway/websocket_gateway/gateway.rs))
là hàm sẵn có. Watcher (§3.3) và route `PUT` cùng phát:

```json
{ "type": "user-profile:changed" }
```

Chỉ báo "đã đổi", **không đính kèm nội dung** — client tự `GET` lại. Như vậy dữ
liệu private không đi qua kênh broadcast tới mọi admin socket.

### 3.7 Web UI + desktop

| | Nơi |
|---|---|
| Web | `web/src/components/settings/` — thêm `UserProfileSettings.tsx`, gắn vào Settings |
| Desktop | [`settings_screen.dart`](../desktop_app/lib/features/settings/settings_screen.dart) `_GeneralSection` (~dòng 1144) |

Form: tên / xưng hô / email / địa điểm / múi giờ / ngôn ngữ / nghề nghiệp, **mỗi
trường một công tắc tier** public/private, cộng danh sách directive chỉ-đọc kèm
nút xoá.

Desktop **bắt buộc thêm khoá i18n** cho cả `vi` và `en`
(`desktop_app/lib/core/i18n/*/settings_screen.dart`) — chuỗi thiếu sẽ hiện ra
nguyên văn tiếng Anh giữa giao diện tiếng Việt.

### 3.8 Bảng tổng file phải sửa

| File | Việc | Cỡ |
|---|---|---|
| `src/user_profile/*` | **Mới** — 4 file | ~400 dòng |
| `src/config.rs` | 2 trường + 2 dòng khởi tạo | 4 dòng |
| `src/lib.rs` | `pub mod` + khối boot 1d | ~10 dòng |
| `src/zen_core/engine.rs` | `ProfileScope` + chữ ký + chỗ gọi + trần 20k | ~40 dòng |
| `src/gateway/ui_server/user_profile.rs` | **Mới** — REST | ~150 dòng |
| `src/gateway/ui_server/core.rs` | 3 route | 3 dòng |
| `src/gateway/group_manager/llm.rs` | get/save enabled | ~15 dòng |
| `src/mcp/user_profile_server.rs` | **Mới** — xem §4 | ~200 dòng |
| `src/mcp/{mod,helper,core_server}.rs` | Đăng ký server | ~30 dòng |
| `web/src/components/settings/UserProfileSettings.tsx` | **Mới** | ~250 dòng |
| `desktop_app/…/settings_screen.dart` + i18n ×2 | Form + khoá dịch | ~300 dòng |

## 4. MCP — hai loại tool, **hai đường khác nhau**

Đây là phần dễ làm sai nhất, vì hai nhánh trông giống nhau nhưng ràng buộc ngược nhau.

### 4.1 `senclaw-profile` — đọc file trực tiếp là ĐƯỢC

MCP server chạy trong subprocess `senclaw core-server`. Nó **có** đủ thứ cần:

- đường dẫn `USER.md` (từ env),
- **`chat_jid`** — `core_mcp_config` đã truyền sẵn `p.chat_jid` cho nhiều server
  con ([`helper.rs:443`](../src/mcp/helper.rs)),

nên nó tự suy `ProfileScope` và tự render được. Không cần loopback.

Tool (đặt tên theo quy ước [CLAUDE.md](../CLAUDE.md): server `senclaw-profile`,
tiền tố `profile_`):

| Tool | Việc |
|---|---|
| `profile_get` | Đọc hồ sơ theo tier của phiên hiện tại |
| `profile_update` | Sửa trường / thêm directive, có thao tác `supersede` |

Các bước đăng ký, theo đúng khuôn đã có:

1. `from_env() -> Result<Option<Self>>` và **`vis = "pub"`** trên
   `#[rmcp::tool_router]` — yêu cầu bắt buộc của aggregator ([CLAUDE.md](../CLAUDE.md)).
2. Thêm nhánh `build_child!` trong [`core_server.rs:178`](../src/mcp/core_server.rs).
3. Thêm `"senclaw-profile"` vào `DEFAULT_CORE_SERVERS`
   ([`helper.rs:412`](../src/mcp/helper.rs)).
4. Thêm builder `user_profile_mcp_config(...)` và gọi trong mảng `parts` của
   `core_mcp_config` ([`helper.rs:442`](../src/mcp/helper.rs)) — env tự merge.
5. Thêm subcommand `user-profile-server` vào [`src/main.rs`](../src/main.rs) để
   debug riêng lẻ được.

### 4.2 `senclaw-vault` — **bắt buộc** loopback

DEK sống trong RAM của **daemon**. Subprocess MCP không chạm tới được — và đó là
tính chất tốt, giữ nguyên. Nên tool phải gọi ngược HTTP vào daemon, đúng mẫu
`space_app_*` ([`mcp/space_apps.rs`](../src/mcp/space_apps.rs)) đã làm vì cùng lý
do (state nằm ở process khác).

Khác một điểm **sống còn** so với `space_app_*`: route vault **không được** dùng
miễn trừ loopback. `space_app_*` dựa vào nó được vì `/start`, `/stop` không phải
bí mật. Route vault thì phải có capability token riêng, cấp cho subprocess qua env
lúc spawn — theo mẫu `SENCLAW_TOKEN_ACCESS_APP` ([`src/apps/token.rs`](../src/apps/token.rs)).
Chi tiết: [soul-core-encryption.md §6.5](soul-core-encryption.md).

### 4.3 Hai bẫy về roster tool

- **`groups.allowed_tools` là whitelist.** Nhóm nào đã cấu hình whitelist thì tool
  mới **không xuất hiện** cho tới khi được thêm tên vào
  ([`pool.rs:1284`](../src/agent/agent_pool/pool.rs)). Đây là nguyên nhân kinh điển
  của "đã thêm tool mà agent không thấy" — xem
  [docs/tool-skill-name-lookup.md](tool-skill-name-lookup.md).
- **Bảng `mcp_tool_aliases` giải trước exact-match.** Một alias trùng tên sẽ chiếm
  tool mới. Kiểm bằng `GET /api/tool-aliases` khi tool "không hành xử như tài liệu"
  ([docs/mcp-tool-alias.md](mcp-tool-alias.md)).

## 5. Thứ tự PR

Mỗi PR tự đứng được, xanh CI, không phụ thuộc PR sau:

| PR | Nội dung | Kiểm chứng |
|---|---|---|
| **1** | `src/user_profile/` + `config.rs` + boot ở `lib.rs`. Chưa ai đọc | `cargo test` — unit test parse/render/tier |
| **2** | Inject: `ProfileScope` + `collect_first_turn_context` + trần 20k | Test: group jid không rò trường private |
| **3** | REST + WS event | `curl` GET/PUT |
| **4** | Web Settings | Xác minh qua Browser pane |
| **5** | Desktop Settings + i18n ×2 | Chạy desktop |
| **6** | MCP `senclaw-profile` | `core_server.rs` test danh sách tool hợp nhất |
| **7** | `TOOLS.md` dùng lại chính hạ tầng đó | — |

Vault (doc 2) là một loạt **riêng**, chạy sau hoặc song song; giao nhau duy nhất
ở chỗ `USER.md` được mã hoá — và nhánh đó fail-closed (vault khoá ⇒ không inject),
nên PR 1–7 không chờ vault.

## 6. Test

Repo đặt unit test trong `#[cfg(test)]` cuối mỗi file nguồn; các luật xuyên-repo
thì đặt ở `tests/` (mẫu: [`tests/space_app_bind_loopback.rs`](../tests/space_app_bind_loopback.rs)
quét toàn bộ app tìm bind `0.0.0.0`).

Đề xuất `tests/user_profile_scope.rs` — **luật rò rỉ phải được test cưỡng chế**,
không để lệ thuộc vào review:

```rust
// Với mọi jid mẫu của mọi kênh, render(scope) cho group KHÔNG BAO GIỜ
// chứa giá trị của trường tier=private.
const GROUP_JIDS: &[&str] = &[
    "tg:123:group:456", "feishu:group:oc_abc", "cowork:team-1",
];
const PRIVATE_JIDS: &[&str] = &[
    "web:main", "app:c1:user:u1", "tg:123:user:456", "feishu:user:ou_abc",
];
```

Cộng thêm: jid lạ (`"gibberish"`) phải ra `PublicOnly`, không phải `Full`.

Unit test trong module: parse front-matter, `supersede` giữ đúng thứ tự và không
để hai `active` mâu thuẫn, cắt ngân sách theo **biên ký tự UTF-8** (tên tiếng Việt
có dấu — xem memory *UTF-8 preview slice panic*, dùng `truncate_on_char_boundary`).

## 7. Bẫy ghép nối

- **Đừng đặt watcher trong khối cognitive** (§1.4). Không có embedding provider là
  mất watcher.
- **Dùng `senclaw_home`, không phải `senclaw_data`** cho đường dẫn (doc 1 §5.1).
- **`render(scope)` là cửa duy nhất.** Đầu đọc nào tự parse file là một chỗ rò.
- **Fail-closed cho jid lạ** → `PublicOnly`.
- **WS chỉ báo "đã đổi", không kèm nội dung** (§3.6).
- **`allowed_tools` whitelist và `mcp_tool_aliases`** (§4.3).
- **Aggregator cần `vis = "pub"`** trên `#[rmcp::tool_router]`, và `from_env`
  phải trả `Result<Option<Self>>`.
- **Desktop cần khoá i18n cả `vi` lẫn `en`**, nếu không lòi chuỗi tiếng Anh.
- **Đừng thêm route dưới `/api/space/apps/`** — `app_auth::split_app_path` gác theo
  app id ở đó và sẽ hiểu segment mới thành tên app.
- **Vault: không kế thừa miễn trừ loopback** (§4.2). Đây là lỗi dễ mắc nhất vì mọi
  route khác trong repo đều được miễn.

---

## 8. Ghép `AGENTS.md` — luật vận hành người dùng sửa được

Mục này trả lời riêng cho dòng `AGENTS.md` ở [doc 1 §3](soul-core-user-profile.md).
Nghiên cứu điểm nối làm lộ ra một thứ khiến câu trả lời khác hẳn dự tính ban đầu.

### 8.1 Phát hiện: prompt vận hành hiện tại có **hai bản**, và bản "đẹp" là code chết

Repo có hai hệ thống prompt song song:

| | `src/agent/system_prompts.rs` + `system_prompt_builder.rs` | `src/zen_core/prompt.rs` + `engine.rs` |
|---|---|---|
| Nội dung | `AGENT_SUMMARY_PROMPT`, `TOOL_USAGE_POLICY_PROMPT`, `DOING_TASKS_PROMPT`, **`SPACE_NOTES`**, **`MEMORY_NOTES`**, … | `SYSTEM_PROMPT` (Safety / Communication / Tools / Real-time data) |
| Ai gọi | **Không ai** | [`engine.rs:1543`](../src/zen_core/engine.rs) |

Kiểm chứng: `grep` cho `format_system_prompt`, `build_agent_system_prompt`,
`MEMORY_NOTES`, `SPACE_NOTES`, `AGENT_SUMMARY_PROMPT`, `TOOL_USAGE_POLICY` trên
toàn `src/` — **không có kết quả nào ngoài chính hai file đó và test của chúng**.
`zen_core::prompt::auto_memory_prompt` cũng không có caller.

Hệ quả cụ thể, đáng sửa độc lập với Soul Core:

- **Hướng dẫn `space_event_*` chưa bao giờ tới model.** `SPACE_NOTES` mô tả 5 tool
  Space, luật "Never invent event IDs", từ khoá tiếng Việt "lịch"/"sự kiện" — tất cả
  nằm trong code chết. Test ở
  [`system_prompt_builder.rs:224`](../src/agent/system_prompt_builder.rs) khẳng định
  "Space section header missing" cho một hàm không ai gọi, nên nó xanh mà vô nghĩa.
- **Hướng dẫn `PersonaUpdate` / `CogRecall` chưa bao giờ tới model.** `MEMORY_NOTES`
  là chỗ duy nhất dặn agent dùng `PersonaUpdate` khi người dùng nói "từ giờ trở đi".
  Nó chết ⇒ tool `PersonaUpdate` ([`tools/persona_update.rs`](../src/tools/persona_update.rs))
  tồn tại nhưng không có gì dạy agent khi nào dùng.

> Đây là bug riêng, **không nên vá bằng `AGENTS.md`**. Mô tả tool của hệ thống thuộc
> về `SYSTEM_PROMPT` (hardcode); `AGENTS.md` là chỗ cho luật của *người dùng*. Trộn
> hai thứ nghĩa là người dùng xoá nhầm một dòng thì mất luôn hướng dẫn tool.

### 8.2 Chỗ nối đã có sẵn — `assemble_system_prompt`

Bù lại, phần khó nhất đã xong. [`engine.rs:1870`](../src/zen_core/engine.rs):

```rust
fn assemble_system_prompt(
    base: &str,                        // ZenCoreOptions.system_prompt, rỗng → SYSTEM_PROMPT
    working_dir: &str,                 // → "# System" (cwd, OS, shell, git)
    skills_reminder: Option<&str>,
    deferred_reminder: Option<&str>,
    plan_mode_reminder: Option<&str>,
    always_skills: Option<&str>,
    user_defaults: Option<&str>,       // ← khối tuỳ chọn cuối cùng, ĐÚNG MẪU CẦN COPY
) -> String
```

`user_defaults` đã là **một khối markdown tuỳ chọn do người dùng cấu hình, nối vào
cuối system prompt**, có setter `set_user_defaults()` ([`engine.rs:2225`](../src/zen_core/engine.rs))
và test riêng (`user_defaults_block_lands_in_system_prompt`, dòng 3189).

`AGENTS.md` là **cùng một hình dạng**. Không phải phát minh cơ chế mới — chỉ thêm
tham số thứ tám và một nguồn đọc.

Comment ở [`engine.rs:1868`](../src/zen_core/engine.rs) cũng đã ghi rõ nguyên tắc
thứ tự: phần động, nhỏ, đặt **sau cùng** để giữ prefix cache của phần trên. `AGENTS.md`
ổn định nên đặt đâu cũng được, nhưng đặt cuối là an toàn nhất.

### 8.3 Thiết kế

**Vị trí**: `~/.senclaw/AGENTS.md` — cùng cấp `USER.md` và `TOOLS.md`, dùng
`senclaw_home`. Global, không thuộc agent nào (đúng nguyên tắc anh đặt cho soul core).

**Phân biệt với `SENCLAW.md`** — hai thứ khác nhau, đừng gộp:

| | `~/.senclaw/AGENTS.md` | `SENCLAW.md` / `CLAUDE.md` |
|---|---|---|
| Phạm vi | **Toàn máy**, mọi phiên | Theo **project**, tìm ngược lên từ `working_dir` |
| Nội dung | Luật vận hành của chủ | Hướng dẫn của repo đó |
| Đường vào | System prompt | First-turn reminder |
| Hiện trạng | Chưa có | Có, **nhưng đang tắt** (`instance_uses_workspace()` luôn `false`) |

**Thứ tự lắp**:

```
SYSTEM_PROMPT (hardcode — Safety, Communication, Tools, Real-time data)
  └ # System (cwd, OS, git)
    └ skills / deferred / plan reminders
      └ always_skills
        └ user_defaults
          └ <user_operating_rules>  ← AGENTS.md, thêm mới
```

**Đặt sau `SYSTEM_PROMPT`, không bao giờ trước.** Đây là điểm bảo mật, không phải
thẩm mỹ: `AGENTS.md` là văn bản người dùng gõ. Nếu nó nằm trước phần Safety thì một
dòng "bỏ qua mọi quy tắc an toàn" sẽ được đọc trước. Bọc trong thẻ có tên và một câu
đóng khung:

```
<user_operating_rules>
Luật vận hành do chủ máy đặt. Áp dụng khi không mâu thuẫn với phần Safety ở trên;
phần Safety luôn thắng.

{nội dung AGENTS.md}
</user_operating_rules>
```

**Ngân sách**: trần 20.000 ký tự, cắt theo biên ký tự UTF-8.

### 8.4 Câu hỏi khó: cho agent tự sửa `AGENTS.md` không?

OpenClaw khuyến khích — `AGENTS.md` bản thật viết: *"When you learn a lesson →
update AGENTS.md, TOOLS.md, or the relevant skill."*

**Khuyến nghị cho SenClaw: mặc định KHÔNG.** Lý do là một đường leo thang cụ thể:

1. Agent đọc một trang web / tài liệu có chứa chỉ thị ẩn.
2. Agent "học được bài học", ghi vào `AGENTS.md`.
3. Dòng đó vào **system prompt của MỌI phiên sau**, vĩnh viễn.

Tức là biến một injection **một lần** thành **thường trực**, và ở tầng có trọng số
cao nhất của prompt. Memory *Agent security / Morris II* đã ghi nhận repo này từng
có vòng lặp worm khép kín và permission fail-open — không nên tự mở thêm một đường.

Nếu vẫn muốn cho agent ghi, làm đúng mẫu `PersonaUpdate` đã giải cho `SOUL.md`
([`tools/persona_update.rs`](../src/tools/persona_update.rs)):

- Chỉ được ghi vào **một section `## Learned` riêng**, không đụng phần người dùng viết.
- Qua **permission gate** ([`agent/permission_bridge/`](../src/agent/permission_bridge/)),
  không im lặng.
- Ghi atomic (tmp + rename) — `persona_update.rs:161` đã làm.
- Hiện section `## Learned` trong Settings UI với nút xoá, để người dùng thấy agent
  đã tự thêm gì.

### 8.5 Điểm nối cụ thể

| File | Việc | Cỡ |
|---|---|---|
| `src/config.rs` | `agents_rules_path` trong `PathsConfig` | 2 dòng |
| `src/user_profile/` | Dùng lại `load/cache/watcher` — cùng hình dạng, không cần module mới | ~40 dòng |
| `src/zen_core/engine.rs` | Tham số thứ 8 cho `assemble_system_prompt` + chỗ gọi 1543 + trần 20k | ~25 dòng |
| `src/agent/agent_pool/engine.rs` | Nạp vào `ZenCoreOptions` khi tạo engine (mẫu `set_user_defaults`) | ~10 dòng |
| `src/gateway/ui_server/user_profile.rs` | Thêm `GET`/`PUT /api/agents-rules` | ~40 dòng |
| Web + desktop Settings | Ô soạn markdown + xem section `## Learned` | ~150 dòng |

Rẻ hơn `USER.md` nhiều, vì không có khái niệm tier (luật vận hành không nhạy cảm
như email) và cơ chế đích đã tồn tại.

### 8.6 Thứ tự

Xếp **sau** PR 1–3 của `USER.md`: nó dùng lại đúng hạ tầng load/cache/watcher đó.
Làm trước thì phải viết hạ tầng hai lần.

Một việc **nên tách riêng và làm trước cả hai**: quyết định số phận
`src/agent/system_prompts.rs` + `system_prompt_builder.rs` (§8.1) — hoặc nối vào
`SYSTEM_PROMPT`, hoặc xoá. Để nguyên thì người sau sẽ lại sửa nhầm file chết, và
`SPACE_NOTES` vẫn không tới được model.

---

## 9. Toàn cảnh thư mục sau khi update

`✚` = thêm mới · `~` = đổi nội dung · không dấu = giữ nguyên.

### 9.1 `~/.senclaw/` — **senclaw_home**, nơi soul core sống

```
~/.senclaw/
│
├── ✚ USER.md                 ← SOUL CORE. Hồ sơ người dùng (doc 1)
├── ✚ TOOLS.md                ← Ghi chú môi trường: SSH host, camera, giọng TTS
├── ✚ AGENTS.md               ← Luật vận hành người dùng sửa được (§8)
├── ✚ vault.json         0600 ← Keyring: salt Argon2id + DEK đã bọc (doc 2)
│
├── ~ config.json        0600 ← 0644 hiện tại → PHẢI đổi 0600 (chứa 4 API key)
├──   oauth.json         0600
├──   api_token          0600
├── ~ senclaw.db         0600 ← 51 MB · hiện 0644
├── ~ senclaw_cognitive.db    ← 26 MB · hiện 0644
├── ~ project-config.json     ← 2,2 MB · hiện 0644
│
├──   hooks.json  ·  mcp.json  ·  marketplace.json
├──   disabled-skills.json  ·  disabled-subagents.json
├──   dispatch-state.json  ·  workflow-runs.json
├──   workspace-state-<agent>.json      × ~30 file
│
├──   apps/  ·  sandbox/  ·  local-models/  ·  models/  ·  ocr-models/
├──   llm_logs/  ·  logs/  ·  screenshots/  ·  plans/
└──   marketplace/  ·  mcp-dispatch/  ·  managed/
```

Bốn file soul core **nằm cùng một chỗ, ngang hàng nhau, ngoài mọi agent** — đúng
nguyên tắc "không phụ thuộc agents". Đối chiếu OpenClaw: cùng ý tưởng, họ để ở
`~/.openclaw/workspace-main/`.

Khi vault bật, `USER.md` và `TOOLS.md` thành `USER.md.enc` / `TOOLS.md.enc`;
`AGENTS.md` để nguyên (luật vận hành không phải bí mật).

### 9.2 `~/senclaw/` — **senclaw_data**, KHÔNG đổi gì

```
~/senclaw/
├── agents/                    ← 34 folder, mỗi cái một persona riêng
│   ├── main/
│   │   ├── SOUL.md            ← persona CỦA AGENT (giữ nguyên nghĩa cũ)
│   │   ├── MEMORY.md
│   │   ├── memory/            ← curated *.md + nhật ký ngày
│   │   └── .sema/sessions/
│   ├── coder/  ·  researcher/  ·  copywriter/  ·  ssh/  … (30 folder nữa)
│   └── schedule_<uuid>/       × 6
├── workspace/                 ← thư mục làm việc theo agent
├── virtual-agents/  ·  wiki/  ·  workflows/  ·  quicknotes/
└── workspace-templates/
```

**Không sửa một dòng nào ở cây này.** Đó là điều làm cho toàn bộ thiết kế an toàn:
`spawn_soul_watcher` chỉ quét `agents_dir` (`~/senclaw/agents`), nên nó không bao giờ
thấy `~/.senclaw/USER.md`. Va chạm với persona tự tan.

Con số 34 cũng là lập luận: hồ sơ người dùng mà đặt trong đây thì phải khai 34 lần.

### 9.3 `src/` — mã nguồn

```
src/
├── ✚ user_profile/               ← module mới, ~400 dòng
│   ├── mod.rs                       UserProfile · ProfileScope · load/save/cache
│   ├── parse.rs                     front-matter + directive + tier
│   ├── render.rs                    render(scope) — LUẬT TIER DUY NHẤT Ở ĐÂY
│   └── watcher.rs                   mtime-poll 30s → WS event
│
├── ✚ vault/                      ← module mới (doc 2)
│   ├── mod.rs                       trạng thái phiên: disabled/locked/unlocked
│   ├── kdf.rs                       Argon2id (m=64MiB, t=3, p=1)
│   ├── envelope.rs                  KEK bọc DEK · đổi mật khẩu không mã hoá lại
│   └── aead.rs                      AES-256-GCM + AAD ràng buộc record
│
├── ~ config.rs                   +4 dòng: user_profile / tools_notes / agents_rules / vault path
├── ~ lib.rs                      +10 dòng: khối boot "1d", NGOÀI nhánh cognitive
│
├── zen_core/
│   ├── ~ engine.rs               ProfileScope · collect_first_turn_context(scope)
│   │                             · assemble_system_prompt tham số thứ 8 · trần 20k
│   └──   prompt.rs               SYSTEM_PROMPT — prompt THẬT (§8.1)
│
├── agent/
│   ├── ⚠ system_prompts.rs       CODE CHẾT — xử lý riêng
│   └── ⚠ system_prompt_builder.rs CODE CHẾT — xử lý riêng
│
├── gateway/ui_server/
│   ├── ✚ user_profile.rs         REST: /api/user-profile · /tools-notes · /agents-rules
│   ├── ✚ vault.rs                REST + capability token (KHÔNG miễn trừ loopback)
│   └── ~ core.rs                 +5 dòng route
│
├── mcp/
│   ├── ✚ user_profile_server.rs  senclaw-profile · profile_get/update — đọc file thẳng
│   ├── ✚ vault_server.rs         senclaw-vault · vault_store/get/list/status/delete — QUA LOOPBACK
│   ├── ~ helper.rs               +2 builder, +2 tên vào DEFAULT_CORE_SERVERS
│   └── ~ core_server.rs        +2 nhánh build_child!
│
└── ~ main.rs                     +2 subcommand để debug riêng lẻ
```

### 9.4 Client

```
web/src/components/settings/
└── ✚ UserProfileSettings.tsx     form + công tắc tier từng trường + danh sách directive

desktop_app/lib/
├── ~ features/settings/settings_screen.dart      thêm vào _GeneralSection
└── ~ core/i18n/{vi,en}/settings_screen.dart      BẮT BUỘC cả hai ngôn ngữ

tests/
└── ✚ user_profile_scope.rs       cưỡng chế: group jid không bao giờ thấy tier private
```

### 9.5 Tổng kết thay đổi

| | Thêm mới | Sửa | Không đụng |
|---|---:|---:|---|
| File dữ liệu (`~/.senclaw`) | 4 | 6 (chỉ quyền) | phần còn lại |
| File dữ liệu (`~/senclaw`) | 0 | 0 | **toàn bộ** |
| Module Rust | 2 thư mục, 12 file | 8 file | — |
| Client | 2 file | 3 file | — |

---

## 10. Đã triển khai — 15/08/2026

Toàn bộ §1–§9 đã code và verify. Ghi lại ở đây cái gì thật sự chạy, và những chỗ
thực tế khác thiết kế.

### 10.1 File đã thêm / sửa

| File | |
|---|---|
| [`src/user_profile/{mod,parse,render}.rs`](../src/user_profile/) | ✚ Module lõi. `render.rs` là **cửa duy nhất** áp luật tier |
| [`src/util/file_perms.rs`](../src/util/file_perms.rs) | ✚ `restrict()` / `restrict_sqlite()` — 0600 |
| [`src/mcp/user_profile_server.rs`](../src/mcp/user_profile_server.rs) | ✚ `profile_get` / `profile_update` |
| [`src/gateway/ui_server/user_profile.rs`](../src/gateway/ui_server/user_profile.rs) | ✚ REST 3 route + WS notify |
| [`tests/user_profile_scope.rs`](../tests/user_profile_scope.rs) | ✚ 8 test cưỡng chế luật rò rỉ |
| [`src/config.rs`](../src/config.rs) | ~ 3 đường dẫn mới dưới `senclaw_home` |
| [`src/lib.rs`](../src/lib.rs) | ~ Khối boot `0b` (chmod sweep) + `1d` (Soul Core), `RealUiApi.broadcast_event` |
| [`src/zen_core/engine.rs`](../src/zen_core/engine.rs) | ~ `collect_first_turn_context(…, instance_id)`, tham số thứ 8 cho `assemble_system_prompt`, trần 20k |
| [`src/db/mod.rs`](../src/db/mod.rs), `group_manager/config.rs`, `mcp/config.rs` | ~ chmod 0600 khi ghi |
| `web/src/components/settings/UserProfileSettings.tsx` | ✚ 3 tab |
| `desktop_app/…/user_profile_section.dart` + i18n `vi` | ✚ Section + 24 khoá dịch |

### 10.2 Khác thiết kế

- **Cache khoá theo path**, không phải `Option<UserProfile>` trần như §3.1 phác.
  Đường dẫn không cố định (`SENCLAW_USER_PROFILE_PATH`, test dùng temp dir), nên
  cache không khoá sẽ trả hồ sơ của path này cho request về path khác.
- **`UiApi::broadcast_event`** phải thêm mới — ui_server trước giờ không có đường
  đẩy WS nào. `RealUiApi` nay giữ thêm `ws_gateway`.
- **`ProfileScope` sống trong `user_profile::render`**, không phải trong
  `zen_core::engine` như §3.4 phác. Cùng module với luật nó phục vụ.
- **`TOOLS.md` đi qua `collect_first_turn_context`** cùng `USER.md`, không phải
  qua system prompt: nó phụ thuộc ngữ cảnh chat, còn system prompt thì không.
- Route `/api/user-profile/enabled` **chưa làm**. File rỗng ⇒ không inject gì,
  nên công tắc là dư ở giai đoạn này.

### 10.3 Hai lỗi thật do verify trên daemon sống bắt được

1. **Template ship kèm một directive mẫu** ⇒ mọi cài mới sẽ inject
   "Prefer trả lời ngắn gọn" vào mọi prompt như thể người dùng đã đặt. Bắt bởi
   test `empty_profile_renders_nothing`. Sửa: ví dụ nằm trong HTML comment, và
   parser học cách bỏ qua comment **nhiều dòng** (trước đó chỉ hiểu comment một
   dòng, nên bullet trong comment vẫn bị parse).
2. **Prose của template bị parse thành `notes`**, rồi `serialize` ghi header cứng
   của nó *cộng* notes ⇒ dòng intro nhân đôi, và mọc thêm một bản sau **mỗi lần
   lưu**. Chỉ lộ ra ở lần lưu thứ hai. Sửa: phần giải thích nằm trong comment.
   Chặn hồi quy: `saving_twice_is_idempotent`.

### 10.3b Lỗi thứ ba — tool có trong roster nhưng model không biết dùng

Báo cáo từ người dùng: gõ "tôi tên là Benji, ghi nhớ điều này" → agent gọi một
tool, trả lời "Tôi đã ghi nhớ tên của bạn", nhưng `USER.md` vẫn **trống**.

Chẩn đoán: `senclaw-profile` **có** trong roster (kiểm chứng: `core-server`
trả 78 tool, cả `profile_get` lẫn `profile_update`). Vấn đề là
`zen_core/prompt.rs::SYSTEM_PROMPT` — prompt **duy nhất** thật sự tới model —
không nhắc `profile_update` một lần nào. Agent bốc tool nhớ nào nó biết, và không
có gì ghi vào hồ sơ.

Đây **đúng cùng một lớp lỗi với §8.1**: tool tồn tại nhưng không có gì dạy model
khi nào dùng. Đăng ký tool vào roster là điều kiện cần, không phải điều kiện đủ.

Sửa hai chỗ:

1. **`SYSTEM_PROMPT` thêm mục "Facts about the user"** — phân biệt rõ: sự thật bền
   về con người + sở thích cố định → `profile_update`; quan sát vụn và ghi chú dự
   án → memory tool. Kèm luật `supersedes`. Có test
   `system_prompt_teaches_the_profile_tools` chặn hồi quy.
2. **Hồ sơ trống nay vẫn phát một dòng nhắc** trong chat riêng tư
   (`block_for_instance`). Trước đó hồ sơ trống ⇒ inject `None` ⇒ model không có
   bằng chứng nào là hệ thống hồ sơ tồn tại. Nhóm chat vẫn im lặng — nhóm không
   ghi được, và nhắc chỉ tổ khiến model đi hỏi người lạ.

### 10.4 Verify

`cargo test` **2008 lib + 8 integration**, `tsc --noEmit` sạch, `flutter analyze`
sạch. Ngoài ra chạy một daemon thật ở cổng 18991 với thư mục tạm (không đụng
daemon 18788 của máy):

- Ba file được tạo lúc boot, quyền `-rw-------`.
- `PUT /api/user-profile` → preview riêng tư có email, preview nhóm chat **không**.
- Lưu hai lần liên tiếp cho ra file y hệt nhau.
- `core-server` với danh sách server đầy đủ: **78 tool**, có cả `profile_get`
  và `profile_update`.
- Gọi thẳng `profile_update {field:"name", value:"Benji"}` → `USER.md` có
  `name: Benji`. Đọc lại từ chat riêng tư thấy tên; từ nhóm chat cũng thấy (tên là
  public); **ghi từ nhóm chat bị từ chối**.
- Web UI mở được ở Settings → Hồ sơ người dùng, không lỗi console.

### 10.5 Còn lại

- **Vault** ([soul-core-encryption.md](soul-core-encryption.md)) — GĐ 1–6.
- **`src/agent/system_prompts.rs` + `system_prompt_builder.rs` là code chết**
  (§8.1). Chưa xử lý; `SPACE_NOTES` và `MEMORY_NOTES` vẫn không tới được model.
- `DREAMS.md`, `IDENTITY.md`, heartbeat gộp — hạng 5–9 ở
  [doc 1 §6.2](soul-core-user-profile.md).
