"""Copy-adapter contract for immutable staging promotion (bot_promotion._copy_image).

Independent spec: replace ONLY ``bot_promotion._command`` external I/O with
strict stateful doubles; exercise the REAL ``bot_promotion._copy_image``.

Positive path:
  Wrangler ephemeral registry credentials -> temporary Docker config ->
  login with password on stdin -> pull admitted digest -> tag/push only the
  fixed production repo -> inspect local RepoDigests AND remote manifest
  Descriptor.digest -> exact copied reference. No build/bundle command.

Covers: credential account/host mismatch, absent/oversize credential,
login/pull/tag/push/inspect failures, local/remote digest drift, foreign
destination/account refusal, no credential retry/fallback, no secret in
argv/output/persistent Docker config, cleanup on failures.

Synthetic data only. No network, credentials, deploy, or implementation edit.
"""

import contextlib
import importlib.util
import io
import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / "scripts"
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))


def _load():
    path = SCRIPTS / "bot_promotion.py"
    assert path.is_file(), "missing promotion command under test"
    spec = importlib.util.spec_from_file_location("bot_promotion_copy_adapter", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


bp = _load()

# Independently derived literals from the assignment (NOT from the module
# under test): the real positive path is fixed to registry.cloudflare.com
# with the staging/production container apps. A mutation of the module's
# own constants must turn these tests RED, never follow along.
REGISTRY = "registry.cloudflare.com"
STAGING_APP = "two-bot-next-twobotcontainer-staging"
PRODUCTION_APP = "two-bot-next-twobotcontainer-production"
ACCOUNT = "testacct01"
OTHER_ACCOUNT = "foreign99"
HEX = "ab" * 32
OTHER_HEX = "cd" * 32
DIGEST = "sha256:" + HEX
OTHER_DIGEST = "sha256:" + OTHER_HEX
SOURCE = f"{REGISTRY}/{ACCOUNT}/{STAGING_APP}@{DIGEST}"
DEST = f"{REGISTRY}/{ACCOUNT}/{PRODUCTION_APP}"
EXPECTED = f"{DEST}@{DIGEST}"
TAG = f"{DEST}:promotion-{HEX}"
PROD_SOURCE = f"{REGISTRY}/{ACCOUNT}/{PRODUCTION_APP}@{DIGEST}"

VALID_CREDS = {
    "account_id": ACCOUNT,
    "registry_host": REGISTRY,
    "username": "ephemeral-user_1",
    "password": "synthetic-ephemeral-password",
}


class FakeCommand:
    """Strict stateful double for bot_promotion._command."""

    def __init__(self, handlers):
        # handlers: list of (label, check_fn(argv, kwargs)->bytes|raise)
        self.handlers = list(handlers)
        self.calls = []
        self.index = 0

    def __call__(self, argv, *, cap, timeout=60, env=None, input_bytes=None):
        call_no = len(self.calls)
        self.calls.append(
            {"argv": list(argv), "cap": cap, "timeout": timeout,
             "env": dict(env) if env is not None else None,
             "input_bytes": input_bytes,
             "has_input": input_bytes is not None}
        )
        assert self.index < len(self.handlers), (
            f"unexpected extra command #{call_no}: {argv} "
            f"(no retry/fallback allowed)"
        )
        label, check = self.handlers[self.index]
        self.index += 1
        return check(list(argv), {"cap": cap, "timeout": timeout,
                                 "env": env, "input_bytes": input_bytes})


def _argv_starts(argv, *prefix):
    return argv[:len(prefix)] == list(prefix)


class CopyAdapterFixture(unittest.TestCase):
    def setUp(self):
        self.old_account = os.environ.get("CLOUDFLARE_ACCOUNT_ID")
        os.environ["CLOUDFLARE_ACCOUNT_ID"] = ACCOUNT
        self.addCleanup(self._restore_account)
        # Any direct subprocess/network bypass is a contract violation.
        for target in ("subprocess.run", "subprocess.Popen",
                       "socket.create_connection", "urllib.request.urlopen"):
            p = patch(target, side_effect=AssertionError("bypassed _command: " + target))
            p.start()
            self.addCleanup(p.stop)
        self.docker_configs = []

    def _restore_account(self):
        if self.old_account is None:
            os.environ.pop("CLOUDFLARE_ACCOUNT_ID", None)
        else:
            os.environ["CLOUDFLARE_ACCOUNT_ID"] = self.old_account

    def run_copy(self, fake, source=SOURCE, destination=DEST):
        stdout, stderr = io.StringIO(), io.StringIO()
        with patch.object(bp, "_command", side_effect=fake):
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                try:
                    result = bp._copy_image(source, destination)
                except Exception as error:
                    self._assert_no_leak(fake, stdout.getvalue(), stderr.getvalue(), error)
                    self._assert_temp_cleaned(fake)
                    raise
                self._assert_no_leak(fake, stdout.getvalue(), stderr.getvalue(), None)
                self._assert_temp_cleaned(fake)
                return result

    def _assert_no_leak(self, fake, stdout_text, stderr_text, error):
        password = VALID_CREDS["password"]
        for call in fake.calls:
            for token in call["argv"]:
                self.assertNotIn(password, str(token),
                                 "secret must never appear in argv")
        # Captured stdio on success AND failure: a mutation that prints the
        # credential (or the refusal path that echoes it) turns RED here.
        self.assertNotIn(password, stdout_text,
                         "secret must never reach stdout")
        self.assertNotIn(password, stderr_text,
                         "secret must never reach stderr")
        if error is not None:
            self.assertNotIn(password, str(error),
                             "secret must never reach the refusal message")
        # Docker config must be ephemeral, never the ambient value.
        for call in fake.calls:
            if call["env"] is not None and "DOCKER_CONFIG" in call["env"]:
                self.docker_configs.append(call["env"]["DOCKER_CONFIG"])
        # Original process env must not gain a persistent Docker config.
        self.assertNotEqual(os.environ.get("DOCKER_CONFIG"), self.docker_configs[-1]
                            if self.docker_configs else "no-config-seen")

    def _assert_temp_cleaned(self, fake):
        for cfg in self.docker_configs:
            self.assertFalse(Path(cfg).exists(),
                             f"temporary Docker config not cleaned: {cfg}")
        # Every docker invocation after credentials must carry the same temp config.
        docker_envs = [c["env"]["DOCKER_CONFIG"] for c in fake.calls
                       if c["env"] is not None and c["argv"][:1] == ["docker"]]
        if docker_envs:
            self.assertEqual(len(set(docker_envs)), 1,
                             "all docker calls must share one temporary DOCKER_CONFIG")
            self.assertIn("promotion-docker-", Path(docker_envs[0]).name
                          if docker_envs[0] else "")

    def positive_handlers(self, creds="__default__", repo_digests="__default__",
                          remote_digest="__default__"):
        creds = VALID_CREDS if creds == "__default__" else creds
        repo_digests = [EXPECTED] if repo_digests == "__default__" else repo_digests
        remote_digest = DIGEST if remote_digest == "__default__" else remote_digest

        def check_creds(argv, kw):
            self.assertEqual(
                argv, ["npx", "--no-install", "wrangler", "containers",
                       "registries", "credentials", REGISTRY,
                       "--pull", "--push", "--expiration-minutes", "15", "--json"])
            self.assertEqual(kw["cap"], 32768)
            self.assertIsNone(kw["env"])
            self.assertIsNone(kw["input_bytes"])
            return json.dumps(creds).encode()

        def check_login(argv, kw):
            self.assertEqual(
                argv, ["docker", "login", "--password-stdin", "--username",
                       creds["username"], REGISTRY])
            self.assertEqual(kw["cap"], 32768)
            self.assertIsNotNone(kw["env"])
            self.assertIn("DOCKER_CONFIG", kw["env"])
            self.assertEqual(kw["input_bytes"], creds["password"].encode())
            return b"Login Succeeded"

        def check_pull(argv, kw):
            self.assertEqual(argv, ["docker", "pull", SOURCE])
            self.assertEqual(kw["cap"], 1024 * 1024)
            self.assertEqual(kw["timeout"], 600)
            self.assertIsNotNone(kw["env"])
            self.assertIsNone(kw["input_bytes"])
            return b"pulled"

        def check_tag(argv, kw):
            self.assertEqual(argv, ["docker", "tag", SOURCE, TAG])
            self.assertEqual(kw["cap"], 32768)
            self.assertIsNone(kw["input_bytes"])
            return b""

        def check_push(argv, kw):
            self.assertEqual(argv, ["docker", "push", TAG])
            self.assertEqual(kw["cap"], 1024 * 1024)
            self.assertEqual(kw["timeout"], 600)
            self.assertIsNone(kw["input_bytes"])
            return b"pushed"

        def check_inspect(argv, kw):
            self.assertEqual(
                argv, ["docker", "image", "inspect", "--format",
                       "{{json .RepoDigests}}", TAG])
            self.assertEqual(kw["cap"], 32768)
            self.assertIsNone(kw["input_bytes"])
            return json.dumps(repo_digests).encode()

        def check_manifest(argv, kw):
            self.assertEqual(
                argv, ["docker", "manifest", "inspect", "--verbose", EXPECTED])
            self.assertEqual(kw["cap"], 1024 * 1024)
            self.assertIsNone(kw["input_bytes"])
            return json.dumps({"Descriptor": {"digest": remote_digest}}).encode()

        return [("creds", check_creds), ("login", check_login),
                ("pull", check_pull), ("tag", check_tag),
                ("push", check_push), ("inspect", check_inspect),
                ("manifest", check_manifest)]


class PositiveCopyTests(CopyAdapterFixture):
    def test_exact_copied_reference_with_dual_digest_proof(self):
        fake = FakeCommand(self.positive_handlers())
        result = self.run_copy(fake)
        self.assertEqual(result, EXPECTED)
        self.assertEqual(len(fake.calls), 7)
        # No build/bundle command ever issued.
        forbidden = ("build", "buildx", "bundle", "deploy", "outdir", "compile")
        for call in fake.calls:
            joined = " ".join(call["argv"]).lower()
            for word in forbidden:
                self.assertNotIn(word, joined,
                                 f"copy must never build/bundle: {call['argv']}")

    def test_password_only_on_stdin_never_argv_or_output(self):
        fake = FakeCommand(self.positive_handlers())
        result = self.run_copy(fake)
        self.assertEqual(result, EXPECTED)
        login = fake.calls[1]
        self.assertEqual(login["argv"][0:2], ["docker", "login"])
        self.assertIn("--password-stdin", login["argv"])
        self.assertNotIn("--password", login["argv"])
        self.assertEqual(login["input_bytes"], VALID_CREDS["password"].encode())
        self.assertNotIn(VALID_CREDS["password"], result)
        self.assertNotIn(VALID_CREDS["password"], " ".join(login["argv"]))


class CredentialRefusalTests(CopyAdapterFixture):
    def _refused_creds(self, creds, reason):
        fake = FakeCommand(self.positive_handlers(creds=creds))
        with self.assertRaises(bp.PromotionError, msg=reason):
            self.run_copy(fake)
        # No credential retry/fallback: exactly one wrangler call, no docker.
        self.assertEqual(len(fake.calls), 1, reason)
        self.assertEqual(fake.calls[0]["argv"][0], "npx")

    def test_credential_account_mismatch_refuses_without_retry(self):
        bad = dict(VALID_CREDS, account_id=OTHER_ACCOUNT)
        self._refused_creds(bad, "account mismatch")

    def test_credential_host_mismatch_refuses_without_retry(self):
        bad = dict(VALID_CREDS, registry_host="registry.example.com")
        self._refused_creds(bad, "host mismatch")

    def test_absent_credential_fields_refuse(self):
        for drop in ("password", "username", "account_id", "registry_host"):
            with self.subTest(drop=drop):
                bad = {k: v for k, v in VALID_CREDS.items() if k != drop}
                self._refused_creds(bad, f"absent {drop}")

    def test_empty_and_oversize_password_refuse(self):
        for password in ("", "x" * 4097):
            with self.subTest(length=len(password)):
                bad = dict(VALID_CREDS, password=password)
                self._refused_creds(bad, f"password length {len(password)}")

    def test_oversize_password_bytes_refuse(self):
        # 4096-byte cap is on encoded bytes; multibyte chars exceed it sooner.
        bad = dict(VALID_CREDS, password="é" * 2049)
        self.assertGreater(len(bad["password"].encode()), 4096)
        self._refused_creds(bad, "multibyte oversize")

    def test_bad_username_shape_refuses(self):
        for username in ("", "has space", "semi;colon", "x" * 129, 123):
            with self.subTest(username=repr(username)):
                bad = dict(VALID_CREDS, username=username)
                self._refused_creds(bad, f"username {username!r}")

    def test_non_object_or_malformed_credential_payload_refuses(self):
        # Valid JSON with the wrong shape is a PromotionError; undecodable
        # bytes refuse as ValueError before any docker call either way.
        malformed = [b"{not json", b"\xff\xfe"]
        shaped = [b"null", b"[]", b'"str"',
                  json.dumps([]).encode(), json.dumps("x").encode(),
                  json.dumps({}).encode()]
        for payload in malformed:
            with self.subTest(payload=payload[:12]):
                def check(argv, kw, payload=payload):
                    self.assertEqual(argv[0], "npx")
                    return payload

                fake = FakeCommand([("creds", check)])
                with self.assertRaises(ValueError):
                    self.run_copy(fake)
                self.assertEqual(len(fake.calls), 1)
        for payload in shaped:
            with self.subTest(payload=payload[:12]):
                def check(argv, kw, payload=payload):
                    self.assertEqual(argv[0], "npx")
                    return payload

                fake = FakeCommand([("creds", check)])
                with self.assertRaises(bp.PromotionError):
                    self.run_copy(fake)
                self.assertEqual(len(fake.calls), 1)

    def test_wrangler_command_failure_refuses_without_fallback(self):
        def boom(argv, kw):
            raise bp.PromotionError("registry authentication refused")

        fake = FakeCommand([("creds", boom)])
        with self.assertRaises(bp.PromotionError):
            self.run_copy(fake)
        self.assertEqual(len(fake.calls), 1)


class ScopeRefusalTests(CopyAdapterFixture):
    def test_foreign_destination_refuses_before_any_command(self):
        for dest in (f"{REGISTRY}/{OTHER_ACCOUNT}/{PRODUCTION_APP}",
                     f"{REGISTRY}/{ACCOUNT}/other-app",
                     f"registry.example.com/{ACCOUNT}/{PRODUCTION_APP}"):
            with self.subTest(dest=dest):
                fake = FakeCommand([])
                with self.assertRaises(bp.PromotionError):
                    self.run_copy(fake, destination=dest)
                self.assertEqual(fake.calls, [], "scope refusal must precede I/O")

    def test_foreign_source_account_or_app_refuses_before_any_command(self):
        bad_sources = [
            f"{REGISTRY}/{OTHER_ACCOUNT}/{STAGING_APP}@{DIGEST}",
            f"{REGISTRY}/{ACCOUNT}/{PRODUCTION_APP}@{DIGEST}",
            f"{REGISTRY}/{ACCOUNT}/other-app@{DIGEST}",
            f"registry.example.com/{ACCOUNT}/{STAGING_APP}@{DIGEST}",
        ]
        for source in bad_sources:
            with self.subTest(source=source):
                fake = FakeCommand([])
                with self.assertRaises(bp.PromotionError):
                    self.run_copy(fake, source=source)
                self.assertEqual(fake.calls, [], "scope refusal must precede I/O")

    def test_mutable_tag_source_refuses(self):
        for source in (f"{REGISTRY}/{ACCOUNT}/{STAGING_APP}:latest",
                       f"{REGISTRY}/{ACCOUNT}/{STAGING_APP}",
                       PROD_SOURCE.replace(DIGEST, ":promotion-" + HEX)):
            with self.subTest(source=source):
                fake = FakeCommand([])
                with self.assertRaises(bp.PromotionError):
                    self.run_copy(fake, source=source)
                self.assertEqual(fake.calls, [])

    def test_source_digest_must_match_account_context(self):
        os.environ["CLOUDFLARE_ACCOUNT_ID"] = OTHER_ACCOUNT
        fake = FakeCommand([])
        with self.assertRaises(bp.PromotionError):
            self.run_copy(fake, source=SOURCE, destination=DEST)
        self.assertEqual(fake.calls, [])


class StepFailureTests(CopyAdapterFixture):
    def _fail_at(self, step_label):
        handlers = self.positive_handlers()
        labels = [label for label, _ in handlers]
        self.assertIn(step_label, labels)
        failing = []
        for label, check in handlers:
            if label == step_label:
                def boom(argv, kw, label=label):
                    raise bp.PromotionError(f"injected {label} failure")
                failing.append((label, boom))
                break
            failing.append((label, check))
        # Strict double: any call past the failure is itself a violation.
        fake = FakeCommand(failing)
        with self.assertRaises(bp.PromotionError, msg=step_label):
            self.run_copy(fake)
        self.assertEqual(fake.calls[-1]["argv"][0] in ("npx", "docker"), True)
        step_index = labels.index(step_label)
        self.assertEqual(len(fake.calls), step_index + 1,
                         f"no steps may run after {step_label} failure")
        return fake

    def test_login_failure_aborts_before_pull(self):
        self._fail_at("login")

    def test_pull_failure_aborts_before_tag(self):
        self._fail_at("pull")

    def test_tag_failure_aborts_before_push(self):
        self._fail_at("tag")

    def test_push_failure_aborts_before_inspect(self):
        self._fail_at("push")

    def test_inspect_failure_aborts_before_manifest(self):
        self._fail_at("inspect")

    def test_manifest_failure_aborts_copy(self):
        self._fail_at("manifest")


class DigestDriftTests(CopyAdapterFixture):
    def test_local_repo_digests_drift_refuses(self):
        # Local proof is the 6th call; drift refuses before the remote check,
        # so exactly 6 commands run and the manifest is never read.
        for digests in ([f"{DEST}@{OTHER_DIGEST}"],
                        [TAG],
                        [],
                        "not-a-list",
                        {},
                        [None]):
            with self.subTest(digests=repr(digests)[:60]):
                fake = FakeCommand(self.positive_handlers(repo_digests=digests))
                with self.assertRaises(bp.PromotionError):
                    self.run_copy(fake)
                self.assertEqual(len(fake.calls), 6)

    def test_remote_manifest_drift_refuses(self):
        for remote in (OTHER_DIGEST, "", None, 123):
            with self.subTest(remote=repr(remote)):
                fake = FakeCommand(self.positive_handlers(remote_digest=remote))
                with self.assertRaises(bp.PromotionError):
                    self.run_copy(fake)
                self.assertEqual(len(fake.calls), 7)

    def test_remote_manifest_shape_drift_refuses(self):
        handlers = self.positive_handlers()

        def bad_shape(argv, kw):
            self.assertEqual(argv[:3], ["docker", "manifest", "inspect"])
            return json.dumps({"Descriptor": {}}).encode()

        fake = FakeCommand(handlers[:-1] + [("manifest", bad_shape)])
        with self.assertRaises(bp.PromotionError):
            self.run_copy(fake)

        def no_descriptor(argv, kw):
            return json.dumps({"digest": DIGEST}).encode()

        fake = FakeCommand(handlers[:-1] + [("manifest", no_descriptor)])
        with self.assertRaises(bp.PromotionError):
            self.run_copy(fake)

    def test_copy_result_must_equal_production_repo_plus_admitted_digest(self):
        # A double returning a drifted ref must never be laundered: the real
        # adapter derives EXPECTED itself and both proofs must bind it.
        fake = FakeCommand(self.positive_handlers(
            repo_digests=[f"{DEST}@{OTHER_DIGEST}"],
            remote_digest=OTHER_DIGEST))
        with self.assertRaises(bp.PromotionError):
            self.run_copy(fake)


class HygieneTests(CopyAdapterFixture):
    def test_tag_and_pushconfined_to_fixed_production_repo(self):
        fake = FakeCommand(self.positive_handlers())
        self.assertEqual(self.run_copy(fake), EXPECTED)
        tag_call = fake.calls[3]
        push_call = fake.calls[4]
        manifest_call = fake.calls[6]
        self.assertEqual(tag_call["argv"], ["docker", "tag", SOURCE, TAG])
        self.assertTrue(TAG.startswith(DEST + ":promotion-"))
        self.assertNotIn(SOURCE.split("@")[0], TAG)
        self.assertEqual(push_call["argv"], ["docker", "push", TAG])
        self.assertEqual(manifest_call["argv"][-1], EXPECTED)
        for call in (tag_call, push_call):
            for token in call["argv"]:
                self.assertNotIn(OTHER_ACCOUNT, token)
                self.assertNotIn("foreign", token)

    def test_temporary_docker_config_used_and_removed(self):
        fake = FakeCommand(self.positive_handlers())
        before = os.environ.get("DOCKER_CONFIG")
        self.assertEqual(self.run_copy(fake), EXPECTED)
        docker_cfgs = {c["env"]["DOCKER_CONFIG"] for c in fake.calls
                       if c["env"] is not None and c["argv"][:1] == ["docker"]}
        self.assertEqual(len(docker_cfgs), 1)
        cfg = next(iter(docker_cfgs))
        self.assertIn("promotion-docker-", Path(cfg).name)
        self.assertFalse(Path(cfg).exists(), "temp Docker config must be removed")
        self.assertEqual(os.environ.get("DOCKER_CONFIG"), before)
        # Temp dir must live under the system temp area, never the repo.
        self.assertNotIn(str(ROOT), cfg)
        self.assertEqual(fake.calls[0]["env"], None,
                         "wrangler credentials call must not inherit Docker config")

    def test_failure_still_cleans_temporary_config(self):
        fake = self._fail_at_push()
        cfgs = {c["env"]["DOCKER_CONFIG"] for c in fake.calls
                if c["env"] is not None}
        for cfg in cfgs:
            self.assertFalse(Path(cfg).exists())

    def _fail_at_push(self):
        handlers = self.positive_handlers()
        failing = []
        for label, check in handlers:
            if label == "push":
                def boom(argv, kw):
                    raise bp.PromotionError("injected push failure")
                failing.append((label, boom))
                break
            failing.append((label, check))
        fake = FakeCommand(failing)
        with self.assertRaises(bp.PromotionError):
            self.run_copy(fake)
        return fake

    def test_no_persistent_docker_config_file_written(self):
        # Synthetic HOME only: the real ~/.docker/config.json is never read.
        tmp = tempfile.TemporaryDirectory(prefix="synthetic-home-")
        self.addCleanup(tmp.cleanup)
        synthetic_home = Path(tmp.name)
        home_config = synthetic_home / ".docker" / "config.json"
        home_config.parent.mkdir(parents=True)
        home_config.write_bytes(b'{"synthetic": "sentinel-config"}')
        old_home = os.environ.get("HOME")
        os.environ["HOME"] = str(synthetic_home)
        self.addCleanup(lambda: os.environ.pop("HOME", None)
                        if old_home is None else os.environ.__setitem__("HOME", old_home))
        # Path.home() must resolve inside the synthetic HOME for this check.
        self.assertEqual(Path.home(), synthetic_home)
        fake = FakeCommand(self.positive_handlers())
        self.assertEqual(self.run_copy(fake), EXPECTED)
        self.assertEqual(home_config.read_bytes(), b'{"synthetic": "sentinel-config"}',
                         "must not touch persistent Docker config")


if __name__ == "__main__":
    unittest.main()
