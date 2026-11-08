"""Offline fixtures for the finite staging authority adapter; stdlib only, no node, no network.

`FakeRuntime` is an independent stand-in for the `staging-use*.mjs` helpers. It is
installed in place of `subprocess.run` and implements the runtime state semantics
the adapter relies on: one immutable per-run grant, native expiry, an active
owner for every acquire/check, and exact deployment/SHA identity.
"""

import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_drill_use as use_module
from worker_container_rollback import CONTROL_TIMEOUT_SECONDS, GateError

PRE = "11111111-1111-4111-8111-111111111111"
TARGET = "22222222-2222-4222-8222-222222222222"
OTHER = "55555555-5555-4555-8555-555555555555"
URL = "https://two-bot-next-staging.offline-fixture.workers.dev"
TOKEN = "OWNERSHIP_TOKEN_SENTINEL_" + "x" * 16
RECEIPT = "RECEIPT_SENTINEL_never_disclose"
CF_TOKEN = "CLOUDFLARE_TOKEN_SENTINEL"
SHA_A = "a" * 40
SHA_B = "c" * 40
WINDOW_MS = 25 * 60 * 1000
BASE_TIME = 1_800_000_000.0
ALLOWED_ENV = {"GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "PATH", "STAGING_WORKER_URL",
               "OWNERSHIP_CONTROL_TOKEN", "STAGING_USE_RECEIPT", "STAGING_USE_EXPIRES_AT",
               "STAGING_USE_DEPLOYMENT", "STAGING_USE_SHA"}


def policy_bindings(sha):
    return [{"name": "STAGING_RUNTIME_POLICY", "type": "plain_text", "text": "bounded"},
            {"name": "TWO_DEPLOY_ENV", "type": "plain_text", "text": "staging"},
            {"name": "CONTAINER_RELEASE", "type": "plain_text", "text": sha}]


def version_payload(version, sha, bindings=None):
    """The Worker version metadata the adapter validates before any mutation."""
    return {"id": version, "annotations": {"workers/message": sha},
            "resources": {"bindings": policy_bindings(sha) if bindings is None else bindings}}


class VersionClient:
    def __init__(self, shas=None, payloads=None):
        self.shas, self.payloads, self.paths = shas or {}, payloads or {}, []

    def api(self, path):
        self.paths.append(path)
        version = path.rsplit("/", 1)[-1]
        if version in self.payloads:
            return self.payloads[version]
        return version_payload(version, self.shas.get(version, SHA_A))


class State:
    """A minimal owner view and clock for tests that do not need a Worker world."""

    def __init__(self, serving=PRE, phase="active", running=True):
        self.serving, self.phase, self.running, self.t = serving, phase, running, BASE_TIME

    def owner(self):
        return {"phase": self.phase, "serving": self.serving, "running": self.running}

    def sha(self, version):
        return SHA_A


class FakeRuntime:
    """Callable replacement for subprocess.run emulating the staging-use helpers."""

    def __init__(self, owner, clock, registry, *, run_id="123456", attempt="1",
                 receipt=RECEIPT, events=None):
        self.owner, self.clock, self.registry = owner, clock, registry
        self.expected = {"GITHUB_RUN_ID": run_id, "GITHUB_RUN_ATTEMPT": attempt,
                         "STAGING_USE_RECEIPT": receipt}
        self.events = events
        self.calls = []  # (helper, action, env, kwargs, argv)
        self.fail = {}  # (action, nth) -> return code or exception to raise
        self.grant = None
        self.released = False
        self.resource = owner.__self__
        self.violations = []

    @property
    def actions(self):
        return [call[1] for call in self.calls]

    def env_of(self, index):
        return self.calls[index][2]

    def __call__(self, command, **kwargs):
        helper = Path(command[1]).name
        action = command[2]
        env = dict(kwargs.get("env") or {})
        self.calls.append((helper, action, env, kwargs, list(command)))
        if self.events is not None:
            self.events.append(("use", action))
        if command[0] != "node" or len(command) != 3:
            self.violations.append("argv")
        if set(env) - ALLOWED_ENV:
            self.violations.append("env:" + ",".join(sorted(set(env) - ALLOWED_ENV)))
        nth = self.actions.count(action)
        failure = self.fail.get((action, nth))
        if isinstance(failure, BaseException):
            raise failure
        code = failure if failure is not None else self.decide(helper, action, env)
        return subprocess.CompletedProcess(command, code, stdout=None, stderr=None)

    def decide(self, helper, action, env):
        expected_helper = "staging-use-pipeline.mjs" if action == "restore-check" else "staging-use.mjs"
        if action not in ("acquire", "ready", "release", "restore-check") or helper != expected_helper:
            return 2
        if any(env.get(key) != value for key, value in self.expected.items()):
            return 1
        try:
            expires = int(env.get("STAGING_USE_EXPIRES_AT", ""))
        except ValueError:
            return 1
        now_ms = int(self.clock() * 1000)
        if self.grant is None:
            if action != "acquire" or not now_ms < expires <= now_ms + WINDOW_MS:
                return 1
        elif expires != self.grant["expires"]:  # the deadline is immutable once granted
            return 1
        if now_ms >= expires and action != "release":
            self.resource.running = False
            return 1
        owner = self.owner()
        if owner["phase"] != "active":
            return 1
        if self.released and action != "release":
            return 1
        if action == "restore-check":
            if "STAGING_USE_DEPLOYMENT" in env:
                return 1
            return 0 if self.grant and owner["serving"] == self.grant["deployment"] else 1
        version, sha = env.get("STAGING_USE_DEPLOYMENT"), env.get("STAGING_USE_SHA")
        if version is None or sha is None or self.registry(version) != sha:
            return 1
        if action == "acquire":
            if owner["serving"] != version:  # acquire follows takeover on the serving Worker
                return 1
            self.grant = {"expires": expires, "deployment": version}
            self.resource.use_expires_at = expires
            self.resource.running = False
            return 0
        if self.grant is None or self.grant["deployment"] != version:
            return 1
        if action == "ready":
            if owner["serving"] != version:
                return 1
            self.resource.running = True
            return 0
        self.resource.running = False
        self.resource.use_expires_at = 0
        self.released = True
        return 0


def base_env(**changes):
    env = {"GITHUB_RUN_ID": "123456", "GITHUB_RUN_ATTEMPT": "1", "STAGING_USE_RECEIPT": RECEIPT,
           "PATH": "/runtime/bin:/usr/bin"}
    env.update(changes)
    return env


class UseHarness(unittest.TestCase):
    def setUp(self):
        self.state = State()
        self.runtime = FakeRuntime(self.state.owner, lambda: self.state.t, self.state.sha)
        patcher = patch.object(use_module.subprocess, "run", self.runtime)
        patcher.start()
        self.addCleanup(patcher.stop)

    def make(self, env=None, **kwargs):
        env = base_env() if env is None else env
        return use_module.StagingDrillUse(env, URL, TOKEN, now=lambda: self.state.t,
                                          root="/repo", **kwargs)

    def started(self, version=PRE):
        use = self.make()
        use.validate(VersionClient(), (PRE, TARGET))
        use.begin(version)
        return use


class ConstructionTests(UseHarness):
    def test_run_attempt_and_receipt_are_required_and_bounded(self):
        cases = [
            ({"GITHUB_RUN_ID": ""}, "staging_drill_run_invalid"),
            ({"GITHUB_RUN_ID": "0"}, "staging_drill_run_invalid"),
            ({"GITHUB_RUN_ID": "12 ; rm"}, "staging_drill_run_invalid"),
            ({"GITHUB_RUN_ATTEMPT": ""}, "staging_drill_run_invalid"),
            ({"GITHUB_RUN_ATTEMPT": "0"}, "staging_drill_run_invalid"),
            ({"GITHUB_RUN_ATTEMPT": "01"}, "staging_drill_run_invalid"),
            ({"GITHUB_RUN_ATTEMPT": str(2 ** 53)}, "staging_drill_run_invalid"),
            ({"STAGING_USE_RECEIPT": ""}, "staging_drill_receipt_missing"),
        ]
        for change, code in cases:
            with self.subTest(change=change):
                with self.assertRaises(GateError) as caught:
                    self.make(base_env(**change))
                self.assertEqual(str(caught.exception), code)
        for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "STAGING_USE_RECEIPT"):
            with self.subTest(missing=key):
                env = base_env()
                del env[key]
                with self.assertRaises(GateError):
                    self.make(env)
        self.assertEqual(self.runtime.calls, [])

    def test_no_window_means_no_control_call(self):
        use = self.make()
        with self.assertRaises(GateError) as caught:
            use.call("acquire", PRE)
        self.assertEqual(str(caught.exception), "staging_drill_window_missing")
        self.assertEqual(self.runtime.calls, [])
        use.finish()  # never started: nothing to release
        self.assertEqual(self.runtime.calls, [])


class ValidateTests(UseHarness):
    def test_validate_records_immutable_shas_and_makes_no_use_call(self):
        use = self.make()
        client = VersionClient({PRE: SHA_A, TARGET: SHA_A})
        use.validate(client, (PRE, TARGET))
        self.assertEqual(use.shas, {PRE: SHA_A, TARGET: SHA_A})
        self.assertEqual(client.paths, [f"/workers/scripts/two-bot-next-staging/versions/{v}"
                                        for v in (PRE, PRE, TARGET, TARGET)])
        self.assertEqual(self.runtime.calls, [])
        self.assertIsNone(use.expires_at)

    def test_metadata_without_finite_policy_is_refused_before_a_window(self):
        def without(name):
            return [row for row in policy_bindings(SHA_A) if row["name"] != name]

        def changed(name, text):
            return [dict(row, text=text) if row["name"] == name else row for row in policy_bindings(SHA_A)]

        cases = {
            "no_policy": (without("STAGING_RUNTIME_POLICY"), "staging_restore_policy_unproved"),
            "indefinite": (changed("STAGING_RUNTIME_POLICY", "indefinite"), "staging_restore_policy_unproved"),
            "wrong_env": (changed("TWO_DEPLOY_ENV", "production"), "staging_restore_policy_unproved"),
            "release_drift": (changed("CONTAINER_RELEASE", SHA_B), "staging_restore_policy_unproved"),
            "duplicate": (policy_bindings(SHA_A) + [policy_bindings(SHA_A)[0]], "staging_restore_policy_unproved"),
            "secret_type": ([dict(row, type="secret_text") for row in policy_bindings(SHA_A)],
                            "staging_restore_policy_unproved"),
        }
        for name, (bindings, code) in cases.items():
            with self.subTest(case=name):
                use = self.make()
                client = VersionClient(payloads={TARGET: version_payload(TARGET, SHA_A, bindings)})
                with self.assertRaises(GateError) as caught:
                    use.validate(client, (PRE, TARGET))
                self.assertEqual(str(caught.exception), code)
                self.assertEqual(self.runtime.calls, [])
                self.assertIsNone(use.expires_at)

    def test_missing_resources_and_unstamped_versions_are_refused(self):
        use = self.make()
        payload = version_payload(PRE, SHA_A)
        payload["resources"] = None
        with self.assertRaises(GateError) as caught:
            use.validate(VersionClient(payloads={PRE: payload}), (PRE,))
        self.assertEqual(str(caught.exception), "staging_restore_policy_unproved")
        unstamped = version_payload(PRE, SHA_A)
        unstamped["annotations"] = {}
        with self.assertRaises(GateError) as caught:
            use.validate(VersionClient(payloads={PRE: unstamped}), (PRE,))
        self.assertEqual(str(caught.exception), "staging_restore_identity_invalid")
        with self.assertRaises(GateError) as caught:
            use.validate(VersionClient(payloads={PRE: version_payload(OTHER, SHA_A)}), (PRE,))
        self.assertEqual(str(caught.exception), "version_identity_drift")
        self.assertEqual(self.runtime.calls, [])

    def test_unvalidated_version_cannot_be_acquired(self):
        use = self.make()
        use.expires_at = 1  # even with a window, an unproved identity is refused
        with self.assertRaises(GateError) as caught:
            use.call("acquire", PRE)
        self.assertEqual(str(caught.exception), "staging_drill_identity_unproved")
        self.assertEqual(self.runtime.calls, [])


class WindowTests(UseHarness):
    def test_begin_acquires_then_readies_with_one_25_minute_deadline(self):
        use = self.started()
        self.assertEqual(use.expires_at, int(BASE_TIME * 1000) + WINDOW_MS)
        self.assertEqual(self.runtime.actions, ["acquire", "ready"])
        for index in range(2):
            env = self.runtime.env_of(index)
            self.assertEqual(env["STAGING_USE_EXPIRES_AT"], str(use.expires_at))
            self.assertEqual((env["STAGING_USE_DEPLOYMENT"], env["STAGING_USE_SHA"]), (PRE, SHA_A))
        self.assertEqual(self.runtime.violations, [])

    def test_deadline_never_moves_across_check_rebind_and_finish(self):
        use = self.started()
        self.state.t += 60
        use.check()
        self.state.serving = TARGET
        self.state.t += 60
        use.rebind(TARGET)
        self.state.t += 60
        use.finish()
        self.assertEqual(self.runtime.actions, ["acquire", "ready", "restore-check", "acquire", "release"])
        self.assertEqual({call[2]["STAGING_USE_EXPIRES_AT"] for call in self.runtime.calls},
                         {str(int(BASE_TIME * 1000) + WINDOW_MS)})

    def test_a_second_window_cannot_begin_or_extend(self):
        use = self.started()
        self.state.t += 600
        with self.assertRaises(GateError) as caught:
            use.begin(PRE)
        self.assertEqual(str(caught.exception), "staging_drill_window_already_started")
        self.assertEqual(self.runtime.actions, ["acquire", "ready"])
        # A stale deadline smuggled into a fresh adapter is rejected by the runtime grant.
        other = self.make()
        other.shas = dict(use.shas)
        other.expires_at = use.expires_at + 1
        with self.assertRaises(GateError):
            other.call("acquire", PRE)

    def test_check_is_open_strictly_before_expiry(self):
        use = self.started()
        self.state.t = BASE_TIME + (WINDOW_MS - 1) / 1000 - 0.001
        use.check()
        for recovery in (False, True):
            with self.subTest(recovery=recovery):
                self.state.t = BASE_TIME + WINDOW_MS / 1000
                calls = len(self.runtime.calls)
                with self.assertRaises(GateError) as caught:
                    use.check(recovery=recovery)
                self.assertEqual(str(caught.exception), "staging_drill_window_ended")
                self.assertEqual(len(self.runtime.calls), calls)

    def test_check_asserts_the_active_owner_but_recovery_does_not_call(self):
        use = self.started()
        use.check()
        self.assertEqual(self.runtime.actions[-1], "restore-check")
        self.assertEqual(self.runtime.calls[-1][0], "staging-use-pipeline.mjs")
        self.assertNotIn("STAGING_USE_DEPLOYMENT", self.runtime.env_of(-1))
        calls = len(self.runtime.calls)
        use.check(recovery=True)  # fenced after a partial leg: retained grant only
        self.assertEqual(len(self.runtime.calls), calls)
        self.state.phase, self.state.running = "fenced", False
        with self.assertRaises(GateError) as caught:
            use.check()
        self.assertEqual(str(caught.exception), "staging_drill_control_failed")
        use.failed = False
        use.check(recovery=True)

    def test_rebind_is_a_low_level_acquire_after_takeover_without_readiness(self):
        use = self.started()
        with self.assertRaises(GateError):  # the runtime still serves PRE
            use.rebind(TARGET)
        use.failed = False
        self.state.serving = TARGET
        use.rebind(TARGET)
        self.assertEqual(self.runtime.actions, ["acquire", "ready", "acquire", "acquire"])
        self.assertEqual(self.runtime.calls[-1][0], "staging-use.mjs")
        self.assertEqual(self.runtime.env_of(-1)["STAGING_USE_DEPLOYMENT"], TARGET)
        use.finish()
        self.assertEqual(self.runtime.env_of(-1)["STAGING_USE_DEPLOYMENT"], TARGET)
        self.assertEqual(self.runtime.actions[-1], "release")


class FailureTests(UseHarness):
    def test_local_identity_refusal_stops_cleanup_after_a_valid_acquisition(self):
        use = self.started()
        with self.assertRaises(GateError) as caught:
            use.rebind(OTHER)
        self.assertEqual(str(caught.exception), "staging_drill_identity_unproved")
        self.assertTrue(use.failed)
        use.finish()
        self.assertEqual(self.runtime.actions, ["acquire", "ready"])
        for recovery in (False, True):
            with self.assertRaises(GateError) as caught:
                use.check(recovery=recovery)
            self.assertEqual(str(caught.exception), "staging_drill_window_ended")

    def test_refusal_marks_failed_and_stops_every_later_call(self):
        use = self.started()
        self.runtime.fail[("restore-check", 1)] = 1
        with self.assertRaises(GateError) as caught:
            use.check()
        self.assertEqual(str(caught.exception), "staging_drill_control_failed")
        self.assertTrue(use.failed)
        calls = len(self.runtime.calls)
        for attempt in (lambda: use.check(), lambda: use.check(recovery=True)):
            with self.assertRaises(GateError) as caught:
                attempt()
            self.assertEqual(str(caught.exception), "staging_drill_window_ended")
        use.finish()  # no cleanup call with possibly rejected authority
        self.assertEqual(len(self.runtime.calls), calls)

    def test_transport_failures_are_fixed_unavailable_and_redacted(self):
        for error in (OSError(RECEIPT), subprocess.TimeoutExpired(["node", TOKEN], 1),
                      subprocess.SubprocessError(CF_TOKEN)):
            with self.subTest(error=type(error).__name__):
                self.runtime.fail.clear()
                self.runtime.calls.clear()
                self.runtime.grant = None
                use = self.make()
                use.validate(VersionClient(), (PRE,))
                self.runtime.fail[("acquire", 1)] = error
                with self.assertRaises(GateError) as caught:
                    use.begin(PRE)
                self.assertEqual(str(caught.exception), "staging_drill_control_unavailable")
                self.assertIsNone(caught.exception.__cause__)
                self.assertTrue(caught.exception.__suppress_context__)
                for text in (str(caught.exception), repr(caught.exception.detail)):
                    for secret in (RECEIPT, TOKEN, CF_TOKEN):
                        self.assertNotIn(secret, text)
                self.assertTrue(use.failed)

    def test_expired_runtime_allows_stop_only_cleanup_without_extending(self):
        use = self.started()
        self.state.t += WINDOW_MS / 1000 + 1
        use.finish()
        self.assertEqual(self.runtime.actions[-1], "release")
        self.assertFalse(self.state.running)
        self.assertEqual(self.runtime.env_of(-1)["STAGING_USE_EXPIRES_AT"], str(use.expires_at))
        with self.assertRaises(GateError) as caught:
            use.call("acquire", PRE)
        self.assertEqual(str(caught.exception), "staging_drill_control_failed")

    def test_foreign_attempt_cannot_reuse_the_original_window(self):
        use = self.started()
        other = self.make(base_env(GITHUB_RUN_ATTEMPT="2"))
        other.shas = dict(use.shas)
        other.expires_at = use.expires_at
        with self.assertRaises(GateError) as caught:
            other.call("acquire", PRE)
        self.assertEqual(str(caught.exception), "staging_drill_control_failed")
        self.assertEqual(self.runtime.grant, {"expires": use.expires_at, "deployment": PRE})
        self.assertEqual(self.runtime.env_of(-1)["STAGING_USE_RECEIPT"], RECEIPT)


class IsolationTests(UseHarness):
    def test_child_environment_is_exactly_the_explicit_inputs(self):
        inherited = base_env(CLOUDFLARE_API_TOKEN=CF_TOKEN, CLOUDFLARE_ACCOUNT_ID="b" * 32,
                             GH_TOKEN=CF_TOKEN, NODE_OPTIONS="--require /tmp/x.js",
                             NODE_PATH=CF_TOKEN, LD_PRELOAD=CF_TOKEN, HTTPS_PROXY=CF_TOKEN,
                             HOME=CF_TOKEN, STAGING_WORKER_URL="https://evil.example",
                             OWNERSHIP_CONTROL_TOKEN="wrong", STAGING_USE_EXPIRES_AT="1",
                             STAGING_USE_DEPLOYMENT=OTHER, STAGING_USE_SHA=SHA_B,
                             OWNERSHIP_ACTOR="wrong", OWNERSHIP_RELEASE_FENCE="true")
        use = self.make(inherited)
        use.validate(VersionClient(), (PRE, TARGET))
        use.begin(PRE)
        use.check()
        self.state.serving = TARGET
        use.rebind(TARGET)
        use.finish()
        self.assertEqual(self.runtime.violations, [])
        expires = str(use.expires_at)
        for helper, action, env, kwargs, argv in self.runtime.calls:
            with self.subTest(action=action):
                common = {"GITHUB_RUN_ID": "123456", "GITHUB_RUN_ATTEMPT": "1",
                          "PATH": "/runtime/bin:/usr/bin", "STAGING_WORKER_URL": URL,
                          "OWNERSHIP_CONTROL_TOKEN": TOKEN, "STAGING_USE_RECEIPT": RECEIPT,
                          "STAGING_USE_EXPIRES_AT": expires}
                if action != "restore-check":
                    common["STAGING_USE_DEPLOYMENT"] = env["STAGING_USE_DEPLOYMENT"]
                    common["STAGING_USE_SHA"] = SHA_A
                self.assertEqual(env, common)
                self.assertEqual(argv[0], "node")
                self.assertEqual(argv[2], action)
                for secret in (TOKEN, RECEIPT, CF_TOKEN):
                    self.assertNotIn(secret, " ".join(argv))
                self.assertIs(kwargs["stdout"], subprocess.DEVNULL)
                self.assertIs(kwargs["stderr"], subprocess.DEVNULL)
                self.assertEqual(kwargs["timeout"], CONTROL_TIMEOUT_SECONDS)
                self.assertIs(kwargs["check"], False)
                self.assertNotIn("capture_output", kwargs)

    def test_path_defaults_when_the_runtime_has_none(self):
        env = base_env()
        del env["PATH"]
        use = self.make(env)
        use.validate(VersionClient(), (PRE,))
        use.begin(PRE)
        self.assertEqual(self.runtime.env_of(0)["PATH"], use_module.os.defpath)

    def test_adapter_state_and_errors_never_hold_the_credentials_in_evidence(self):
        use = self.started()
        self.runtime.fail[("restore-check", 1)] = 1
        with self.assertRaises(GateError) as caught:
            use.check()
        text = json.dumps({"shas": use.shas, "expires": use.expires_at, "failed": use.failed,
                           "error": str(caught.exception), "detail": caught.exception.detail,
                           "version": use.version})
        for secret in (TOKEN, RECEIPT, CF_TOKEN):
            self.assertNotIn(secret, text)

    def test_source_never_passes_captured_output_or_shell(self):
        source = Path(use_module.__file__).read_text()
        self.assertNotIn("capture_output", source)
        self.assertNotIn("shell=True", source)
        self.assertNotIn("print(", source)


if __name__ == "__main__":
    unittest.main()
