#!/usr/bin/env python3
"""End-to-end tests for Datagen against a real, isolated `senclaw` daemon.

The unit tests in src/datagen/ drive each pipeline with a scripted model, and
nothing tests the layers above them: the REST routes, the job registry, owner
scoping, the files a job leaves on disk, a real model's output. This does.

It starts its own daemon (scratch HOME + SENCLAW_HOME + cwd, two free ports,
never 18788/18789/18790), talks REST to it, checks what the pipelines claim
against the files and against its own code (a quote really is in the document,
a stored answer really is the output of the stored program), and stops it.

    python3 scripts/datagen-e2e/run.py --binary target/release/senclaw
    python3 scripts/datagen-e2e/run.py --binary <senclaw> \\
        --llm-url http://192.168.0.93:1234/v1 --llm-model google/gemma-4-e4b
    python3 scripts/datagen-e2e/run.py --list
    python3 scripts/datagen-e2e/run.py --self-test        # no daemon, no model

Two groups of cases:

  offline  no model needed. A dead model URL (connection refused) and a
           blackhole one (accepts, never answers) stand in for one, so
           validation, import, owner scoping, concurrency, cancel and "the
           model is down" are all covered without a real LLM.
  llm      needs --llm-url (any OpenAI-compatible endpoint). Skipped, not
           failed, without it. Asserts invariants a nondeterministic model
           cannot break (grounded quotes, checked answers, kept records), not
           exact counts.

`executable_qa` needs a real OS sandbox, which cannot be nested, so that one
case runs on a second daemon without the network guard (REST only, no chat).

Exit status: 0 when nothing failed. A skip does not count (unless --fail-on-skip). A
case marked `xfail` records a known defect: it is reported XFAIL while it fails and
XPASS once it passes (a failure only with --strict-xfail).
"""
from __future__ import annotations

import argparse
import collections
import hashlib
import json
import os
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

REAL_PORTS = {18788, 18789, 18790}
TERMINAL = {"done", "failed", "cancelled", "interrupted"}
CATALOG_IDS = {"senclaw-flows-vi", "massive-vi", "xquad-vi", "gsm8k", "self-instruct-seed", "dolly-15k"}
SANDBOXES = {"seatbelt", "bubblewrap", "appcontainer", "container", "docker"}
# Seatbelt rule: a stray call to the user's real daemon fails instead of landing.
GUARD_PROFILE = (
    '(version 1)(allow default)'
    + "".join(f'(deny network-outbound (remote tcp "localhost:{p}"))' for p in sorted(REAL_PORTS))
)


class Skip(Exception):
    """The case cannot run here (no model, no sandbox, a prerequisite failed)."""


def expect(cond, msg):
    if not cond:
        raise AssertionError(msg)


# --------------------------------------------------------------------------- helpers


def free_port():
    while True:
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        if port not in REAL_PORTS:
            return port


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def read_jsonl(path):
    with open(path, encoding="utf-8") as f:
        return [json.loads(line) for line in f if line.strip()]


def words(text):
    return re.findall(r"\w+", text.lower())


def best_quote_match(quote, doc_words):
    """(matched words, start index) of the best in-order match of `quote`.

    A word of the quote counts when it appears, in order, inside a window of the
    document no longer than the quote plus two words — the rule the pipeline
    states, written again here so a bug in one is not a bug in both.
    """
    q = words(quote)
    if not q:
        return 0, -1
    best, best_i = 0, -1
    for i in range(len(doc_words)):
        j, hit = i, 0
        for w in q:
            k = j
            while k < len(doc_words) and k - i < len(q) + 2 and doc_words[k] != w:
                k += 1
            if k < len(doc_words) and k - i < len(q) + 2:
                hit += 1
                j = k + 1
        if hit > best:
            best, best_i = hit, i
    return best, best_i


def sentence_index(doc, quote):
    """Index of the sentence of `doc` that holds most of `quote`."""
    sentences = [s for s in re.split(r"(?<=[.!?])\s+", doc.strip()) if s]
    scored = [(best_quote_match(quote, words(s))[0], idx) for idx, s in enumerate(sentences)]
    top = max(scored, default=(0, -1))
    return top[1] if top[0] else -1


def first_number(text):
    m = re.search(r"-?\d[\d,]*\.?\d*", text.replace("$", ""))
    return float(m.group(0).replace(",", "")) if m else None


# --------------------------------------------------------------------------- daemon


class Blackhole:
    """A TCP server that accepts and never answers: a model that hangs."""

    def __init__(self):
        self.sock = socket.socket()
        self.sock.bind(("127.0.0.1", free_port()))
        self.sock.listen(32)
        self.port = self.sock.getsockname()[1]
        self.held = []
        self.alive = True
        threading.Thread(target=self._accept, daemon=True).start()

    def _accept(self):
        while self.alive:
            try:
                conn, _ = self.sock.accept()
                self.held.append(conn)
            except OSError:
                return

    def close(self):
        self.alive = False
        for c in self.held:
            try:
                c.close()
            except OSError:
                pass
        self.sock.close()


class Daemon:
    def __init__(self, binary, root, llm, hang_port, guarded):
        self.binary, self.root, self.guarded = binary, root, guarded
        self.ui, self.ws = free_port(), free_port()
        self.home = os.path.join(root, "home")
        self.sh = os.path.join(root, "sh")
        self.cwd = os.path.join(root, "cwd")
        for d in (self.home, self.sh, self.cwd):
            os.makedirs(d, exist_ok=True)
        base = {"provider": "custom", "adapt": "openai", "apiKey": "", "maxTokens": 4096, "contextLength": 32768}
        configs = [
            dict(base, id="dead", label="dead", baseURL="http://127.0.0.1:9/v1", modelName="none"),
            dict(base, id="hang", label="hang", baseURL=f"http://127.0.0.1:{hang_port}/v1", modelName="none"),
        ]
        active = "dead"
        if llm:
            configs.append(dict(base, id="real", label="real", baseURL=llm["url"], modelName=llm["model"],
                                apiKey=llm.get("key", "")))
            active = "real"
        with open(os.path.join(self.sh, "config.json"), "w") as f:
            json.dump({"llmConfigs": configs, "activeLlmConfigId": active, "activeQuickLlmConfigId": active,
                       "activeCognitiveLlmConfigId": active}, f)
        self.log_path = os.path.join(root, f"daemon-{self.ui}.log")
        self.proc = None

    @property
    def base(self):
        return f"http://127.0.0.1:{self.ui}"

    def start(self):
        env = {k: v for k, v in os.environ.items()
               if not k.startswith("SENCLAW_") and k not in ("HOME",)}
        env.update(HOME=self.home, SENCLAW_HOME=self.sh, SENCLAW_UI_PORT=str(self.ui), SENCLAW_WS_PORT=str(self.ws))
        cmd = [self.binary, "start"]
        if self.guarded and sys.platform == "darwin" and shutil.which("sandbox-exec"):
            cmd = ["sandbox-exec", "-p", GUARD_PROFILE] + cmd
        self.log = open(self.log_path, "w")
        self.proc = subprocess.Popen(cmd, cwd=self.cwd, env=env, stdout=self.log, stderr=subprocess.STDOUT,
                                     start_new_session=True)
        deadline = time.time() + 90
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"daemon exited early ({self.proc.returncode}); see {self.log_path}")
            try:
                with urllib.request.urlopen(self.base + "/api/datagen/catalog", timeout=3) as r:
                    if r.status == 200:
                        return
            except (urllib.error.URLError, OSError):
                time.sleep(0.5)
        raise RuntimeError(f"daemon did not answer within 90 s; see {self.log_path}")

    def pids(self):
        if not self.proc:
            return []
        try:
            out = subprocess.run(["ps", "-o", "pid=", "-g", str(self.proc.pid)], capture_output=True, text=True).stdout
        except OSError:
            return []
        return [int(p) for p in out.split()]

    def stop(self):
        if not self.proc:
            return
        try:
            os.killpg(self.proc.pid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            pass
        try:
            self.proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(self.proc.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        self.log.close()
        self.proc = None


class Api:
    def __init__(self, base):
        self.base = base

    def call(self, method, path, body=None, timeout=30):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method,
                                     headers={"content-type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                status, raw = r.status, r.read()
        except urllib.error.HTTPError as e:
            status, raw = e.code, e.read()
        text = raw.decode("utf-8", "replace")
        try:
            js = json.loads(text)
        except ValueError:
            js = None
        return status, js, text

    def start_job(self, body):
        return self.call("POST", "/api/datagen/jobs", body)

    def job(self, job_id, owner=None):
        q = f"?owner={owner}" if owner else ""
        return self.call("GET", f"/api/datagen/jobs/{job_id}{q}")

    def wait(self, job_id, timeout, owner=None):
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            st, js, _ = self.job(job_id, owner)
            if st == 200 and js:
                last = js["job"]
                if last["state"] in TERMINAL:
                    return last
            time.sleep(1)
        raise AssertionError(f"job {job_id} still {last and last['state']!r} after {timeout} s")

    def run(self, body, timeout=60):
        st, js, text = self.start_job(body)
        expect(st == 200 and js and js.get("ok"), f"start refused: HTTP {st} {text[:300]}")
        return self.wait(js["job"]["jobId"], timeout)


class Ctx:
    def __init__(self, daemons, llm, scale, tmp):
        self.daemons, self.llm, self.scale, self.tmp = daemons, llm, scale, tmp
        self.api = Api(daemons["guarded"].base)
        self.open_api = Api(daemons["open"].base)
        self.shared = {}

    def timeout(self, seconds):
        return seconds * self.scale

    def need(self, key):
        if key not in self.shared:
            raise Skip(f"needs the result of {key}, which did not pass")
        return self.shared[key]

    def need_llm(self):
        if not self.llm:
            raise Skip("no --llm-url")

    def dataset_count(self):
        st, js, _ = self.api.call("GET", "/api/datagen/datasets")
        return len(js.get("datasets", [])) if st == 200 and js else -1


# --------------------------------------------------------------------------- cases

# ----- record checks (pure: the cases call them, and --self-test feeds them broken records)


def check_expand_records(recs, job, seeds, labels):
    expect(len(recs) == job["accepted"], f"{len(recs)} lines for {job['accepted']} accepted")
    texts = [r["text"].strip().lower() for r in recs]
    expect(len(set(texts)) == len(texts), "duplicate texts")
    expect(not set(texts) & {s["text"].lower() for s in seeds}, "a seed came back as a new record")
    for r in recs:
        expect(r["label"] in labels, f"label {r['label']!r}")
        expect(r["id"].startswith(job["jobId"]) and r["text"].strip(), f"bad record {r}")


def check_docqa_record(doc, r):
    dw = words(doc)
    facts = r["supporting_facts"]
    expect(len(facts) >= 2, f"{len(facts)} supporting facts")
    for fact in facts:
        hit, _ = best_quote_match(fact, dw)
        expect(hit >= 0.8 * len(words(fact)), f"fact not in the document: {fact!r}")
    expect(len({sentence_index(doc, f) for f in facts}) >= 2, f"facts from one sentence: {facts}")
    expect(r["question"].strip() and r["answer"].strip(), "empty question or answer")


def check_reasoning_record(r, refs, threshold=0.7, max_iterations=2):
    expect(r["problem"] in refs, f"unknown problem {r['problem']!r}")
    expect(first_number(r["answer"]) == refs[r["problem"]], f"answer {r['answer']!r} != {refs[r['problem']]}")
    expect(min(r["scores"].values()) >= threshold, f"scores {r['scores']}")
    expect(r["iterations"] <= max_iterations, f"{r['iterations']} iterations")


def check_exec_record(r, run_python=True):
    expect(r["isolation"] in SANDBOXES, f"isolation {r['isolation']!r}")
    if run_python:
        out = subprocess.run([sys.executable, "-I", "-c", r["code"]], capture_output=True, text=True, timeout=15)
        expect(out.stdout.strip() == str(r["answer"]).strip(),
               f"stored answer {r['answer']!r} but the program prints {out.stdout.strip()!r}")


CASES = []


def case(cid, group, needs_open=False, xfail=None):
    """Register a case. `xfail` names a known defect: the case is expected to fail,
    is reported as XFAIL (not a failure of the run), and as XPASS once it passes."""
    def deco(fn):
        CASES.append({"id": cid, "group": group, "open": needs_open, "xfail": xfail, "fn": fn,
                      "doc": (fn.__doc__ or "").strip()})
        return fn
    return deco


def final_manifest(job):
    """manifest.json of a finished job. The daemon writes it before it reports the job
    ended (see `manifest_is_final_when_a_job_reports_done`), so one read is enough:
    waiting here for it to catch up would hide a regression from every case that
    reads the file."""
    manifest = json.load(open(os.path.join(job["dir"], "manifest.json")))
    expect(manifest.get("state") == job["state"],
           f"the job is {job['state']!r} but its manifest.json says {manifest.get('state')!r}")
    return manifest


@case("catalog_lists_entries", "offline")
def _(ctx):
    """The catalog lists the six entries and a fresh daemon has installed none."""
    st, js, _ = ctx.api.call("GET", "/api/datagen/catalog")
    expect(st == 200, f"HTTP {st}")
    entries = js.get("entries") or js.get("catalog") or []
    ids = {e["id"] for e in entries}
    expect(CATALOG_IDS <= ids, f"missing {CATALOG_IDS - ids}")
    expect(not any(e.get("installed") for e in entries), "a fresh daemon reports an installed entry")


@case("import_catalog_bundled", "offline")
def _(ctx):
    """Importing the bundled catalog entry writes 144 balanced, hash-matching records without a model."""
    job = ctx.api.run({"pipeline": "import", "name": "flows-vi", "source": {"catalog": "senclaw-flows-vi"}},
                      ctx.timeout(60))
    expect(job["state"] == "done", f"state {job['state']}: {job.get('error')}")
    expect(job["accepted"] == 144 and not job["rejected"], f"accepted {job['accepted']} rejected {job['rejected']}")
    expect(job["llmCalls"] == 0, f"an import made {job['llmCalls']} model calls")
    data = os.path.join(job["dir"], "data.jsonl")
    recs = read_jsonl(data)
    expect(len(recs) == 144, f"{len(recs)} lines")
    labels = collections.Counter(r["label"] for r in recs)
    expect(len(labels) == 12 and set(labels.values()) == {12}, f"labels not 12 x 12: {dict(labels)}")
    prov = final_manifest(job)["provenance"]
    expect(prov["dataSha256"] == sha256_file(data), "manifest dataSha256 differs from the file")
    expect(prov["license"] == "MIT", f"license {prov['license']}")
    if os.name == "posix":
        expect(stat.S_IMODE(os.stat(job["dir"]).st_mode) == 0o700, "dataset folder is not 0700")
        expect(stat.S_IMODE(os.stat(data).st_mode) == 0o600, "data.jsonl is not 0600")
    ctx.shared["flows"] = {"name": "flows-vi", "dir": job["dir"], "recs": recs, "sha": prov["dataSha256"]}


@case("catalog_marks_installed", "offline")
def _(ctx):
    """After the import the catalog says that entry is installed and names the dataset."""
    ctx.need("flows")
    st, js, _ = ctx.api.call("GET", "/api/datagen/catalog")
    entry = next(e for e in (js.get("entries") or js.get("catalog")) if e["id"] == "senclaw-flows-vi")
    expect(entry.get("installed"), "entry not marked installed")
    expect(entry.get("dataset") == "flows-vi", f"dataset {entry.get('dataset')!r}")


@case("records_pagination", "offline")
def _(ctx):
    """The records route pages in order, reports the total, and never returns more than 200 a page."""
    flows = ctx.need("flows")
    st, js, _ = ctx.api.call("GET", "/api/datagen/datasets/flows-vi/records?offset=10&limit=5")
    expect(st == 200 and js["total"] == 144, f"HTTP {st} total {js and js.get('total')}")
    expect([r["id"] for r in js["records"]] == [r["id"] for r in flows["recs"][10:15]], "wrong page")
    st, js, _ = ctx.api.call("GET", "/api/datagen/datasets/flows-vi/records?offset=0&limit=1000")
    expect(st == 200 and 0 < len(js["records"]) <= 200, f"{len(js['records'])} records in one page")
    st, js, _ = ctx.api.call("GET", "/api/datagen/datasets/flows-vi/records?offset=500&limit=5")
    expect(st == 200 and js["records"] == [], "a page past the end is not empty")


INLINE_CSV = (
    "utterance,intent\n"
    "turn on the lights,home\n"
    "turn on the lights,home\n"       # duplicate
    "play some jazz,music\n"
    ",music\n"                        # no text
    "set a timer for ten minutes,home\n"
)


@case("import_inline_file", "offline")
def _(ctx):
    """An inline CSV is mapped, de-duplicated, and recorded with its license and hash, without its content."""
    body = {"pipeline": "import", "name": "inline-csv", "kind": "labeled_text", "license": "private",
            "mapping": {"text": "utterance", "label": "intent"},
            "source": {"file": {"name": "utterances.csv", "content": INLINE_CSV}}}
    job = ctx.api.run(body, ctx.timeout(60))
    expect(job["state"] == "done", f"state {job['state']}: {job.get('error')}")
    expect(job["accepted"] == 3, f"accepted {job['accepted']} rejected {job['rejected']}")
    expect(job["rejected"].get("duplicate") == 1, f"rejected {job['rejected']}")
    expect(sum(v for k, v in job["rejected"].items() if k.startswith("missing")) == 1, f"rejected {job['rejected']}")
    recs = read_jsonl(os.path.join(job["dir"], "data.jsonl"))
    expect({r["text"] for r in recs} == {"turn on the lights", "play some jazz", "set a timer for ten minutes"},
           f"texts {[r['text'] for r in recs]}")
    manifest = final_manifest(job)
    expect("play some jazz" not in json.dumps(manifest), "the file's content leaked into the manifest")
    prov = manifest["provenance"]
    expect(prov["license"] == "private" and prov["source"]["type"] == "file" and prov["source"].get("sha256"),
           f"provenance {prov}")


@case("import_requires_license", "offline")
def _(ctx):
    """An import with no license is refused before a job starts."""
    before = ctx.dataset_count()
    st, js, text = ctx.api.start_job({"pipeline": "import", "name": "nolicense", "kind": "labeled_text",
                                      "source": {"file": {"name": "a.csv", "content": INLINE_CSV}}})
    expect(st in (400, 422), f"HTTP {st}: {text[:200]}")
    expect("licen" in text.lower(), f"the refusal does not mention the license: {text[:200]}")
    expect(ctx.dataset_count() == before, "a refused import left a dataset behind")


@case("import_preview_writes_nothing", "offline")
def _(ctx):
    """The import preview shows columns, a total and records, and writes no dataset."""
    before = ctx.dataset_count()
    st, js, text = ctx.api.call("POST", "/api/datagen/import/preview", {
        "source": {"file": {"name": "utterances.csv", "content": INLINE_CSV}}, "kind": "labeled_text",
        "mapping": {"text": "utterance", "label": "intent"}})
    expect(st == 200, f"HTTP {st}: {text[:200]}")
    expect(js.get("columns") == ["utterance", "intent"], f"columns {js.get('columns')}")
    expect(js.get("total") == 5, f"total {js.get('total')}")
    expect(js.get("records"), "no records in the preview")
    expect(ctx.dataset_count() == before, "the preview wrote a dataset")


@case("import_refuses_loopback_url", "offline")
def _(ctx):
    """An import URL that points at the daemon itself is refused, not fetched."""
    st, js, text = ctx.api.start_job({
        "pipeline": "import", "name": "ssrf", "kind": "labeled_text", "license": "private",
        "source": {"url": f"{ctx.api.base}/api/datagen/catalog"}})
    expect(st in (400, 422), f"HTTP {st}: {text[:200]}")
    st, js, _ = ctx.api.call("GET", "/api/datagen/datasets")
    expect("ssrf" not in {d.get("name") for d in js.get("datasets", [])}, "a refused URL left a dataset behind")


@case("validation_errors", "offline")
def _(ctx):
    """Bad requests get the documented status codes with a useful body."""
    st, _, text = ctx.api.start_job({"pipeline": "expand_seeds", "max_llm_calls": 5, "seeds": ["a"]})
    expect(st == 422 and "unknown field" in text and "maxLlmCalls" in text, f"unknown key: HTTP {st} {text[:160]}")
    st, _, text = ctx.api.start_job({"pipeline": "nope"})
    expect(st == 422, f"unknown pipeline: HTTP {st}")
    expect(ctx.api.call("GET", "/api/datagen/jobs/does-not-exist")[0] == 404, "unknown job is not 404")
    expect(ctx.api.call("POST", "/api/datagen/jobs/does-not-exist/cancel")[0] == 404, "cancel of an unknown job")
    expect(ctx.api.call("GET", "/api/datagen/datasets/nope/records")[0] == 404, "records of an unknown dataset")
    ctx.need("flows")
    st, _, text = ctx.api.start_job({"pipeline": "expand_seeds", "seeds": ["a"], "seedsFrom": {"dataset": "flows-vi"}})
    expect(st == 400 and "seed" in text.lower(), f"seeds + seedsFrom: HTTP {st} {text[:160]}")


@case("owner_scoping", "offline")
def _(ctx):
    """A job started for one chat is invisible to another: list, read, cancel."""
    st, js, text = ctx.api.start_job({"pipeline": "import", "name": "owned", "owner": "chat:a", "kind": "labeled_text",
                                      "license": "private", "mapping": {"text": "utterance"},
                                      "source": {"file": {"name": "a.csv", "content": INLINE_CSV}}})
    expect(st == 200, f"HTTP {st}: {text[:200]}")
    jid = js["job"]["jobId"]
    ctx.api.wait(jid, ctx.timeout(60), owner="chat:a")
    expect(ctx.api.job(jid, owner="chat:a")[0] == 200, "the owner cannot read its job")
    expect(ctx.api.job(jid, owner="chat:b")[0] == 404, "another chat can read the job")
    st, js, _ = ctx.api.call("GET", "/api/datagen/jobs?owner=chat:b")
    expect(st == 200 and not [j for j in js["jobs"] if j["jobId"] == jid], "another chat lists the job")
    expect(ctx.api.call("POST", f"/api/datagen/jobs/{jid}/cancel?owner=chat:b")[0] == 404, "another chat can cancel it")


@case("delete_dataset", "offline")
def _(ctx):
    """Deleting a dataset removes its folder; deleting it again is a 404."""
    job = ctx.api.run({"pipeline": "import", "name": "to-delete", "kind": "labeled_text", "license": "private",
                       "mapping": {"text": "utterance"},
                       "source": {"file": {"name": "a.csv", "content": INLINE_CSV}}}, ctx.timeout(60))
    expect(os.path.isdir(job["dir"]), "dataset folder missing before the delete")
    expect(ctx.api.call("DELETE", "/api/datagen/datasets/to-delete")[0] == 200, "delete was not 200")
    expect(not os.path.exists(job["dir"]), "the folder survived the delete")
    expect(ctx.api.call("DELETE", "/api/datagen/datasets/to-delete")[0] == 404, "second delete was not 404")


@case("model_down_fails_the_job", "offline")
def _(ctx):
    """With the model unreachable a generating job ends `failed` with a reason, not stuck."""
    t0 = time.time()
    job = ctx.api.run({"pipeline": "expand_seeds", "name": "model-down", "profile": "dead", "target": 2,
                       "seeds": ["open the shopee page and look for earphones"]}, ctx.timeout(90))
    expect(job["state"] == "failed", f"state {job['state']}")
    expect(job["accepted"] == 0, f"accepted {job['accepted']}")
    expect(job.get("error"), "a failed job has no error text")
    expect(job["llmCalls"] <= 10, f"{job['llmCalls']} calls to a dead model")
    expect(time.time() - t0 < ctx.timeout(90), "slow to give up")


@case("concurrency_cap_and_cancel", "offline")
def _(ctx):
    """Two generating jobs run at once, a third is refused (409), and a cancel ends a job waiting on a hung model."""
    body = {"pipeline": "expand_seeds", "profile": "hang", "target": 2, "seeds": ["mở trang web tìm giá laptop"]}
    started = []
    for n in (1, 2):
        st, js, text = ctx.api.start_job(dict(body, name=f"hang-{n}"))
        expect(st == 200, f"job {n}: HTTP {st} {text[:160]}")
        started.append(js["job"]["jobId"])
    try:
        st, _, text = ctx.api.start_job(dict(body, name="hang-3"))
        expect(st == 409, f"third job: HTTP {st} {text[:160]}")
    finally:
        for jid in started:
            ctx.api.call("POST", f"/api/datagen/jobs/{jid}/cancel")
    for jid in started:
        job = ctx.api.wait(jid, ctx.timeout(60))
        expect(job["state"] == "cancelled", f"a cancelled job ended {job['state']!r}")


@case("manifest_is_final_when_a_job_reports_done", "offline")
def _(ctx):
    """The instant the API reports a job done, the durable manifest already says so and holds its provenance."""
    stale = []
    for n in range(5):
        st, js, text = ctx.api.start_job({
            "pipeline": "import", "name": f"final-{n}", "kind": "labeled_text", "license": "private",
            "mapping": {"text": "utterance"}, "source": {"file": {"name": "a.csv", "content": INLINE_CSV}}})
        expect(st == 200, f"HTTP {st}: {text[:160]}")
        jid, path = js["job"]["jobId"], os.path.join(js["job"]["dir"], "manifest.json")
        while True:
            job = ctx.api.job(jid)[1]["job"]
            if job["state"] in TERMINAL:
                break
        manifest = json.load(open(path))
        if manifest.get("state") != job["state"] or "provenance" not in manifest:
            stale.append((manifest.get("state"), "provenance" in manifest))
    expect(not stale, f"{len(stale)} of 5: the API said done but manifest.json said (state, has provenance) {stale[:1]}")


@case("checker_detects_tampering", "offline")
def _(ctx):
    """The hash check this suite relies on does fail when a dataset file is changed."""
    flows = ctx.need("flows")
    copy = os.path.join(ctx.tmp, "tampered.jsonl")
    shutil.copy(os.path.join(flows["dir"], "data.jsonl"), copy)
    expect(sha256_file(copy) == flows["sha"], "the untouched copy does not hash to the manifest")
    with open(copy, "a") as f:
        f.write('{"id":"x","text":"added later","label":"none"}\n')
    expect(sha256_file(copy) != flows["sha"], "a changed file still hashes to the manifest")
    expect(best_quote_match("an invented sentence about nothing", words("The tower was built in 1889."))[0] == 0,
           "the quote checker accepted an invented sentence")


# ----- llm group


@case("expand_seeds_labels", "llm")
def _(ctx):
    """expand_seeds returns new, labelled records that respect the labels, the target and the call cap."""
    ctx.need_llm()
    seeds = [{"text": "chia việc làm báo cáo thị trường cho 3 người rồi gộp lại", "label": "team_dag"},
             {"text": "mở trang shopee tìm tai nghe dưới 500k", "label": "browser"},
             {"text": "mỗi sáng 7h tóm tắt email chưa đọc", "label": "scheduled"}]
    body = {"pipeline": "expand_seeds", "name": "expand-e2e", "target": 6, "batch": 3, "language": "Vietnamese",
            "maxLlmCalls": 30, "seeds": seeds,
            "labels": {"team_dag": "split into parts for several specialists, run in parallel, then merge",
                       "browser": "open, read or fill web pages",
                       "scheduled": "repeat or run later at a set time"}}
    job = ctx.api.run(body, ctx.timeout(420))
    expect(job["state"] == "done", f"state {job['state']}: {job.get('error')} {job['rejected']}")
    expect(job["accepted"] >= 3, f"only {job['accepted']} accepted")
    expect(job["llmCalls"] <= 30, f"{job['llmCalls']} calls, cap 30")
    recs = read_jsonl(os.path.join(job["dir"], "data.jsonl"))
    check_expand_records(recs, job, seeds, {"team_dag", "browser", "scheduled"})


@case("doc_qa_grounding", "llm")
def _(ctx):
    """Every doc_qa pair quotes the document in order and draws on at least two different sentences."""
    ctx.need_llm()
    doc = ("The Eiffel Tower was completed in 1889 for the World Fair in Paris. It was designed by the engineering "
           "company of Gustave Eiffel, and it stood as the tallest man-made structure in the world until 1930. The "
           "tower is repainted every seven years, which requires about sixty tonnes of paint. Visitors can climb the "
           "stairs or take the lifts to three levels, and the top platform offers a view of Paris from nearly three "
           "hundred metres above the ground.")
    job = ctx.api.run({"pipeline": "doc_qa", "name": "docqa-e2e", "texts": [doc], "target": 2, "perPassage": 3,
                       "multiHop": True, "verify": True, "maxLlmCalls": 20}, ctx.timeout(420))
    expect(job["state"] == "done", f"state {job['state']}: {job.get('error')} {job['rejected']}")
    recs = read_jsonl(os.path.join(job["dir"], "data.jsonl"))
    expect(recs, "no pair accepted")
    for r in recs:
        check_docqa_record(doc, r)


@case("reasoning_traces_answers", "llm")
def _(ctx):
    """Accepted solutions end in the reference answer (with or without a unit) and clear the score threshold."""
    ctx.need_llm()
    problems = [
        {"problem": "A train travels 60 km in 1.5 hours. At the same speed, how far does it travel in 4 hours?", "answer": 160},
        {"problem": "What is 17 multiplied by 23?", "answer": 391},
        {"problem": "A car travels 90 km in 1.5 hours. What is its average speed in km/h?", "answer": 60},
    ]
    job = ctx.api.run({"pipeline": "reasoning_traces", "name": "reason-e2e", "problems": problems, "target": 3,
                       "maxIterations": 2, "threshold": 0.7, "maxLlmCalls": 40}, ctx.timeout(600))
    expect(job["state"] == "done", f"state {job['state']}: {job.get('error')} {job['rejected']}")
    expect(job["accepted"] >= 2, f"only {job['accepted']} of 3 accepted ({job['rejected']})")
    refs = {p["problem"]: p["answer"] for p in problems}
    for r in read_jsonl(os.path.join(job["dir"], "data.jsonl")):
        check_reasoning_record(r, refs, threshold=0.7, max_iterations=2)


@case("seeds_from_dataset", "llm")
def _(ctx):
    """seedsFrom copies one seed per label out of an on-disk dataset into the job's params, then generates from them."""
    ctx.need_llm()
    ctx.need("flows")
    job = ctx.api.run({"pipeline": "expand_seeds", "name": "from-flows", "seedsFrom": {"dataset": "flows-vi", "perLabel": 1},
                       "target": 3, "batch": 3, "maxLlmCalls": 30}, ctx.timeout(420))
    expect(job["state"] == "done", f"state {job['state']}: {job.get('error')} {job['rejected']}")
    seeds = final_manifest(job)["params"]["seeds"]
    expect(len(seeds) == 12, f"{len(seeds)} seeds for 12 labels x 1")
    expect(len({s["label"] for s in seeds}) == 12, "a label is missing from the seeds")
    expect(job["accepted"] >= 1, "nothing generated from the seeds")


@case("problems_from_dataset", "llm")
def _(ctx):
    """problemsFrom reads a problem_answer dataset and every accepted solution matches that dataset's answer."""
    ctx.need_llm()
    csv = "problem,answer\n" + "".join(f'"What is {a} plus {b}?",{a + b}\n' for a, b in [(12, 30), (45, 17), (8, 9), (101, 99)])
    job = ctx.api.run({"pipeline": "import", "name": "arith", "kind": "problem_answer", "license": "private",
                       "source": {"file": {"name": "arith.csv", "content": csv}}}, ctx.timeout(60))
    expect(job["accepted"] == 4, f"imported {job['accepted']} of 4")
    expected = {a_b[0]: a_b[1] for a_b in
                [(f"What is {a} plus {b}?", a + b) for a, b in [(12, 30), (45, 17), (8, 9), (101, 99)]]}
    job = ctx.api.run({"pipeline": "reasoning_traces", "name": "arith-solved", "problemsFrom": {"dataset": "arith"},
                       "target": 2, "maxIterations": 1, "maxLlmCalls": 30}, ctx.timeout(420))
    expect(job["state"] == "done", f"state {job['state']}: {job.get('error')} {job['rejected']}")
    recs = read_jsonl(os.path.join(job["dir"], "data.jsonl"))
    expect(recs, "nothing solved")
    for r in recs:
        expect(r["problem"] in expected, f"unknown problem {r['problem']!r}")
        expect(first_number(r["answer"]) == expected[r["problem"]], f"{r['answer']!r} for {r['problem']!r}")


@case("cancel_keeps_written_records", "llm")
def _(ctx):
    """Cancelling a running job stops it promptly and keeps the records already written."""
    ctx.need_llm()
    st, js, text = ctx.api.start_job({
        "pipeline": "expand_seeds", "name": "cancel-e2e", "target": 60, "batch": 5, "maxLlmCalls": 60,
        "seeds": ["mở trang web tìm giá laptop", "đặt lịch họp lúc 9h sáng mai", "tóm tắt email chưa đọc"]})
    expect(st == 200, f"HTTP {st}: {text[:200]}")
    jid = js["job"]["jobId"]
    deadline = time.time() + ctx.timeout(300)
    while time.time() < deadline:
        job = ctx.api.job(jid)[1]["job"]
        if job["accepted"] >= 1 or job["state"] in TERMINAL:
            break
        time.sleep(2)
    expect(job["state"] == "running" and job["accepted"] >= 1, f"no record before the deadline: {job['state']}")
    ctx.api.call("POST", f"/api/datagen/jobs/{jid}/cancel")
    final = ctx.api.wait(jid, ctx.timeout(60))
    expect(final["state"] == "cancelled", f"ended {final['state']!r}")
    lines = read_jsonl(os.path.join(final["dir"], "data.jsonl"))
    expect(len(lines) == final["accepted"] >= 1, f"{len(lines)} lines for {final['accepted']} accepted")


@case("executable_qa_runs_in_a_sandbox", "llm", needs_open=True)
def _(ctx):
    """Each executable_qa answer is the stdout of the stored program, and the program ran isolated."""
    ctx.need_llm()
    job = ctx.open_api.run({"pipeline": "executable_qa", "name": "execqa-e2e", "target": 2, "batch": 2,
                            "timeoutMs": 10000, "maxLlmCalls": 20,
                            "seeds": [{"question": "A shop sells pencils at 12 cents each. How many cents do 15 pencils cost?"},
                                      {"question": "What is the sum of the first 10 positive integers?"}]},
                           ctx.timeout(420))
    if job["state"] == "failed" and "isolat" in (job.get("error") or "").lower():
        raise Skip(f"no OS sandbox on this machine: {job['error']}")
    expect(job["state"] == "done", f"state {job['state']}: {job.get('error')} {job['rejected']}")
    recs = read_jsonl(os.path.join(job["dir"], "data.jsonl"))
    expect(recs, "nothing accepted")
    for r in recs:
        check_exec_record(r)


@case("no_traffic_to_the_real_daemon", "offline")
def _(ctx):
    """None of the daemons this run started holds a connection to the real daemon's ports."""
    if not shutil.which("lsof"):
        raise Skip("no lsof")
    pids = sorted({p for d in ctx.daemons.values() for p in d.pids()})
    expect(pids, "no daemon process found")
    out = subprocess.run(["lsof", "-nP", "-a", "-p", ",".join(map(str, pids)), "-iTCP"], capture_output=True, text=True).stdout
    bad = [line for line in out.splitlines() if any(f":{p}" in line.split("->")[-1] for p in REAL_PORTS) and "->" in line]
    expect(not bad, "connection to a real port:\n" + "\n".join(bad))


# --------------------------------------------------------------------------- runner


def self_test():
    """Pure checks of the helpers, no daemon: the quote matcher and the number reader."""
    doc = "The tower was completed in 1889. It is repainted every seven years, which takes sixty tonnes of paint."
    dw = words(doc)
    full, _ = best_quote_match("repainted every seven years", dw)
    expect(full == 4, f"verbatim quote matched {full} of 4 words")
    expect(best_quote_match("the tower was demolished in 1950", dw)[0] < 5, "an invented sentence matched too well")
    expect(best_quote_match("sixty tonnes of paint tower 1889", dw)[0] < 6, "scattered words matched as a quote")
    expect(sentence_index(doc, "repainted every seven years") == 1, "wrong sentence for a quote")
    expect(first_number("$70,000") == 70000 and first_number("160 km") == 160 and first_number("no digits") is None,
           "first_number")
    expect(best_quote_match("", dw) == (0, -1), "an empty quote matched")

    # A check that cannot fail proves nothing: every good record passes, every broken one is caught.
    tried = []

    def caught(fn, *a, **kw):
        tried.append(fn.__name__)
        try:
            fn(*a, **kw)
        except AssertionError:
            return True
        return False

    doc2 = ("The tower was completed in 1889 for the fair. It is repainted every seven years, which takes sixty "
            "tonnes of paint. Visitors climb the stairs to three levels.")
    good = {"question": "When was it finished and how often is it painted?", "answer": "1889; every seven years",
            "supporting_facts": ["The tower was completed in 1889 for the fair.", "It is repainted every seven years"]}
    check_docqa_record(doc2, good)
    expect(caught(check_docqa_record, doc2, dict(good, supporting_facts=[good["supporting_facts"][0], "The tower was demolished in 1950 by a storm."])),
           "doc_qa: an invented supporting fact was accepted")
    expect(caught(check_docqa_record, doc2, dict(good, supporting_facts=["The tower was completed in 1889", "completed in 1889 for the fair"])),
           "doc_qa: two facts from one sentence were accepted as multi-hop")
    expect(caught(check_docqa_record, doc2, dict(good, supporting_facts=good["supporting_facts"][:1])), "doc_qa: a single fact was accepted")
    expect(caught(check_docqa_record, doc2, dict(good, answer=" ")), "doc_qa: an empty answer was accepted")

    refs = {"What is 17 x 23?": 391}
    r_good = {"problem": "What is 17 x 23?", "answer": "391", "iterations": 0,
              "scores": {"correctness": 1.0, "clarity": 0.9, "completeness": 1.0}}
    check_reasoning_record(r_good, refs)
    check_reasoning_record(dict(r_good, answer="$391."), refs)
    expect(caught(check_reasoning_record, dict(r_good, answer="392"), refs), "reasoning: a wrong answer was accepted")
    expect(caught(check_reasoning_record, dict(r_good, scores={"correctness": 1.0, "clarity": 0.4, "completeness": 1.0}), refs),
           "reasoning: a score under the threshold was accepted")
    expect(caught(check_reasoning_record, dict(r_good, iterations=5), refs), "reasoning: too many iterations were accepted")
    expect(caught(check_reasoning_record, dict(r_good, problem="What is 3 x 3?"), refs), "reasoning: an unknown problem was accepted")

    e_good = {"question": "5 x 12?", "answer": "60", "code": "print(5 * 12)", "isolation": "seatbelt"}
    check_exec_record(e_good)
    expect(caught(check_exec_record, dict(e_good, answer="61")), "executable_qa: an answer the program does not print was accepted")
    expect(caught(check_exec_record, dict(e_good, code="print(5 * 13)")), "executable_qa: changed code still matched the answer")
    expect(caught(check_exec_record, dict(e_good, isolation="none")), "executable_qa: an unisolated run was accepted")

    seeds = [{"text": "open the shop page", "label": "browser"}]
    job = {"accepted": 2, "jobId": "abc"}
    recs = [{"id": "abc-1", "text": "find cheap earphones online", "label": "browser"},
            {"id": "abc-2", "text": "look up the weather page", "label": "browser"}]
    check_expand_records(recs, job, seeds, {"browser"})
    expect(caught(check_expand_records, recs[:1], job, seeds, {"browser"}), "expand_seeds: a missing record went unnoticed")
    expect(caught(check_expand_records, [recs[0], dict(recs[1], label="scheduled")], job, seeds, {"browser"}),
           "expand_seeds: an unknown label was accepted")
    expect(caught(check_expand_records, [recs[0], dict(recs[1], text="Open the shop page")], job, seeds, {"browser"}),
           "expand_seeds: a seed returned as a new record was accepted")
    expect(caught(check_expand_records, [recs[0], dict(recs[1], text=recs[0]["text"])], job, seeds, {"browser"}),
           "expand_seeds: duplicate texts were accepted")
    print(f"self-test: ok (helpers, and {len(tried)} deliberately broken records were all caught)")


def pick_binary(arg):
    here = os.path.dirname(os.path.abspath(__file__))
    candidates = [arg, os.environ.get("SENCLAW_BIN"), os.path.join(here, "..", "..", "target", "release", "senclaw"),
                  os.path.join(here, "..", "..", "target", "debug", "senclaw"),
                  "/Applications/SenClaw Desktop.app/Contents/Resources/senclaw"]
    for c in candidates:
        if c and os.path.isfile(c) and os.access(c, os.X_OK):
            return os.path.abspath(c)
    raise SystemExit("no senclaw binary: pass --binary or set SENCLAW_BIN")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--binary", help="path to the senclaw binary under test")
    ap.add_argument("--llm-url", help="OpenAI-compatible base URL, e.g. http://host:1234/v1; enables the llm group")
    ap.add_argument("--llm-model", help="model name for --llm-url")
    ap.add_argument("--llm-key", default="", help="API key for --llm-url (empty for a local server)")
    ap.add_argument("--only", help="comma-separated case ids (or groups: offline, llm)")
    ap.add_argument("--list", action="store_true", help="list the cases and exit")
    ap.add_argument("--self-test", action="store_true", help="check the helpers without a daemon, then exit")
    ap.add_argument("--no-guard", action="store_true", help="do not wrap the main daemon in the macOS network guard")
    ap.add_argument("--timeout-scale", type=float, default=1.0, help="multiply every wait (slow models)")
    ap.add_argument("--report", help="write a JSON report here")
    ap.add_argument("--keep", action="store_true", help="keep the scratch directory")
    ap.add_argument("--fail-on-skip", action="store_true", help="a skipped case is a failure (CI)")
    ap.add_argument("--strict-xfail", action="store_true",
                    help="a known-defect case that now passes is a failure (so its xfail gets removed)")
    args = ap.parse_args()

    if args.self_test:
        self_test()
        return 0
    if args.list:
        for c in CASES:
            print(f"{c['group']:8} {c['id']:34} {c['doc']}{'  [known defect]' if c['xfail'] else ''}")
        return 0
    if bool(args.llm_url) != bool(args.llm_model):
        raise SystemExit("--llm-url and --llm-model go together")

    wanted = set(filter(None, (args.only or "").split(",")))
    selected = [c for c in CASES if not wanted or c["id"] in wanted or c["group"] in wanted]
    binary = pick_binary(args.binary)
    llm = {"url": args.llm_url, "model": args.llm_model, "key": args.llm_key} if args.llm_url else None
    tmp = tempfile.mkdtemp(prefix="datagen-e2e-")
    hole = Blackhole()
    daemons = {}
    results = []
    try:
        print(f"binary  {binary}\nscratch {tmp}\nmodel   {llm['model'] + ' @ ' + llm['url'] if llm else 'none (llm group skipped)'}")
        daemons["guarded"] = Daemon(binary, os.path.join(tmp, "a"), llm, hole.port, guarded=not args.no_guard)
        daemons["open"] = Daemon(binary, os.path.join(tmp, "b"), llm, hole.port, guarded=False)
        daemons["guarded"].start()
        if any(c["open"] for c in selected):
            daemons["open"].start()
        else:
            daemons["open"] = daemons["guarded"]
        ctx = Ctx(daemons, llm, args.timeout_scale, tmp)
        for c in selected:
            t0 = time.time()
            status, detail = "PASS", ""
            try:
                c["fn"](ctx)
            except Skip as e:
                status, detail = "SKIP", str(e)
            except AssertionError as e:
                status, detail = "FAIL", str(e)
            except Exception as e:  # a crash in the case is a failure of the case, not of the run
                status, detail = "FAIL", f"{type(e).__name__}: {e}"
            if c["xfail"] and status == "FAIL":
                status, detail = "XFAIL", f"known defect: {c['xfail']}  [{detail}]"
            elif c["xfail"] and status == "PASS":
                status, detail = "XPASS", f"passes now, drop its xfail: {c['xfail']}"
            results.append({"id": c["id"], "group": c["group"], "status": status, "seconds": round(time.time() - t0, 1),
                            "detail": detail})
            print(f"{status:4}  {c['id']:34} {results[-1]['seconds']:6.1f}s  {detail.splitlines()[0] if detail else ''}", flush=True)
    finally:
        for d in {id(x): x for x in daemons.values()}.values():
            d.stop()
        hole.close()
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)

    counts = collections.Counter(r["status"] for r in results)
    print(f"\n{counts['PASS']} passed, {counts['FAIL']} failed, {counts['SKIP']} skipped, "
          f"{counts['XFAIL']} known defects (xfail), {counts['XPASS']} xpass")
    if args.report:
        with open(args.report, "w") as f:
            json.dump({"binary": binary, "llm": llm and llm["model"], "results": results}, f, indent=2, ensure_ascii=False)
    for r in results:
        if r["status"] == "FAIL":
            print(f"\nFAIL {r['id']}\n  {r['detail']}")
    bad = counts["FAIL"] or (args.fail_on_skip and counts["SKIP"]) or (args.strict_xfail and counts["XPASS"])
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
