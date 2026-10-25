"""Offline intake regressions independent of the original protected contract.

Breaks caught: selecting inventory[0] instead of the current certificate,
assuming repository ID 1, accepting a fork, widening the closed receipt,
allowing local modules to drift, or an environment override rebuilding bytes.
Only GitHub/image-copy I/O is replaced by the original strict offline fixture.
"""

import contextlib
import copy
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import production_deploy
import test_immutable_bot_promotion as contract


# Minimal worker served to native Wrangler by post-fix fixtures: it exports the
# TwoBotContainer class named by the container/durable-object bindings, so a
# real --no-bundle/--dry-run validation accepts the generated config.
NATIVE_WORKER_JS = (
    b"export class TwoBotContainer {\n"
    b"  async fetch() { return new Response(\"ok\"); }\n"
    b"}\n"
    b"export default {\n"
    b"  async fetch() { return new Response(\"ok\"); },\n"
    b"};\n"
)

# Bundle the fixture packaging shim writes; the shell's configure_bundle step
# consumes exactly this path.
PACKAGING_BUNDLE_JS = b'export default {fetch(){return new Response("ok")}};'


def _child_path(bindir):
    """PATH for fixture children: shim dir first, then node, then system."""
    node = shutil.which("node")
    parts = [str(bindir)]
    if node:
        parts.append(str(Path(node).parent))
    parts.extend(("/usr/bin", "/bin"))
    return ":".join(parts)


def _pinned_native_wrangler(case, home):
    """Resolve the pinned native Wrangler without downloading anything.

    Considers only PROMOTION_TEST_WRANGLER_BIN (explicit operator-provided
    path), the repo's installed dependency, and a PATH-resolved binary. The
    binary must report the version pinned in wrangler/package.json or it is
    refused. Skips (reported, no download) when nothing usable is installed.
    """
    repo = Path(__file__).resolve().parents[1]
    pinned = json.loads((repo / "wrangler" / "package.json").read_text()
                        )["devDependencies"]["wrangler"]
    candidates = []
    configured = os.environ.get("PROMOTION_TEST_WRANGLER_BIN", "").strip()
    if configured:
        candidates.append(("PROMOTION_TEST_WRANGLER_BIN", Path(configured)))
    candidates.append(("repo node_modules",
                       repo / "wrangler" / "node_modules" / ".bin" / "wrangler"))
    found = shutil.which("wrangler")
    if found:
        candidates.append(("PATH", Path(found)))
    checked = []
    for origin, path in candidates:
        if not path.is_file():
            checked.append(f"{origin}={path} (absent)")
            continue
        if not os.access(path, os.X_OK):
            checked.append(f"{origin}={path} (not executable)")
            continue
        probe = subprocess.run(
            [str(path), "--version"], capture_output=True, text=True, timeout=60,
            env={"PATH": _child_path(path.parent), "HOME": str(home),
                 "WRANGLER_SEND_METRICS": "false", "WRANGLER_UPDATE_CHECK": "false",
                 "CI": "true"})
        if probe.returncode == 0 and pinned in (probe.stdout + probe.stderr).split():
            return path, pinned
        checked.append(f"{origin}={path} (unusable: exit {probe.returncode})")
    case.skipTest(f"no usable pinned Wrangler {pinned}; no downloads attempted "
                  f"(checked {'; '.join(checked) or 'nothing'})")


class PromotionInventoryIntegrationTests(contract.PromotionFixture):
    def inventory_with_diagnostics(self):
        matching = copy.deepcopy(self.github.artifact)
        others = [dict(matching, id=7697, name="staging-rollout-evidence"),
                  dict(matching, id=7698, name="voice-synthetic-report"),
                  dict(matching, id=7699, name="bot-promotion-501-1")]
        self.github.listing = {"total_count": 4, "artifacts": [others[0], matching, *others[1:]]}

    def bind_repository(self, repository_id):
        record = contract.receipt()
        record["repository"]["id"] = repository_id
        self.replace_package(record)
        self.github.run["repository"]["id"] = repository_id
        self.github.run["head_repository"]["id"] = repository_id
        self.github.artifact["workflow_run"]["repository_id"] = repository_id
        self.github.artifact["workflow_run"]["head_repository_id"] = repository_id
        self.github.listing["artifacts"] = [copy.deepcopy(self.github.artifact)]

    def test_complete_mixed_inventory_admits_only_exact_current_certificate(self):
        self.inventory_with_diagnostics()
        self.assertEqual(self.admit(), 0)
        self.assertEqual(self.mutations, [(contract.STAGE_IMAGE, contract.PROD_REPO)])
        self.assertEqual(self.github.calls, [contract.RUN_ROUTE, contract.ARTIFACTS_ROUTE,
                                            contract.ARTIFACT_ROUTE, contract.ZIP_ROUTE,
                                            contract.RUN_ROUTE])
        for name, data in contract.MODULES.items():
            self.assertEqual((self.out / "modules" / name).read_bytes(), data)

    def test_mixed_inventory_with_truncated_count_refuses_before_copy(self):
        self.inventory_with_diagnostics()
        self.github.listing["total_count"] = 5
        self.refused()
        self.assertNotIn(contract.ZIP_ROUTE, self.github.calls)

    def test_mixed_inventory_with_duplicate_matching_name_refuses_before_copy(self):
        self.inventory_with_diagnostics()
        self.github.listing["artifacts"].append(dict(self.github.artifact, id=7701))
        self.github.listing["total_count"] = 5
        self.refused()
        self.assertNotIn(contract.ZIP_ROUTE, self.github.calls)

    def test_mixed_inventory_without_current_attempt_certificate_refuses(self):
        self.inventory_with_diagnostics()
        self.github.listing["artifacts"].pop(1)
        self.github.listing["total_count"] = 3
        self.refused()

    def test_consistent_nonfixture_repository_id_passes_end_to_end(self):
        self.bind_repository(1098765432)
        self.inventory_with_diagnostics()
        self.assertEqual(self.admit(), 0)
        admitted = json.loads((self.out / "admitted.json").read_text())
        self.assertEqual(admitted["receipt"]["repository"],
                         {"id": 1098765432, "full_name": contract.REPO})
        self.assertEqual(self.mutations, [(contract.STAGE_IMAGE, contract.PROD_REPO)])

    def test_fork_and_cross_record_repository_ids_refuse_before_mutation(self):
        for location in ("run-head", "artifact-repository", "artifact-head", "receipt", "reread"):
            with self.subTest(location=location):
                self.bind_repository(1098765432)
                if location == "run-head":
                    self.github.run["head_repository"]["id"] = 1098765433
                elif location.startswith("artifact-"):
                    key = "repository_id" if location == "artifact-repository" else "head_repository_id"
                    self.github.artifact["workflow_run"][key] = 1098765433
                    self.github.listing["artifacts"] = [copy.deepcopy(self.github.artifact)]
                elif location == "receipt":
                    self.github.run["repository"]["id"] = 1098765433
                    self.github.run["head_repository"]["id"] = 1098765433
                    self.github.artifact["workflow_run"]["repository_id"] = 1098765433
                    self.github.artifact["workflow_run"]["head_repository_id"] = 1098765433
                    self.github.listing["artifacts"] = [copy.deepcopy(self.github.artifact)]
                else:
                    self.github.refreshed_run = copy.deepcopy(self.github.run)
                    self.github.refreshed_run["repository"]["id"] = 1098765433
                    self.github.refreshed_run["head_repository"]["id"] = 1098765433
                self.refused()

    def test_closed_staging_receipt_rejects_even_known_diagnostic_extras(self):
        for key, value in (("config", {"vars": {"FIXTURE": "not-runtime-config"}}),
                           ("unsafe_config", {"main": "other.js"}),
                           ("application_listing_lag", True), ("active_lag", True)):
            with self.subTest(key=key):
                record = contract.receipt()
                record["staging"][key] = value
                self.replace_package(record)
                self.refused()

    def test_known_lag_diagnostics_are_projected_out_before_certification(self):
        evidence = dict(contract.receipt()["staging"], application_listing_lag=True, active_lag=True)
        self.assertEqual(self.certify(stage=evidence), 0)
        with zipfile.ZipFile(self.output_zip) as archive:
            record = json.loads(archive.read("promotion-receipt.json"))
            self.assertEqual(record, contract.receipt())
            self.assertNotIn("application_listing_lag", record["staging"])
            self.assertNotIn("active_lag", record["staging"])
        self.github = contract.GitHubFixture(self.output_zip.read_bytes())
        self.assertEqual(self.admit(), 0)

    def test_hash_consistent_but_unsafe_module_suffixes_refuse_before_copy(self):
        for path in ("extra.ts", "extra.json", "extra.map", "extra.js.exe", "nested/.hidden.js"):
            with self.subTest(path=path):
                members = dict(contract.MODULES, **{path: b"fixture-only"})
                record = contract.receipt()
                record["worker"]["modules"].append({"path": path, "size": 12,
                                                      "sha256": hashlib.sha256(b"fixture-only").hexdigest()})
                self.replace_package(record, members)
                self.refused()


class SavedBundleIntegrationTests(contract.PromotionFixture):
    def write_admitted(self, image=contract.PROD_IMAGE):
        self.out.mkdir()
        self.artifact = self.out / "admitted.json"
        self.artifact.write_text(json.dumps({"schema_version": 1, "receipt": contract.receipt(),
                                             "production_image": image}))
        (self.out / "modules").mkdir()
        for name, data in contract.MODULES.items():
            (self.out / "modules" / name).write_bytes(data)

    def render(self):
        target = self.root / "rendered.json"
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = production_deploy.main([
                "render", "--config", str(self.config), "--out", str(target),
                "--sha", contract.SHA, "--run-id", "900", "--attempt", "1",
                "--deploy-id", contract.EVENT, "--artifact", str(self.artifact)])
        return code, json.loads(target.read_text()) if target.exists() else None

    def test_added_served_module_after_admission_refuses_render_without_output(self):
        self.write_admitted()
        (self.out / "modules" / "injected.js").write_bytes(b"export default 'not certified';")
        code, config = self.render()
        self.assertEqual(code, 1)
        self.assertIsNone(config)

    def test_saved_module_symlink_refuses_even_when_target_bytes_match(self):
        self.write_admitted()
        target = self.root / "external.js"
        target.write_bytes(contract.MODULES["index.js"])
        entrypoint = self.out / "modules" / "index.js"
        entrypoint.unlink()
        entrypoint.symlink_to(target)
        code, config = self.render()
        self.assertEqual(code, 1)
        self.assertIsNone(config)

    def test_same_digest_in_foreign_production_account_refuses_render(self):
        self.write_admitted(image=contract.PROD_IMAGE.replace("/acct/", "/foreign/"))
        code, config = self.render()
        self.assertEqual(code, 1)
        self.assertIsNone(config)

    def test_environment_main_and_build_overrides_cannot_replace_saved_bytes(self):
        self.write_admitted()
        self.config.write_text(contract.CONFIG.replace(
            "[env.production.vars]", '[env.production]\nmain = "evil.ts"\n'
            'base_dir = "/untrusted"\nno_bundle = false\nminify = true\n'
            '[env.production.build]\ncommand = "fixture-must-never-execute"\n'
            '[env.production.vars]'))
        code, config = self.render()
        self.assertEqual(code, 0)
        self.assertTrue(config["no_bundle"])
        self.assertFalse(config["minify"])
        self.assertEqual(Path(config["main"]).read_bytes(), contract.MODULES["index.js"])
        self.assertEqual(Path(config["base_dir"]), self.out / "modules")
        production = config["env"]["production"]
        for key in ("main", "build", "base_dir", "no_bundle", "minify"):
            self.assertNotIn(key, production)
        self.assertEqual(production["vars"]["ENV_SENTINEL"], "production-only")
        self.assertEqual(production["containers"][0]["image"], contract.PROD_IMAGE)
        self.assertNotIn("image_vars", production["containers"][0])


class StagingPackagingOutputIsolationTests(unittest.TestCase):
    """Native regression: the staging packaging dry-run must not share the
    real-deploy receipt file.

    Wrangler 4.151.0 appends one wrangler-session record per invocation (and a
    deploy record for a deploy) to the file selected by
    WRANGLER_OUTPUT_FILE_PATH. The staging workflow exports that variable for
    the real deploy receipt, and staging_rollout.wrangler_sessions demands
    exactly one deploy invocation. When the packaging dry-run inherits the
    variable it contributes its own deploy pair, so the shared file carries
    two deploys and the receipt gate fails.

    These tests execute the ACTUAL "Bundle the Worker once before staging
    serves it" shell from deploy-staging.yml with a fixture `npx` that mimics
    only the native output-file selection (append session+deploy records when
    the variable is present; create the outdir bundle otherwise). No network,
    credentials, Docker, or downloads are used.
    """

    PACKAGING_STEP = "Bundle the Worker once before staging serves it"

    def packaging_shell(self):
        import yaml
        root = Path(__file__).resolve().parents[1]
        workflow = yaml.load((root / ".github/workflows/deploy-staging.yml").read_text(),
                             Loader=yaml.BaseLoader)
        for step in workflow["jobs"]["deploy"]["steps"]:
            if step.get("name") == self.PACKAGING_STEP:
                return step["run"]
        self.fail("packaging step missing from deploy-staging.yml")

    def write_npx_shim(self, bindir):
        """Mimic Wrangler 4.151.0 output-file selection, nothing else."""
        bindir = Path(bindir)
        bindir.mkdir(parents=True, exist_ok=True)
        shim = bindir / "npx"
        shim.write_text(
            "#!/bin/bash\n"
            "set -euo pipefail\n"
            "# Only the staging packaging invocation is supported.\n"
            "if [ \"${1:-}\" != \"--no-install\" ] || [ \"${2:-}\" != \"wrangler\" ]"
            " || [ \"${3:-}\" != \"deploy\" ]; then\n"
            "  echo \"unexpected npx invocation: $*\" >&2; exit 99\n"
            "fi\n"
            "outdir=\"\"\n"
            "prev=\"\"\n"
            "has_dry=0\n"
            "has_none=0\n"
            "for arg in \"$@\"; do\n"
            "  if [ \"$arg\" = \"--dry-run\" ]; then has_dry=1; fi\n"
            "  if [ \"$arg\" = \"--containers-rollout=none\" ]; then has_none=1; fi\n"
            "  if [ \"$prev\" = \"--containers-rollout\" ] && [ \"$arg\" = \"none\" ]; then has_none=1; fi\n"
            "  if [ \"$prev\" = \"--outdir\" ]; then outdir=\"$arg\"; fi\n"
            "  prev=\"$arg\"\n"
            "done\n"
            "[ \"$has_dry\" = 1 ] || { echo \"packaging must pass --dry-run\" >&2; exit 99; }\n"
            "[ \"$has_none\" = 1 ] || { echo \"packaging must pass --containers-rollout=none\" >&2; exit 99; }\n"
            "[ -n \"$outdir\" ] || { echo \"no --outdir in packaging invocation\" >&2; exit 99; }\n"
            "mkdir -p \"$outdir\"\n"
            "# Minimal bundle the later configure_bundle step can consume.\n"
            "printf 'export default {fetch(){return new Response(\"ok\")}};' > \"$outdir/index.js\"\n"
            "target=\"${WRANGLER_OUTPUT_FILE_PATH:-}\"\n"
            "if [ -n \"$target\" ]; then\n"
            "  now=\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\"\n"
            "  printf '%s\\n' '{\"version\":1,\"type\":\"wrangler-session\","
            "\"wrangler_version\":\"4.151.0\",\"command_line_args\":"
            "[\"deploy\",\"--config\",\"staging-deploy.json\",\"--env\",\"staging\"]}' >> \"$target\"\n"
            "  printf '%s\\n' \"{\\\"version\\\":1,\\\"type\\\":\\\"deploy\\\","
            "\\\"worker_name\\\":\\\"two-bot-next-staging\\\","
            "\\\"wrangler_environment\\\":\\\"staging\\\","
            "\\\"worker_name_overridden\\\":false,"
            "\\\"timestamp\\\":\\\"$now\\\","
            "\\\"version_id\\\":\\\"9f8e7d6c-5b4a-4c3d-9e2f-1a0b9c8d7e6f\\\"}\" >> \"$target\"\n"
            "fi\n"
            "exit 0\n"
        )
        shim.chmod(0o755)
        docker = bindir / "docker"
        docker.write_text("#!/bin/bash\necho \"docker must not run during packaging\" >&2\nexit 99\n")
        docker.chmod(0o755)
        return shim

    def packaging_workspace(self, root):
        """Fixture workspace mirroring the workflow layout.

        cwd is wrangler/, the pinned tooling checkout lives at
        ../tooling/scripts relative to it, and the generated staging config
        carries a buildable entrypoint plus a real local Dockerfile, so both
        the fixture shim and a delegated native binary can package it.
        """
        (root / "workspace" / "wrangler").mkdir(parents=True)
        tooling = root / "workspace" / "tooling" / "scripts"
        tooling.mkdir(parents=True)
        for helper in ("bot_promotion.py", "build_identity.py"):
            shutil.copy2(Path(__file__).resolve().parent / helper,
                         tooling / helper)
        rollout = root / "rollout"
        rollout.mkdir()
        dockerfile = root / "Dockerfile"
        dockerfile.write_text("FROM scratch\n")
        src = rollout / "src"
        src.mkdir()
        (src / "index.js").write_bytes(NATIVE_WORKER_JS)
        deploy_config = rollout / "staging-deploy.json"
        deploy_config.write_text(json.dumps({
            "name": "two-bot-next",
            "main": "src/index.js",
            "compatibility_date": "2026-09-29",
            "compatibility_flags": ["nodejs_compat",
                                    "nodejs_compat_do_not_populate_process_env"],
            "env": {"staging": {"vars": {},
                                "durable_objects": {"bindings": [
                                    {"name": "TWO_BOT",
                                     "class_name": "TwoBotContainer"}]},
                                "exports": {"TwoBotContainer": {
                                    "type": "durable-object",
                                    "storage": "sqlite"}},
                                "containers": [{"class_name": "TwoBotContainer",
                                                "image": str(dockerfile),
                                                "image_vars": {
                                                    "BOT_BUILD_REVISION": contract.SHA,
                                                    "BOT_BUILD_ID": contract.BUILD,
                                                    "BOT_DEPLOY_ID": contract.STAMP},
                                                "rollout_kind": "full_auto",
                                                "max_instances": 1}]}},
        }) + "\n")
        bindir = root / "bin"
        self.write_npx_shim(bindir)
        return rollout, bindir

    def write_native_npx_shim(self, bindir):
        """npx shim delegating the packaging invocation to native Wrangler.

        Validates the workflow command shape and packaging flags exactly like
        the fixture shim, then execs the pinned native binary with the real
        deploy arguments. No download, no package manager involvement.
        """
        bindir = Path(bindir)
        shim = bindir / "npx"
        shim.write_text(
            "#!/bin/bash\n"
            "set -euo pipefail\n"
            "if [ \"${1:-}\" != \"--no-install\" ] || [ \"${2:-}\" != \"wrangler\" ]"
            " || [ \"${3:-}\" != \"deploy\" ]; then\n"
            "  echo \"unexpected npx invocation: $*\" >&2; exit 99\n"
            "fi\n"
            "has_dry=0\n"
            "has_none=0\n"
            "prev=\"\"\n"
            "for arg in \"$@\"; do\n"
            "  if [ \"$arg\" = \"--dry-run\" ]; then has_dry=1; fi\n"
            "  if [ \"$arg\" = \"--containers-rollout=none\" ]; then has_none=1; fi\n"
            "  if [ \"$prev\" = \"--containers-rollout\" ] && [ \"$arg\" = \"none\" ]; then has_none=1; fi\n"
            "  prev=\"$arg\"\n"
            "done\n"
            "[ \"$has_dry\" = 1 ] || { echo \"packaging must pass --dry-run\" >&2; exit 99; }\n"
            "[ \"$has_none\" = 1 ] || { echo \"packaging must pass --containers-rollout=none\" >&2; exit 99; }\n"
            "exec \"$PROMOTION_TEST_WRANGLER_BIN\" \"${@:3}\"\n"
        )
        shim.chmod(0o755)

    def run_packaging_shell(self):
        shell = self.packaging_shell()
        tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        rollout, bindir = self.packaging_workspace(root)
        shared = rollout / "wrangler-output.ndjson"
        env = {
            "PATH": _child_path(bindir),
            "HOME": str(root),
            "ROLLOUT_DIR": str(rollout),
            "WRANGLER_OUTPUT_FILE_PATH": str(shared),
            # No credentials, tokens, or account IDs are provided: a packaging
            # dry-run must not need them. Metrics and update checks stay off.
            "WRANGLER_SEND_METRICS": "false",
            "WRANGLER_UPDATE_CHECK": "false",
            "CI": "true",
        }
        proc = subprocess.run(["bash", "-c", shell],
                              cwd=str(root / "workspace" / "wrangler"),
                              env=env, capture_output=True, text=True, timeout=60)
        return proc, shared, rollout, env

    def test_packaging_shell_bundles_modules_with_dry_run_and_no_container_build(self):
        # Behavioral, not textual: the fixture npx refuses unless the workflow
        # command carries --dry-run and --containers-rollout=none, and the
        # shell's configure_bundle step must leave a consumable bundle behind.
        proc, shared, rollout, _env = self.run_packaging_shell()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        bundle = rollout / "worker-modules" / "index.js"
        self.assertTrue(bundle.is_file(), proc.stdout + proc.stderr)
        self.assertEqual(bundle.read_bytes(), PACKAGING_BUNDLE_JS)

    def test_packaging_dry_run_leaves_shared_output_untouched(self):
        # RED on the unfixed baseline: the shell inherits
        # WRANGLER_OUTPUT_FILE_PATH, so the shim appends the dry-run
        # session+deploy pair to the shared receipt file. The expected
        # post-fix behavior is an untouched (absent or empty) shared file.
        proc, shared, _rollout, _env = self.run_packaging_shell()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        content = shared.read_bytes() if shared.exists() else b""
        self.assertEqual(content, b"",
                         "packaging dry-run polluted the shared Wrangler output file")

    def real_deploy_records(self):
        now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        return [
            {"version": 1, "type": "wrangler-session", "wrangler_version": "4.151.0",
             "command_line_args": ["--version"]},
            {"version": 1, "type": "wrangler-session", "wrangler_version": "4.151.0",
             "command_line_args": ["deploy", "--config", "staging-deploy.json",
                                   "--env", "staging"]},
            {"version": 1, "type": "deploy", "worker_name": "two-bot-next-staging",
             "worker_name_overridden": False, "wrangler_environment": "staging",
             "timestamp": now, "version_id": "9f8e7d6c-5b4a-4c3d-9e2f-1a0b9c8d7e6f"},
        ]

    def write_receipt_case(self, records):
        import staging_rollout
        tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        baseline = {"started": time.time() - 5,
                    "application_id": "11111111-1111-4111-8111-111111111111",
                    "namespace_id": "22222222-2222-4222-8222-222222222222",
                    "rollout_ids": [], "revision": contract.SHA,
                    "build_id": contract.BUILD, "deploy_id": contract.STAMP}
        receipt = root / "baseline.json"
        receipt.write_text(json.dumps(baseline))
        output = root / "wrangler-output.ndjson"
        output.write_text("\n".join(json.dumps(record) for record in records) + "\n")
        evidence = root / "receipt.json"
        return staging_rollout, receipt, output, evidence

    def test_synthetic_real_deploy_pair_is_well_formed_in_isolation(self):
        # Guard for the chain below: the synthetic real pair alone must pass
        # the actual validator, so a chained rejection can only come from
        # packaging pollution, never from a malformed synthetic record.
        import staging_rollout
        rollout, receipt, output, evidence = self.write_receipt_case(self.real_deploy_records())
        script = Path(staging_rollout.__file__)
        env = {"PATH": _child_path(output.parent), "HOME": str(output.parent),
               "WRANGLER_SEND_METRICS": "false", "CI": "true"}
        proc = subprocess.run([sys.executable, str(script), "receipt",
                               "--receipt", str(receipt), "--output", str(output),
                               "--evidence", str(evidence)],
                              env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(json.loads(evidence.read_text()),
                         {"worker_version": "9f8e7d6c-5b4a-4c3d-9e2f-1a0b9c8d7e6f"})

    def test_packaging_then_single_real_deploy_receipt_passes(self):
        # End-to-end chain through the actual validator: the real packaging
        # shell runs first into the shared file, then one synthetic real
        # deployment pair is appended, and staging_rollout receipt judges the
        # result. RED on the baseline: the packaging pair pollutes the file,
        # so the validator sees two deploys and refuses.
        import staging_rollout
        tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        baseline = {"started": time.time() - 5,
                    "application_id": "11111111-1111-4111-8111-111111111111",
                    "namespace_id": "22222222-2222-4222-8222-222222222222",
                    "rollout_ids": [], "revision": contract.SHA,
                    "build_id": contract.BUILD, "deploy_id": contract.STAMP}
        receipt = root / "baseline.json"
        receipt.write_text(json.dumps(baseline))
        rollout, bindir = self.packaging_workspace(root)
        shared = rollout / "wrangler-output.ndjson"
        env = {
            "PATH": _child_path(bindir),
            "HOME": str(root),
            "ROLLOUT_DIR": str(rollout),
            "WRANGLER_OUTPUT_FILE_PATH": str(shared),
            "WRANGLER_SEND_METRICS": "false",
            "WRANGLER_UPDATE_CHECK": "false",
            "CI": "true",
        }
        proc = subprocess.run(["bash", "-c", self.packaging_shell()],
                              cwd=str(root / "workspace" / "wrangler"),
                              env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with shared.open("a") as handle:
            for record in self.real_deploy_records():
                handle.write(json.dumps(record) + "\n")
        evidence = root / "receipt.json"
        script = Path(staging_rollout.__file__)
        judge = subprocess.run(
            [sys.executable, str(script), "receipt",
             "--receipt", str(receipt), "--output", str(shared),
             "--evidence", str(evidence)],
            env={"PATH": _child_path(bindir), "HOME": str(root),
                 "WRANGLER_SEND_METRICS": "false", "CI": "true"},
            capture_output=True, text=True, timeout=60)
        self.assertEqual(judge.returncode, 0, judge.stdout + judge.stderr)
        self.assertEqual(json.loads(evidence.read_text()),
                         {"worker_version": "9f8e7d6c-5b4a-4c3d-9e2f-1a0b9c8d7e6f"})

    def test_shared_path_with_packaging_and_real_deploy_rejects(self):
        # The buggy shared file carries the packaging dry-run pair plus the
        # real deploy pair: two deploy sessions where the validator demands
        # exactly one. This control passes on the baseline and proves the
        # validator still catches the pollution.
        import staging_rollout
        packaging = [record for record in self.real_deploy_records()
                     if record.get("type") == "deploy"
                     or record.get("command_line_args", [None])[0] == "deploy"]
        records = packaging + self.real_deploy_records()
        deploys = [row for row in records
                   if row.get("type") == "wrangler-session"
                   and (row.get("command_line_args") or [None])[0] == "deploy"]
        self.assertEqual(len(deploys), 2)
        started = time.time() - 5
        with self.assertRaises(staging_rollout.GateError) as caught:
            staging_rollout.deploy_version(records, started)
        self.assertEqual(str(caught.exception), "wrong_wrangler_receipt")

    def test_native_wrangler_packaging_isolation_with_real_dry_run(self):
        # Credential-free actual dry-run through the ACTUAL workflow packaging
        # command: the npx shim validates the command shape and flags, then
        # execs the pinned native binary with the real arguments. The shared
        # file then receives one synthetic real pair and the ACTUAL receipt
        # validator judges the result. RED on the baseline when a usable
        # native binary is present: the dry-run inherits
        # WRANGLER_OUTPUT_FILE_PATH and pollutes the file, so the validator
        # sees two deploys. Skips (reported) with no usable pinned binary and
        # never downloads anything.
        import staging_rollout
        tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        binary, _pinned = _pinned_native_wrangler(self, root)
        baseline = {"started": time.time() - 5,
                    "application_id": "11111111-1111-4111-8111-111111111111",
                    "namespace_id": "22222222-2222-4222-8222-222222222222",
                    "rollout_ids": [], "revision": contract.SHA,
                    "build_id": contract.BUILD, "deploy_id": contract.STAMP}
        receipt = root / "baseline.json"
        receipt.write_text(json.dumps(baseline))
        rollout, bindir = self.packaging_workspace(root)
        self.write_native_npx_shim(bindir)
        shared = rollout / "wrangler-output.ndjson"
        env = {
            "PATH": _child_path(bindir),
            "HOME": str(root),
            "ROLLOUT_DIR": str(rollout),
            "WRANGLER_OUTPUT_FILE_PATH": str(shared),
            "PROMOTION_TEST_WRANGLER_BIN": str(binary),
            "WRANGLER_SEND_METRICS": "false",
            "WRANGLER_UPDATE_CHECK": "false",
            "CI": "true",
        }
        proc = subprocess.run(["bash", "-c", self.packaging_shell()],
                              cwd=str(root / "workspace" / "wrangler"),
                              env=env, capture_output=True, text=True, timeout=120)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        bundle = rollout / "worker-modules" / "index.js"
        self.assertTrue(bundle.is_file(), proc.stdout + proc.stderr)
        self.assertGreater(bundle.stat().st_size, 0)
        with shared.open("a") as handle:
            for record in self.real_deploy_records():
                handle.write(json.dumps(record) + "\n")
        evidence = root / "receipt.json"
        script = Path(staging_rollout.__file__)
        judge = subprocess.run(
            [sys.executable, str(script), "receipt",
             "--receipt", str(receipt), "--output", str(shared),
             "--evidence", str(evidence)],
            env={"PATH": _child_path(bindir), "HOME": str(root),
                 "WRANGLER_SEND_METRICS": "false", "CI": "true"},
            capture_output=True, text=True, timeout=60)
        self.assertEqual(judge.returncode, 0, judge.stdout + judge.stderr)
        self.assertEqual(json.loads(evidence.read_text()),
                         {"worker_version": "9f8e7d6c-5b4a-4c3d-9e2f-1a0b9c8d7e6f"})


class ArtifactRenderLocalImageResolutionTests(unittest.TestCase):
    """Native regression: render --artifact must resolve every unused local
    container image, not just the promoted production one.

    The non-artifact render resolves ALL declared images (top-level plus every
    environment) relative to the original TOML, because Wrangler validates the
    whole file. The artifact path replaces only the production container with
    the admitted digest; on the baseline the default and staging entries keep
    their literal "../Dockerfile", so a native dry-run of the generated config
    resolves them against the wrong directory (RUNNER_TEMP) instead of the
    original TOML.

    Render is exercised through the ACTUAL production_deploy CLI in a clean
    subprocess with no credentials; module bytes must be unchanged.
    """

    def render_layout(self):
        """Source layout with a real local Dockerfile and DO-compatible modules.

        The TOML keeps every unused local image (`../Dockerfile`, top-level
        and staging) exactly like the real config; the Dockerfile exists so
        post-fix absolute resolutions point at a real file, and the admitted
        modules export the TwoBotContainer class a native dry-run validates.
        """
        tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        project = root / "project"
        project.mkdir()
        source = project / "wrangler.toml"
        # The shared contract TOML carries the container/image wiring under
        # test; the live exports block below only mirrors the real production
        # config so native validation sees the same DO shape (warning-free).
        source.write_text(contract.CONFIG + "\n[env.production.exports.TwoBotContainer]\n"
                                            "type = \"durable-object\"\n"
                                            "storage = \"sqlite\"\n")
        (root / "Dockerfile").write_text("FROM scratch\n")
        modules = {"index.js": NATIVE_WORKER_JS}
        record = contract.receipt()
        record["worker"] = dict(record["worker"], entrypoint="index.js", modules=[
            {"path": name, "size": len(data),
             "sha256": hashlib.sha256(data).hexdigest()}
            for name, data in modules.items()])
        artifact = root / "admitted.json"
        artifact.write_text(json.dumps({"schema_version": 1, "receipt": record,
                                        "production_image": contract.PROD_IMAGE}))
        moddir = root / "modules"
        moddir.mkdir()
        for name, data in modules.items():
            (moddir / name).write_bytes(data)
        out = root / "production.json"
        script = Path(__file__).resolve().parent / "production_deploy.py"
        env = {"PATH": _child_path(root / "bin"), "HOME": str(root),
               "WRANGLER_SEND_METRICS": "false", "CI": "true"}
        proc = subprocess.run([sys.executable, str(script), "render",
                               "--config", str(source), "--out", str(out),
                               "--sha", contract.SHA, "--run-id", "900",
                               "--attempt", "1", "--deploy-id", contract.EVENT,
                               "--artifact", str(artifact)],
                              env=env, capture_output=True, text=True, timeout=60)
        rendered = json.loads(out.read_text()) if out.exists() else None
        return proc, source, rendered, moddir, modules

    def test_artifact_render_resolves_all_unused_local_images(self):
        proc, source, rendered, _moddir, _modules = self.render_layout()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIsNotNone(rendered)
        expected = str((source.parent / "../Dockerfile").resolve())
        self.assertTrue(Path(expected).is_file())
        self.assertTrue(rendered.get("containers"))
        for container in rendered["containers"]:
            self.assertEqual(container["image"], expected)
        staging = rendered["env"]["staging"]["containers"]
        self.assertTrue(staging)
        for container in staging:
            self.assertEqual(container["image"], expected)
        # The literal local path must be gone everywhere: it would resolve
        # against RUNNER_TEMP instead of the original TOML directory.
        self.assertNotIn("../Dockerfile", json.dumps(rendered))

    def test_artifact_render_keeps_production_digest_without_build_inputs(self):
        proc, _source, rendered, moddir, modules = self.render_layout()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        production = rendered["env"]["production"]["containers"][0]
        self.assertEqual(production["image"], contract.PROD_IMAGE)
        self.assertNotIn("image_vars", production)
        self.assertNotIn("image_build_context", production)
        self.assertTrue(rendered.get("no_bundle"))
        self.assertEqual(Path(rendered["main"]).read_bytes(), NATIVE_WORKER_JS)
        for name, data in modules.items():
            self.assertEqual((moddir / name).read_bytes(), data)

    def test_native_wrangler_accepts_rendered_artifact_config_when_pinned_available(self):
        proc, _source, rendered, moddir, modules = self.render_layout()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        binary, _pinned = _pinned_native_wrangler(self, moddir.parent)
        before = {name: (moddir / name).read_bytes() for name in modules}
        bindir = moddir.parent / "bin"
        bindir.mkdir(exist_ok=True)
        sentinel = bindir / "docker"
        sentinel.write_text("#!/bin/bash\necho \"docker must not run with --containers-rollout=none\" >&2\nexit 99\n")
        sentinel.chmod(0o755)
        outdir = moddir.parent / "dryrun"
        env = {"PATH": _child_path(bindir), "HOME": str(moddir.parent),
               "WRANGLER_SEND_METRICS": "false", "WRANGLER_UPDATE_CHECK": "false",
               "CI": "true"}
        config = moddir.parent / "production-rendered.json"
        config.write_text(json.dumps(rendered))
        dry = subprocess.run([str(binary), "deploy", "--config", str(config),
                              "--env", "production", "--no-bundle", "--dry-run",
                              "--containers-rollout=none", "--outdir", str(outdir)],
                             env=env, capture_output=True, text=True, timeout=120)
        self.assertEqual(dry.returncode, 0, dry.stdout + dry.stderr)
        for name, data in before.items():
            self.assertEqual((moddir / name).read_bytes(), data)
            self.assertEqual((outdir / name).read_bytes(), data,
                             "native --no-bundle dry-run must emit the admitted bytes unchanged")


if __name__ == "__main__":
    unittest.main()
