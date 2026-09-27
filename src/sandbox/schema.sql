-- Sandbox app schema. One row per sandbox, one row per run.

CREATE TABLE IF NOT EXISTS sandboxes (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  backend      TEXT NOT NULL,          -- 'direct' | 'docker'
  image        TEXT,                   -- docker only
  workdir      TEXT NOT NULL,          -- host path, always under workspaces_dir
  network      INTEGER NOT NULL DEFAULT 0,
  cpus         REAL NOT NULL DEFAULT 1.0,
  memory_mb    INTEGER NOT NULL DEFAULT 512,
  pids_limit   INTEGER NOT NULL DEFAULT 256,
  timeout_ms   INTEGER NOT NULL DEFAULT 30000,
  env_json     TEXT NOT NULL DEFAULT '{}',
  -- Host folders bound into the sandbox: [{source,target,readOnly}]
  mounts_json  TEXT NOT NULL DEFAULT '[]',
  -- Read isolation: 'strict' | 'allowlist' | 'open' (see fsmode.rs)
  fs_mode      TEXT NOT NULL DEFAULT 'strict',
  -- Optional activity tracing for testing (see trace.rs). Off by default.
  trace_enabled INTEGER NOT NULL DEFAULT 0,
  -- Opened ports: {"listen":[…],"connect":[…]} (see ports.rs)
  ports_json   TEXT NOT NULL DEFAULT '{}',
  status       TEXT NOT NULL DEFAULT 'stopped',  -- stopped | running | error
  container_id TEXT,
  last_error   TEXT,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  last_used_at INTEGER
);

CREATE TABLE IF NOT EXISTS runs (
  id          TEXT PRIMARY KEY,
  sandbox_id  TEXT NOT NULL,
  kind        TEXT NOT NULL,           -- 'exec' | 'code'
  language    TEXT,
  source      TEXT NOT NULL,           -- command line, or the code that ran
  exit_code   INTEGER,
  stdout      TEXT NOT NULL DEFAULT '',
  stderr      TEXT NOT NULL DEFAULT '',
  truncated   INTEGER NOT NULL DEFAULT 0,
  timed_out   INTEGER NOT NULL DEFAULT 0,
  -- What the OS actually enforced for THIS run: 'seatbelt' | 'bubblewrap' |
  -- 'container' | 'degraded'. Recorded per-run because a sandbox created when
  -- bwrap was installed must not claim bubblewrap after it is removed.
  isolation   TEXT NOT NULL,
  network     INTEGER NOT NULL DEFAULT 0,
  duration_ms INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL,
  FOREIGN KEY (sandbox_id) REFERENCES sandboxes(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_runs_sandbox ON runs(sandbox_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_sandboxes_used ON sandboxes(last_used_at DESC);

-- App-level settings (one row, key 'app'). See settings.rs.
CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- Traced activity, one row per observed event. Linked to the run that caused it.
CREATE TABLE IF NOT EXISTS events (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  sandbox_id TEXT NOT NULL,
  run_id     TEXT,
  ts_ms      INTEGER NOT NULL,
  pid        INTEGER NOT NULL DEFAULT 0,
  source     TEXT NOT NULL,
  kind       TEXT NOT NULL,
  target     TEXT NOT NULL,
  detail     TEXT NOT NULL DEFAULT '',
  FOREIGN KEY (sandbox_id) REFERENCES sandboxes(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_events_sandbox ON events(sandbox_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id);
