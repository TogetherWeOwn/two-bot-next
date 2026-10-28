"""Idle/wake lifecycle for voice-synthetic (finding5). Offline, protected.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_staging_idle_voice_lifecycle_acceptance.py' -v
Fake staging plus python3 wrapper for Discord; no live net. Fixture token.
Executes real `run:` shells; readiness is /readyz 200 with gateway JSON
(/health is liveness only). Env is per-step scoped from workflow bindings.
Limitation: if step parsing misaligns, execution falls back to merged env.
"""
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/voice-synthetic.yml"
FAKE_TOKEN = "fixture-voice-synthetic-token-not-a-real-credential"
DEFAULT_IDLE = 10.0
SHORT_IDLE = 2.0
LIFECYCLE = 5.0
QUEUE_DELAY = 25.0
BOUND_SEC = 60.0
BOUND_PROBES = 50
READYZ_OK = b'{"components":[["process","ready"],["gateway","ready"]],"jobs":{}}'
READYZ_DOWN = b'{"components":[["process","ready"],["gateway","starting"]],"jobs":{}}'
HEALTH_OK = b'{"status":"ok"}'

_RUN_RE = re.compile(r"^(?P<prefix>[ ]*(?:-[ ]+)?)run:[ ]*(?P<value>.*)$")
_BLOCK_RE = re.compile(r"^[|>][+-]?[0-9]*$")


def extract_run_blocks(text):
    lines, blocks, i = text.splitlines(), [], 0
    while i < len(lines):
        m = _RUN_RE.match(lines[i])
        i += 1
        if not m:
            continue
        v = m.group("value").strip()
        mark = v.split("#", 1)[0].strip() if v else ""
        if mark and _BLOCK_RE.match(mark):
            col, body = len(m.group("prefix")), []
            while i < len(lines):
                ln = lines[i]
                if not ln.strip():
                    body.append("")
                    i += 1
                    continue
                if len(ln) - len(ln.lstrip(" ")) <= col:
                    break
                body.append(ln)
                i += 1
            blocks.append(body)
        elif v:
            blocks.append([m.group("value")])
    return blocks


def workflow_run_steps():
    text = WORKFLOW.read_text()
    return [textwrap.dedent("\n".join(b)) for b in extract_run_blocks(text)]


def resolve_dict(raw, fake_origin):
    apr_vars = {"VOICE_SYNTHETIC_REQUIRED": "true",
                "STAGING_EMPTY_GRACE_SECONDS": "60",
                "STAGING_WORKER_URL": fake_origin}
    out = {}
    for k, v in raw.items():
        v = v.strip()
        if "${{" in v and "}}" in v:
            inner = v.split("${{", 1)[1].split("}}", 1)[0].strip()
            if inner.startswith("secrets."):
                name = inner[8:].strip()
                assert name == "TWO_VOICE_SYNTHETIC_BOT_TOKEN", f"unapproved secret {name}"
                out[k] = FAKE_TOKEN
            elif inner.startswith("vars."):
                name = inner.split("||", 1)[0].strip()[5:].strip()
                assert name in apr_vars, f"unapproved var {name}"
                out[k] = apr_vars[name]
            elif inner == "github.run_id":
                out[k] = "test-run"
            else:
                raise AssertionError(f"unapproved expr {inner!r}")
        else:
            out[k] = v.strip("'\"")
    return out


def parse_env_block(lines, idx):
    col, i, raw = len(lines[idx]) - len(lines[idx].lstrip(" ")), idx + 1, {}
    while i < len(lines):
        ln = lines[i]
        if not ln.strip():
            i += 1
            continue
        if len(ln) - len(ln.lstrip(" ")) <= col:
            break
        m = re.match(r"^\s+([A-Za-z0-9_]+):\s*(.*)$", ln)
        i += 1
        if m:
            raw[m.group(1)] = m.group(2)
    return raw, i


def render_env(text, fake_origin):
    lines, i, merged = text.splitlines(), 0, {}
    while i < len(lines):
        if re.match(r"^\s*env:\s*(#.*)?$", lines[i]):
            raw, i = parse_env_block(lines, i)
            merged.update(raw)
        else:
            i += 1
    return resolve_dict(merged, fake_origin)


def workflow_steps_with_env(text, fake_origin):
    lines = text.splitlines()
    try:
        si = next(i for i, n in enumerate(lines) if re.match(r"^\s*steps:\s*$", n))
    except StopIteration:
        return []
    scol = len(lines[si]) - len(lines[si].lstrip(" "))
    job_raw = {}
    i = 0
    while i < si:
        if re.match(r"^\s*env:\s*(#.*)?$", lines[i]):
            raw, i = parse_env_block(lines, i)
            job_raw.update(raw)
        else:
            i += 1
    blocks, cur = [], None
    i = si + 1
    while i < len(lines):
        ln = lines[i]
        if ln.strip() and len(ln) - len(ln.lstrip(" ")) <= scol:
            break
        if re.match(r"^\s*-\s", ln) and len(ln) - len(ln.lstrip(" ")) == scol + 2:
            if cur is not None:
                blocks.append(cur)
            cur = [ln]
        elif cur is not None:
            cur.append(ln)
        i += 1
    if cur is not None:
        blocks.append(cur)
    pairs = []
    for b in blocks:
        bt = "\n".join(b)
        if "run:" not in bt:
            continue
        shells = [textwrap.dedent("\n".join(x)) for x in extract_run_blocks(bt)]
        if not shells:
            continue
        raw = dict(job_raw)
        for j, ln in enumerate(b):
            if re.match(r"^\s*env:\s*(#.*)?$", ln):
                r, _ = parse_env_block(b, j)
                raw.update(r)
                break
        pairs.append((shells[0], resolve_dict(raw, fake_origin)))
    return pairs


class _H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        self._ans()

    def do_HEAD(self):
        self._ans()

    def _ans(self):
        s, now = self.server, time.monotonic()
        path = self.path.split("?", 1)[0]
        if path == "/_test_state":
            with s.lock:
                awake = s.deadline is not None and now <= s.deadline
                n = len(s.hits)
            body = json.dumps({"awake": awake, "hits": n}).encode()
            self.send_response(200)
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            if self.command == "GET":
                self.wfile.write(body)
            return
        with s.lock:
            s.hits.append({"t": now, "path": path})
            if s.mode == "never_ready":
                st, body = (503, HEALTH_OK) if path == "/health" else (503, READYZ_DOWN)
            elif s.mode == "health200_readyz503":
                if path == "/health":
                    st, body = 200, HEALTH_OK
                elif path == "/readyz":
                    st, body = 503, READYZ_DOWN
                else:
                    st, body = 404, b"not found"
            elif path == "/health":
                if s.deadline is not None and now <= s.deadline:
                    s.deadline = now + s.idle_window
                    st, body = 200, HEALTH_OK
                elif s.health_seen == 0:
                    s.health_seen = 1
                    st, body = 503, b'{"status":"starting"}'
                else:
                    st, body = 200, HEALTH_OK
            elif path == "/readyz":
                if s.deadline is not None and now <= s.deadline:
                    s.deadline = now + s.idle_window
                    st, body = 200, READYZ_OK
                elif s.since_sleep == 0:
                    s.since_sleep = 1
                    st, body = 503, READYZ_DOWN
                else:
                    s.deadline = now + s.idle_window
                    s.since_sleep = 0
                    st, body = 200, READYZ_OK
            else:
                st, body = 404, b"not found"
            s.hits[-1].update(status=st, body=body.decode())
        self.send_response(st)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        if self.command == "GET":
            self.wfile.write(body)


class FakeStaging:
    def __init__(self, mode="sleeping_baseline", idle_window=DEFAULT_IDLE):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _H)
        self.server.hits, self.server.lock = [], threading.Lock()
        self.server.mode, self.server.idle_window = mode, idle_window
        self.server.deadline, self.server.since_sleep = None, 0
        self.server.health_seen = 0
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.origin = f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


PY_WRAPPER = textwrap.dedent('''\
    import json, os, sys, time, urllib.request
    open(os.environ["FAKE_PYTHON_LOG"], "a").write(json.dumps({"t": time.monotonic(), "argv": sys.argv[1:]}) + "\\n")
    argv = sys.argv[1:]
    if any("staging_voice_synthetic.py" in a for a in argv):
        dlog, base = os.environ["FAKE_DISCORD_LOG"], os.environ.get("STAGING_WORKER_URL", "")
        life = float(os.environ.get("FAKE_LIFECYCLE_DURATION", "5"))
        start = time.monotonic()
        open(dlog, "a").write(json.dumps({"event": "synthetic_start", "t": start}) + "\\n")
        def awake():
            try:
                with urllib.request.urlopen(base.rstrip("/") + "/_test_state", timeout=5) as r:
                    return json.loads(r.read().decode()).get("awake", False)
            except Exception:
                return False
        if not awake():
            open(dlog, "a").write(json.dumps({"event": "synthetic_fail", "t": time.monotonic(), "reason": "asleep at start"}) + "\\n")
            print("FAKE synthetic: staging asleep, no room", flush=True)
            sys.exit(1)
        end = start + life
        while time.monotonic() < end:
            time.sleep(0.2)
            if not awake():
                open(dlog, "a").write(json.dumps({"event": "synthetic_fail", "t": time.monotonic(), "reason": "slept mid-lifecycle"}) + "\\n")
                print("FAKE synthetic: slept mid-lifecycle", flush=True)
                sys.exit(1)
        open(dlog, "a").write(json.dumps({"event": "synthetic_end", "t": time.monotonic()}) + "\\n")
        print("FAKE synthetic: PASS", flush=True)
        sys.exit(0)
    real = os.environ["FAKE_REAL_PYTHON"]
    os.execv(real, [real] + argv)
    ''')

CURL_WRAPPER = textwrap.dedent('''\
    import json, os, sys, time, urllib.request, urllib.error
    open(os.environ["FAKE_CURL_LOG"], "a").write(json.dumps({"t": time.monotonic(), "argv": sys.argv[1:]}) + "\\n")
    args = sys.argv[1:]
    url = next((a for a in reversed(args) if a.startswith("http://") or a.startswith("https://")), None)
    if url is None or ("127.0.0.1" not in url and "localhost" not in url):
        print("fake curl: refused (need loopback URL)", file=sys.stderr)
        sys.exit(1)
    out = args[args.index("-o") + 1] if "-o" in args and args.index("-o") + 1 < len(args) else None
    w = args[args.index("-w") + 1] if "-w" in args and args.index("-w") + 1 < len(args) else None
    fail = "-f" in args or "--fail" in args
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            st, body = r.status, r.read()
    except urllib.error.HTTPError as e:
        st, body = e.code, e.read() or b""
    except Exception as e:
        print(f"fake curl: {e}", file=sys.stderr)
        sys.exit(7)
    if out and out != "/dev/null":
        open(out, "wb").write(body)
    elif not out:
        sys.stdout.buffer.write(body)
    if w is not None and "http_code" in w:
        sys.stdout.write(str(st) + ("\\n" if "\\\\n" in w or "\\n" in w else ""))
        sys.stdout.flush()
    sys.exit(22 if (fail and st >= 400) else 0)
    ''')

SLEEP_WRAPPER = textwrap.dedent('''\
    import json, os, sys, time
    try:
        req = float(sys.argv[1] if len(sys.argv) > 1 else "0")
    except ValueError:
        req = 0.0
    act = min(max(req, 0.0), 0.2)
    open(os.environ["FAKE_SLEEP_LOG"], "a").write(json.dumps({"requested": req, "actual": act}) + "\\n")
    time.sleep(act)
    ''')


def write_bin(d, real):
    d.mkdir(parents=True, exist_ok=True)
    (d / "python3").write_text(f"#!{real}\n" + PY_WRAPPER)
    (d / "python3").chmod(0o755)
    try:
        (d / "python").symlink_to("python3")
    except FileExistsError:
        pass
    (d / "curl").write_text(f"#!{real}\n" + CURL_WRAPPER)
    (d / "curl").chmod(0o755)
    (d / "sleep").write_text(f"#!{real}\n" + SLEEP_WRAPPER)
    (d / "sleep").chmod(0o755)


def read_jsonl(p):
    if not p.exists():
        return []
    return [json.loads(n) for n in p.read_text().splitlines() if n.strip()]


def run_one(sh, cwd, env):
    pr = subprocess.Popen(["bash", "-c", sh], cwd=cwd, env=env,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, start_new_session=True)
    try:
        out, err = pr.communicate(timeout=120)
        return out, err, pr.returncode
    except subprocess.TimeoutExpired:
        try:
            os.killpg(pr.pid, signal.SIGKILL)
        except Exception:
            pass
        try:
            out, err = pr.communicate(timeout=10)
        except Exception:
            out, err = "", ""
        return out, err, 124


def run_shells(shells, *, workflow_text=None, idle_window=DEFAULT_IDLE,
               lifecycle=LIFECYCLE, mode="sleeping_baseline",
               scratch=None, queue_delay=None):
    tmp = tempfile.TemporaryDirectory(dir=scratch)
    try:
        root = Path(tmp.name)
        work = root / "work"
        work.mkdir()
        (work / "scripts").symlink_to(ROOT / "scripts", target_is_directory=True)
        bind = root / "bin"
        write_bin(bind, sys.executable)
        plog, dlog, clog = root / "py.jsonl", root / "di.jsonl", root / "cu.jsonl"
        slog = root / "sl.jsonl"
        for p in (plog, dlog, clog, slog):
            p.write_text("")
        staging = FakeStaging(mode=mode, idle_window=idle_window)
        try:
            base = {"PATH": str(bind) + os.pathsep + os.defpath,
                    "FAKE_PYTHON_LOG": str(plog), "FAKE_DISCORD_LOG": str(dlog),
                    "FAKE_CURL_LOG": str(clog), "FAKE_SLEEP_LOG": str(slog),
                    "FAKE_REAL_PYTHON": sys.executable,
                    "FAKE_LIFECYCLE_DURATION": str(lifecycle),
                    "PYTHONDONTWRITEBYTECODE": "1"}
            if workflow_text is not None:
                pairs = workflow_steps_with_env(workflow_text, staging.origin)
                dsh = [p[0] for p in pairs]
                if pairs and dsh == list(shells):
                    plan = [(s, {**base, **e}) for s, e in pairs]
                else:
                    merged = render_env(workflow_text, staging.origin)
                    plan = [(s, {**base, **merged}) for s in shells]
            else:
                plan = [(s, {**base, "STAGING_WORKER_URL": staging.origin}) for s in shells]
            sched = time.monotonic() - queue_delay if queue_delay is not None else None
            mark = time.monotonic() if queue_delay is not None else None
            assert plan, "need at least one shell"
            t0, outs, errs, code = time.monotonic(), [], [], 0
            for sh, env in plan:
                out, err, code = run_one(sh, work, env)
                outs.append(out)
                errs.append(err)
                if code:
                    break
            t1 = time.monotonic()
            with staging.server.lock:
                hits = [dict(r) for r in staging.server.hits]
            return SimpleNamespace(returncode=code, stdout="".join(outs),
                                   stderr="".join(errs), hits=hits,
                                   py=read_jsonl(plog), di=read_jsonl(dlog),
                                   curl=read_jsonl(clog), t0=t0, t1=t1,
                                   sched=sched, mark=mark)
        finally:
            staging.close()
    finally:
        tmp.cleanup()


def run_real(*, idle_window=DEFAULT_IDLE, mode="sleeping_baseline",
             scratch=None, queue_delay=None):
    return run_shells(workflow_run_steps(), workflow_text=WORKFLOW.read_text(),
                      idle_window=idle_window, mode=mode, scratch=scratch,
                      queue_delay=queue_delay)


def good(hits):
    g = [r for r in hits if r.get("path") == "/readyz" and r.get("status") == 200
         and "gateway" in r.get("body", "") and "ready" in r.get("body", "")]
    return min(g, key=lambda r: r["t"]) if g else None


def starts(di):
    return [r for r in di if r.get("event") == "synthetic_start"]


FIXED = """set -euo pipefail
for i in $(seq 1 10); do
  code=$(curl -s -o /dev/null -w "%{http_code}" "$STAGING_WORKER_URL/readyz" || true)
  if [ "$code" = "200" ]; then break; fi
  sleep 0.2
done
code=$(curl -s -o /dev/null -w "%{http_code}" "$STAGING_WORKER_URL/readyz" || true)
if [ "$code" != "200" ]; then echo "not ready"; exit 1; fi
python3 scripts/staging_voice_synthetic.py --evidence out.json
"""

KEEPALIVE = """set -euo pipefail
for i in $(seq 1 10); do
  code=$(curl -s -o /dev/null -w "%{http_code}" "$STAGING_WORKER_URL/readyz" || true)
  if [ "$code" = "200" ]; then break; fi
  sleep 0.2
done
(while true; do curl -s -o /dev/null "$STAGING_WORKER_URL/readyz" || true; sleep 1; done >/dev/null 2>&1) &
KP=$!
trap 'kill $KP 2>/dev/null || true; wait $KP 2>/dev/null || true' EXIT
python3 scripts/staging_voice_synthetic.py --evidence out.json
"""

ONE_PROBE = """set -euo pipefail
curl -s "$STAGING_WORKER_URL/readyz" || true
python3 scripts/staging_voice_synthetic.py --evidence out.json
"""

HEALTH_ONLY = """set -euo pipefail
for i in $(seq 1 10); do
  code=$(curl -s -o /dev/null -w "%{http_code}" "$STAGING_WORKER_URL/health" || true)
  if [ "$code" = "200" ]; then break; fi
  sleep 0.2
done
python3 scripts/staging_voice_synthetic.py --evidence out.json
"""


class VoiceIdleLifecycleAcceptance(unittest.TestCase):
    scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")

    def test_sleeping_baseline_wakes_before_discord(self):
        r = run_real(scratch=self.scratch)
        g, st = good(r.hits), starts(r.di)
        self.assertIsNotNone(g, f"need post-start /readyz 200; hits={r.hits} di={r.di}")
        self.assertTrue(st, "lifecycle must attempt Discord")
        self.assertLess(g["t"], min(x["t"] for x in st), f"/readyz must precede Discord; {g} {st}")
        self.assertEqual(r.returncode, 0, f"must succeed after wake; {r.stdout[-1500:]}")
        self.assertLess(r.t1 - r.t0, BOUND_SEC)
        self.assertLessEqual(len(r.hits), BOUND_PROBES)

    def test_queued_start_wakes_post_start(self):
        r = run_real(scratch=self.scratch, queue_delay=QUEUE_DELAY)
        self.assertGreater(r.mark - r.sched, DEFAULT_IDLE)
        g, st = good(r.hits), starts(r.di)
        self.assertTrue(g and g["t"] >= r.mark, f"need post-start /readyz 200; hits={r.hits}")
        self.assertTrue(st)
        self.assertLess(g["t"], min(x["t"] for x in st))
        self.assertEqual(r.returncode, 0, f"queued must succeed; {r.stdout[-1500:]}")

    def test_short_idle_requires_lifetime_maintenance(self):
        r = run_real(scratch=self.scratch, idle_window=SHORT_IDLE)
        ends = [x for x in r.di if x.get("event") == "synthetic_end"]
        self.assertTrue(ends, f"short idle 2s lifecycle 5s must complete; di={r.di} hits={r.hits}")
        self.assertEqual(r.returncode, 0)

    def test_never_ready_fails_without_discord(self):
        r = run_real(scratch=self.scratch, mode="never_ready")
        self.assertEqual(starts(r.di), [], f"no Discord when never ready; {r.di} {r.hits}")
        self.assertNotEqual(r.returncode, 0)
        self.assertLess(r.t1 - r.t0, BOUND_SEC)
        self.assertLessEqual(len(r.hits), BOUND_PROBES)
        self.assertIsNone(good(r.hits))

    def test_no_discord_before_readiness(self):
        r = run_real(scratch=self.scratch)
        g, st = good(r.hits), starts(r.di)
        self.assertTrue(st)
        self.assertIsNotNone(g, f"need /readyz 200 first; hits={r.hits} starts={st}")
        self.assertLess(g["t"], min(x["t"] for x in st))

    def test_env_binding_and_fences(self):
        text = WORKFLOW.read_text()
        secrets = sorted(set(re.findall(r"secrets\.([A-Za-z0-9_]+)", text)))
        self.assertEqual(secrets, ["TWO_VOICE_SYNTHETIC_BOT_TOKEN"])
        code = "\n".join(n for n in text.splitlines() if not n.strip().startswith("#"))
        low = code.lower().replace(" ", "")
        for mk in ("production", "wrangler", "cloudflare_api_token",
                   "tojson(secrets", "secrets[", "set-x", "setx"):
            self.assertNotIn(mk, low)
        rendered = render_env(text, "http://127.0.0.1:9")
        allowed = {"TWO_VOICE_SYNTHETIC_BOT_TOKEN", "VOICE_SYNTHETIC_REQUIRED",
                   "TWO_STAGING_EMPTY_GRACE_SECONDS", "RUN_ID", "STAGING_WORKER_URL"}
        self.assertTrue(set(rendered) <= allowed, f"only approved bindings; {rendered}")
        r = run_real(scratch=self.scratch)
        self.assertNotIn(FAKE_TOKEN, r.stdout + r.stderr)
        for row in r.py + r.curl:
            self.assertNotIn(FAKE_TOKEN, json.dumps(row))
        for c in r.curl:
            for a in c.get("argv", []):
                if a.startswith("http://") or a.startswith("https://"):
                    self.assertTrue("127.0.0.1" in a or "localhost" in a)
        probe = run_shells(['echo "X${STAGING_WORKER_URL:-}Y"'],
                           workflow_text=text, scratch=self.scratch)
        if "STAGING_WORKER_URL" in rendered:
            self.assertNotIn("XY", probe.stdout)
        else:
            self.assertIn("XY", probe.stdout, "undeclared var must stay empty")

    def test_positive_corrected_passes(self):
        r = run_shells([FIXED], scratch=self.scratch)
        g, st = good(r.hits), starts(r.di)
        self.assertIsNotNone(g)
        self.assertTrue(st)
        self.assertLess(g["t"], min(x["t"] for x in st))
        self.assertEqual(r.returncode, 0)
        self.assertTrue(any(x.get("event") == "synthetic_end" for x in r.di))

    def test_positive_short_idle_keepalive_passes(self):
        r = run_shells([KEEPALIVE], idle_window=SHORT_IDLE, scratch=self.scratch)
        self.assertEqual(r.returncode, 0, f"keepalive must hold 2s idle; di={r.di}")
        self.assertTrue(any(x.get("event") == "synthetic_end" for x in r.di))

    def test_positive_health_maintenance_after_readiness_passes(self):
        # Liveness is insufficient to admit Discord, but an owned /health
        # request after verified readiness can renew an active idle deadline.
        maintained = KEEPALIVE.replace(
            'while true; do curl -s -o /dev/null "$STAGING_WORKER_URL/readyz"',
            'while true; do curl -s -o /dev/null "$STAGING_WORKER_URL/health"',
        )
        r = run_shells([maintained], idle_window=SHORT_IDLE, scratch=self.scratch)
        g, st = good(r.hits), starts(r.di)
        self.assertTrue(g and st and g["t"] < min(x["t"] for x in st))
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertTrue(any(x.get("event") == "synthetic_end" for x in r.di))
        self.assertTrue(any(x.get("path") == "/health" for x in r.hits))

    def test_negative_one_probe_fails(self):
        r = run_shells([ONE_PROBE], scratch=self.scratch)
        g, st = good(r.hits), starts(r.di)
        self.assertTrue(st)
        self.assertFalse(g and g["t"] < min(x["t"] for x in st),
                         f"one probe must not satisfy ordering; hits={r.hits}")
        self.assertNotEqual(r.returncode, 0)

    def test_negative_wake_once_short_idle_fails(self):
        r = run_shells([FIXED], idle_window=SHORT_IDLE, scratch=self.scratch)
        g, st = good(r.hits), starts(r.di)
        self.assertTrue(g and st and g["t"] < min(x["t"] for x in st),
                        f"wake-once must start then die mid-life; hits={r.hits} di={r.di}")
        fails = [x for x in r.di if x.get("event") == "synthetic_fail"]
        self.assertTrue(any("mid-lifecycle" in x.get("reason", "") for x in fails),
                        f"short idle must kill wake-once mid-life; di={r.di}")
        self.assertNotEqual(r.returncode, 0)

    def test_negative_health_only_fails(self):
        r = run_shells([HEALTH_ONLY], mode="health200_readyz503", scratch=self.scratch)
        g, st = good(r.hits), starts(r.di)
        self.assertTrue(any(x.get("path") == "/health" and x.get("status") == 200 for x in r.hits),
                        f"control needs /health 200; hits={r.hits}")
        self.assertIsNone(g, f"/health 200 is not readiness; hits={r.hits}")
        self.assertNotEqual(r.returncode, 0)


if __name__ == "__main__":
    unittest.main()
