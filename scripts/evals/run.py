#!/usr/bin/env python3
"""Run and score SenClaw eval cases against the §13 task schema.

Two things happen here, and they used to be one:

  RUN    a task with `user_scenario.task_instructions` is executed — a fresh
         workspace built from `initial_state`, a one-shot agent
         (`senclaw agent-task`) pointed at it, and its trajectory + a
         control-plane trace recorded under a per-task jid. Nothing is shared
         between tasks: the workspace is a new temp dir and
         SENCLAW_TRAJECTORIES_DIR is redirected, so one task cannot score
         another's leftovers. Credentials and model config are NOT isolated —
         they come from the real home, because the run needs a model (and,
         same as before, the control-plane trace lands under the real
         `~/.senclaw/control-plane/traces/` for the same reason).

  SCORE  `criteria.env_assertions`/`nl_assertions` check the OUTCOME (what the
         workspace and the final answer look like afterwards).
         `reference`/`match` and `rubric` are the older agentevals path-based
         checks, kept for a task that still wants them.

Grade outcomes, not routes. A task that pins the tool sequence fails the first
time the agent finds a different correct way, and passes an agent that made
all the right calls and still broke the file.

Task schema (§13), one JSON file per task in evals/cases/:
  {
    "id": "vi-retry-timeout", "source": "handcrafted", "lang": "vi",
    "difficulty": "L1", "split": "dev",
    "initial_state": {"files": {"app.py": "..."}, "copy": "evals/fixtures/x"},
    "user_scenario": {
      "task_instructions": "Doi timeout tu 5 giay thanh 30 giay trong app.py",
      "known_info": "...", "unknown_info": "..."
    },
    "criteria": {
      "env_assertions": {"app.py": {"contains": ["TIMEOUT_SECONDS = 30"]}},
      "nl_assertions": {"contains": ["..."]},
      "decision_assertions": [{"spec_id": "input.guard.override", "acceptable": ["false"], "forbidden": ["true"]}],
      "veto": ["hallucination"]
    },
    "versions": {"jev_specs": 1, "tools": "Read,Edit,Write", "llm": "auto", "judge": "none"},
    "tools": "Read,Edit,Write,Glob,Grep", "timeoutMs": 300000
  }

Usage:
  python3 scripts/evals/run.py --dry-run                  # validate every task, run nothing
  python3 scripts/evals/run.py [--cases evals/cases] [--binary target/release/senclaw]
                                [--k 3] [--jev-off] [--lang vi]
  python3 scripts/evals/run.py --g1-replay                # G1 prefix regression (needs a running daemon)

Needs agentevals only for `reference` / `rubric` tasks: pip install agentevals
"""
import argparse
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request


# ---------------------------------------------------------------- schema ---

VALID_SOURCES = {"public", "handcrafted", "prod_backflow"}
VALID_DIFFICULTIES = {"L1", "L2", "L3"}
VALID_SPLITS = {"train", "dev", "heldout"}
VALID_VETO = {"hallucination", "policy_violation"}
REQUIRED_FIELDS = ["id", "source", "lang", "difficulty", "split", "user_scenario", "criteria", "versions"]


def validate_task(case, path):
    """Schema-only validation (§13) — no execution. Returns a list of error
    strings; empty means the task is well-formed."""
    errors = []
    for field in REQUIRED_FIELDS:
        if field not in case:
            errors.append(f"{path}: missing required field {field!r}")
    if errors:
        return errors  # the rest assumes these are present

    if not isinstance(case["id"], str) or not case["id"].strip():
        errors.append(f"{path}: id must be a non-empty string")
    if case["source"] not in VALID_SOURCES:
        errors.append(f"{path}: source {case['source']!r} must be one of {sorted(VALID_SOURCES)}")
    if not isinstance(case["lang"], str) or not case["lang"].strip():
        errors.append(f"{path}: lang must be a non-empty string")
    if case["difficulty"] not in VALID_DIFFICULTIES:
        errors.append(f"{path}: difficulty {case['difficulty']!r} must be one of {sorted(VALID_DIFFICULTIES)}")
    if case["split"] not in VALID_SPLITS:
        errors.append(f"{path}: split {case['split']!r} must be one of {sorted(VALID_SPLITS)}")

    us = case.get("user_scenario")
    if not isinstance(us, dict) or not (us.get("task_instructions") or "").strip():
        errors.append(f"{path}: user_scenario.task_instructions is required and must be non-empty")

    crit = case.get("criteria")
    if not isinstance(crit, dict):
        errors.append(f"{path}: criteria must be an object")
    else:
        if not crit.get("env_assertions") and not crit.get("nl_assertions"):
            errors.append(f"{path}: criteria needs at least one of env_assertions/nl_assertions — nothing would be graded")
        for i, da in enumerate(crit.get("decision_assertions", [])):
            if not isinstance(da, dict) or not da.get("spec_id"):
                errors.append(f"{path}: criteria.decision_assertions[{i}] needs a spec_id")
            elif not da.get("acceptable") and not da.get("forbidden"):
                errors.append(f"{path}: criteria.decision_assertions[{i}] ({da.get('spec_id')}) needs acceptable and/or forbidden")
        for v in crit.get("veto", []):
            if v not in VALID_VETO:
                errors.append(f"{path}: veto entry {v!r} must be one of {sorted(VALID_VETO)}")

    if not isinstance(case.get("versions"), dict):
        errors.append(f"{path}: versions must be an object")

    init = case.get("initial_state")
    if init is not None and not isinstance(init, dict):
        errors.append(f"{path}: initial_state must be an object")

    if case.get("trap") is not None and not isinstance(case["trap"], bool):
        errors.append(f"{path}: trap must be a boolean")

    return errors


# ---------------------------------------------------------------- reading ---

def load_jsonl(path):
    with open(path, encoding="utf-8") as f:
        return [json.loads(l) for l in f if l.strip()]


def fetch_latest_turn(daemon, jid):
    base = f"{daemon}/api/chats/{urllib.request.quote(jid, safe='')}/trajectory"
    with urllib.request.urlopen(base) as r:
        turns = json.load(r)["turns"]
    if not turns:
        raise SystemExit(f"no recorded turns for {jid}; enable with PUT {base}/settings {{\"enabled\": true}}")
    with urllib.request.urlopen(f"{base}/{turns[0]['turnId']}") as r:
        return json.load(r)["messages"]


def to_openai_messages(lines):
    """SenClaw lines are already OpenAI-style; drop meta rows."""
    return [l for l in lines if l.get("role") in ("user", "assistant", "tool")]


# -------------------------------------------------------------------- run ---

def safe_id(jid):
    """Same folder mangling as `crate::trajectory::safe` /
    `crate::control_plane::safe_id` — both mangle identically."""
    return "".join(c if (c.isalnum() and c.isascii()) or c in "-_" else "_" for c in jid)


def find_binary(explicit):
    for cand in (explicit, os.environ.get("SENCLAW_BIN"),
                 "target/release/senclaw", "target/debug/senclaw",
                 shutil.which("senclaw")):
        if cand and os.path.exists(cand):
            return os.path.abspath(cand)
    raise SystemExit("senclaw binary not found — build it or pass --binary")


def build_workspace(case, root):
    """A task's fixture, materialised fresh. Never reused between tasks or
    between repeats of the same task (`--k`)."""
    init = case.get("initial_state") or {}
    work = os.path.join(root, "work")
    if init.get("copy"):
        shutil.copytree(init["copy"], work)
    else:
        os.makedirs(work, exist_ok=True)
    for rel, content in (init.get("files") or {}).items():
        dest = os.path.join(work, rel)
        os.makedirs(os.path.dirname(dest) or work, exist_ok=True)
        with open(dest, "w", encoding="utf-8") as f:
            f.write(content)
    return work


def trace_stats(jid):
    """Read the latest control-plane trace for `jid` back from the real home
    (trace is on by default and, like the trajectory redirect above, is
    deliberately not isolated — it needs the real `~/.senclaw`). Returns
    `None` if no trace was written (old binary, or the turn produced no LLM
    call at all)."""
    base = os.path.expanduser(os.path.join("~", ".senclaw", "control-plane", "traces", safe_id(jid)))
    files = sorted(glob.glob(os.path.join(base, "*.json")))
    if not files:
        return None
    try:
        trace = json.load(open(files[-1], encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    calls = trace.get("llm_calls", [])
    tokens_in = sum(c.get("tokens_in", 0) for c in calls)
    tokens_out = sum(c.get("tokens_out", 0) for c in calls)
    cache_read = sum(c.get("cache_read", 0) for c in calls)
    return {
        "tokensIn": tokens_in,
        "tokensOut": tokens_out,
        "cacheReadTokens": cache_read,
        "cacheHitRatio": (cache_read / tokens_in) if tokens_in else 0.0,
        # A provider-agnostic proxy for cost, not a dollar figure: this repo
        # has no single source of per-provider pricing reachable from Python,
        # and inventing a rate would be exactly the fabricated-result mistake
        # this harness must not make. Net tokens actually billed (cache reads
        # excluded) is real, computed from the real trace.
        "billableTokens": max(0, tokens_in - cache_read) + tokens_out,
        "decisions": trace.get("decisions", []),
    }


def run_case(case, binary, root, jev_off, verbose=False):
    """Execute one task once. Returns (trajectory lines, workspace, final
    text, jid) — `jid` is what `trace_stats`/`fetch_latest_turn`-style lookups
    key on."""
    work = build_workspace(case, root)
    traj_root = os.path.join(root, "trajectories")
    jid = "eval-" + re.sub(r"[^A-Za-z0-9_-]", "-", case["id"])

    env = dict(os.environ)
    # The only state redirect needed: a one-shot has no chat history and its
    # agent data dir is the (fresh) workspace, so the trajectory directory is
    # what would otherwise be shared. HOME is deliberately left alone — the run
    # needs the machine's model config.
    env["SENCLAW_TRAJECTORIES_DIR"] = traj_root
    env["SENCLAW_INTERNAL_AGENT"] = "1"
    if jev_off:
        env["SENCLAW_JEV_OFF"] = "1"

    prompt = case["user_scenario"]["task_instructions"]
    cmd = [binary, "agent-task", "--prompt", prompt,
           "--working-dir", work, "--trajectory", jid]
    if case.get("tools"):
        cmd += ["--tools", case["tools"]]
    if case.get("timeoutMs"):
        cmd += ["--timeout", str(case["timeoutMs"])]
    if case.get("systemPrompt"):
        cmd += ["--system-prompt", case["systemPrompt"]]

    proc = subprocess.run(cmd, env=env, capture_output=True, text=True)
    if verbose and proc.stderr:
        print(proc.stderr, file=sys.stderr)

    turn_dir = os.path.join(traj_root, safe_id(jid))
    files = sorted(glob.glob(os.path.join(turn_dir, "*.jsonl")))
    lines = load_jsonl(files[-1]) if files else []
    return lines, work, proc.stdout.strip(), jid


# ------------------------------------------------------------------ check ---

def check_env_assertions(env_assertions, work):
    bad = []
    for rel, rule in (env_assertions or {}).items():
        path = os.path.join(work, rel)
        exists = os.path.exists(path)
        if rule.get("exists") is False:
            if exists:
                bad.append(f"{rel}: expected it not to exist")
            continue
        if not exists:
            bad.append(f"{rel}: missing")
            continue
        body = open(path, encoding="utf-8", errors="replace").read()
        for needle in rule.get("contains", []):
            if needle not in body:
                bad.append(f"{rel}: missing {needle!r}")
        for needle in rule.get("notContains", []):
            if needle in body:
                bad.append(f"{rel}: still contains {needle!r}")
        if rule.get("matches") and not re.search(rule["matches"], body, re.S):
            bad.append(f"{rel}: does not match /{rule['matches']}/")
    return bad


def check_nl_assertions(nl_assertions, final_text):
    bad = []
    na = nl_assertions or {}
    for needle in na.get("contains", []):
        if needle.lower() not in final_text.lower():
            bad.append(f"final answer: missing {needle!r}")
    for needle in na.get("notContains", []):
        if needle.lower() in final_text.lower():
            bad.append(f"final answer: contains {needle!r}")
    return bad


def check_outcome(criteria, work, final_text):
    """Outcome assertions from `criteria`. Returns a list of failure strings
    (empty = pass). `decision_assertions`/`veto` are not graded here — the
    former needs `--g1-replay` against a live daemon, the latter has no
    automated detector in this harness (see module docs); both are validated
    for shape by `validate_task` so a malformed one is still caught."""
    return check_env_assertions(criteria.get("env_assertions"), work) + check_nl_assertions(criteria.get("nl_assertions"), final_text)


# ------------------------------------------------------------------- main ---

def run_and_score(args, paths):
    cases = []
    for p in paths:
        case = json.load(open(p, encoding="utf-8"))
        if args.lang and case.get("lang") != args.lang:
            continue
        cases.append((p, case))

    binary = None
    if any(c.get("user_scenario", {}).get("task_instructions") for _, c in cases):
        binary = find_binary(args.binary)

    judge = None
    failed = 0
    per_task_attempts = []  # (task_id, [bool, ...]) for Pass@1 / Pass^k

    for path, case in cases:
        name = case["id"]
        attempts = []
        for attempt in range(max(1, args.k)):
            result = {"case": name, "attempt": attempt + 1}
            work = None
            root = None
            jid = None

            root = tempfile.mkdtemp(prefix=f"senclaw-eval-{safe_id(name)}-")
            lines, work, final_text, jid = run_case(case, binary, root, args.jev_off, args.verbose)

            outputs = to_openai_messages(lines)
            result["turnLines"] = len(lines)

            bad = check_outcome(case["criteria"], work or ".", final_text)
            passed = not bad
            result["outcome"] = {"pass": passed, "failures": bad}
            attempts.append(passed)
            if not passed:
                failed += 1

            if case.get("reference"):
                from agentevals.trajectory.match import create_trajectory_match_evaluator
                ev = create_trajectory_match_evaluator(
                    trajectory_match_mode=case.get("match", "subset"),
                    tool_args_match_mode="ignore")
                result["match"] = ev(outputs=outputs, reference_outputs=case["reference"])
            if case.get("rubric"):
                if judge is None:
                    from agentevals.trajectory.llm import (
                        create_trajectory_llm_as_judge, TRAJECTORY_ACCURACY_PROMPT)
                    judge = create_trajectory_llm_as_judge(
                        prompt=TRAJECTORY_ACCURACY_PROMPT + "\n\nRubric: " + case["rubric"],
                        model=os.environ.get("EVALS_JUDGE", "openai:gpt-4.1-mini"))
                result["judge"] = judge(outputs=outputs)

            stats = trace_stats(jid) if jid else None
            if stats:
                result["trace"] = stats

            if args.keep:
                result["workspace"] = work
            else:
                shutil.rmtree(root, ignore_errors=True)
            print(json.dumps(result, default=str))

        per_task_attempts.append((name, attempts))

    total_attempts = sum(len(a) for _, a in per_task_attempts)
    pass_at_1 = (sum(sum(a) for _, a in per_task_attempts) / total_attempts) if total_attempts else 0.0
    pass_hat_k = (sum(1 for _, a in per_task_attempts if all(a)) / len(per_task_attempts)) if per_task_attempts else 0.0
    print(json.dumps({
        "summary": {
            "tasks": len(per_task_attempts),
            "k": args.k,
            "jevOff": args.jev_off,
            "pass@1": round(pass_at_1, 4),
            "pass^k": round(pass_hat_k, 4),
        }
    }))
    return 1 if failed else 0


def run_g1_replay(args):
    """G1 prefix regression: for every recorded decision input
    (`controlPlane.recordDecisionInputs`), re-ask the *current* spec via
    `POST /api/control-plane/decisions/replay` and check the answer lands in
    the matching task's `criteria.decision_assertions`. Needs a running
    daemon with a decision runtime installed — this is a live check, not a
    dry-run."""
    inputs_dir = os.path.expanduser(
        os.environ.get("SENCLAW_DECISION_INPUTS_DIR") or os.path.join("~", ".senclaw", "control-plane", "decision-inputs")
    )
    files = sorted(glob.glob(os.path.join(inputs_dir, "*", "*.json")))
    if not files:
        print("no recorded decision inputs found — enable controlPlane.recordDecisionInputs and run a task first", file=sys.stderr)
        return 1

    # Build spec_id -> [{acceptable, forbidden}] from every task's assertions,
    # regardless of which task originally produced the recording — a
    # regression check compares the *spec*, not "this exact task's turn".
    assertions_by_spec = {}
    for p in glob.glob(os.path.join(args.cases, "*.json")):
        case = json.load(open(p, encoding="utf-8"))
        for da in (case.get("criteria", {}).get("decision_assertions") or []):
            assertions_by_spec.setdefault(da["spec_id"], []).append(da)

    total = 0
    failed = 0
    for f in files:
        rec = json.load(open(f, encoding="utf-8"))
        spec_id = rec["spec"]
        checks = assertions_by_spec.get(spec_id)
        if not checks:
            continue  # nothing to assert for this spec — recorded, not tested
        total += 1
        body = json.dumps({"spec_id": spec_id, "state": rec["state"]}).encode("utf-8")
        req = urllib.request.Request(
            f"{args.daemon}/api/control-plane/decisions/replay", data=body,
            headers={"Content-Type": "application/json"}, method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=10) as r:
                decision = json.load(r).get("decision")
        except (urllib.error.URLError, TimeoutError) as e:
            print(f"{f}: replay request failed: {e}", file=sys.stderr)
            failed += 1
            continue
        answer = (decision or {}).get("answer")
        bad = []
        for c in checks:
            if c.get("acceptable") and answer not in c["acceptable"]:
                bad.append(f"answer {answer!r} not in acceptable {c['acceptable']}")
            if c.get("forbidden") and answer in c["forbidden"]:
                bad.append(f"answer {answer!r} is forbidden")
        print(json.dumps({"file": f, "spec": spec_id, "answer": answer, "pass": not bad, "failures": bad}))
        if bad:
            failed += 1

    print(json.dumps({"summary": {"replayed": total, "failed": failed}}))
    return 1 if failed else 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", default="evals/cases")
    ap.add_argument("--daemon", default=os.environ.get("SENCLAW_UI", "http://127.0.0.1:18788"))
    ap.add_argument("--binary", default=None, help="senclaw binary used to run a task")
    ap.add_argument("--keep", action="store_true", help="keep each task's temp workspace")
    ap.add_argument("--verbose", action="store_true")
    ap.add_argument("--dry-run", action="store_true", help="validate every task's schema; run nothing")
    ap.add_argument("--k", type=int, default=1, help="repeat each task k times for Pass@1 / Pass^k (§13)")
    ap.add_argument("--jev-off", action="store_true", help="ablation baseline: SENCLAW_JEV_OFF=1 for the run")
    ap.add_argument("--lang", default=None, help="only run/validate tasks whose lang matches exactly")
    ap.add_argument("--g1-replay", action="store_true", help="G1 prefix regression against a running daemon (see module docs)")
    args = ap.parse_args()

    paths = sorted(glob.glob(os.path.join(args.cases, "*.json")))
    if not paths:
        print("no cases found", file=sys.stderr)
        return 1

    if args.dry_run:
        total_errors = 0
        for p in paths:
            case = json.load(open(p, encoding="utf-8"))
            errs = validate_task(case, p)
            total_errors += len(errs)
            for e in errs:
                print(e, file=sys.stderr)
        print(json.dumps({"validated": len(paths), "errors": total_errors}))
        return 1 if total_errors else 0

    if args.g1_replay:
        return run_g1_replay(args)

    return run_and_score(args, paths)


if __name__ == "__main__":
    sys.exit(main())
