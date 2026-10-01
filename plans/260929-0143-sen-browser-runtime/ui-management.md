# Browser engine — management UI (desktop + web)

Status: in progress · Owner: this plan · Daemon: `feat/sen-browser-v2` (senclaw@7db3d79)

Both clients get the same **Settings → Browser** section and recognise the new `browser` runtime slot. Same
strings in both (English is the key; Vietnamese below).

## Daemon API (all under the daemon UI port, normal API auth)

| Call | Shape |
|---|---|
| `GET /api/browser-agent/settings` | `{settings: BrowserSettings, engine: "v2"\|"legacy", runtimeInstalled: bool}` — never starts the runtime |
| `PUT /api/browser-agent/settings` | body: any subset of `BrowserSettings` fields → same shape as GET; `422 {error, code:"invalid_settings"}` with a person-readable `error`, nothing saved |
| `GET /api/browser-agent/extension` | `{connected: null \| {ext_id, version, chrome, piped}, pending: [{code, ext_id, age_secs}], paired: [{ext_id, paired_at}]}` — never starts the runtime |
| `POST /api/browser-agent/extension/pairings/:code/approve` | `{paired: ext_id}`; `404 {error, code:"bad_code"}` (unknown/expired code) |
| `DELETE /api/browser-agent/extension/paired/:ext_id` | `{revoked: bool}` |
| `GET /api/browser-agent/tabs` | `{sessions: [{id, driver, profile, headless, tabs: [{id, owner, driver, url, hold, closed}]}], current, extension: {connected, shared_tabs}}` — **starts the runtime**: call on explicit refresh only, not on a timer. Without `?chat_jid=` it lists every chat's tabs (the settings screens); an agent's call is filtered to its own chat |
| `GET /api/browser-agent/approvals` | `{approvals: [{approval_id, task_id, chat, goal, action, operation, text, driver, url, waiting_secs}]}` oldest first — never starts the runtime |
| `POST /api/browser-agent/approvals/:id` | body `{approve: bool}` → the task's outcome `{task_id, status, message, …}` once it pauses or ends again (can take minutes: the task continues inside this call); `404 {code:"no_approval"}` when someone already answered |
| `GET /api/runtimes` | existing; now also lists slot `browser` (type `browser`, capability `browser`) |

`BrowserSettings` (camelCase JSON, every field optional on PUT):

| Field | Type / values | Default | UI |
|---|---|---|---|
| `engine` | `auto` \| `v2` \| `legacy` | `auto` | Select. Help: "Auto uses the new engine once the Browser runtime is installed. Applies to chats started afterwards." |
| `defaultDriver` | `managed` \| `extension` | `managed` | Select: "SenClaw's own Chrome" / "Your Chrome (extension)" |
| `decisionBackend` | `auto` \| `local` \| `hosted` \| `llm-only` | `auto` | Select: "Auto (local, hosted only for allowed sites)" / "Local model only" / "Hosted Jev" / "LLM only (no decision model)" |
| `localModel` | string | `laya-browser` | Text |
| `hostedModel` | string \| null | null | Text, empty = null |
| `hostedDomains` | string[] (host names) | [] | Tags. Help: "Only these sites' page text may go to the hosted decision model." |
| `sensitiveDomains` | string[] | [] | Tags. Help: "Always decided locally, with stricter rules." |
| `domainDrivers` | `{host: "managed"\|"extension"}` | {} | Editable rows host → driver |
| `textModel` | LLM config id \| null | null | Select from `GET /api/llm-config` `configs[].id/name`; null = "Active chat model" |
| `fallbackModel` | LLM config id \| null | null | same |
| `bandsLocal` / `bandsHosted` | `{act, fallback}` 0..1, fallback ≤ act | `{0.6,0.2}` / `{0.5,0.25}` | Advanced, numbers |
| `maxSteps` | 1..120 | 40 | Number |
| `headless` | bool | true | Switch "Run SenClaw's Chrome without a window" |
| `profile` | `[a-z0-9_-]{1,40}` | `default` | Text "Chrome profile name" |
| `startUrl` | http(s) URL | `https://duckduckgo.com/` | Text |

Save = PUT of the changed fields only; show the 422 `error` text verbatim on refusal.

## Screen: Settings → Browser

1. **Engine status** — "Engine in use: New (Jev + LLM) \| Legacy (extension scripts)" from `engine`; if
   `runtimeInstalled` is false: warning "The Browser runtime is not installed." + button "Open Runtime settings"
   (navigates to the Runtime section; the runtime's slot is `browser`).
2. **Settings form** — fields above, grouped: *General* (engine, defaultDriver, headless, profile, maxSteps,
   startUrl), *Decisions* (decisionBackend, localModel, hostedModel, hostedDomains, sensitiveDomains, textModel,
   fallbackModel), *Sites* (domainDrivers), *Advanced* (bands). One Save button.
3. **Chrome extension** — connected card (ext id, version, Chrome version) or "Not connected"; **pending
   pairing codes** each with *Approve* (POST approve; toast "Connected <ext_id>"); **paired browsers** each with
   *Remove* (DELETE, confirm first: "This browser will need to pair again."). Poll `GET extension` every 3 s while
   the section is visible (cheap, no runtime start). Hint: "Install the SenClaw extension in Chrome and open its
   side panel; its pairing code appears here — or send `pair approve <CODE>` in any chat."
4. **Activity** — button "Show open tabs" (GET tabs; starts the runtime) → sessions with driver/profile and their
   tabs (owner chat, URL, hold state: none/handover/user_active/detached).
5. **Waiting for your approval** — shown above the settings form (it is the one thing here that needs the person
   now). Poll `GET approvals` every 5 s while the section is visible (cheap, no runtime start). Each row: the
   action label (bold) with its operation as a tag (`CLICK` "Click", `KEY_ENTER` "Press Enter", `DIALOG_ACCEPT`
   "Confirm a dialog", `TYPE_TEXT` "Type text", anything else as is), the task's goal, the page URL, the chat and
   "Waiting {time}". Buttons *Approve* (confirm first: "SenClaw will do this in the browser now.") and *Decline*.
   While the POST runs the row shows a spinner and both buttons are disabled; afterwards a toast "The task went
   on: {status} — {message}" (or "Declined" when `approve` was false and the task ended), and the list reloads.
   A 404 means it was already answered elsewhere: toast the daemon's error, reload. Empty list: hide the card.
   Hint under the list: "A browser task paused before this action. Approve only if you want SenClaw to do it."
   Agents in chats that ask nobody (workflow steps, background runs, prompts turned off) cannot approve these
   themselves — this card and the chat's own approval prompt are the only ways.

## Runtime screen

- Add slot/type `browser` to the slot/type unions and the catalog type filter ("Browser").
- sen-browser is in the runtime catalog (`runtimes/index.json`) and shows *Not published yet* until its first
  release: a `v*` tag in the sen-browser repo publishes per-platform archives (`.github/workflows/release.yml`), then
  the index lists their URL, sha256 and size. Until then: `make package`, and install the archive from the catalog
  menu (desktop *Install from folder or archive…*, web *Install from a local path*). A runtime installed from a local
  package shows *Installed*, not *Not published yet*. The live catalog is fetched from GitHub `SenClaw/senclaw` main,
  so the entry reaches installed apps once this branch is merged and pushed.
- Checked 29/09 on a desktop build of this branch against an isolated daemon: Browser slot row, catalog entry,
  Uninstall, install from an archive through the native picker, start (Browser → *Show open tabs*), the Running list
  with Stop, and View logs (runtime logs now come without colour codes).

## Vietnamese strings (both apps)

| English | Tiếng Việt |
|---|---|
| Browser | Trình duyệt |
| Browser engine | Engine trình duyệt |
| Engine in use | Engine đang dùng |
| New (Jev + LLM) | Mới (Jev + LLM) |
| Legacy (extension scripts) | Cũ (script trong extension) |
| The Browser runtime is not installed. | Runtime Browser chưa được cài. |
| Engine | Engine |
| Auto | Tự động |
| Auto uses the new engine once the Browser runtime is installed. Applies to chats started afterwards. | Tự động dùng engine mới khi đã cài runtime Browser. Áp dụng cho các cuộc trò chuyện mở sau đó. |
| Default browser | Trình duyệt mặc định |
| SenClaw's own Chrome | Chrome riêng của SenClaw |
| Your Chrome (extension) | Chrome của bạn (extension) |
| Decision backend | Backend quyết định |
| Auto (local, hosted only for allowed sites) | Tự động (cục bộ, hosted chỉ cho site được phép) |
| Local model only | Chỉ model cục bộ |
| Hosted Jev | Jev hosted |
| LLM only (no decision model) | Chỉ LLM (không dùng model quyết định) |
| Local decision model | Model quyết định cục bộ |
| Hosted model | Model hosted |
| Sites allowed for hosted decisions | Site được dùng quyết định hosted |
| Only these sites' page text may go to the hosted decision model. | Chỉ nội dung trang của các site này được gửi tới model quyết định hosted. |
| Sensitive sites | Site nhạy cảm |
| Always decided locally, with stricter rules. | Luôn quyết định cục bộ, với luật chặt hơn. |
| Browser per site | Trình duyệt theo site |
| Add site | Thêm site |
| Text writer model | Model viết chữ |
| Fallback model | Model dự phòng |
| Active chat model | Model chat đang dùng |
| Confidence bands | Ngưỡng tin cậy |
| Act at or above | Tự làm từ mức |
| Ask the LLM at or above | Hỏi LLM từ mức |
| Local | Cục bộ |
| Hosted | Hosted |
| Step budget | Số bước tối đa |
| Run SenClaw's Chrome without a window | Chạy Chrome của SenClaw không hiện cửa sổ |
| Chrome profile name | Tên profile Chrome |
| Start page | Trang bắt đầu |
| Decisions | Quyết định |
| Sites | Site |
| Advanced | Nâng cao |
| Chrome extension | Extension Chrome |
| Not connected | Chưa kết nối |
| Connected | Đã kết nối |
| Waiting to pair | Đang chờ ghép cặp |
| Approve | Duyệt |
| Paired browsers | Trình duyệt đã ghép cặp |
| Remove | Gỡ |
| This browser will need to pair again. | Trình duyệt này sẽ phải ghép cặp lại. |
| Install the SenClaw extension in Chrome and open its side panel; its pairing code appears here — or send `pair approve <CODE>` in any chat. | Cài extension SenClaw trong Chrome và mở side panel; mã ghép cặp sẽ hiện ở đây — hoặc gửi `pair approve <MÃ>` trong bất kỳ cuộc trò chuyện nào. |
| Activity | Hoạt động |
| Show open tabs | Xem các tab đang mở |
| No open tabs | Không có tab nào đang mở |
| Settings saved | Đã lưu cài đặt |
| Waiting for your approval | Đang chờ bạn duyệt |
| A browser task paused before this action. Approve only if you want SenClaw to do it. | Một tác vụ trình duyệt đã dừng lại trước thao tác này. Chỉ duyệt khi bạn muốn SenClaw thực hiện nó. |
| SenClaw will do this in the browser now. | SenClaw sẽ thực hiện thao tác này trên trình duyệt ngay bây giờ. |
| Decline | Từ chối |
| Declined | Đã từ chối |
| Waiting {time} | Đã chờ {time} |
| The task went on: {status} — {message} | Tác vụ đã tiếp tục: {status} — {message} |
| Click | Nhấp |
| Press Enter | Nhấn Enter |
| Confirm a dialog | Xác nhận hộp thoại |
| Type text | Nhập chữ |
| Goal | Mục tiêu |
