# Soul Core — hồ sơ người dùng (`USER.md`)

Nghiên cứu thiết kế cho tính năng "soul core": nơi lưu **thông tin của con người
sở hữu agent** (tên, địa điểm, email, cách xưng hô, thói quen làm việc) và cơ chế
**inject vào prompt khi agent cần biết về chủ**.

> **Trạng thái: ĐÃ TRIỂN KHAI (15/08/2026).** `USER.md`, `TOOLS.md`, `AGENTS.md`
> đã chạy end-to-end — module [`src/user_profile/`](../src/user_profile/), inject
> ở lượt đầu, REST + WS, MCP server `senclaw-profile`, Web UI và desktop.
> Phần **vault mã hoá** ([soul-core-encryption.md](soul-core-encryption.md)) vẫn
> là nghiên cứu, trừ giai đoạn 0 (quyền file 0600) đã làm.
>
> Tài liệu này giữ nguyên phần nghiên cứu và lập luận thiết kế; bản đồ điểm nối
> thực tế nằm ở [soul-core-integration.md](soul-core-integration.md).

---

## 1. Hiện trạng: `SOUL.md` của SenClaw là persona của **agent**, không phải hồ sơ **người dùng**

SenClaw đã có `SOUL.md` và nó đã được nối dây khá sâu — nhưng nó trả lời câu hỏi
*"agent là ai"*, không phải *"chủ của agent là ai"*.

| Thành phần | Vị trí | Vai trò |
|---|---|---|
| File | `~/senclaw/agents/<folder>/SOUL.md` | **Một file cho mỗi agent profile**, không phải một file chung |
| Nguồn sự thật song song | Cột `agents.core_prompt` (SQLite) | DB và file được ghi đồng thời — `write_soul_md` gọi từ [`agent_manager.rs:55`](../src/gateway/agent_manager.rs) |
| Template mặc định | [`group_manager/soul.rs`](../src/gateway/group_manager/soul.rs) `default_soul_md` | H1 tên + `## Identity` / `## Guidelines` / `## Memory Management` / `## Working Directory` |
| Đọc / ghi | [`group_manager/dirs.rs`](../src/gateway/group_manager/dirs.rs) | `read_soul_md`, `write_soul_md`, `ensure_agent_dirs` |
| REST | [`ui_server/profile_files.rs`](../src/gateway/ui_server/profile_files.rs) | `GET`/`PUT` soul + memory theo folder |
| Web UI | [`AgentSettings.tsx:440`](../web/src/components/settings/AgentSettings.tsx) | Ô "Persona (SOUL.md)" |
| Nạp vào cognitive graph | [`memory/cognitive/soul_ingest.rs`](../src/memory/cognitive/soul_ingest.rs) | Tách theo `## ` → node gắn `NodeSet::Persona(folder, "soul")` + `soul:<section-slug>` |
| Watcher | [`lib.rs:1368`](../src/lib.rs) `spawn_soul_watcher` | Sửa file bằng vim / `git pull` → tự re-ingest |
| Agent tự sửa | [`tools/persona_update.rs`](../src/tools/persona_update.rs) | Tool `PersonaUpdate` — patch theo section, ghi atomic (tmp + rename) |
| Bộ sửa có cấu trúc | [`memory/cognitive/soul_editor.rs`](../src/memory/cognitive/soul_editor.rs) | `SoulPatch` / `apply_patch` |

Persona đến được model **qua cognitive recall**, không phải qua system prompt:
`system_prompts.rs:195` nói thẳng — *"Persona facts surface in CogRecall under
`Persona(folder, "soul")` scope. Pre-retrieval already injects these into your
context."* Engine chat chính ([`agent_pool/engine.rs:184`](../src/agent/agent_pool/engine.rs))
dựng `ZenCoreOptions` **không** set `system_prompt` từ `core_prompt`.

## 2. Khoảng trống: agent không biết gì về chủ

Quét toàn bộ `src/` và `web/src/` cho `user_name` / `owner_name` / `user_email` /
`user_profile` / `display_name` (ngữ cảnh người dùng) → **không có gì**. Mọi
`display_name` tìm được đều thuộc plugin registry hoặc provider list.

Hệ quả cụ thể:

- Agent không biết gọi chủ là gì → xưng hô chung chung mỗi phiên.
- "Đặt lịch 9h sáng mai" → không có múi giờ / địa điểm của chủ để quy chiếu.
- "Gửi báo cáo cho tôi" → không có email đích, phải hỏi lại mỗi lần.
- Không có nơi đặt sở thích ổn định ("trả lời tiếng Việt", "code không thêm comment thừa").

Toàn bộ thứ đó hiện phải sống trong `MEMORY.md` của từng agent hoặc trong
cognitive graph — nghĩa là **lặp lại cho mỗi profile** và trộn lẫn với ghi chú
sự vụ.

## 3. OpenClaw làm gì

OpenClaw **không có "một file soul"** — nó tách một workspace thành **bảy file**,
mỗi file trả lời đúng một câu hỏi, cộng một thư mục memory. Đây là điểm quan
trọng nhất của cả phần nghiên cứu: câu hỏi "để thông tin người dùng vào đâu" đã
được họ trả lời bằng cách **tách nhỏ ra**, không phải nhét chung.

Đo trên bản cài thật (`~/.openclaw/workspace-main/`, 15/08):

| File | Cỡ thật | Trả lời | Inject |
|---|---:|---|---|
| `AGENTS.md` | 7,9 KB | **Vận hành** — luật, ưu tiên, quy tắc dùng memory | Mỗi phiên |
| `SOUL.md` | 1,7 KB | Agent **là ai** — tính cách, giọng, ranh giới | Mỗi phiên |
| `IDENTITY.md` | 636 B | Danh thiếp agent — `name`, `creature`, `vibe`, `emoji`, `avatar` | Mỗi phiên |
| `USER.md` | 489 B | **Con người** — hồ sơ + sở thích ổn định | Mỗi phiên, **ngân sách riêng**, *optional* |
| `TOOLS.md` | 860 B | **Ghi chú môi trường cục bộ** — tên camera, SSH host, giọng TTS | Mỗi phiên |
| `HEARTBEAT.md` | 168 B | Checklist cho nhịp poll định kỳ | Khi heartbeat |
| `BOOTSTRAP.md` | 1,5 KB | Nghi thức lần đầu — **tự xoá sau khi xong** | Chỉ khi tồn tại |
| `MEMORY.md` | — | Memory dài hạn đã chắt lọc | **Chỉ main session** |
| `memory/YYYY-MM-DD.md` | — | Nhật ký thô theo ngày | Hôm nay + hôm qua, khi `/new` |
| `DREAMS.md` | — | Kết quả hợp nhất memory tự động ("dreaming") | Để người đọc |

Vị trí: `~/.openclaw/workspace-main/<FILE>` — **cấp workspace, không phải cấp
agent**. Đúng nguyên tắc "soul core không nằm trong agents" mà anh đặt ra.

### 3.0 Thứ tự đọc lúc khởi động — quy định thẳng trong `AGENTS.md`

Bản thật ghi rõ, và đây là thứ SenClaw đang không có tương đương:

```
1. Read SOUL.md     — this is who you are
2. Read USER.md     — this is who you're helping
3. Read memory/YYYY-MM-DD.md (today + yesterday) for recent context
4. If in MAIN SESSION (direct chat with your human): Also read MEMORY.md

Don't ask permission. Just do it.
```

Hai điều đáng lấy:

- **Thứ tự có chủ ý**: bản thân agent (`SOUL`) → người dùng (`USER`) → ngữ cảnh
  gần (`daily`) → tri thức bền (`MEMORY`). Từ tổng quát đến cụ thể.
- **Bước 4 có điều kiện.** `MEMORY.md` chỉ nạp ở "main session". Trong
  `openclaw.json` có hẳn khoá `session.dmScope = main` để định nghĩa khái niệm
  đó. Đây là **cơ chế**, không phải lời khuyên — chính là thứ §5.4 của tài liệu
  này cần tương đương ở SenClaw.

`AGENTS.md` nói lý do bằng đúng một dòng: *"This is for **security** — contains
personal context that shouldn't leak to strangers."*

### 3.1 Định dạng `USER.md`: directive có vòng đời

Không phải form key-value. Mỗi mục là **một dòng metadata + một câu mệnh lệnh**:

```md
<!-- observed: YYYY-MM-DD | status: active -->

- Prefer concise progress updates during implementation work.
```

Luật:

- Bắt đầu bằng động từ mệnh lệnh: `Always` / `Never` / `Prefer`.
- **Một hành vi cho một directive** — không gộp.
- `status` chỉ nhận `active` hoặc `superseded`.
- Khi người dùng đổi ý: **đánh dấu mục cũ `superseded` và viết lại mục active tại
  chỗ**, đặt ngay cạnh nhau. Tuyệt đối không append một directive active mâu thuẫn.

Điều cuối là điểm thiết kế quan trọng nhất, và OpenClaw nêu rõ failure mode nó
chữa: khi hai directive mâu thuẫn cùng `active`, *"systems often select an
originally stated preference after the user has changed it"* — model bốc trúng
cái cũ. Ghi đè tại chỗ khiến trạng thái hiện hành không thể mơ hồ.

### 3.2 Ngân sách và quyền riêng tư

- `bootstrapMaxChars` mặc định **20.000** / file, `bootstrapTotalMaxChars`
  **60.000** tổng — nhưng `USER.md` có **ngân sách riêng 4.000 ký tự**, nhỏ hơn hẳn.
- `USER.md` **không bao giờ vào shared/group context** — chỉ phiên riêng tư.
  `MEMORY.md` cũng vậy: *"Only load MEMORY.md in the main, private session"*.
- Vắng file → bỏ qua im lặng, không lỗi.

### 3.3 Cái gì **không** thuộc `USER.md`

Quan sát vụn → daily memory (`memory/YYYY-MM-DD.md`). Hành động đúng giờ → scheduled
task. Hành động tương lai có điều kiện → standing intent. Sự thật bền nhưng không
phải hồ sơ (quyết định kỹ thuật, tóm tắt) → `MEMORY.md`.

### 3.4 Đối chiếu bản cài thật — docs và sản phẩm **không giống nhau**

Máy dev có sẵn một bản OpenClaw (`~/.openclaw/workspace/`), nên kiểm chứng được
trực tiếp thay vì tin docs. Kết quả đáng chú ý: **file thật đơn giản hơn hẳn tài
liệu tham chiếu.**

Template ship kèm sản phẩm (`~/.openclaw/workspace/USER.md`) — là **danh sách
trường**, không phải directive:

```md
# USER.md - User Profile

- Name:
- Preferred address:
- Pronouns (optional):
- Timezone (optional):
- Notes:
```

Bản đã bootstrap và đang dùng (`~/.openclaw/workspace-main/USER.md`) thêm một vùng
tự do:

```md
# USER.md - About Your Human

_Learn about the person you're helping. Update this as you go._

- **Name:** Benji
- **What to call them:** Benji
- **Pronouns:** _(optional)_
- **Timezone:**
- **Notes:**

## Context

_(What do they care about? What projects are they working on? …)_
```

Ba điều rút ra:

1. **Định dạng directive có metadata `<!-- observed: … | status: … -->` (§3.1) là
   pattern nâng cao trong tài liệu tham chiếu, không phải cái đang ship.** Thực tế
   người dùng nhận được một form trường đơn giản. Điều này **xác nhận thiết kế lai
   ở §5.2** — front-matter cho trường + directive cho sở thích chính là hợp của
   hai bản này, không phải sáng tạo thừa.
2. **Không có `email`, không có địa chỉ.** OpenClaw cố ý chỉ thu Name / xưng hô /
   pronouns / timezone / notes. Yêu cầu của anh có thêm email + địa điểm — tức là
   đi xa hơn OpenClaw đúng ở hai trường **nhạy cảm nhất**, và đó là lý do §5.4 cần
   phân tầng.
3. Chính file mẫu tự cài một guardrail đáng học, ngay dòng cuối: *"you're learning
   about a person, not building a dossier. Respect the difference."*

`IDENTITY.md` bản đã bootstrap — năm trường, kèm gợi ý viết cho chính agent tự điền:

```md
# IDENTITY.md - Who Am I?
- **Name:**     _(pick something you like)_
- **Creature:** _(AI? robot? familiar? ghost in the machine? something weirder?)_
- **Vibe:**     _(sharp? warm? chaotic? calm?)_
- **Emoji:**    _(your signature)_
- **Avatar:**   _(workspace-relative path, http(s) URL, or data URI)_
```

### 3.5 `TOOLS.md` — tách "skill dùng chung" khỏi "môi trường của tôi"

Đây là file SenClaw **không có tương đương**, và ý tưởng đằng sau nó đáng lấy
nguyên. Trích chính file:

> *"Skills define **how** tools work. This file is for **your** specifics."*
> *"Skills are shared. Your setup is yours. Keeping them apart means you can
> update skills without losing your notes, and share skills without leaking your
> infrastructure."*

Nội dung ví dụ: tên camera và vị trí, SSH host + alias, giọng TTS ưa dùng, tên
loa/phòng, nickname thiết bị.

Với SenClaw điều này giải một vấn đề có thật: skill `ssh-manager` (xem memory
*SSH Manager app*) hay các app IoT cần **dữ liệu môi trường riêng của máy** — hiện
không có chỗ chuẩn để đặt, nên nó hoặc bị nhét vào `SOUL.md` (rồi bị ingest nhầm
thành persona) hoặc vào config của từng app (rồi mỗi app một bản).

Nên coi `TOOLS.md` là **phần thứ hai của soul core**: cùng cấp global, cùng cách
đọc, nhưng tier mặc định là `private` vì nó chứa IP nội bộ và tên host.

### 3.6 `HEARTBEAT.md` + "dreaming" — hai cơ chế bảo trì memory tự động

**Heartbeat**: một prompt poll định kỳ; nếu không có gì cần làm thì agent trả
`HEARTBEAT_OK` và im lặng. `HEARTBEAT.md` là checklist agent **tự sửa được**. File
mẫu thật chỉ có comment, kèm dòng: *"Keep this file empty (or with only comments)
to skip heartbeat API calls"* — tức mặc định **không tốn gì**.

`AGENTS.md` có sẵn bảng quyết định heartbeat-hay-cron, đáng đối chiếu với
scheduler của SenClaw (5 context mode: `isolated` / `group` / `notify` / `script` /
`script-agent`):

| Dùng heartbeat khi | Dùng cron khi |
|---|---|
| Gộp nhiều việc kiểm tra vào một lượt (mail + lịch + thông báo) | Cần đúng giờ ("9:00 sáng thứ Hai") |
| Cần ngữ cảnh hội thoại gần đây | Cần cô lập khỏi lịch sử main session |
| Lệch giờ chút không sao (~30 phút) | Cần model / mức thinking khác |
| Muốn giảm số lần gọi API | Nhắc một lần ("20 phút nữa") |

SenClaw có `cron`/`interval`/`once` nhưng **không có nhịp heartbeat gộp** — mỗi
việc định kỳ là một task riêng. Với người dùng có 10 việc kiểm tra thì đó là 10
lần gọi LLM thay vì 1.

**Dreaming**: hợp nhất memory tự động, **mặc định BẬT**, tắt bằng
`plugins.entries.memory-core.config.dreaming.enabled: false`. Nó tự đẩy thứ đáng
giữ từ recall ngắn hạn lên `MEMORY.md`, và ghi lại quá trình vào `DREAMS.md` **để
con người đọc và kiểm tra**. Thêm nữa, **trước mỗi lần compaction** có một lượt
chạy im lặng nhắc agent lưu ngữ cảnh quan trọng (`compaction.memoryFlush`).

Đối chiếu SenClaw: đã có auto-reflection theo session window
([`agent_pool/reflection.rs`](../src/agent/agent_pool/reflection.rs)) và curated
memory — nên cơ chế tương đương phần lớn đã có. Thứ **thiếu** là `DREAMS.md`: một
file người đọc được, ghi lại "hệ thống đã tự quyết định nhớ/quên cái gì". Không có
nó thì memory tự động là hộp đen.

Chi tiết đáng lấy: `memory/imports/` được giữ **tách khỏi** `MEMORY.md` bootstrap —
tìm kiếm được nhưng không tự nhập vào ngữ cảnh mỗi phiên. Đây đúng là thứ cần khi
người dùng import một đống dữ liệu cũ mà không muốn nó chiếm ngân sách prompt.

### 3.7 Ngân sách bootstrap — con số cụ thể

| Khoá | Mặc định | Ý nghĩa |
|---|---:|---|
| `agents.defaults.bootstrapMaxChars` | **20.000** | Trần cho **từng** file |
| `agents.defaults.bootstrapTotalMaxChars` | **60.000** | Trần **tổng** mọi file bootstrap |
| ngân sách riêng của `USER.md` | **~4.000** | Nhỏ hơn hẳn, cố ý |
| `agents.defaults.startupContext` | — | Ngoại lệ: lượt đầu sau reset được chèn thêm daily memory gần đây |

Cách cắt: cắt từng file về trần riêng **trước**, rồi mới cắt tổng. File quá ngân
sách vẫn **nguyên vẹn trên đĩa** — chỉ phần inject bị cắt.

Một chi tiết tinh tế: khi model có sẵn **tool memory**, OpenClaw **không paste**
`MEMORY.md` vào prompt nữa mà chỉ đưa một "memory pointer" và để agent tự gọi tool
khi cần. Tức là ngân sách được đánh đổi lấy một lần gọi tool — đúng hướng SenClaw
đã đi với `memory_recall`/`CogRecall`.

### 3.8 Luật group chat — viết thẳng trong `AGENTS.md`

Liên quan trực tiếp tới §5.4 của tài liệu này. Trích nguyên văn hai câu:

> *"You have access to your human's stuff. That doesn't mean you **share** their
> stuff. In groups, you're a participant — not their voice, not their proxy."*

> *"Private things stay private. Period."*

Cộng thêm luật im lặng có cấu trúc: trả lời khi được nhắc tên / có giá trị thật /
đính chính thông tin sai; **im lặng** khi là chuyện phiếm giữa người với người,
khi đã có người trả lời, khi câu trả lời chỉ là "yeah"/"nice". Và *"avoid the
triple-tap"* — không trả lời cùng một tin nhiều lần.

Với SenClaw đây là vùng đã có một phần: `trigger_checker.rs` phân biệt
`ChatType::Private` và yêu cầu mention trong nhóm. Thứ chưa có là **luật nội dung**
— hiện không có gì nói với model rằng dữ liệu của chủ không được phát ra nhóm.

## 4. Va chạm tên: tại sao **không** nên đặt hồ sơ người dùng vào `soul.md`

Yêu cầu ban đầu là lưu vào "file soul.md chính của senclaw". Có một vấn đề thật
sự với cách đặt tên đó, xin nêu ngắn rồi vẫn thiết kế tiếp:

`SOUL.md` trong SenClaw **đã bị chiếm** với ngữ nghĩa "persona của agent", và nó
không chỉ là một cái tên — nó kéo theo bốn cơ chế đang chạy:

1. `spawn_soul_watcher` theo dõi mọi `SOUL.md` dưới `agents_dir` và re-ingest khi đổi.
2. `ingest_all_souls` gắn mọi section vào `NodeSet::Persona(folder, …)` — dữ liệu
   người dùng sẽ bị gắn nhãn *persona của agent* trong cognitive graph.
3. Tool `PersonaUpdate` cho agent quyền patch file đó, mô tả là *"Update your
   persona"* — agent sẽ sửa hồ sơ người dùng khi được yêu cầu đổi tính cách.
4. `write_soul_md` đồng bộ hai chiều với cột `agents.core_prompt`; ghi hồ sơ người
   dùng vào đây sẽ đẩy nó vào system persona của agent.

Nhét hồ sơ người dùng vào cùng file sẽ khiến "đổi tính cách agent" và "sửa email
của chủ" đi chung một đường ghi. **Đề xuất: dùng `USER.md`** — theo đúng OpenClaw,
không va chạm gì, và giữ `SOUL.md` nguyên nghĩa hiện tại. Phần còn lại của tài
liệu dùng tên `USER.md`; nếu vẫn muốn tên `soul.md` thì mọi thiết kế bên dưới giữ
nguyên, chỉ cần đổi hằng số đường dẫn và **loại thư mục chứa nó ra khỏi
`spawn_soul_watcher`**.

## 5. Thiết kế đề xuất

### 5.1 Vị trí: **global**, nằm ngoài `agents/`, không phụ thuộc agent

SenClaw có **hai** cây thư mục, dễ nhầm ([`config.rs:486`](../src/config.rs)):

```rust
let senclaw_home = h.join(".senclaw");   // db, config.json, oauth.json, api_token
let senclaw_data = h.join("senclaw");    // agents/, workspace/
```

Nên đường dẫn thật là:

```
~/.senclaw/USER.md                  ← soul core, tài nguyên chung   (senclaw_home)
~/senclaw/agents/<folder>/SOUL.md   ← persona, per-agent như cũ     (senclaw_data)
```

Hai thứ nằm ở **hai cây khác nhau**, không chỉ khác thư mục — mức tách còn mạnh
hơn dự tính ban đầu.

> ⚠️ Docstring của [`soul_ingest.rs:3`](../src/memory/cognitive/soul_ingest.rs)
> ghi nhầm là `~/.senclaw/agents/<folder>/SOUL.md`. Thực tế trên máy dev là
> `~/senclaw/agents/`. Nên sửa comment đó luôn khi đụng vào.

Kiểm chứng trên máy hiện tại (15/08): `~/senclaw/agents/` có **34 folder** — `main`,
`coder`, `researcher`, `copywriter`, `ssh`, các `schedule_<uuid>`… Mỗi folder một
`SOUL.md` riêng. Nếu hồ sơ người dùng đi theo agent thì người dùng phải khai tên
và email **34 lần** và giữ chúng đồng bộ. Đó là lập luận thực nghiệm cho việc để
soul core ở cấp global.

Soul core **không nằm trong `agents/`, không thuộc về agent nào, và mọi bên đều
đọc được**. Tên / địa điểm / email là thuộc tính của **con người** — nó không đổi
khi người dùng chuyển profile, và không việc gì phải khai lại n lần. `SOUL.md`
giữ nguyên per-agent vì persona *phải* khác nhau giữa các profile.

Một phụ phẩm quan trọng của việc để nó **ngoài** `agents_dir`: `spawn_soul_watcher`
([`lib.rs:1368`](../src/lib.rs)) chỉ quét dưới `agents_dir`, nên `USER.md` **tự
động không bị** watcher persona đụng tới, không bị `ingest_all_souls` gắn nhãn
`NodeSet::Persona`, và không bị tool `PersonaUpdate` ghi đè. Toàn bộ va chạm nêu
ở §4 tự tan chỉ nhờ chọn đúng chỗ đặt file.

Đường dẫn thêm vào `PathsConfig` ([`config.rs:109`](../src/config.rs)) theo đúng
mẫu có sẵn:

```rust
user_profile_path: env_path("SENCLAW_USER_PROFILE_PATH", senclaw_data.join("USER.md")),
```

Vì là tài nguyên chung được đọc bởi nhiều bên, nó cần một **module riêng có
cache**, không phải đọc đĩa lại ở mỗi lượt:

```
src/user_profile/
├── mod.rs      — UserProfile { frontmatter, directives }, load/save, cache RwLock
├── parse.rs    — front-matter + directive (active/superseded)
└── render.rs   — dựng block <user_profile> theo tier + ngân sách
```

Kèm watcher **riêng** (không dùng chung với soul watcher) để người dùng sửa
`USER.md` bằng vim / đồng bộ qua git thì cache tự nạp lại, và phát một event WS
để UI đang mở cập nhật theo.

### 5.1b Bề mặt truy cập — ai đọc được, bằng đường nào

| Bên tiêu thụ | Đường | Ghi chú |
|---|---|---|
| Agent chat chính (mọi profile) | Inject `<user_profile>` lượt đầu | §5.3 |
| Subagent / cowork worker / virtual worker | Cùng đường inject | Kế thừa từ phiên cha |
| Agent chủ động hỏi giữa chừng | MCP tool `user_profile_get` | Khi cần trường không có trong block đã inject |
| Web UI / desktop | `GET /api/user-profile` | §5.7 |
| Space App | Qua bridge của daemon, **không đọc file trực tiếp** | Xem cảnh báo §5.4 |
| Scheduler / workflow / rule-engine | Đọc qua `src/user_profile` như thư viện | Cùng cache |

Điểm chốt: **một nguồn sự thật, nhiều đầu đọc**. Không bên nào được tự parse
`USER.md` bằng tay — tất cả đi qua `src/user_profile`, nếu không thì luật tier ở
§5.4 sẽ bị đi vòng.

### 5.2 Định dạng: front-matter có cấu trúc + directive

Yêu cầu có hai nửa khác bản chất — form settings (tên/địa điểm/email, người dùng
gõ) và sở thích (agent học dần). Gộp một file, tách hai vùng:

```md
---
name: Nguyễn Văn A
preferred_name: anh A
email: a@example.com
location: Hà Nội, Việt Nam
timezone: Asia/Ho_Chi_Minh
language: vi
occupation: Backend engineer
---

# USER.md — Hồ sơ người dùng

## Directives

<!-- observed: 2026-08-15 | status: active -->

- Always trả lời bằng tiếng Việt trừ khi được yêu cầu khác.

<!-- observed: 2026-08-15 | status: superseded -->

- Prefer báo cáo tiến độ chi tiết từng bước.

<!-- observed: 2026-08-20 | status: active -->

- Prefer báo cáo tiến độ ngắn gọn, chỉ nêu kết quả.
```

- **Front-matter** = form Settings ghi, ổn định, dễ validate, dễ render lại UI.
- **Directives** = agent ghi qua tool, theo đúng vòng đời `active`/`superseded`
  của OpenClaw.

Mọi trường front-matter đều **optional** — file thiếu trường nào thì bỏ trường đó
ra khỏi block inject, không chèn `name: (unknown)`.

### 5.3 Điểm inject: **first-turn context**, không phải mỗi lượt

SenClaw có sẵn hai chỗ chèn, chi phí khác hẳn nhau:

| Chỗ | Ở đâu | Tần suất | Phù hợp? |
|---|---|---|---|
| Block `<memory>` / `<cognitive_memory>` / `<memory_recall>` | [`agent_pool/pool.rs:1753`](../src/agent/agent_pool/pool.rs) | **Mỗi lượt** | Không — hồ sơ không đổi theo lượt, trả token lặp vô ích |
| `<system-reminder>` lượt đầu | [`zen_core/engine.rs:2343`](../src/zen_core/engine.rs) `collect_first_turn_context` | **Lượt đầu mỗi phiên** | **Đúng chỗ** |

`collect_first_turn_context` được viết chính xác cho loại dữ liệu này — comment ở
`engine.rs:1617` giải thích: chèn context ổn định dạng `<system-reminder>` ẩn để
**không phá prompt caching của system prompt**. Hiện nó chỉ mang ngày tháng
(`SENCLAW.md` bị tắt vì `instance_uses_workspace()` luôn trả `false` —
[`engine.rs:2330`](../src/zen_core/engine.rs)).

Thêm một nhánh — tham số là **tier được phép**, không phải cờ bật/tắt, để luật
§5.4 nằm gọn một chỗ:

```rust
fn collect_first_turn_context(
    working_dir: &str,
    include_project_doc: bool,
    profile_scope: ProfileScope,   // mới: None | PublicOnly | Full
) -> Option<String>
```

Phiên riêng tư (`ProfileScope::Full`):

```
<user_profile>
Người dùng: Nguyễn Văn A (xưng hô: anh A) · a@example.com · Hà Nội, Việt Nam (Asia/Ho_Chi_Minh)

Directives đang áp dụng:
- Always trả lời bằng tiếng Việt trừ khi được yêu cầu khác.
- Prefer báo cáo tiến độ ngắn gọn, chỉ nêu kết quả.
</user_profile>
```

Group chat (`ProfileScope::PublicOnly`) — cùng một file, cùng một hàm render:

```
<user_profile>
Người dùng: Nguyễn Văn A (xưng hô: anh A) · múi giờ Asia/Ho_Chi_Minh · ngôn ngữ vi

Directives đang áp dụng:
- Always trả lời bằng tiếng Việt trừ khi được yêu cầu khác.
- Prefer báo cáo tiến độ ngắn gọn, chỉ nêu kết quả.
</user_profile>
```

Chỉ directive `status: active` được đưa vào. Mục `superseded` ở lại trong file làm
lịch sử cho con người và cho tool, **không tốn token của model**.

Block bị lược trường **không được ghi chú là đã lược** ("(email: ẩn)") — nói cho
model biết có thứ nó không được thấy chỉ khiến nó đi hỏi người dùng trong nhóm.

### 5.4 Phân tầng trường — cách để "ai cũng truy cập được" mà không phát tán email ra group

Soul core là tài nguyên **chung**, mọi bên đọc được (§5.1b). Nhưng SenClaw là
gateway đa kênh: một agent profile bind được vào **group chat** Telegram / Feishu /
QQ / WeChat. Nếu "ai cũng đọc được" áp dụng đồng loạt cho mọi trường, thì email và
địa chỉ nhà của chủ sẽ đi vào prompt của một nhóm chat có người lạ — và model sẽ
nhắc lại chúng khi được hỏi.

Cách giải **không phải** chặn truy cập theo bên gọi (làm vậy là phá đúng yêu cầu
"không phụ thuộc agent"), mà là **phân tầng theo trường**. Cùng một file, hai tier:

| Tier | Trường | Ai thấy |
|---|---|---|
| `public` | `name`, `preferred_name`, `language`, `timezone`, `occupation`, directive về phong cách | **Mọi nơi** — kể cả group chat, virtual worker, Space App |
| `private` | `email`, `phone`, `location`/địa chỉ, và mọi trường người dùng tự đánh dấu | Chỉ phiên riêng tư của chính chủ |

Lý do phân đúng chỗ này: tier `public` là thứ khiến agent **hữu ích ở mọi ngữ
cảnh** — biết gọi tên đúng, trả lời đúng ngôn ngữ, quy chiếu đúng múi giờ khi đặt
lịch. Không có gì nhạy cảm khi một nhóm chat biết chủ bot tên gì và nói tiếng Việt.
Tier `private` là thứ chỉ có giá trị trong hội thoại 1-1 và là thứ gây hại khi lộ.

Đánh dấu ngay trong front-matter, mặc định **an toàn** (trường lạ → `private`):

```yaml
name: Nguyễn Văn A          # public
preferred_name: anh A       # public
language: vi                # public
timezone: Asia/Ho_Chi_Minh  # public
email: a@example.com        # private
location: Hà Nội, Việt Nam  # private
```

Mỗi directive cũng mang tier riêng; mặc định `public` vì directive là quy tắc hành
vi ("trả lời ngắn gọn"), không phải dữ liệu định danh.

Tín hiệu để xác định phiên riêng tư — **đã có sẵn**:

- `ChatType::{Private, Group}` — [`types.rs:14`](../src/types.rs)
- JID mã hoá sẵn loại chat: `chat_id_to_jid` ([`telegram.rs:30`](../src/channels/telegram.rs))
  sinh `tg:<bot>:user:<id>` cho private, `tg:<bot>:group:<id>` cho nhóm

| Ngữ cảnh | `public` | `private` |
|---|---|---|
| `web:*`, `app:*` (UI của chính chủ) | ✅ | ✅ |
| DM kênh ngoài (`tg:*:user:*`, tương đương) | ✅ | ✅ |
| Group chat (`*:group:*`) | ✅ | ❌ |
| `virtual:*`, subagent, cowork worker | ✅ | Kế thừa từ phiên cha |
| Space App qua bridge | ✅ | ❌ mặc định, bật từng app trong Settings |

Ba điều bắt buộc khi triển khai:

1. **Luật tier áp ở `src/user_profile::render`, không ở chỗ gọi.** Mọi đầu đọc
   (inject, MCP tool, REST, bridge) đều phải đi qua cùng một hàm nhận tham số ngữ
   cảnh. Để mỗi bên tự lọc là bảo đảm sẽ có một bên quên.
2. **Không chắc ngữ cảnh → chỉ trả `public`.** Cần kiểm chứng JID của Feishu / QQ /
   WeChat có mang `:group:` không, hay phải truyền `ChatType` xuống thay vì đoán
   từ chuỗi. Sai theo hướng "thiếu thông tin" chỉ làm agent kém tiện; sai theo
   hướng ngược lại là rò dữ liệu cá nhân ra nhóm chat.
3. **Space App mặc định chỉ thấy `public`.** Space App **không có xác thực của
   riêng nó** và toàn bộ cơ chế `SENCLAW_TOKEN_ACCESS_APP` tồn tại chính là để cô
   lập app-với-app (xem [space-app-api-token.md](space-app-api-token.md)). Mở
   `private` cho một app là quyết định của người dùng, phải bấm tay từng app, và
   phải ghi audit.

> OpenClaw chọn cách thô hơn — bỏ nguyên `USER.md` khỏi mọi shared/group context
> (*"never included in shared/group contexts"*). Phân tầng giữ được lợi ích ở
> group chat mà vẫn chặn đúng phần nguy hiểm; đổi lại là thêm một khái niệm người
> dùng phải hiểu. Nếu muốn đơn giản tuyệt đối, đặt **mọi** trường thành `private`
> và ta rơi về đúng hành vi OpenClaw.

### 5.5 Ngân sách

Theo OpenClaw: **4.000 ký tự** cho khối `<user_profile>` sau khi dựng. Vượt thì
cắt phần directive từ cũ nhất (front-matter luôn giữ — nó là phần định danh).
Cắt phải theo biên ký tự UTF-8 — dùng `truncate_on_char_boundary` đã có trong
repo (xem memory *UTF-8 preview slice panic*), không slice byte thô.

### 5.6 Công tắc bật/tắt

Theo đúng mẫu `get_memory_recall_enabled` / `save_memory_recall_enabled`
([`group_manager/llm.rs:101`](../src/gateway/group_manager/llm.rs)) — lưu trong
global config JSON, đọc theo request, không cần restart daemon:

```rust
pub fn get_user_profile_enabled(config_path: &Path) -> bool   // mặc định true khi file tồn tại
pub fn save_user_profile_enabled(config_path: &Path, enabled: bool) -> Result<()>
```

### 5.7 REST + UI

REST đặt cạnh `profile_files.rs`:

| Route | Việc |
|---|---|
| `GET /api/user-profile` | Trả `{ frontmatter: {...}, directives: [...], raw: "..." }` |
| `PUT /api/user-profile` | Ghi front-matter (form Settings) |
| `GET`/`PUT /api/user-profile/enabled` | Công tắc |

UI: **Settings → General** (web `web/src/components/settings/`, desktop
`desktop_app/lib/features/settings/settings_screen.dart` mục `_GeneralSection`)
— form tên / xưng hô / email / địa điểm / múi giờ / ngôn ngữ, cộng danh sách
directive chỉ-đọc kèm nút xoá.

### 5.8 Tool cho agent: `UserProfileUpdate`

Sao chép cấu trúc [`tools/persona_update.rs`](../src/tools/persona_update.rs) —
nó đã giải xong đúng bài toán này cho `SOUL.md`: patch theo section, ghi atomic
(tmp + rename), không để file hỏng giữa chừng.

Khác biệt bắt buộc: **thao tác `supersede`**. Khi người dùng đổi ý, tool phải đổi
`status: active` → `superseded` ở mục cũ và chèn mục mới ngay dưới, trong **một**
lần ghi. Nếu để agent tự do append, ta tái tạo đúng failure mode OpenClaw đã mô tả.

Mô tả tool phải nói rõ ranh giới, nếu không agent sẽ đổ mọi thứ vào đây: chỉ ghi
**sở thích ổn định và sự thật hồ sơ**; quan sát vụn → `memory_save`; việc đúng giờ
→ `schedule_task`.

### 5.9 Cognitive ingest — có nên?

`SOUL.md` được ingest vào graph dưới `NodeSet::Persona`. Với `USER.md`:

- **Nên**: một `NodeSet::User` riêng (không dùng lại `Persona` — sẽ lẫn "agent là
  ai" với "chủ là ai" khi recall).
- **Chưa cần ở giai đoạn đầu**: hồ sơ đã được inject nguyên văn ở lượt đầu, recall
  thêm là dư. Để dành cho lúc file lớn hơn ngân sách 4.000 và cần truy hồi chọn lọc.

## 6. Đối chiếu SenClaw ↔ OpenClaw và danh sách đáng port

### 6.1 Ma trận

| Khái niệm | OpenClaw | SenClaw hôm nay | Khoảng cách |
|---|---|---|---|
| Persona agent | `SOUL.md` (workspace) | `SOUL.md` **per-agent** + `core_prompt` | Có, chỉ khác cấp |
| Hồ sơ người dùng | `USER.md` | **Không có gì** | **Trống hoàn toàn** |
| Danh thiếp agent | `IDENTITY.md` (name/creature/vibe/emoji/avatar) | Chỉ `agents.name` | Thiếu 4/5 trường |
| Luật vận hành | `AGENTS.md` | `zen_core/prompt.rs::SYSTEM_PROMPT` (hardcode) | **Người dùng không sửa được** — [chi tiết](soul-core-integration.md) §8 |
| Ghi chú môi trường | `TOOLS.md` | **Không có** | **Trống** |
| Memory dài hạn | `MEMORY.md` (gate main-session) | `MEMORY.md` per-agent | Có, **thiếu cổng** |
| Nhật ký ngày | `memory/YYYY-MM-DD.md` | **Đã tắt** (xem `pool.rs:1774`) | Đã bỏ có chủ ý |
| Hợp nhất memory | dreaming + `DREAMS.md` | auto-reflection + curated memory | Có, **thiếu file người đọc được** |
| Nhịp chủ động | heartbeat gộp | chỉ cron/interval/once | Thiếu cơ chế gộp |
| Nghi thức lần đầu | `BOOTSTRAP.md` tự xoá | `setup.rs` (chỉ hỏi permission) | Thiếu phần identity |
| Trần ngân sách prompt | 20k/file, 60k tổng | **Không có trần** | **Trống** |
| Luật group chat | Viết trong `AGENTS.md` | `trigger_checker.rs` (chỉ khi nào nói) | Thiếu **luật nội dung** |

### 6.2 Xếp theo giá trị / công sức

1. **`USER.md`** — chính là phần thân của tài liệu này. Trống hoàn toàn, giá trị cao nhất.
2. **Vòng đời directive `active`/`superseded`** — nên áp cho **cả** curated memory
   hiện tại, không riêng `USER.md`. SenClaw mới có `supersede=true` lúc ghi
   ([`mcp/memory_server.rs:181`](../src/mcp/memory_server.rs)), **không có trạng
   thái đọc được trên từng mục** — nên không phân biệt được "sở thích cũ đã bỏ" với
   "sở thích hiện tại", đúng failure mode OpenClaw mô tả.
3. **Trần ngân sách bootstrap.** SenClaw đọc `SENCLAW.md` **không giới hạn** trong
   `collect_first_turn_context`. `CLAUDE.md` của chính repo này đã **34 KB**; nếu
   ai đó bật `instance_uses_workspace()` lên `true` thì mỗi phiên nuốt trọn. Thêm
   `20k/file, 60k tổng` là vá phòng thủ rẻ.
4. **`TOOLS.md`** (§3.5) — chỗ chuẩn cho dữ liệu môi trường, để skill giữ được
   tính chia sẻ được. Rẻ, và giải một vấn đề đang có thật với các app SSH/IoT.
5. **`DREAMS.md`** — làm cho memory tự động **kiểm tra được bằng mắt**. SenClaw đã
   có auto-reflection chạy ngầm nhưng người dùng không thấy nó quyết định gì.
6. **Luật nội dung cho group chat** — một đoạn trong system prompt nói rõ dữ liệu
   của chủ không phát ra nhóm. Gần như miễn phí, và là hàng rào thứ hai sau §5.4
   (phòng khi tier bị cấu hình sai).
7. **`IDENTITY.md`** — `emoji`/`avatar`/`vibe`. Với gateway đa kênh (nhiều bot cùng
   một nhóm) đây là thứ giúp phân biệt bot nào đang nói.
8. **`AGENTS.md` người dùng sửa được** — hoá ra **rẻ hơn dự tính nhiều**:
   `assemble_system_prompt` ([`engine.rs:1870`](../src/zen_core/engine.rs)) đã có
   sẵn khối `user_defaults` đúng hình dạng cần, kèm setter và test. Chỉ là thêm
   tham số thứ tám. Kèm theo một phát hiện đáng lo: `src/agent/system_prompts.rs`
   và `system_prompt_builder.rs` **là code chết, không ai gọi** — nên `SPACE_NOTES`
   và `MEMORY_NOTES` chưa bao giờ tới model. Thiết kế đầy đủ + cảnh báo về việc cho
   agent tự sửa: [soul-core-integration.md §8](soul-core-integration.md).
9. **Heartbeat gộp** — chồng lấn nhiều với scheduler đã có; chỉ đáng làm nếu thấy
   người dùng thật sự tạo nhiều task poll rời rạc.

## 7. Kế hoạch theo giai đoạn

| GĐ | Nội dung | Chạm vào |
|---|---|---|
| 1 | `src/user_profile/` — parser front-matter + directive, **tier**, cache + watcher, `PathsConfig` | mới, `config.rs`, `lib.rs` |
| 2 | Inject qua `ProfileScope` + ngân sách 4.000 + cắt theo biên UTF-8 | `zen_core/engine.rs`, `agent_pool/pool.rs` |
| 3 | REST + Settings UI (web + desktop), gồm cả cột tier cho từng trường | `ui_server/`, `web/src/`, `desktop_app/` |
| 4 | Tool `user_profile_get` + `UserProfileUpdate` với `supersede` | `src/tools/`, `src/mcp/` |
| 5 | Trần bootstrap 20k/60k cho `SENCLAW.md` (vá phòng thủ, độc lập) | `zen_core/engine.rs` |
| 6 | `TOOLS.md` (§3.5), `IDENTITY.md`, bootstrap ritual | `agent_manager.rs`, `setup.rs`, desktop onboarding |
| 7 | `DREAMS.md` — làm auto-reflection nhìn thấy được | `agent_pool/reflection.rs` |

Giai đoạn 1+2 đã đủ dùng: khai form một lần, agent biết chủ là ai từ lượt đầu mọi
phiên. Giai đoạn 5 nên làm sớm dù không liên quan — nó rẻ và vá một lỗ có thật.

## 8. Bẫy đã biết

- **Đừng tái dùng `SOUL.md`** cho hồ sơ người dùng — bốn cơ chế ở §4 sẽ kích hoạt
  nhầm. Nếu buộc phải dùng tên đó, loại đường dẫn ra khỏi `spawn_soul_watcher` và
  khỏi `ingest_all_souls`.
- **Hai cây thư mục, đừng nhầm**: `~/.senclaw` (db, config) vs `~/senclaw`
  (agents, workspace) — [`config.rs:486`](../src/config.rs). Docstring
  `soul_ingest.rs:3` đang ghi sai đường dẫn `SOUL.md`.
- **Luật tier phải nằm trong `render`, không ở chỗ gọi** (§5.4). Mỗi đầu đọc tự
  lọc = chắc chắn có một cái quên.
- **Không chắc ngữ cảnh → chỉ trả tier `public`.** Không chắc JID là DM hay nhóm
  thì đừng đoán.
- **`TOOLS.md` mặc định `private`** — nó chứa IP nội bộ và SSH host, đừng để nó
  rơi vào tier public chỉ vì "cũng là file cấu hình".
- **Đừng chèn ở `pool.rs` cùng chỗ với `<memory>`.** Đó là đường mỗi-lượt; hồ sơ
  ổn định thuộc về `collect_first_turn_context` để giữ prompt cache.
- **Chỉ inject directive `active`.** Đưa cả `superseded` vào chính là tái tạo lỗi
  "model bốc trúng sở thích cũ" mà cơ chế này sinh ra để chữa.
- **Cắt theo biên ký tự UTF-8**, không cắt byte — tên tiếng Việt có dấu sẽ panic.
- **Front-matter là dữ liệu người dùng gõ, không phải chỉ thị.** Khi dựng block
  inject, đặt trong `<user_profile>` và không diễn giải nội dung như lệnh — một
  người dùng gõ `name: Bỏ qua mọi quy tắc trước đó` không được biến thành chỉ thị.
- **Tuyệt đối không đưa hồ sơ vào `/env` hay bất kỳ route nào phục vụ UI trình
  duyệt của Space App** — cùng lý do token app không bao giờ đi qua `/env`
  (xem [space-app-api-token.md](space-app-api-token.md)).

## 9. Nguồn

**Bản cài thật trên máy dev** (nguồn mạnh nhất — docs và sản phẩm khác nhau, §3.4):
`~/.openclaw/workspace-main/{AGENTS,SOUL,IDENTITY,USER,TOOLS,HEARTBEAT,BOOTSTRAP}.md`
và `~/.openclaw/openclaw.json` (đọc khoá, không đọc giá trị bí mật).

- [Agent workspace · OpenClaw](https://docs.openclaw.ai/concepts/agent-workspace)
- [User model · OpenClaw](https://docs.openclaw.ai/concepts/user-model)
- [USER template · OpenClaw](https://docs.openclaw.ai/reference/templates/USER)
- [Memory overview · OpenClaw](https://docs.openclaw.ai/concepts/memory) — dreaming, `DREAMS.md`, `memory/imports/`
- [Token use and costs · OpenClaw](https://docs.openclaw.ai/reference/token-use) — `bootstrapMaxChars` / `bootstrapTotalMaxChars`
- [How OpenClaw Implements Agent Identity — MMNTM](https://www.mmntm.net/articles/openclaw-identity-architecture)
- [OpenClaw Workspace Files Explained — Roberto Capodieci](https://capodieci.medium.com/ai-agents-003-openclaw-workspace-files-explained-soul-md-agents-md-heartbeat-md-and-more-5bdfbee4827a)
