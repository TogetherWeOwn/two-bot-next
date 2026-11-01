"""Offline pins for scripts/post_deploy_watch.py.

No network: time is scripted, Cloudflare is faked in memory, the probes are a
scripted fetch, and the error-rate gate is a stub that never spawns node.
"""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
# E402-safe by convention: every scripts/ test adds its own directory first.
import staging_rollout as base
import post_deploy_watch as watcher
from test_worker_container_rollback import FakeClient

SHA = "0123456789abcdef0123456789abcdef01234567"
VERSION = "11111111-1111-4111-8111-111111111111"
OTHER = "22222222-2222-4222-8222-222222222222"
URL = "https://two-bot-next-production.example.workers.dev"


class Clock:
    def __init__(self):
        self.t = 0.0

    def now(self):
        return self.t

    def sleep(self, seconds):
        self.t += seconds


def serving(versions):
    """Deployments route answering the next version from `versions` (last one repeats)."""
    seq = list(versions)

    def handler():
        version = seq.pop(0) if len(seq) > 1 else seq[0]
        if version is None:
            return {"success": False}
        return {"success": True, "result": {"deployments": [
            {"versions": [{"version_id": version, "percentage": 100}]}]}}
    return {("GET", f"/workers/scripts/{watcher.WORKER}/deployments"): handler}


def readyz(revision=SHA, gateway="ready"):
    return json.dumps({"components": [["process", "ready"], ["gateway", gateway]],
                       "build_revision": revision, "build_id": "1-1"}).encode()


def fetcher(answers):
    """A fetch returning (status, body) per URL suffix from a callable or a constant."""
    def fetch(url):
        answer = answers[url.rsplit("/", 1)[-1]]
        return answer() if callable(answer) else answer
    return fetch


HEALTHY = {"health": (200, b"{}"), "readyz": (200, readyz())}


class StubGate:
    def __init__(self, verdicts=()):
        self.verdicts = list(verdicts)
        self.started, self.stopped, self.running, self.errors = 0, 0, False, 0

    def start(self):
        self.started += 1
        self.running = True

    def poll(self):
        # Each window finishes on the poll after it starts.
        if not self.running:
            return None
        self.running = False
        return self.verdicts.pop(0) if self.verdicts else "pass"

    def stop(self):
        self.stopped += 1


def run(answers=None, versions=(VERSION,), gate=None, duration=watcher.DURATION_SECONDS):
    clock = Clock()
    gate = gate or StubGate()
    evidence = watcher.watch(url=URL, sha=SHA, version=VERSION, client=FakeClient(serving(versions)),
                             gate=gate, now=clock.now, sleep=clock.sleep,
                             fetch_fn=fetcher(answers or HEALTHY), duration=duration)
    return evidence, gate, clock


class ReadyTests(unittest.TestCase):
    def test_only_health_200_and_ready_gateway_on_the_promoted_revision_pass(self):
        cases = {
            "ready": (HEALTHY, (True, "ready")),
            "health down": ({**HEALTHY, "health": (503, b"")}, (False, "health_not_200")),
            "transport": ({**HEALTHY, "health": (0, b"")}, (False, "health_not_200")),
            "parked": ({**HEALTHY, "readyz": (503, readyz(gateway="starting"))}, (False, "readyz_not_200")),
            "gateway": ({**HEALTHY, "readyz": (200, readyz(gateway="down"))}, (False, "gateway_not_ready")),
            "old build": ({**HEALTHY, "readyz": (200, readyz(revision="f" * 40))}, (False, "revision_mismatch")),
            "not json": ({**HEALTHY, "readyz": (200, b"<html>")}, (False, "readyz_not_json")),
            "duplicate keys": ({**HEALTHY, "readyz": (200, b'{"a": 1, "a": 2}')}, (False, "readyz_not_json")),
            "over cap": ({**HEALTHY, "readyz": (200, b" " * (watcher.MAX_READYZ_BYTES + 1))},
                         (False, "readyz_over_cap")),
        }
        for name, (answers, expected) in cases.items():
            with self.subTest(name):
                self.assertEqual(watcher.ready(URL, SHA, fetcher(answers)), expected)


class WatchTests(unittest.TestCase):
    def test_a_healthy_window_is_clean_after_30_minutes(self):
        evidence, gate, clock = run()
        self.assertEqual((evidence["verdict"], evidence["reason"]), ("clean", "no_breach"))
        self.assertGreaterEqual(clock.t, watcher.DURATION_SECONDS)
        self.assertEqual(evidence["polls"], watcher.DURATION_SECONDS // watcher.POLL_SECONDS)
        # Error-rate windows keep running back to back through the watch.
        self.assertGreaterEqual(gate.started, 100)
        self.assertEqual(gate.stopped, 1)

    def test_sustained_readiness_misses_breach_and_a_blip_does_not(self):
        polls = {"n": 0}

        def flaky():
            polls["n"] += 1
            return (503, b"") if polls["n"] % 10 == 0 else (200, b"{}")
        evidence, _, _ = run({**HEALTHY, "health": flaky})
        self.assertEqual(evidence["verdict"], "clean")
        self.assertEqual(evidence["max_miss_streak"], 1)

        start = {"n": 0}

        def dies():
            start["n"] += 1
            return (200, b"{}") if start["n"] < 30 else (503, b"")
        evidence, _, clock = run({**HEALTHY, "health": dies})
        self.assertEqual((evidence["verdict"], evidence["reason"]), ("breach", "readiness:health_not_200"))
        self.assertEqual(evidence["max_miss_streak"], watcher.MISS_LIMIT)
        self.assertLess(clock.t, watcher.DURATION_SECONDS)

    def test_a_failing_error_rate_window_breaches(self):
        evidence, gate, _ = run(gate=StubGate(["pass", "fail"]))
        self.assertEqual((evidence["verdict"], evidence["reason"]), ("breach", "rest_5xx"))
        self.assertEqual(evidence["gate_windows"], ["pass", "fail"])
        self.assertEqual(gate.stopped, 1)

    def test_unscrapeable_metrics_breach_only_when_repeated(self):
        evidence, _, _ = run(gate=StubGate(["error", "pass", "error", "pass"]))
        self.assertEqual(evidence["verdict"], "clean")
        evidence, _, _ = run(gate=StubGate(["pass", "error", "error"]))
        self.assertEqual((evidence["verdict"], evidence["reason"]), ("breach", "rest_5xx_unreadable"))

    def test_a_skipped_or_quiet_gate_never_breaches(self):
        evidence, _, _ = run(gate=StubGate(["skip"] * 20 + ["insufficient_data"] * 20))
        self.assertEqual(evidence["verdict"], "clean")

    def test_a_moved_version_stops_the_watch_without_a_breach(self):
        # A newer deploy or an operator rollback took over: never roll that back.
        evidence, _, _ = run(answers={**HEALTHY, "readyz": (200, readyz(revision="f" * 40))},
                             versions=(VERSION, OTHER))
        self.assertEqual((evidence["verdict"], evidence["reason"]), ("superseded", "version_no_longer_serving"))

    def test_unreadable_cloudflare_is_an_error_not_a_rollback(self):
        evidence, _, _ = run(versions=(None,))
        self.assertEqual((evidence["verdict"], evidence["reason"]), ("error", "cloudflare_unreadable"))


class MainTests(unittest.TestCase):
    def run_main(self, *, versions=(VERSION,), answers=None, gate=None, argv=None):
        clock = Clock()
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            out, summary = Path(tmp) / "out", Path(tmp) / "summary"
            evidence = Path(tmp) / "watch.json"
            env = {"GITHUB_OUTPUT": str(out), "GITHUB_STEP_SUMMARY": str(summary),
                   "CLOUDFLARE_ACCOUNT_ID": "a" * 32, "CLOUDFLARE_API_TOKEN": "t"}
            stdout = io.StringIO()
            with mock.patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(stdout):
                code = watcher.main(
                    argv or ["--worker-url", URL, "--sha", SHA, "--version", VERSION,
                             "--evidence", str(evidence)],
                    client_factory=lambda account, token: FakeClient(serving(versions)),
                    gate_factory=lambda *args: gate or StubGate(),
                    now=clock.now, sleep=clock.sleep, fetch_fn=fetcher(answers or HEALTHY))
            outputs = dict(line.split("=", 1) for line in out.read_text().splitlines()) if out.exists() else {}
            return code, outputs, stdout.getvalue(), json.loads(evidence.read_text()) if evidence.exists() else None

    def test_clean_breach_superseded_and_error_exit_codes(self):
        code, outputs, _, evidence = self.run_main()
        self.assertEqual((code, outputs["verdict"]), (0, "clean"))
        self.assertEqual(evidence["verdict"], "clean")
        code, outputs, stdout, _ = self.run_main(gate=StubGate(["fail"]))
        self.assertEqual((code, outputs["verdict"], outputs["reason"]), (1, "breach", "rest_5xx"))
        self.assertIn("::error::post-deploy watch breach", stdout)
        code, outputs, _, _ = self.run_main(versions=(VERSION, OTHER))
        self.assertEqual((code, outputs["verdict"]), (0, "superseded"))
        code, outputs, _, _ = self.run_main(versions=(None,))
        self.assertEqual((code, outputs["verdict"]), (3, "error"))

    def test_bad_inputs_refuse_before_watching(self):
        for argv in (["--worker-url", "http://x", "--sha", SHA, "--version", VERSION, "--evidence", "e"],
                     ["--worker-url", URL, "--sha", "abc", "--version", VERSION, "--evidence", "e"],
                     ["--worker-url", URL, "--sha", SHA, "--version", "v1", "--evidence", "e"]):
            with self.subTest(argv=argv):
                code, outputs, _, _ = self.run_main(argv=argv)
                self.assertEqual((code, outputs), (3, {}))

    def test_evidence_carries_no_remote_body(self):
        secret = b'{"components": [], "token": "do-not-leak"}'
        _, _, stdout, evidence = self.run_main(answers={**HEALTHY, "readyz": (200, secret)})
        self.assertNotIn("do-not-leak", stdout + json.dumps(evidence))


class GateProcessTests(unittest.TestCase):
    def test_gate_runs_the_reviewed_error_rate_gate_and_reads_its_verdict(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            gate = watcher.Gate(URL, SHA, tmp, "/tooling")
            with mock.patch("subprocess.Popen") as popen:
                gate.start()
                argv = popen.call_args.args[0]
                self.assertEqual(argv[:2], ["node", "/tooling/wrangler/scripts/promotion-error-gate.mjs"])
                self.assertIn("--metrics-url", argv)
                self.assertEqual(argv[argv.index("--sample-interval-seconds") + 1],
                                 str(watcher.GATE_SAMPLE_SECONDS))
                popen.return_value.poll.return_value = None
                self.assertIsNone(gate.poll())
                gate.child = popen.return_value
                popen.return_value.poll.return_value = 1
                Path(tmp, "watch-error-rate-1.json").write_text(json.dumps({"verdict": "fail"}))
                self.assertEqual(gate.poll(), "fail")
                gate.start()
                gate.child = popen.return_value
                Path(tmp, "watch-error-rate-2.json").write_text("not json")
                self.assertEqual(gate.poll(), "error")


if __name__ == "__main__":
    unittest.main()
