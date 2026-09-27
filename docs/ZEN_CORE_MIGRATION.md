# Zen-Core Migration Blueprint

Muc tieu: viet `zen-core` (Rust) de thay the `sema-core` trong luong migrate TS -> Rust, giu tuong thich hanh vi voi cac call-site hien tai.

## Pham vi da dung trong SenClaw

Nguon map: `src-old/agent/AgentPool.ts`, `src-old/agent/PermissionBridge.ts`, `src-old/agent/VirtualWorkerPool.ts`, `src-old/gateway/UIServer.ts`.

- `SemaCore` lifecycle:
  - `createSession(sessionId?)`
  - `processUserInput(prompt)`
  - `pauseSession()`
  - `interruptSession()`
  - `dispose()`
- Runtime mutation:
  - `setWorkingDir()`, `clearWorkingDir()`
  - `updateSkipPermissions()`, `updateThinking()`
  - `reloadSkills()`, `hasSessionToolResults()`
- MCP:
  - `addOrUpdateMCPServer(cfg, "project")`
- Permission roundtrip:
  - event: `tool:permission:request`, `ask:question:request`
  - response: `respondToToolPermission`, `respondToAskQuestion`
- Events:
  - `message:complete`, `state:update`, `session:error`
  - `todos:update`, `compact:start`, `compact:exec`
- Model manager (P1):
  - `setModelConfigPathOverride`
  - `getModelManager()`: add/switch/get/apply model config

## Semantics bat buoc giu (compat contract)

- Listener phai duoc bind truoc `processUserInput` de khong mat event dau.
- `processAndWait` cua AgentPool ket thuc khi nhan `state:update = idle`.
- `state = paused` phai suspend timeout, khong duoc coi la error.
- Permission flow la first-responder-wins; request chi resolve 1 lan.
- MCP server add la best-effort: fail thi warn, khong duoc lam sap startup.
- Timeouts can giu:
  - createSession: 60s (main agent path)
  - add MCP: 30s (workspace MCP 10s)
  - processAndWait inactivity timeout: 30 minutes (reset theo activity)

## Priority implementation

### P0 (chay duoc he thong)

- `zen_core::ZenCore` trait va event bus.
- Session lifecycle + pause/resume/interrupt.
- MCP registration runtime.
- Permission request + response bridge.
- Event phat toi AgentPool: `message:complete`, `state:update`, `session:error`.

### P1 (parity van hanh)

- `todos:update`, `compact:*`, `has_session_tool_results`.
- `reload_skills`, `update_thinking`, `update_skip_permissions`.
- Model manager API parity cho UI.

### P2 (hardening)

- Retry taxonomy cho session error.
- Toi uu race giua MCP init va workspace switching.
- Snapshot/session persistence full parity.

## Tang Rust da khoi tao

Da tao module `src/zen_core/mod.rs`:

- Type payload cho toan bo event tren.
- `McpServerConfig`, `ZenCoreOptions`.
- Trait `ZenCore` (contract 1:1 voi call-site hien tai).
- `ZenCoreHandlers` de AgentPool adapter gan callback.

Module nay la "API contract" de implement engine that su ben trong (OpenAI/OpenRouter/Ollama + tool runner + MCP subprocess manager) ma khong lam vo layer gateway/agent pool.

## Next steps de code tiep

1. Tao `ZenCoreRuntime` implement `ZenCore` (in-memory session + event dispatch).
2. Port MCP subprocess manager tu luong TS (`stdio` transport, scope project).
3. Noi `AgentPool::CoreApi` -> adapter goi `ZenCoreRuntime`.
4. Thay `RuntimeCoreApi` placeholder bang `ZenCoreRuntime` trong `run_daemon`.
5. Them integration tests:
   - process_and_wait resolve on idle
   - permission request/resolution
   - MCP add timeout fallback

