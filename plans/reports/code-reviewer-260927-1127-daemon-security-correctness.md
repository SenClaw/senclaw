# Daemon runtime-split + control-plane: security & correctness review

Read-only review of `src/runtime/**`, `src/local_models/**`, `crates/sen-runtime-sdk/src/server.rs`,
`src/control_plane/**`, `src/decision/client.rs` + gate/skill_route rewiring, against
`docs/runtime-protocol.md`. No source modified. The full test suite is green; everything below is what the
tests do not cover.

Repo has no git history (`No commits yet`), so review is against the working tree, not a diff.

---

## High

### H1 — `GET /api/traces/:id` is an arbitrary `*.json` file read (path traversal)
`src/control_plane/trace.rs:268-273` (`read_by_id`), handler `src/gateway/ui_server/control_plane.rs:100-104`,
route `src/gateway/ui_server/core.rs:672`.

```rust
pub fn read_by_id(id: &str) -> Option<serde_json::Value> {
    let (chat, stem) = id.split_once(':')?;
    let path = root().join(chat).join(format!("{stem}.json"));
    let raw = std::fs::read(path).ok()?;
    ...
}
```

`id` is the raw axum path param. `chat` and `stem` are joined into a filesystem path with **no** `safe_id`
mangling or `..` rejection — even though the in-code comment claims "`chat` is already a `safe_id`-mangled
directory name (no colons survive)". That invariant holds only for ids produced by `list_all`; the HTTP entry
point feeds attacker-controlled input straight in.

Failure scenario: `GET /api/traces/..:..%2fconfig` → `id = "..:../config"` → `root()` is
`~/.senclaw/control-plane/traces`, so the path resolves to `~/.senclaw/config.json` — which holds provider API
keys and tokens. Any file ending in `.json` that the daemon can read is reachable. This defeats the module's own
"metadata only, never a message or a file's content" guarantee. Reachable by any loopback peer in the default
`auth: auto` mode (loopback is exempt from the token — `src/gateway/ui_server/auth.rs:407-413`).

Fix: mangle both components with `super::safe_id(...)` in `read_by_id` (the same function `list_all` already uses
to build the id), or reject any `chat`/`stem` containing a path separator or `..`. `chat_dir` already uses
`safe_id`; `read_by_id` must match.

### H2 — A runtime that crashes after becoming healthy is never detected, never restarted, and `launches` never counts it
`src/runtime/supervisor.rs`. Documented contract (`docs/runtime-protocol.md` §3.2 step 9; CLAUDE.md "crash
accounting"): "a crash (process exit while in use) is recorded, counted in `launches`, and the next request
starts it again." None of this is implemented.

- `launches` is set to `AtomicU32::new(1)` at spawn (line 313) and **never incremented anywhere** — the only
  `fetch_add` in the file is a test health-server's call counter (line 714). Every restart creates a fresh
  `RunningProcess` with `launches = 1`, so the crash-loop signal the field exists for is dead.
- After `wait_healthy` succeeds, `state` is set to `Ready` (line 347) and the child handle sits in
  `proc.child` untouched. Nothing polls `try_wait` on the ready path (the only `try_wait`, line 605, runs during
  startup only). If the engine exits (OOM on a large model, llama.cpp assertion), `state` stays `Ready`.
- `ensure_started` fast-path (lines 219-224 / 227-232) returns the dead process because `is_ready()` is still
  true, so every subsequent request dials a refused port and returns `Upstream`/502. There is no restart.
- The process is only cleared once `sweep_idle` stops it after the full idle timeout (service 300s, model 900s),
  and only then does the *next* request respawn it.

Failure scenario: a local GGUF/MLX model crashes mid-session; the user's model is broken with confusing
`could not reach the runtime`/BadGateway errors for up to 5–15 minutes, despite the documented promise of an
automatic restart on the next request. No test covers a post-ready crash (only startup health-gating is tested).

Fix: on a dial/relay connection failure, or via a lightweight liveness check, mark the process `Failed` and
remove it from the map so the next `ensure_started` respawns; increment `launches` on each (re)spawn for the same
key so the crash-loop signal works. Minimally: in `RuntimeManager::dial`/`proxy` on `Upstream`, if
`child.try_wait()` shows the process exited, evict it before returning.

---

## Medium

### M1 — Streaming responses drop the `in_flight` guard at header time, so the idle sweep can stop a process mid-stream
`src/runtime/proxy.rs:45-57` (`forward`) and `:220-269` (`proxy_model`), against the invariant in
`src/runtime/manager.rs:347-359` and CLAUDE.md ("the sweep skips any process with a nonzero count").

`relay` returns as soon as response **headers** arrive, wrapping `upstream.bytes_stream()` in
`Body::from_stream`. `forward` then immediately calls `manager.end_request(...)`, dropping `in_flight` to 0 while
the body is still streaming to the client. `begin_request`/`end_request` therefore cover only the header
round-trip, not the token stream — exactly the opposite of the documented "increment before proxying, decrement
after" guarantee.

Failure scenario: a long local-model generation (`no total timeout` by design) streams for longer than the idle
timeout (model default 900s; a slow CPU model producing a large output can approach it). During the stream
`in_flight == 0` and `last_used_at` is frozen at request start, so the 10s idle sweep stops and kills the process
mid-generation, breaking the stream.

Fix: hold the in-flight guard across the body, not just the headers — e.g. wrap the returned stream so
`end_request` fires on stream completion/drop rather than right after `relay` returns.

### M2 — `advance_pinned_slots_to_newer_installs` ignores the `autoUpdate` setting
`src/runtime/manager.rs:439-472`, called unconditionally from `sweep_idle_once` (line 427).

The function moves any explicitly-pinned slot to the newest installed version whenever the old version is idle.
It never reads `settings.auto_update`. The contract (`docs/runtime-protocol.md` §7.3, `src/runtime/updates.rs`
header) is LM-Studio semantics: slots advance *"with auto-update on."* Here they advance even when auto-update is
off.

Failure scenario: a user disables auto-update, pins a slot to `0.1.0` to roll back a regression while keeping
`0.2.0` installed for later. Within 10s the idle sweep silently re-pins the slot to `0.2.0`, undoing the
rollback — a silent reversal of an explicit user decision. `Selection` also carries no provenance bit to
distinguish "auto-update landed a newer build" from "user deliberately pinned this exact older version," so even
with the `auto_update` guard added, a deliberate downgrade-with-newer-installed cannot be expressed.

Fix: gate the advance on `self.settings().auto_update`. If deliberate downgrades must survive an auto-update
window, add a provenance flag to `Selection` set when the user pins explicitly vs. when the §7.3 mover sets it.

### M3 — `GET /api/runtimes/:id/logs` reads an arbitrary `.log` file (path traversal via `id` and `key`)
`src/runtime/manager.rs:172-197` (`resolve_log_path`), handler `src/runtime/rest.rs:361-369`.

Neither the `:id` path param nor the `?key=` query value is validated. With a `model:`-prefixed key the path is
`runtime_logs_dir.join(format!("{id}--{model_key}.log"))`, where `model_key` is everything after `model:`. A
value such as `?key=model:../../../../some/path` (and `..`-laden `id`) resolves outside the logs directory. The
read is constrained to files ending in `.log` and returns the last N lines, so it is a bounded arbitrary-file
read rather than full disclosure, but it is still traversal on an otherwise trusted management route.

Fix: validate `id` with the SDK's `sen_runtime_sdk::manifest::valid_id`, and reject any `key` whose `model:`
suffix contains a path separator or `..` (or derive the filename through the tracked process rather than string
interpolation).

---

## Low

### L1 — MLX download joins an HTTP-API-supplied path onto the filesystem without the traversal guard
`src/local_models/download.rs:250-253`. For GGUF the code takes the basename only (safe); for MLX it does
`dest_root.join(&entry.path)` where `entry.path` comes from HuggingFace's tree API for an arbitrary
user-specified repo. Git tree entry names cannot be `.`/`..`, so this is not exploitable today, but the archive
extractor already has `safe_relative_path` for exactly this reason and the downloader should not trust an
external API's paths for a filesystem join. Fix: run each `entry.path` through `store::safe_relative_path` (or an
equivalent) before joining.

### L2 — Installing llama.cpp from an extracted *directory* drops its dylib symlinks
`src/runtime/store.rs:120-135` (`copy_dir_all`) neither copies nor follows symlinks. That is correct for the
"copies, never symlinks" rule, but upstream llama.cpp ships in-package dylib symlinks
(`libllama.0.dylib → libllama.0.5.0.dylib`) that the archive path deliberately preserves
(`extract_tar_gz` + `symlink_stays_inside`). `install-local` on an already-extracted directory silently drops
them, which can leave the runtime unable to load its own libraries. Fix: recreate in-package symlinks in
`copy_dir_all` using the same `symlink_stays_inside` guard the archive path uses, instead of skipping them.

### L3 — Request-side hop-by-hop headers are forwarded to the runtime
`src/runtime/proxy.rs:24`. `STRIP_REQUEST_HEADERS` covers the credential and framing headers
(`authorization`, `x-senclaw-token`, `cookie`, `host`, `content-length`) but not the RFC 7230 hop-by-hop set
(`connection`, `upgrade`, `keep-alive`, `te`, `trailer`, `proxy-*`). Forwarding them to the loopback runtime is
low-risk but incorrect proxy behavior; add them to the strip list.

### L4 — `start_locks` grows without bound
`src/runtime/supervisor.rs:209-212`. `keyed_lock` inserts one `Arc<Mutex<()>>` per process key and never removes
it. Over a long-lived daemon that loads many distinct models, the map grows unboundedly (small, but unbounded).
Fix: drop the entry when the process is stopped, or prune on stop.

### L5 — `safe_id` collides distinct chat jids into one trace/workspace directory
`src/control_plane/mod.rs` `safe_id` maps every non-`[A-Za-z0-9_-]` char to `_`, so `a:b` and `a_b` share a
directory. Traces/workspaces for two chats can intermix. Low impact (metadata), but a hash suffix would make ids
injective.

### L6 — Defense-in-depth: tar `entry.unpack()` bypasses the tar crate's own symlink canonicalization
`src/runtime/store.rs:323`. `extract_tar_gz` calls `entry.unpack(&out_path)` (i.e. `target_base: None`), which
skips the `validate_inside_dst` canonicalization the tar crate performs only in `unpack_in`. Extraction safety
therefore rests entirely on the module's own lexical `symlink_stays_inside` check. That check is sound (it never
lets net depth go negative and rejects absolute targets), so this is not a live bug, but the extraction runs into
a pre-existing tree via `create_dir_all(parent)` before each entry; a note for future maintainers that the
in-house guard is the only line of defense here.

---

## Verified OK (risky areas that hold up)

- **No SSRF / arbitrary host reach through the proxy.** `relay` builds `format!("{base_url}{path_and_query}")`
  where `base_url` is the daemon-chosen `http://127.0.0.1:{port}` and `path_and_query` always begins with `/`
  (origin-form). The host/port cannot be overridden by the request path; `proxy_model`'s URI rewrite keeps the
  same fixed base. (`src/runtime/proxy.rs:62-105, 246-263`)
- **Client credentials are stripped before forwarding and replaced with the per-launch bearer.**
  `STRIP_REQUEST_HEADERS` removes `authorization`/`x-senclaw-token`/`cookie`; `relay`/`call_decision`/`clients::call`
  all `bearer_auth(&dial.token)`. The runtime never sees the daemon's own token. (`src/runtime/proxy.rs:24,78`)
- **Request-body casing does not drift** (the prior cross-repo trap). The new request structs are single-word
  fields or carry `#[serde(rename_all = "camelCase")]` where multi-word (`SettingsBody`, `PartialIdleTimeouts`,
  `local_models::rest::SettingsBody`, `LoadBody`). `post_load` and `decision_settings_put` take
  `axum::body::Bytes` and treat an empty body as default, avoiding the axum-0.7 `Json<T>` 415-on-empty trap.
  `ReplayBody { spec_id, state }` is snake_case and matches its only client (`scripts/evals/run.py`).
- **Decision key-order preservation is intact.** Requests serialize through `decision::json::Json` +
  `AskRequest`'s field order (`src/decision/client.rs`, `src/decision/json.rs`); the response side converts to
  `serde_json::Value` but only ever does key *lookups* (`gate/shell.rs::checks`, `ladder::extract_answer`,
  `skill_route`), never order-dependent iteration.
- **`install-local` is acceptable behind the existing auth modes.** It executes arbitrary local code, but so does
  the already-shipped `GET /api/ws/terminal` (a full PTY shell) and Space-App install; in `auto` mode any loopback
  peer is trusted, in `always` mode a token is required. It does not widen the trust boundary. No hardening needed
  for the desktop dev flow. Manifest `id`/`version` are charset/`..`-validated, so `finish_install`'s target path
  cannot escape `runtimes_dir`. (`src/runtime/store.rs:202-238`, `crates/.../manifest.rs:528-548`)
- **Archive symlink confinement.** `symlink_stays_inside` rejects absolute targets and any target whose lexical
  depth climbs above the package root; hard links are refused outright; zip entries are written as regular files
  (never as symlinks). (`src/runtime/store.rs:260-360`)
- **Orphan cleanup cannot kill an unrelated pid-reused process.** `cleanup_orphans` requires both `pid_alive` and
  a working-directory match via `process_cwd` (lsof `-d cwd`); "cannot verify" leaves the process alone.
  `stop`/`stop_process` kill through the owned `tokio::process::Child` handle (unreaped, so the pid cannot be
  reused). (`src/runtime/supervisor.rs:443-478, 369-395`)
- **No std `Mutex` held across `.await`** in the supervisor hot paths (`wait_healthy` drops the child guard before
  the HTTP call; `sweep_idle`/`stop`/`stop_all` collect under the lock, drop, then await).
- **Download integrity + cancel.** sha256 is verified when the index/GitHub digest provides it
  (`jobs.rs::verify_sha256`, `llamacpp.rs::download_to` streaming hash); `sha256:` prefix is stripped; downloads
  are HTTPS or `file://`; cancellation is checked between chunks and cleans up partials.
- **SDK server auth/bind.** Loopback-only unless `SENCLAW_RUNTIME_ALLOW_REMOTE=1` (daemon never sets it); bearer
  required on every route but `/health`; constant-time compare (length check leaks only length, and the token is a
  fixed 64-hex string); parent watchdog exits ~2s after the daemon disappears. (`crates/.../server.rs`)
- **Policy gate fail-closed behavior is correct as designed.** `may_auto_approve` treats empty/unparseable input
  as risky (never approvable) and `HIDDEN` catches command substitution / line continuations. The static risk list
  is intentionally *not* exhaustive: it is a pre-filter, the gate can only *skip* a prompt (never deny), is `off`
  by default, and anything not on the list is sent to the decision engine, which only skips the prompt on a
  confident per-command reversibility verdict. Commands outside the list are judged by the engine, not
  auto-approved — so the list's non-exhaustiveness is not a bypass. (`src/control_plane/policy_gate.rs`,
  `src/decision/gate/mod.rs`)
- **Control-plane shadow specs add no turn latency and never call the runtime when shadow is off.**
  `run_control_plane_shadow_specs` returns immediately on `jev_off`, then `tokio::spawn`s off the turn;
  `ladder::should_ask` returns false for `SpecMode::Shadow` unless `settings.shadow` is on and false for `Off`,
  so a default install never starts `sen-sysone`. (`src/agent/agent_pool/pool.rs:1688-1730`,
  `src/control_plane/ladder.rs:59-68`)
- **`<agent_status>` never mutates history and does not break prefix caching.** `outgoing_messages` returns
  `Cow::Borrowed` untouched when off, and a per-call clone (status appended to the last user-role message) when on;
  `messages` itself, compaction input (`summarize_history` bypasses it), and the trajectory are unaffected. Status
  text is bounded to ~300 tokens (`context_assembler::MAX_TOKENS`). (`src/zen_core/conversation.rs:215-229`)
- **L1 offload is correctly gated.** The in-memory `OFFLOAD_THRESHOLD_BYTES` length check runs before
  `config.json` is read, the `substitute_tool_output` switch defaults off, `write_artifact` confines `call_id` via
  `safe_id`, and quota pruning only ever deletes under `artifacts/`. (`src/zen_core/run_tools.rs:874-887`,
  `src/control_plane/workspace.rs`)
- **Trace privacy holds.** `Trace` is metadata only (spec/answer/confidence/band, token counts, tool names,
  folded error classes); `record_decision_input` (the one state-bearing writer) is a strict opt-in to a separate
  directory; files are 0600 in 0700 dirs; retention prunes oldest. The only privacy gaps are H1 (read path) and
  L5 (id collision), not the write path. (`src/control_plane/trace.rs`)
- **Decision-settings merge only touches a 2xx body.** `call_decision` passes a non-2xx runtime response through
  with its own status/body; `merge_control_plane` runs only on the `Ok` path. Gate/skills are stripped from the
  forwarded `PUT`, and `save_decision_settings`/`save_control_plane_settings` round-trip the runtime's
  `backend`/`local`/`online` sub-keys untouched. (`src/runtime/proxy.rs:143-213`,
  `src/gateway/group_manager/{llm,control_plane}.rs`)

---

## Unresolved questions

1. **H2 severity depends on product intent:** is the documented auto-restart-on-crash a hard requirement, or is
   "recover on next idle-sweep + respawn" acceptable? If the latter, the docs/CLAUDE.md wording and the dead
   `launches` field should be corrected rather than the behavior.
2. **M2:** should a deliberate downgrade survive an auto-update window at all? That needs a `Selection` provenance
   bit, which is a small schema change to `runtimes/settings.json` — confirm before adding.
3. Could not exercise the GGUF metadata reader against a real `.gguf` (none on this machine); reviewed by reading
   `src/local_models/gguf.rs` only — the bounded string length and array-skip logic look correct, but this was not
   run end-to-end beyond the crate's own `fake_gguf` test.

Status: DONE_WITH_CONCERNS
Summary: Runtime split, proxy, downloads, SDK auth, and control-plane privacy are largely sound; two High issues
(a `GET /api/traces/:id` `.json` path traversal that can read `config.json`, and an unimplemented crash-recovery
path where a crashed runtime is never restarted and `launches` never counts) plus three Medium correctness gaps
(streaming drops the idle-sweep guard, pinned-slot auto-advance ignores `autoUpdate`, and a logs traversal) should
be fixed before landing.
