#!/usr/bin/env bash
# Shared harness for the browser engine v2 end-to-end checks.
#
# An isolated SenClaw daemon (scratch HOME, ports 28788/28789) with three
# runtimes installed from local packages — sen-browser (this branch),
# sen-sysone and sen-mlx (copies of the installed packages) — and models
# linked READ-ONLY from where they already are: laya-browser from the Laya-jev
# checkout, the multilingual Laya checkpoint and a small MLX chat model from
# ~/.senclaw/local-models. A fixture site (serve.py) is on 127.0.0.1:28795.
#
# Never touches the live daemon (18788/18789) or writes under ~/.senclaw.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../../.." && pwd)"                  # …/SenClaw
E2E_DIR="${E2E_DIR:-${TMPDIR:-/tmp}/senclaw-browser-e2e}"
SCRATCH_HOME="$E2E_DIR/home"
SC="$SCRATCH_HOME/.senclaw"
UI_PORT=28788
WS_PORT=28789
FIXTURE_PORT=28795
API="http://127.0.0.1:$UI_PORT"
FIXTURE="http://127.0.0.1:$FIXTURE_PORT"

SENCLAW_BIN="${SENCLAW_BIN:-$ROOT/.worktrees/senclaw-browser/target/debug/senclaw}"
SEN_BROWSER_REPO="$ROOT/sen-browser"
LAYA_BROWSER_SRC="${LAYA_BROWSER_SRC:-$ROOT/../Laya-jev/models/laya-browser}"
REAL_MODELS="$HOME/.senclaw/local-models"
REAL_RUNTIMES="$HOME/.senclaw/runtimes"
MLX_MODEL="${MLX_MODEL:-mlx-community__Qwen3.5-2B-OptiQ-4bit}"

log() { printf '[e2e] %s\n' "$*" >&2; }
die() { log "FAIL: $*"; exit 1; }

# The daemon and CLI see only the scratch HOME and the test ports.
scratch_env() {
  env -i HOME="$SCRATCH_HOME" USER="${USER:-e2e}" TMPDIR="${TMPDIR:-/tmp}" \
    PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin" LANG=en_US.UTF-8 \
    SENCLAW_UI_PORT=$UI_PORT SENCLAW_WS_PORT=$WS_PORT SENCLAW_UI_BIND_HOST=127.0.0.1 \
    RUST_LOG="${RUST_LOG:-info}" "$@"
}

senclaw() { scratch_env "$SENCLAW_BIN" "$@"; }

api() { # api METHOD PATH [JSON]
  local method=$1 path=$2 body=${3:-}
  if [[ -n "$body" ]]; then
    curl -sS --max-time "${API_TIMEOUT:-900}" -X "$method" -H 'content-type: application/json' --data "$body" "$API$path"
  else
    curl -sS --max-time "${API_TIMEOUT:-60}" -X "$method" "$API$path"
  fi
}

# A connect probe: `lsof` can take minutes on a busy machine.
port_free() { ! python3 -c 'import socket,sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 0.5)' "$1" 2>/dev/null; }

# A directory of symlinks to each file of $1, so anything a runtime writes
# lands in the scratch copy and never in the source.
link_tree() {
  local src=$1 dst=$2
  mkdir -p "$dst"
  (cd "$src" && find . -type f) | while read -r f; do
    mkdir -p "$dst/$(dirname "$f")"
    ln -sf "$src/${f#./}" "$dst/${f#./}"
  done
}

prepare() {
  [[ -x "$SENCLAW_BIN" ]] || die "daemon binary missing: $SENCLAW_BIN"
  [[ -d "$LAYA_BROWSER_SRC" ]] || die "laya-browser checkpoint missing: $LAYA_BROWSER_SRC"
  for p in $UI_PORT $WS_PORT $FIXTURE_PORT; do port_free "$p" || die "port $p is busy"; done
  mkdir -p "$E2E_DIR" "$SC"

  # sen-browser from this branch, packaged the way `make package` lays it out.
  # SEN_BROWSER_PROFILE=release builds what a release ships (timing runs);
  # SEN_BROWSER_PKG=<dir> installs an already-built package instead (a
  # released version to compare against).
  local pkg="$E2E_DIR/pkg/sen-browser"
  rm -rf "$pkg" && mkdir -p "$pkg/bin"
  if [[ -n "${SEN_BROWSER_PKG:-}" ]]; then
    cp "$SEN_BROWSER_PKG/bin/sen-browser" "$pkg/bin/sen-browser"
    cp "$SEN_BROWSER_PKG/senclaw-runtime.json" "$pkg/senclaw-runtime.json"
  else
    local profile="${SEN_BROWSER_PROFILE:-debug}" release=""
    if [[ "$profile" == release ]]; then release="--release"; fi
    (cd "$SEN_BROWSER_REPO" && cargo build --quiet --bin sen-browser $release)
    cp "$SEN_BROWSER_REPO/target/$profile/sen-browser" "$pkg/bin/sen-browser"
    cp "$SEN_BROWSER_REPO/senclaw-runtime.json" "$pkg/senclaw-runtime.json"
  fi

  if [[ ! -d "$SC/runtimes/sen-browser" ]] || ! cmp -s "$pkg/bin/sen-browser" "$(ls -d "$SC"/runtimes/sen-browser/*/ | head -1)bin/sen-browser"; then
    rm -rf "$SC/runtimes/sen-browser"
    senclaw runtime install-local "$pkg" >/dev/null
  fi
  for id in sen-sysone sen-mlx; do
    if [[ ! -d "$SC/runtimes/$id" ]]; then
      local version
      version=$(ls "$REAL_RUNTIMES/$id" | sort -V | tail -1)
      senclaw runtime install-local "$REAL_RUNTIMES/$id/$version" >/dev/null
    fi
  done
  senclaw runtime select browser sen-browser >/dev/null
  senclaw runtime select decision sen-sysone >/dev/null
  senclaw runtime select mlx sen-mlx >/dev/null
  # A test must not upgrade the runtimes under it.
  python3 - "$SC/runtimes/settings.json" <<'PY'
import json, sys
path = sys.argv[1]
s = json.load(open(path))
s["autoUpdate"] = False
json.dump(s, open(path, "w"), indent=2)
PY

  # Models, linked read-only.
  local laya="$SC/local-models/laya"
  if [[ ! -f "$laya/laya-browser/senclaw-laya.json" ]]; then
    link_tree "$LAYA_BROWSER_SRC" "$laya/laya-browser"
    python3 - "$laya/laya-browser" "$LAYA_BROWSER_SRC" <<'PY'
import json, os, sys, time
dst, src = sys.argv[1], sys.argv[2]
files = []
for dirpath, _, names in os.walk(dst):
    for n in names:
        p = os.path.join(dirpath, n)
        if n.startswith("senclaw-laya"):
            continue
        files.append({"path": os.path.relpath(p, dst), "size": os.path.getsize(p)})
meta = {"id": "laya-browser", "label": "laya-browser v10s (read-only link)", "kind": "multilingual",
        "source": {"type": "folder", "path": src}, "installed_at": int(time.time() * 1000), "files": files}
open(os.path.join(dst, "senclaw-laya.json"), "w").write(json.dumps(meta, indent=2))
PY
  fi
  if [[ ! -e "$laya/multilingual" && -d "$REAL_MODELS/laya/multilingual" ]]; then
    link_tree "$REAL_MODELS/laya/multilingual" "$laya/multilingual"
    rm -f "$laya/multilingual/senclaw-laya.json"
    sed 's/"label": "/"label": "(read-only link) /' "$REAL_MODELS/laya/multilingual/senclaw-laya.json" > "$laya/multilingual/senclaw-laya.json"
  fi
  if [[ ! -e "$SC/local-models/$MLX_MODEL" ]]; then
    link_tree "$REAL_MODELS/$MLX_MODEL" "$SC/local-models/$MLX_MODEL"
  fi

  python3 - "$SC/config.json" <<'PY'
import json, os, sys
path = sys.argv[1]
cfg = json.load(open(path)) if os.path.exists(path) else {}
cfg["decisionConfig"] = {"backend": "local", "local": {"defaultModel": "multilingual", "autoLoad": True}}
agent = cfg.setdefault("browserAgent", {})
agent.update({"engine": "v2", "defaultDriver": "managed", "decisionBackend": "local",
              "localModel": "laya-browser", "headless": True, "maxSteps": 20, "profile": "e2e"})
json.dump(cfg, open(path, "w"), indent=2)
PY
}

FIXTURE_PID=""
DAEMON_PID=""

start() {
  python3 "$HERE/serve.py" "$FIXTURE_PORT" "$HERE/fixtures" >"$E2E_DIR/fixture.log" 2>&1 &
  FIXTURE_PID=$!
  # `exec` so $! is the daemon itself, not a subshell around it.
  (cd "$E2E_DIR" && exec env -i HOME="$SCRATCH_HOME" USER="${USER:-e2e}" TMPDIR="${TMPDIR:-/tmp}" \
    PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin" LANG=en_US.UTF-8 \
    SENCLAW_UI_PORT=$UI_PORT SENCLAW_WS_PORT=$WS_PORT SENCLAW_UI_BIND_HOST=127.0.0.1 \
    RUST_LOG="${RUST_LOG:-info}" ${SENCLAW_WEB_DIST:+SENCLAW_WEB_DIST="$SENCLAW_WEB_DIST"} \
    ${SENCLAW_RUNTIME_INDEX_URL:+SENCLAW_RUNTIME_INDEX_URL="$SENCLAW_RUNTIME_INDEX_URL"} \
    "$SENCLAW_BIN" start >"$E2E_DIR/daemon.log" 2>&1) &
  DAEMON_PID=$!
  echo "$DAEMON_PID" >"$E2E_DIR/daemon.pid"
  for _ in $(seq 1 120); do
    if curl -fsS "$API/api/auth/status" >/dev/null 2>&1 && curl -fsS "$FIXTURE/" >/dev/null 2>&1; then
      log "daemon up on $UI_PORT/$WS_PORT (log $E2E_DIR/daemon.log), fixture on $FIXTURE_PORT"
      use_local_llm
      return 0
    fi
    kill -0 "$DAEMON_PID" 2>/dev/null || die "daemon exited: $(tail -20 "$E2E_DIR/daemon.log")"
    sleep 0.5
  done
  die "daemon did not come up"
}

# Point the loop's text writer and fallback at the linked MLX model.
use_local_llm() {
  local id
  id=$(api GET /api/llm-config | python3 -c '
import json, re, sys
slug = re.sub(r"[^a-z0-9]+", "-", sys.argv[1].lower()).strip("-")
ids = [c.get("id", "") for c in json.load(sys.stdin).get("configs", [])]
print(next((i for i in ids if i.startswith("local:") and slug in i), ""))' "$MLX_MODEL")
  [[ -n "$id" ]] || die "no local LLM config (is $MLX_MODEL linked?)"
  python3 - "$SC/config.json" "$id" <<'PY'
import json, sys
path, model = sys.argv[1], sys.argv[2]
cfg = json.load(open(path))
cfg.setdefault("browserAgent", {}).update({"textModel": model, "fallbackModel": model})
json.dump(cfg, open(path, "w"), indent=2)
PY
  log "LLM for the loop: $id"
}

stop() {
  if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    # The daemon's own SIGTERM path stops its runtimes (and their Chrome).
    kill -TERM "$DAEMON_PID" 2>/dev/null || true
    for _ in $(seq 1 60); do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 0.25; done
    kill -KILL "$DAEMON_PID" 2>/dev/null || true
  fi
  if [[ -n "$FIXTURE_PID" ]]; then kill "$FIXTURE_PID" 2>/dev/null || true; fi
  # Anything still running with the scratch HOME in its arguments (runtimes,
  # the managed Chrome and its profile) is ours.
  pkill -TERM -f "$SC/" 2>/dev/null || true
  sleep 0.5
  pkill -KILL -f "$SC/" 2>/dev/null || true
}
