---
name: bash-executor
description: Run Bash scripts in a secure pure-Rust sandbox (brush) — no environment, empty PATH (external programs like ls/curl/rm by name are blocked), a temp working dir, and a kill-enforced timeout. Use for shell logic, arithmetic, string/text processing and quick scripting without touching the host. NOT an OS jail (absolute-path binaries can still be reached), so treat untrusted input accordingly.
version: 1.0.0
when-to-use: When you need to actually execute Bash to compute or verify a result — loops, conditionals, arithmetic ($((...))), parameter expansion, string munging, building small pipelines from builtins — rather than reasoning about it on paper. Prefer this over a raw shell for throwaway or untrusted snippets, because the sandbox has no env, an empty PATH (external commands by name fail), and a hard timeout.
triggers:
  - run bash
  - execute bash
  - chạy bash
  - chạy shell
  - thực thi bash
  - bash script
  - shell script
  - sandboxed bash
  - tính bằng bash
mcp_servers:
  - senclaw-js
allowed-tools:
  - Read
  - Write
  - bash_run
  - js_capabilities
metadata:
  openclaw:
    os: [darwin, linux, win32]
---

# Bash Executor — Sandboxed (brush, pure-Rust)

Execute Bash through the built-in code-executor MCP server (`senclaw-js`). The
engine is **brush** — a bash-compatible shell written entirely in Rust —
configured as a sandbox. Each run is **out-of-process** so the timeout can be
enforced by killing a runaway script.

## Sandbox model

- **No environment** is inherited and **`PATH` is empty** → external programs
  referenced by bare name (`ls`, `cat`, `curl`, `rm`, `python`, …) resolve to
  *command not found*. Shell **builtins** and **shell logic** still work:
  `echo`/`printf`/`test`/`read`/`declare`, `for`/`while`/`if`/`case`,
  arithmetic `$(( ))`, parameter expansion `${...}`, command substitution, etc.
- The `exec` / `command` / `enable` builtins are removed.
- Runs in a **temp working directory**; output-redirection cannot overwrite
  existing regular files; output is capped.
- **Hard wall-clock timeout** (default 5s, max 60s) — enforced by killing the
  sandbox child process, so even `while :; do :; done` is stopped.

> Caveat: this is process- + shell-level isolation, **not an OS jail**. A script
> that invokes a binary by *absolute path* (e.g. `/bin/sh`) could still reach it.
> For hard isolation of untrusted input, an OS-level sandbox is still required.

## Tool

Registers on the `senclaw-js` MCP server. Canonical bridge name:

- **`mcp__senclaw-js__bash_run`** `(code, timeout_ms?)` — run a Bash script.
  Returns `{ ok, result, result_type, exit_code, logs, error, timed_out,
  duration_ms }`. `result` is stdout; `logs` are the stderr lines; `ok` is true
  only when `exit_code == 0` and it didn't time out.

If the tool isn't visible, load it first:

```
ToolSearch { query: "select:mcp__senclaw-js__bash_run" }
```

## How to use it

1. **Write builtins-based scripts.** Anything that needs `ls`, `grep`, `sed`,
   `curl`, etc. will fail (empty PATH) — rephrase using shell builtins, or read
   files with the host `Read` tool and pass content in.
2. **Read the outcome.** On success `ok: true`, `result` holds stdout. On
   failure `ok: false`, `exit_code` and `error` explain why, and `logs` carry
   stderr. If `timed_out: true`, the script ran too long and was killed —
   simplify it or raise `timeout_ms`.
3. **Don't claim host effects.** The sandbox can't install packages, hit the
   network, or run system tools. If the user truly needs those, say so and use
   the appropriate host tooling instead of pretending `bash_run` did it.

## Examples

> "Sum 1..10 in bash."
> → `bash_run({ code: "s=0; for i in $(seq 1 10); do s=$((s+i)); done; echo $s" })`
> (note: `seq` is external → blocked; use `for i in {1..10}` or a `while` loop instead)
> → `bash_run({ code: "s=0; i=1; while [ $i -le 10 ]; do s=$((s+i)); i=$((i+1)); done; echo $s" })` → `result: "55"`

> "Uppercase a string and count its chars."
> → `bash_run({ code: "x=hello; echo \"${x^^} ${#x}\"" })` → `result: "HELLO 5"`

> "Format a small report."
> → `bash_run({ code: "for n in alice bob; do printf '%-8s ok\\n' \"$n\"; done" })`
