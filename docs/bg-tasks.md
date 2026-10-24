# Background tasks: what an agent left running

A *background task* here is an OS process (or a few) that an agent started from a chat and left running — a dev server, a watcher, `python -m http.server`.
It is **not** a scheduled background run (`background_*`, Background → Scheduled) and not a watch (`schedule_watch`); those have their own screens.

## What the person sees

- **Strip above the composer** (web and desktop): this chat's running tasks — the agent's one-line description, the first line of the command, uptime, a 🔒 when
  the command ran in the OS sandbox, a chip per listening port (opens `http://<host>:<port>/`), **Log** (the end of the output, followed while it runs) and **Stop**.
  A line under it says how many run in *other* chats, with a link to the full list.
- **Background → Processes** (web tab, desktop card): every task of every chat, *Show finished*, *Clear finished*, *Stop all*.
- A count badge on the Background button of the sidebar / nav rail.

Stop sends SIGTERM to the whole process group, SIGKILL after 2 s. Closing a chat stops its tasks; quitting the daemon stops all of them.

## How a task gets in

| way | what happens |
|---|---|
| `Bash {run_in_background: true}` | The daemon starts the shell (in the OS sandbox when Plugins → Sandbox → exec is on), captures its output into the job, looks at the first 2 s and answers with the job id (and the ports it already listens on). A command that dies in those 2 s is reported as a failed start, not a task. |
| a foreground `Bash` that leaves something running (`nohup … &`, `cmd &`) | The run ends when the *shell* does. What is still alive in its process group is registered ("adopted"); pipes it still holds keep feeding the job's output. The result the model reads gets a line naming the job id, so it can `PeekBgJob` / `StopBgJob` it and does not start a second one. |
| after a daemon crash | `bg-tasks.json` lists what was running; the next boot re-registers the entries whose recorded pids are still in their group (`restored`, output not kept). |

The model's own tools: `PeekBgJob {job_id, wait}` (output so far), `StopBgJob {job_id}`.

## REST

| route | |
|---|---|
| `GET /api/bg-tasks?chatJid=&includeFinished=` | tasks, newest first (`pid`, `pids`, `ports`, `status`, `origin`, `isolation`, `uptimeSecs`, `exitCode`, …) |
| `GET /api/bg-tasks/:id?tail=N` | one task with the end of its output (default 8 000 bytes) |
| `POST /api/bg-tasks/:id/stop` | 200, or 409 when it is not running / did not exit |
| `DELETE /api/bg-tasks/:id` | forget a finished task (409 while running) |
| `POST /api/bg-tasks/stop-all?chatJid=` | stop every running task, of one chat or all |
| `POST /api/bg-tasks/clear-finished?chatJid=` | forget every finished task |

## Limits

- Unix only for adoption, process groups and port detection (`ps`, `lsof`); on Windows Stop works through `taskkill /T`.
- A command that redirects its output to a file has no log in the list (the model and the person can read the file).
- Output is kept 1 MiB per task, in memory; finished tasks are kept 6 h / 50 entries.
- A process that leaves the group on purpose (`setsid cmd &`) is not found.

## Verifying

`plans/reports/bg-tasks-e2e/` drives the real daemon with a scripted mock LLM: four ways of starting a server, Peek/Stop from the agent, REST, shutdown, SIGKILL +
restore, chat deletion, with and without the Seatbelt sandbox (`up.sh`, `e2e.mjs scenarios | start-two | restored`).
