# Background tasks: list, view and stop what an agent left running

Request (2026-10-24, from a chat where the agent ran `nohup npx vite … &` and opened the page): when a task starts a
long-running process, the user needs a list of those background tasks to view, manage and stop.

## Findings (code, 2026-10-24, main b45ee4d)

- `tools/bg_jobs.rs` (`BgJobManager`) + `PeekBgJob` / `StopBgJob` exist and are in every agent's tool list, but **nothing
  creates a job**: `Bash` has no `run_in_background`, and both tool descriptions already promise "Bash with background=true".
- A `nohup cmd > log 2>&1 &` inside `Bash` is invisible to the daemon. In the sandbox path the shell is a `setsid` group
  leader (`sandbox/backend/direct.rs`), the run returns when the pipes close, and the group is only signalled on timeout —
  the dev server lives on, owned by nobody, until the machine reboots. The raw path (`bash -c` via `Command::output`) does
  not even make a group and leaks the child on timeout.
- A `cmd &` that does not redirect holds the pipes, so `wait_with_output` blocks until the Bash timeout and then SIGKILLs
  the server the agent just started.
- A tool has no chat id (`ToolContext` carries `agent_id = "main"`); `RunContext` has none either, `QueryConfig.chat_jid` does.
- The watch strip (`/api/watches`, `WatchStrip` in web and desktop) is the precedent for a per-chat strip with a Stop button.

## Decisions

1. One registry, `BgJobManager`, extended with owner chat, process group, ports, origin, exit code. `PeekBgJob` /
   `StopBgJob` work on every entry.
2. Two ways in: `Bash {run_in_background: true}` (daemon spawns, captures output, returns a job id after a short look at
   the first output) and **adoption** — after a foreground `Bash`, anything still alive in the shell's process group is
   registered, and the pipes it still holds keep streaming into the job. Small models keep writing `nohup … &`; adoption is
   what makes the list true for them.
3. A task is "running" while its process group has a live member. Stop = SIGTERM the group, SIGKILL after 2 s; the group
   is only signalled when a recorded pid is still in it (`getpgid`), never by pattern or port.
4. Persisted in `<home>/bg-tasks.json`. A graceful shutdown stops every task (the desktop gives 800 ms before SIGKILL);
   after a crash the next boot re-adopts the entries it can verify, so a ghost is listed, not lost.
5. REST `/api/bg-tasks` (list, output tail, stop, dismiss, stop-all). Clients poll (3 s) like the watch strip.
6. Clients: strip above the composer (this chat's running tasks: title, ports as links, uptime, log, Stop) plus a
   "all background tasks" list for the other chats. Web and desktop.

## Non-goals

Windows: stop works through `taskkill /T`; adoption and port listing are Unix-only. Watches, `background_*` runs,
datagen jobs, DAG tasks and Space Apps keep their own screens. Tasks do not follow a chat deletion yet.

## Acceptance

- `Bash {run_in_background:true}` returns at once with a job id; `PeekBgJob` reads its output; `StopBgJob` ends the group.
- `nohup python3 -m http.server … > log 2>&1 &` in a foreground Bash call appears in `GET /api/bg-tasks` with its port,
  and `POST /api/bg-tasks/:id/stop` frees the port. Same for a `&` that holds the pipes.
- Quitting the daemon leaves no task process behind; killing it with SIGKILL leaves entries the next boot lists.
- Web and desktop show the strip, and Stop works from both.
