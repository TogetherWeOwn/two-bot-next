"""Offline acceptance contract for staging-byte promotion (intentionally RED).

One new deterministic seam, not an implementation:
  bot_promotion.main(argv, *, github=None, copy_image=None) -> exit status
  github.get_json(repo_relative_route), github.get_bytes(repo_relative_route)
  copy_image(staging_digest_ref, production_repository) -> copied_digest_ref

The real CLI must supply those external adapters. Tests replace only those I/O
boundaries, never validation, packaging, rendering or readiness. No credentials,
HTTP, registry calls, compilation or bundling are allowed in these fixtures.

certify --modules DIR --entrypoint index.js --evidence FILE --config TOML
        --sha SHA --run-id N --attempt N --deploy-id ID
        --readiness-result success --synthetic-result success --fault-mode none
        --out ZIP
admit --sha SHA --run-id N --attempt N --account-id ACCOUNT --out DIR

certify emits promotion-receipt.json and modules/<relative path>, nothing else.
admit independently reads the current source run and immutable artifact metadata
via GitHub, validates the whole ZIP before copy_image, rechecks the current run,
and writes DIR/{admitted.json,modules/...}. admitted.json has exactly
{schema_version: 1, receipt: <receipt>, production_image: <copied digest ref>}.
The receipt shape is the literal fixture below. Artifact name binds run/attempt.
Intake ceilings: 64 KiB receipt, 32 modules, 8 MiB/member, 16 MiB total expanded.
Compatibility date/flags must match the checked-out production config.

Existing production_deploy render/readyz gain optional --artifact FILE. render
uses saved modules and production config only, with a production-local image,
no image_vars/build context and no bundling. readyz additionally requires
--expected-worker-version UUID --worker-version UUID --image DIGEST_REF, and
compares the admitted ORIGINAL stamps, not this production event's IDs.
Legacy render/readyz/recovery invocations remain unchanged.

Workflow tests bind the seam to actual producer/consumer jobs; a disconnected
validator cannot satisfy the contract. The implementation adopts these tests
without changing their fixtures or weakening the existing production controls.
"""

import contextlib
import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import tomllib
import unittest
import warnings
from unittest.mock import patch
import zipfile

import yaml

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / "scripts"
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))
import production_deploy
import worker_container_rollback

REPO = "TogetherWeOwn/two-bot-next"
SHA = "0123456789abcdef0123456789abcdef01234567"
STAGE_RUN = 501
ATTEMPT = 2
BUILD = "501-2"
STAMP = "2026.10.21.1+0123456"
EVENT = "2026.10.22.3+0123456"
STAGE_VERSION = "9f8e7d6c-5b4a-4c3d-9e2f-1a0b9c8d7e6f"
PROD_VERSION = "0b6e5e3c-1f2a-4b3c-8d4e-5f6a7b8c9d0e"
DIGEST = "sha256:" + "c" * 64
STAGE_IMAGE = "registry.cloudflare.com/acct/two-bot-next-twobotcontainer-staging@" + DIGEST
PROD_REPO = "registry.cloudflare.com/acct/two-bot-next-twobotcontainer-production"
PROD_IMAGE = PROD_REPO + "@" + DIGEST
MODULES = {"index.js": b'import payload from "./payload.wasm"; export default {fetch(){return payload}};\n',
           "payload.wasm": b"\x00asm\x01\x00\x00\x00"}
CONFIG = '''name = "two-bot-next"
main = "src/index.ts"
compatibility_date = "2026-09-29"
compatibility_flags = ["nodejs_compat", "nodejs_compat_do_not_populate_process_env"]
[[containers]]
class_name = "TwoBotContainer"
image = "../Dockerfile"
max_instances = 1
[env.staging.vars]
ENV_SENTINEL = "staging-only"
TWO_DEPLOY_ENV = "staging"
[[env.staging.containers]]
class_name = "TwoBotContainer"
image = "../Dockerfile"
max_instances = 1
[env.production.vars]
ENV_SENTINEL = "production-only"
[[env.production.containers]]
class_name = "TwoBotContainer"
image = "../Dockerfile"
max_instances = 1
[[env.production.durable_objects.bindings]]
name = "TWO_BOT"
class_name = "TwoBotContainer"
[[env.production.hyperdrive]]
binding = "REDIRECT_DB"
id = "production-binding"
'''
RUN_ROUTE = f"repos/{REPO}/actions/runs/{STAGE_RUN}"
ARTIFACTS_ROUTE = RUN_ROUTE + "/artifacts?per_page=100"
ARTIFACT_ROUTE = f"repos/{REPO}/actions/artifacts/7700"
ZIP_ROUTE = ARTIFACT_ROUTE + "/zip"
NAME = f"bot-promotion-{STAGE_RUN}-{ATTEMPT}"


def receipt():
    return {
        "schema_version": 1,
        "repository": {"id": 1, "full_name": REPO},
        "source": {"sha": SHA, "run_id": STAGE_RUN, "run_attempt": ATTEMPT,
                   "branch": "main", "workflow": ".github/workflows/deploy-staging.yml"},
        "build": {"revision": SHA, "build_id": BUILD, "deploy_id": STAMP},
        "staging": {"worker_version": STAGE_VERSION,
                    "application_id": "11111111-1111-4111-8111-111111111111",
                    "rollout_id": "22222222-2222-4222-8222-222222222222", "target_version": 7,
                    "image": STAGE_IMAGE, "revision": SHA, "build_id": BUILD,
                    "deploy_id": STAMP, "readyz": 200, "health": 200},
        "container": {"image": STAGE_IMAGE, "digest": DIGEST},
        "worker": {"entrypoint": "index.js", "compatibility_date": "2026-09-29",
                   "compatibility_flags": ["nodejs_compat", "nodejs_compat_do_not_populate_process_env"],
                   "modules": [{"path": name, "size": len(data),
                                "sha256": hashlib.sha256(data).hexdigest()}
                               for name, data in MODULES.items()]},
    }


def run_record():
    return {"id": STAGE_RUN, "run_number": 5, "run_attempt": ATTEMPT,
            "head_sha": SHA, "head_branch": "main",
            "path": ".github/workflows/deploy-staging.yml", "workflow_id": 17,
            "status": "completed", "conclusion": "success", "event": "push",
            "repository": {"id": 1, "full_name": REPO},
            "head_repository": {"id": 1, "full_name": REPO}}


def package(record=None, members=None, raw=None):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("promotion-receipt.json", raw if raw is not None else json.dumps(
            receipt() if record is None else record))
        for name, data in (MODULES if members is None else members).items():
            archive.writestr("modules/" + name, data)
    return buffer.getvalue()


class GitHubFixture:
    """Only read endpoints; unknown routes fail rather than reaching a service."""

    def __init__(self, blob):
        self.run = run_record()
        self.refreshed_run = None
        self.run_reads = 0
        self.calls = []
        self.blob = blob
        self.artifact = {
            "id": 7700, "name": NAME, "expired": False,
            "size_in_bytes": len(blob),
            "digest": "sha256:" + hashlib.sha256(blob).hexdigest(),
            "workflow_run": {"id": STAGE_RUN, "repository_id": 1, "head_repository_id": 1,
                             "head_branch": "main", "head_sha": SHA},
        }
        self.listing = {"total_count": 1, "artifacts": [copy.deepcopy(self.artifact)]}

    def get_json(self, route):
        self.calls.append(route)
        if route == RUN_ROUTE:
            self.run_reads += 1
            return copy.deepcopy(self.refreshed_run if self.run_reads > 1 and self.refreshed_run
                                 else self.run)
        if route == ARTIFACTS_ROUTE:
            return copy.deepcopy(self.listing)
        if route == ARTIFACT_ROUTE:
            return copy.deepcopy(self.artifact)
        raise AssertionError("unexpected GitHub read: " + route)

    def get_bytes(self, route):
        self.calls.append(route)
        if route != ZIP_ROUTE:
            raise AssertionError("archive was not fetched by immutable artifact ID")
        return self.blob


class PromotionFixture(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.config = self.root / "wrangler.toml"
        self.config.write_text(CONFIG)
        self.out = self.root / "admitted"
        self.mutations = []
        self.github = GitHubFixture(package())
        self.copy_result = PROD_IMAGE
        # A compiler/bundler process is a contract violation, not a usable fallback.
        for target in ("subprocess.run", "subprocess.Popen", "socket.create_connection",
                       "urllib.request.urlopen"):
            p = patch(target, side_effect=AssertionError("unexpected subprocess/network: " + target))
            p.start()
            self.addCleanup(p.stop)

    def command(self, argv):
        path = SCRIPTS / "bot_promotion.py"
        self.assertTrue(path.is_file(),
                        "missing immutable promotion command: production still rebuilds staged bytes")
        spec = importlib.util.spec_from_file_location("bot_promotion", path)
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            try:
                return module.main(argv, github=self.github, copy_image=self.copy_image)
            except SystemExit as exc:
                return exc.code

    def copy_image(self, source, destination):
        self.mutations.append((source, destination))
        return self.copy_result

    def admit(self):
        return self.command(["admit", "--sha", SHA, "--run-id", str(STAGE_RUN),
                             "--attempt", str(ATTEMPT), "--account-id", "acct", "--out", str(self.out)])

    def replace_package(self, record=None, members=None, raw=None, blob=None):
        self.github = GitHubFixture(package(record, members, raw) if blob is None else blob)

    def refused(self):
        code = self.admit()
        self.assertIsInstance(code, int)
        self.assertNotEqual(code, 0)
        self.assertEqual(self.mutations, [], "invalid intake reached production image copy")
        self.assertFalse((self.out / "admitted.json").exists())
        self.assertFalse((self.out / "modules/index.js").exists())

    def certify(self, readiness="success", synthetic="success", fault="none", stage=None):
        modules = self.root / "bundle"
        modules.mkdir(exist_ok=True)
        for name, data in MODULES.items():
            (modules / name).write_bytes(data)
        # Local environment material must never enter the module allowlist.
        (modules / ".env").write_text("SYNTHETIC=never-copy")
        evidence = self.root / "verified.json"
        evidence.write_text(json.dumps(receipt()["staging"] if stage is None else stage))
        self.output_zip = self.root / "certified.zip"
        return self.command([
            "certify", "--modules", str(modules), "--entrypoint", "index.js",
            "--evidence", str(evidence), "--config", str(self.config), "--sha", SHA,
            "--run-id", str(STAGE_RUN), "--attempt", str(ATTEMPT), "--deploy-id", STAMP,
            "--readiness-result", readiness, "--synthetic-result", synthetic,
            "--fault-mode", fault, "--out", str(self.output_zip)])


class ImmutablePromotionIntakeTests(PromotionFixture):
    def test_exact_staged_bytes_and_digest_reach_real_production_render_without_build(self):
        self.assertEqual(self.certify(), 0)
        self.github = GitHubFixture(self.output_zip.read_bytes())
        self.assertEqual(self.admit(), 0)
        self.assertGreaterEqual(self.github.run_reads, 2, "current source attempt was not rechecked")
        self.assertIn(ARTIFACT_ROUTE, self.github.calls)
        self.assertIn(ZIP_ROUTE, self.github.calls)
        self.assertEqual(self.mutations, [(STAGE_IMAGE, PROD_REPO)])
        for name, data in MODULES.items():
            self.assertEqual((self.out / "modules" / name).read_bytes(), data)
        admitted = json.loads((self.out / "admitted.json").read_text())
        self.assertEqual(admitted, {"schema_version": 1, "receipt": receipt(),
                                    "production_image": PROD_IMAGE})
        rendered = self.root / "production.json"
        code = production_deploy.main([
            "render", "--config", str(self.config), "--out", str(rendered), "--sha", SHA,
            "--run-id", "900", "--attempt", "1", "--deploy-id", EVENT,
            "--artifact", str(self.out / "admitted.json")])
        self.assertEqual(code, 0)
        config = json.loads(rendered.read_text())
        container = config["env"]["production"]["containers"][0]
        self.assertEqual(container["image"], PROD_IMAGE)
        self.assertNotIn("image_vars", container)
        self.assertNotIn("image_build_context", container)
        self.assertTrue(config.get("no_bundle"), "production must not recompile the staged module")
        self.assertEqual(Path(config["main"]).read_bytes(), MODULES["index.js"])
        original = tomllib.loads(CONFIG)["env"]["production"]
        self.assertEqual(config["env"]["production"]["hyperdrive"], original["hyperdrive"])
        self.assertEqual(config["env"]["production"]["durable_objects"], original["durable_objects"])
        self.assertEqual(config["env"]["production"]["vars"]["ENV_SENTINEL"], "production-only")
        self.assertNotIn("TWO_DEPLOY_ENV", config["env"]["production"]["vars"])
        self.assertEqual(self.config.read_text(), CONFIG)

    def test_consumer_rechecks_current_attempt_before_image_copy(self):
        self.github.refreshed_run = dict(run_record(), run_attempt=3)
        self.refused()

    def test_run_identity_mismatches_refuse_before_mutation(self):
        cases = [("head_sha", "f" * 40), ("id", 502), ("run_attempt", 1),
                 ("head_branch", "feature"), ("path", ".github/workflows/other.yml"),
                 ("status", "in_progress"), ("conclusion", "failure"),
                 ("repository", {"id": 2, "full_name": "elsewhere/bot"}),
                 ("head_repository", {"id": 2, "full_name": REPO}),
                 ("run_attempt", True)]
        for key, value in cases:
            with self.subTest(field=key, value=value):
                self.replace_package()
                self.github.run[key] = value
                self.refused()

    def test_missing_ambiguous_expired_and_incomplete_artifact_inventory_refuse(self):
        for case in ("missing", "ambiguous", "expired", "incomplete"):
            with self.subTest(case=case):
                self.replace_package()
                if case == "missing":
                    self.github.listing = {"total_count": 0, "artifacts": []}
                elif case == "ambiguous":
                    self.github.listing = {"total_count": 2, "artifacts": [
                        self.github.artifact, dict(self.github.artifact, id=7701)]}
                elif case == "expired":
                    self.github.artifact["expired"] = True
                    self.github.listing["artifacts"][0]["expired"] = True
                else:
                    self.github.listing["total_count"] = 101
                self.refused()

    def test_artifact_identity_and_archive_digest_are_independently_bound(self):
        for key, value in (("id", 7701), ("name", "bot-promotion-501-1"),
                           ("workflow_run", {"id": 502}),
                           ("digest", "sha256:" + "a" * 64), ("expired", True)):
            with self.subTest(field=key):
                self.replace_package()
                self.github.artifact[key] = value
                self.refused()

    def test_receipt_source_and_original_stamp_mismatches_refuse(self):
        cases = [("source", "sha", "f" * 40), ("source", "run_id", 502),
                 ("source", "run_attempt", 1), ("source", "branch", "feature"),
                 ("source", "workflow", ".github/workflows/rollback-drill.yml"),
                 ("repository", "id", 2), ("repository", "full_name", "elsewhere/bot"),
                 ("build", "revision", "f" * 40), ("build", "build_id", "501-1"),
                 ("build", "deploy_id", "2026.10.20.1+fffffff"),
                 ("staging", "build_id", "501-1"), ("staging", "readyz", 503)]
        for group, key, value in cases:
            with self.subTest(group=group, field=key):
                record = receipt()
                record[group][key] = value
                self.replace_package(record)
                self.refused()

    def test_duplicate_keys_wrong_schema_types_and_extra_fields_refuse(self):
        valid = json.dumps(receipt()).encode()
        cases = [valid.replace(b'"schema_version": 1', b'"schema_version": 1,"schema_version": 1'),
                 valid.replace(b'"schema_version": 1', b'"schema_version": 1,"\\u0073chema_version": 1'),
                 b'[]', b'null']
        for group, key, value in ((None, "schema_version", True), (None, "schema_version", 2),
                                 (None, "unsafe_config", {"DATABASE_URL": "fixture-only"}),
                                 ("source", "run_id", True), ("source", "run_attempt", "2"),
                                 ("staging", "target_version", True), ("staging", "health", True)):
            record = receipt()
            (record if group is None else record[group])[key] = value
            cases.append(json.dumps(record).encode())
        record = receipt()
        record["worker"]["modules"][0]["size"] = True
        cases.append(json.dumps(record).encode())
        for number, raw in enumerate(cases):
            with self.subTest(case=number):
                self.replace_package(raw=raw)
                self.refused()

    def test_traversal_absolute_backslash_duplicate_and_symlink_members_refuse(self):
        for name in ("../escape.js", "/absolute.js", "a/../../escape.js", "a\\escape.js"):
            with self.subTest(path=name):
                record = receipt()
                record["worker"]["modules"][0]["path"] = name
                record["worker"]["entrypoint"] = name
                self.replace_package(record, {name: MODULES["index.js"], "payload.wasm": MODULES["payload.wasm"]})
                self.refused()
        for kind in ("symlink", "duplicate"):
            with self.subTest(kind=kind):
                buffer = io.BytesIO()
                with warnings.catch_warnings(record=True), zipfile.ZipFile(buffer, "w") as archive:
                    archive.writestr("promotion-receipt.json", json.dumps(receipt()))
                    for name, data in MODULES.items():
                        info = zipfile.ZipInfo("modules/" + name)
                        info.create_system = 3
                        info.external_attr = ((stat.S_IFLNK | 0o777) if kind == "symlink" and name == "index.js"
                                              else (stat.S_IFREG | 0o644)) << 16
                        archive.writestr(info, data)
                    if kind == "duplicate":
                        archive.writestr("modules/index.js", MODULES["index.js"])
                self.replace_package(blob=buffer.getvalue())
                self.refused()

    def test_unsafe_extra_file_missing_module_and_corrupt_archive_refuse(self):
        for members in (dict(MODULES, **{".env": b"SYNTHETIC=never-copy"}),
                        {"index.js": MODULES["index.js"]},
                        dict(MODULES, **{"wrangler.json": b'{}'})):
            with self.subTest(members=tuple(members)):
                self.replace_package(members=members)
                self.refused()
        self.replace_package(blob=b"not a ZIP")
        self.refused()

    def test_bounded_receipt_and_module_intake_refuses_before_mutation(self):
        self.replace_package(raw=json.dumps(receipt()).encode() + b" " * (64 * 1024))
        self.refused()
        for members in ({"index.js": b"x" * (8 * 1024 * 1024 + 1)},
                        {f"m{i}.js": b"x" for i in range(33)},
                        {f"m{i}.js": b"x" * (6 * 1024 * 1024) for i in range(3)}):
            with self.subTest(count=len(members), total=sum(map(len, members.values()))):
                record = receipt()
                record["worker"]["entrypoint"] = next(iter(members))
                record["worker"]["modules"] = [{"path": key, "size": len(data),
                                                "sha256": hashlib.sha256(data).hexdigest()}
                                               for key, data in members.items()]
                self.replace_package(record, members)
                self.refused()

    def test_module_hash_size_and_container_digest_mismatches_refuse(self):
        for mutation in ("bytes", "hash", "size", "image", "entrypoint"):
            with self.subTest(mutation=mutation):
                record, members = receipt(), dict(MODULES)
                if mutation == "bytes":
                    members["index.js"] += b"changed"
                elif mutation in ("hash", "size"):
                    record["worker"]["modules"][0]["sha256" if mutation == "hash" else "size"] = (
                        "0" * 64 if mutation == "hash" else 1)
                elif mutation == "image":
                    record["container"]["digest"] = "sha256:" + "d" * 64
                else:
                    record["worker"]["entrypoint"] = "absent.js"
                self.replace_package(record, members)
                self.refused()

    def test_foreign_source_account_repository_or_mutable_image_refuses(self):
        for image in (STAGE_IMAGE.replace("/acct/", "/foreign/"),
                      STAGE_IMAGE.replace("-staging@", "-other@"),
                      STAGE_IMAGE.split("@")[0] + ":latest", PROD_IMAGE):
            with self.subTest(image=image):
                record = receipt()
                record["container"]["image"] = record["staging"]["image"] = image
                self.replace_package(record)
                self.refused()

    def test_copy_digest_drift_and_foreign_destination_never_publish_admission(self):
        for image in (PROD_REPO + "@sha256:" + "d" * 64,
                      PROD_IMAGE.replace("/acct/", "/foreign/"), STAGE_IMAGE):
            with self.subTest(image=image):
                self.copy_result = image
                self.assertNotEqual(self.admit(), 0)
                self.assertFalse((self.out / "admitted.json").exists())
                self.assertFalse((self.out / "modules/index.js").exists())


class ImmutablePromotionProducerTests(PromotionFixture):
    def test_certification_packages_only_allowlisted_served_modules_and_closed_receipt(self):
        self.assertEqual(self.certify(), 0)
        with zipfile.ZipFile(self.output_zip) as archive:
            self.assertEqual(set(archive.namelist()), {"promotion-receipt.json", *(
                "modules/" + name for name in MODULES)})
            self.assertEqual(json.loads(archive.read("promotion-receipt.json")), receipt())
            for name, data in MODULES.items():
                self.assertEqual(archive.read("modules/" + name), data)
        self.assertEqual(self.mutations, [])

    def test_success_flags_cannot_certify_unready_or_mismatched_rollout_evidence(self):
        for key, value in (("readyz", 503), ("health", 503), ("build_id", "501-1"),
                           ("revision", "f" * 40), ("image", PROD_IMAGE)):
            with self.subTest(field=key):
                evidence = dict(receipt()["staging"], **{key: value})
                self.assertNotEqual(self.certify(stage=evidence), 0)
                self.assertFalse(self.output_zip.exists())

    def test_failed_or_skipped_gate_and_drill_never_certify_but_keep_diagnostics(self):
        for readiness, synthetic, fault in (("failure", "success", "none"),
                                           ("success", "failure", "none"),
                                           ("success", "skipped", "none"),
                                           ("success", "success", "rollback"),
                                           ("success", "success", "gateway-failure"),
                                           ("success", "success", "pinned-history")):
            with self.subTest(readiness=readiness, synthetic=synthetic, fault=fault):
                diagnostic = self.root / "rollout-diagnostic.json"
                diagnostic.write_bytes(b"diagnostic-preserved")
                self.assertNotEqual(self.certify(readiness, synthetic, fault), 0)
                self.assertFalse(self.output_zip.exists())
                self.assertEqual(diagnostic.read_bytes(), b"diagnostic-preserved")


class AdmittedArtifactRenderTests(unittest.TestCase):
    def render(self, record=None, config=CONFIG, image=PROD_IMAGE):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            root = Path(tmp)
            source = root / "wrangler.toml"
            source.write_text(config)
            artifact = root / "admitted.json"
            artifact.write_text(json.dumps({"schema_version": 1, "receipt": receipt() if record is None else record,
                                             "production_image": image}))
            (root / "modules").mkdir()
            for name, data in MODULES.items():
                (root / "modules" / name).write_bytes(data)
            out = root / "production.json"
            output = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                try:
                    code = production_deploy.main([
                        "render", "--config", str(source), "--out", str(out), "--sha", SHA,
                        "--run-id", "900", "--attempt", "1", "--deploy-id", EVENT,
                        "--artifact", str(artifact)])
                except SystemExit as exc:
                    code = exc.code
            rendered = json.loads(out.read_text()) if out.exists() else None
            return code, rendered, output.getvalue()

    def test_render_uses_staged_module_and_production_digest_without_restamping(self):
        code, rendered, message = self.render()
        self.assertEqual(code, 0, message)
        self.assertTrue(rendered["no_bundle"])
        production = rendered["env"]["production"]
        self.assertEqual(production["containers"][0]["image"], PROD_IMAGE)
        self.assertNotIn("image_vars", production["containers"][0])
        self.assertNotIn("image_build_context", production["containers"][0])
        self.assertTrue(rendered["main"].endswith("/modules/index.js"))
        self.assertEqual(production["vars"]["ENV_SENTINEL"], "production-only")
        self.assertNotIn("TWO_DEPLOY_ENV", production["vars"])

    def test_incompatible_bundle_or_nonlocal_image_never_falls_back_to_build(self):
        code, _, message = self.render()
        self.assertEqual(code, 0, message)
        for config in (CONFIG.replace("2026-09-29", "2026-09-30"),
                       CONFIG.replace('"nodejs_compat", ', "")):
            with self.subTest(config=config):
                code, rendered, _ = self.render(config=config)
                self.assertNotEqual(code, 0)
                self.assertIsNone(rendered)
        for image in (STAGE_IMAGE, PROD_IMAGE.replace("-production@", "-foreign@"),
                      PROD_REPO + "@sha256:" + "d" * 64):
            with self.subTest(image=image):
                code, rendered, _ = self.render(image=image)
                self.assertNotEqual(code, 0)
                self.assertIsNone(rendered)


class AdmittedArtifactReadinessTests(unittest.TestCase):
    def judge(self, **changes):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            root = Path(tmp)
            artifact = root / "admitted.json"
            artifact.write_text(json.dumps({"schema_version": 1, "receipt": receipt(),
                                             "production_image": PROD_IMAGE}))
            body = {"build_revision": SHA, "build_id": BUILD, "deploy_id": STAMP,
                    "components": [["gateway", "ready"]]}
            status = changes.pop("status", "200")
            version = changes.pop("worker_version", PROD_VERSION)
            image = changes.pop("image", PROD_IMAGE)
            body.update(changes)
            path = root / "readyz.json"
            path.write_text(json.dumps(body))
            output = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                try:
                    code = production_deploy.main([
                        "readyz", "--sha", SHA, "--mode", "deploy", "--build-id", "900-1",
                        "--deploy-id", EVENT, "--status", status, "--body", str(path),
                        "--artifact", str(artifact), "--expected-worker-version", PROD_VERSION,
                        "--worker-version", version, "--image", image])
                except SystemExit as exc:
                    code = exc.code
            return code, output.getvalue()

    def test_original_staging_stamps_pass_with_new_production_worker_not_new_event_ids(self):
        code, message = self.judge()
        self.assertEqual(code, 0, message)
        self.assertNotEqual(STAGE_VERSION, PROD_VERSION)

    def test_wrong_original_stamp_same_sha_build_serving_version_image_or_gateway_refuses(self):
        # First prove the positive path; an unrecognized CLI option must not
        # masquerade as successful validation of these negative fixtures.
        code, message = self.judge()
        self.assertEqual(code, 0, message)
        for changes in ({"build_id": "501-1"}, {"build_id": "900-1"},
                        {"build_revision": "f" * 40}, {"deploy_id": EVENT},
                        {"deploy_id": "unknown"}, {"worker_version": STAGE_VERSION},
                        {"image": PROD_REPO + "@sha256:" + "d" * 64},
                        {"image": STAGE_IMAGE}, {"status": "503"},
                        {"components": [["gateway", "down"]]}):
            with self.subTest(changes=changes):
                code, message = self.judge(**changes)
                self.assertEqual(code, 1, message)

    def test_existing_rollback_receipts_remain_environment_local(self):
        application = "two-bot-next-twobotcontainer-production"
        self.assertEqual(worker_container_rollback.restore_image_shape(PROD_IMAGE, application), PROD_IMAGE)
        for image in (STAGE_IMAGE, PROD_IMAGE.replace("-production@", "-foreign@")):
            with self.subTest(image=image):
                with self.assertRaises(worker_container_rollback.GateError):
                    worker_container_rollback.restore_image_shape(image, application)


class ImmutablePromotionWorkflowTests(unittest.TestCase):
    def workflow(self, name):
        return yaml.load((ROOT / ".github/workflows" / name).read_text(), Loader=yaml.BaseLoader)

    def test_certification_is_downstream_of_every_staging_gate_and_not_diagnostics_upload(self):
        jobs = self.workflow("deploy-staging.yml")["jobs"]
        producers = [(name, job) for name, job in jobs.items() if any(
            "bot_promotion.py certify" in step.get("run", "") for step in job.get("steps", []))]
        self.assertEqual(len(producers), 1, "staging does not certify the actual served bundle")
        name, job = producers[0]
        ancestors, pending = set(), [name]
        while pending:
            needs = jobs[pending.pop()].get("needs", [])
            for dependency in ([needs] if isinstance(needs, str) else needs):
                if dependency not in ancestors:
                    ancestors.add(dependency)
                    pending.append(dependency)
        for gate in ("voice-synthetic", "deploy", "pinned-history-guard"):
            self.assertIn(gate, ancestors)
        self.assertNotIn("always()", job.get("if", ""))
        staged_deploy = next(step for step in jobs["deploy"]["steps"]
                             if step.get("name") == "Deploy to staging")
        self.assertIn("--no-bundle", staged_deploy.get("with", {}).get("command", staged_deploy.get("run", "")))
        commands = "\n".join(step.get("run", "") for step in job["steps"])
        self.assertIn("--fault-mode", commands)
        uploads = [step for step in job["steps"] if "actions/upload-artifact@" in step.get("uses", "")]
        self.assertTrue(uploads, "no immutable certification artifact upload")
        for step in uploads:
            self.assertNotIn("always()", step.get("if", ""))
            self.assertNotEqual(step.get("with", {}).get("overwrite"), "true")
        diagnostics = [step for step in jobs["deploy"]["steps"]
                       if step.get("name") == "Upload the rollout evidence"]
        self.assertEqual(len(diagnostics), 1)
        self.assertIn("always()", diagnostics[0].get("if", ""))

    def test_ordinary_production_consumes_admission_before_mutation_and_never_rebundles(self):
        job = self.workflow("deploy-production.yml")["jobs"]["production"]
        steps = job["steps"]
        consumers = [i for i, step in enumerate(steps)
                     if "bot_promotion.py admit" in step.get("run", "")]
        self.assertEqual(len(consumers), 1, "ordinary promotion has no immutable artifact consumer")
        consumer = consumers[0]
        deploy = next(i for i, step in enumerate(steps) if step.get("name") == "Deploy to production")
        self.assertLess(consumer, deploy)
        script = steps[consumer]["run"]
        self.assertIn("--run-id", script)
        self.assertIn("--attempt", script)
        command = steps[deploy].get("with", {}).get("command", steps[deploy].get("run", ""))
        self.assertIn("--no-bundle", command)
        gate = next(step for step in steps if step.get("name") == "Gate on stable /health + /readyz gateway-ready")
        self.assertIn("--artifact", gate["run"])
        self.assertIn("--expected-worker-version", gate["run"])
        self.assertIn("--image", gate["run"])

    def test_recovery_does_not_acquire_an_artifact_dependency(self):
        steps = self.workflow("deploy-production.yml")["jobs"]["production"]["steps"]
        consumers = [step for step in steps if "bot_promotion.py admit" in step.get("run", "")]
        self.assertEqual(len(consumers), 1, "missing ordinary-promotion seam")
        guard = consumers[0].get("if", "")
        self.assertRegex(guard, r"(?:env.MODE|needs.guard.outputs.mode)\s*==\s*'deploy'")
        self.assertRegex(guard, r"(?:env.ALLOW_UNSTAGED|needs.guard.outputs.allow_unstaged)\s*!=\s*'true'")
        for name in ("Roll back production Worker version and container image",
                     "Auto-roll back Worker version and container image"):
            step = next(step for step in steps if step.get("name") == name)
            self.assertNotIn("bot_promotion.py", step.get("run", ""))


if __name__ == "__main__":
    unittest.main()
