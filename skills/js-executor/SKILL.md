---
name: js-executor
description: Run JavaScript in a secure, isolated sandbox (QuickJS) with no filesystem, network, or process access. Use for calculations, data/JSON transforms, regex testing, algorithm checks, and verifying small JS logic without touching the host. Every run is bounded by a wall-clock timeout and a memory limit.
version: 1.0.0
when-to-use: When you need to actually execute JavaScript to compute or verify a result — math, array/object/JSON manipulation, string/regex work, date math, quick algorithm prototyping — rather than reasoning about it on paper. Prefer this over the shell `node`/`Bash` for untrusted or throwaway code, because the sandbox cannot read files, reach the network, or spawn processes.
triggers:
  - run javascript
  - execute js
  - run js
  - chạy javascript
  - chạy js
  - thực thi js
  - tính bằng js
  - evaluate javascript
  - js sandbox
  - test regex
  - kiểm tra regex
mcp_servers:
  - senclaw-js
allowed-tools:
  - Read
  - Write
  - js_eval
  - js_eval_ts
  - js_eval_file
  - js_capabilities
metadata:
  openclaw:
    os: [darwin, linux, win32]
---

# JavaScript Executor — Sandboxed

Execute JavaScript through the built-in `senclaw-js` MCP server. The engine is
**QuickJS**, running in-process with **no host bindings**: code cannot read the
filesystem, open a network connection, spawn a process, or read environment
variables. Each evaluation gets a fresh runtime (no state persists) and is
killed if it exceeds the wall-clock timeout or memory limit.

## Tools

Tools register on the `senclaw-js` MCP server. The canonical bridge names are:

- **`mcp__senclaw-js__js_eval`** `(code, timeout_ms?, memory_mb?)` — run a
  snippet. Returns `{ ok, result, result_type, logs, error, timed_out,
  duration_ms }`. `result` is the rendered value of the final
  expression/statement; `logs` collects `console.log/info/warn/error/debug`.
- **`mcp__senclaw-js__js_eval_ts`** `(code, timeout_ms?, memory_mb?)` — same as
  `js_eval`, but the source is **TypeScript**: it is transpiled to JS (types
  stripped, no type-checking) and then run in the sandbox. Use for interfaces,
  generics, enums, `as`/`satisfies` casts.
- **`mcp__senclaw-js__js_eval_file`** `(path, timeout_ms?, memory_mb?)` — read a
  `.js`/`.mjs` file from disk and run it in the same sandbox.
- **`mcp__senclaw-js__js_capabilities`** `()` — describe the limits and what is
  / isn't available. Call it if unsure before running code.

If a tool isn't visible, load it first:

```
ToolSearch { query: "select:mcp__senclaw-js__js_eval,mcp__senclaw-js__js_eval_file,mcp__senclaw-js__js_capabilities" }
```

## How to use it

1. **Return a value.** The result is the value of the **last expression**. To
   return an object literal, wrap it in parentheses so it isn't parsed as a
   block: `({a: 1})`, not `{a: 1}`.
2. **Surface intermediate output** with `console.log(...)` — every line is
   captured in `logs`.
3. **Read the outcome.** On success `ok: true` and `result` holds the value. On
   failure `ok: false` and `error` holds the thrown message + stack. If
   `timed_out: true`, the script ran too long and was killed — simplify it or
   pass a larger `timeout_ms`.
4. **Tune limits only when needed.** Defaults are 5000 ms / 128 MiB. For heavier
   work pass `timeout_ms` (max 60000) and `memory_mb` (max 1024).

## What's available vs blocked

- **Available:** all ECMAScript intrinsics — `Object`, `Array`, `String`,
  `Number`, `BigInt`, `Math`, `JSON`, `Date`, `RegExp`, `Map`, `Set`, `Symbol`,
  `Proxy`, `Reflect`, typed arrays — plus a captured `console`.
- **Blocked:** `fetch` / `XMLHttpRequest` (no network), `require` / `import` /
  `fs` (no filesystem), `process` / env, and `setTimeout` / `setInterval`
  (there is no async event loop — promises resolve synchronously only).

## Examples

> "What's the sum of squares of 1..10 in JS?"
> → `js_eval({ code: "Array.from({length:10},(_,i)=>(i+1)**2).reduce((a,b)=>a+b,0)" })`
> → `result: "385"`

> "Pull the unique domains out of this list of emails."
> → `js_eval({ code: "const e=['a@x.com','b@y.io','c@x.com']; [...new Set(e.map(s=>s.split('@')[1]))]" })`

> "Does /^\\d{4}-\\d{2}-\\d{2}$/ match '2026-06-28'?"
> → `js_eval({ code: "/^\\d{4}-\\d{2}-\\d{2}$/.test('2026-06-28')" })` → `result: "true"`

> "Run my script at /tmp/calc.js"
> → `js_eval_file({ path: "/tmp/calc.js" })`
