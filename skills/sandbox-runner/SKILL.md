---
name: sandbox-runner
description: >-
  Run the user's shell commands and source code isolated from the real machine,
  through the built-in OS sandbox (`senclaw-sandbox` MCP server). Use it when
  they want to try a Python / JavaScript / Bash snippet, compute something with
  code, check whether a command works, run code they pasted from somewhere and
  do not yet trust, install a library and then run something, or when they say
  outright that they want it "in a sandbox / a VM / docker". Examples: "run
  this python for me", "work it out with code", "is this script safe? try it",
  "install pandas then run this file", "run this but don't touch my machine".
  Vietnamese users say the same things as "chạy thử đoạn python này", "chạy
  trong sandbox", "cài pandas rồi chạy", "chạy lệnh này nhưng đừng động vào máy
  tôi".
version: 1.0.0
when-to-use: >-
  When code or a command should actually execute — and the host machine must
  stay untouched. Prefer sbx_* over a raw shell for anything untrusted, for
  real Python/Node (the QuickJS js_eval sandbox has no filesystem or network,
  and brush bash has no external programs), for package installs, and for
  serving an app on a port without giving it the internet.
triggers:
  - chạy thử đoạn code
  - chạy đoạn python
  - chạy code python
  - chạy thử lệnh
  - thực thi lệnh
  - chạy cách ly
  - chạy an toàn
  - sandbox
  - chạy trong docker
  - test đoạn code
  - tính toán bằng python
  - cài thư viện rồi chạy
  - run this code
  - run python
  - execute command
  - run in sandbox
  - isolated execution
  - code interpreter
  - sandbox dùng bao nhiêu ram
  - tiến trình đang chạy trong sandbox
  - dừng tiến trình
  - kill process
  - mount thư mục
  - cho sandbox đọc thư mục
  - theo dõi hoạt động
  - đoạn mã này đụng vào gì
  - script này làm gì
  - nó gọi mạng đi đâu
  - trace
  - mở cổng cho sandbox
  - chạy app trong sandbox
  - serve trên cổng
  - open a port
  - run a server in the sandbox
  - chỉ cho gọi 1 web
  - giới hạn app chỉ vào một trang
  - chặn app gọi ra ngoài
  - only allow one website
  - restrict outbound to one site
mcp_servers:
  - senclaw-sandbox
allowed-tools:
  - sbx_capabilities
  - sbx_run
  - sbx_create
  - sbx_list
  - sbx_exec
  - sbx_run_in
  - sbx_install
  - sbx_update
  - sbx_delete
  - sbx_files
  - sbx_file_read
  - sbx_file_write
  - sbx_stats
  - sbx_kill
  - sbx_mount
  - sbx_unmount
  - sbx_fs_mode
  - sbx_settings
  - sbx_ports
  - sbx_trace
  - sbx_events
  - sbx_runs
metadata:
  openclaw:
    os: [darwin, linux, win32]
---

# Sandbox Runner

You drive the OS sandbox **built into the daemon** through the
`senclaw-sandbox` MCP server. Full tool names look like
`mcp__senclaw-sandbox__sbx_*`. (This replaces the old Space-App server
`mcp__sandbox-mcp__*` — same tools, new home.)

Isolation is enforced by the operating system: macOS Seatbelt, Linux
bubblewrap, Windows AppContainer — or a Docker container. Users manage the
same sandboxes visually at **Plugins → Sandbox** in the Web UI, including the
enforcement switches (agent exec / Python / Node.js / scheduler scripts).

## First decision: one shot or several steps

Most requests fall on the left.

| Situation | Use |
|---|---|
| Run one snippet, read the result, done | `sbx_run` — throwaway sandbox, auto-deleted |
| Several steps, keeping files or installed packages | `sbx_create`, then `sbx_run_in` / `sbx_exec` |

Do not `sbx_create` for a single calculation. Sandboxes made and never deleted
pile up.

## Normal sequence

1. **`sbx_run`** with `language` + `code`. That is usually the whole job — no
   setup call needed. If the machine cannot run it, the error says why and how
   to fix it.
2. If the error says a backend is unavailable → `sbx_capabilities` for the
   detail, then tell the user the one thing they need to do (usually: start
   Docker Desktop).
3. Multi-step work: `sbx_create` → `sbx_file_write` (feed data in) →
   `sbx_run_in` / `sbx_exec` → `sbx_delete` when finished.

If a run is refused with "switched off (Plugins → Sandbox)", the user has
disabled that runtime in the Web UI (Plugins → Sandbox). Say so and point them there —
do not look for another way to run the code.

## Tools

| Tool | For |
|---|---|
| `mcp__senclaw-sandbox__sbx_capabilities` | What isolation this machine supports |
| `mcp__senclaw-sandbox__sbx_run` | **Default** — run a snippet, then clean up |
| `mcp__senclaw-sandbox__sbx_create` | Create a long-lived sandbox |
| `mcp__senclaw-sandbox__sbx_list` | List existing sandboxes |
| `mcp__senclaw-sandbox__sbx_exec` | Shell command in an existing sandbox |
| `mcp__senclaw-sandbox__sbx_run_in` | Snippet in an existing sandbox |
| `mcp__senclaw-sandbox__sbx_install` | Install pip / npm / apt packages |
| `mcp__senclaw-sandbox__sbx_file_write` | Put data into the sandbox |
| `mcp__senclaw-sandbox__sbx_file_read` | Read a result file |
| `mcp__senclaw-sandbox__sbx_files` | List files |
| `mcp__senclaw-sandbox__sbx_update` | Network on/off, CPU/RAM limits |
| `mcp__senclaw-sandbox__sbx_delete` | Delete a sandbox |
| `mcp__senclaw-sandbox__sbx_runs` | Run history |
| `mcp__senclaw-sandbox__sbx_stats` | CPU/RAM in use + running processes |
| `mcp__senclaw-sandbox__sbx_kill` | Stop one process, or all of them |
| `mcp__senclaw-sandbox__sbx_mount` | Mount a real folder into the sandbox |
| `mcp__senclaw-sandbox__sbx_unmount` | Unmount it again |
| `mcp__senclaw-sandbox__sbx_fs_mode` | Change a sandbox's disk read isolation |
| `mcp__senclaw-sandbox__sbx_settings` | Read/change the sandbox defaults |
| `mcp__senclaw-sandbox__sbx_trace` | Turn activity tracing on/off (testing) |
| `mcp__senclaw-sandbox__sbx_events` | Read the file/process/network events |
| `mcp__senclaw-sandbox__sbx_ports` | Open specific ports (serve / dial out / one local service) while the rest stays shut |

## Opening ports (running an app in a sandbox)

The network switch is all-or-nothing. `sbx_ports` is the middle ground: closed
except what you name.

- `listen: [8000]` — the sandbox may serve on 8000, and **you reach it at
  `http://127.0.0.1:8000`**. This is how you run someone's app in a sandbox and
  look at it in a browser.
- `connect: [443]` — the only remote **port** it may dial out to. HTTPS and
  nothing else.
- `loopback: [8899]` — the only service **on this machine** it may call. Empty
  (the default) means none at all.

All three lists **replace** the current ones; send them complete. Empty lists
close everything again. Listening ports must be 1024 or above.

You do not need `network: true` for this — the port rules are the whole
permission, which is the point: an app that serves on 8000 does not also get to
phone home.

**`connect` is per port, never per host.** `connect: [443]` means *every* site
on 443, not one site — macOS's sandbox language cannot express a host at all
(measured: it refuses anything but `*` and `localhost`). To restrict a sandbox
to one website, see the pattern below.

**This machine's own services are closed even when `network: true`.** That is
deliberate: SenClaw's REST API on loopback needs no credentials, so a sandbox
that could reach it would simply ask the daemon for the files it is forbidden to
read — and could create itself a second, unrestricted sandbox. Name a port in
`loopback` when the sandbox genuinely needs a local service.

## Running an app inside a sandbox

Two things that will otherwise waste your time, both measured:

- **Start long-lived servers as `( cmd < /dev/null > log 2>&1 & )`.** A plain
  `&` keeps `sbx_exec` blocked until its deadline, and the deadline kills the
  whole process group — server included. The subshell form returns immediately
  and the server survives.
- **Do not mount the app read-only and expect it to run.** Anything that writes
  next to its own code (a SQLite file, a lock, a cache) fails. Copy the app into
  the sandbox workspace, which is writable, and mount only its *data* read-only.

## Restricting a sandbox to ONE website

The port rules cannot do it, so put an allowlisting HTTP proxy on this machine
and make it the sandbox's only door:

1. `sbx_ports` with `connect: []` (no direct egress) and
   `loopback: [<proxy port>]`.
2. Run the proxy outside the sandbox; it decides which hostnames are allowed.
3. Give the sandbox `HTTPS_PROXY=http://127.0.0.1:<proxy port>`.

Anything that ignores the proxy hits the sandbox wall, so the failure mode is
closed. With no `connect` ports the sandbox also gets no resolver, which closes
the DNS-tunnel channel too. Verified end to end in
`docs/sandbox-security-experiment.md`.

**Name resolution**: a sandbox gets a resolver only when it may dial out
(`network: true` or a non-empty `connect`). Opening `connect: [53]` is not how
DNS works on macOS — resolution goes through a local socket, which the engine
grants along with outbound permission.

**Enforcement differs by backend, and the reply tells you.** On macOS both
directions are exact. On docker and Linux, opening a listening port gives the
sandbox a network, so outbound is *not* limited to the `connect` list — the
`note` field in the reply says so, and you should pass that on rather than
implying the restriction held.

## Three levels of disk READ isolation

Writes are always confined to the sandbox directory. Reads have three levels,
set with `sbx_fs_mode` or at creation with `fsMode`:

| Level | The sandbox can read |
|---|---|
| `strict` (**default**) | Its own directory + mounted folders + system libraries |
| `allowlist` | As `strict`, plus folders declared in settings |
| `open` | The whole disk, except `~/.ssh`, `~/.aws`, Keychain, SenClaw data |

**Do not reach for `open`.** The `strict` default means the code cannot read
the user's data, which is the point of the sandbox. When a snippet needs one
particular folder, the right move is `sbx_mount` for that folder (read-only if
that is enough) — **not** opening the whole disk. Use `open` only when the user
explicitly asks for it.

The docker backend ignores this setting — a container is already fully isolated.

`sbx_settings` changes defaults for **new** sandboxes; existing ones keep
theirs. Note that its `allowlist` **replaces** the whole list rather than adding
to it — read the current list first, then send it back complete.

## Two backends

| | `direct` | `docker` |
|---|---|---|
| Needs | nothing | a running Docker daemon |
| Start-up | instant | seconds, plus a first-time image pull |
| Blocks writes outside the sandbox | yes | yes |
| Blocks reading the rest of the disk | yes, at `strict`/`allowlist` (default) | yes |
| Enforced RAM ceiling | only on Windows | yes |

**Leaving `backend` empty is right almost always** — the engine picks what the
machine can actually do.

## Rules

- **The network is OFF by default.** Only turn it on (`network: true`) when the
  work needs it, and **say so when you do**. Untrusted code plus a network is
  how data leaves.
- **Installing packages needs the network.** `sbx_install` refuses while it is
  off; turn it on with `sbx_update` first and explain why.
- **Read `isolation` in the result.** It reports what actually confined that
  run: `seatbelt`, `bubblewrap`, `appcontainer`, `container` or `degraded`.
- **`degraded` means NO barrier at all.** The machine lacks an isolation tool.
  Tell the user before running anything else, not after.
- **Mount read-only by default**, and do not mount anything when the code is
  untrusted.
- **"File not found" when the file plainly exists** is usually `strict`
  blocking the read, not a wrong path. Mount that folder; do not switch to
  `open`.
- **`purge: true` deletes files irreversibly.** Only for sandboxes you created
  yourself, or when the user asks for a full delete. Sandboxes named
  `agent:<folder>` back the exec-enforcement feature — leave them alone.

## Reading a result

- `ok` — finished with exit code 0
- `exitCode` — `null` means killed (timed out), not success
- `timedOut` — over the deadline; **output printed earlier may be lost**, so do
  not conclude the code is wrong. Say it timed out and offer a larger
  `timeoutMs`
- `truncated` — output was cut; the tail may be missing
- `isolation` — what actually confined it
- `stdout` / `stderr`

Present the result first, isolation second; one line is enough. Do not paste
the user's own code back at them.

## Activity tracing, for testing

Off by default. Turn it on with `sbx_trace`, **run the code again**, then read
`sbx_events`.

It records file reads and writes, process launches with argv, network
connections with addresses, and hostname lookups. Filter with `kind` =
`file` | `proc` | `net`, or `runId` for a single run.

It works through in-process hooks (Python `sys.addaudithook`, Node `--require`)
plus a directory diff, so other languages still show their file writes. Hooks
are inherited by child processes.

**This is an observation tool for testing, NOT security evidence.** The hook
runs inside the sandbox and the log lives in the sandbox directory — code that
deliberately hides can evade it. Never tell the user "the log is clean, so this
code is safe." What actually stops hostile code is the sandbox itself, enforced
by the kernel.

Use it to answer "what did this code *actually* touch", and to point out
suspicious behaviour — an installer that reads `~/.aws`, or a script that calls
out to an unfamiliar domain.

## When Docker is not running

`sbx_capabilities` returns `docker.detail` with the measured reason — usually
"The Docker CLI is present but the daemon is not answering".

Tell the user to start Docker Desktop, then call `sbx_capabilities` again with
`refresh: true`. **Do not** conclude the machine cannot sandbox — the `direct`
backend works on nearly every macOS and Linux machine.

## Language

Tool results are English. Reply in whatever language the user is writing in.
