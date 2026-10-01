"""Offline release target, SBOM completeness and workflow safety regressions."""

import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name.replace("-", "_"), ROOT / "scripts" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


validator = load("validate-sbom")
selector = load("release-sbom-target")
SHA = "a" * 40


class SupplyChainTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP"))
        self.addCleanup(self.tmp.cleanup)
        self.directory = Path(self.tmp.name)
        self.packages = tomllib.loads((ROOT / "Cargo.lock").read_text())["package"]
        self.rust = [{"type": "library", "name": pkg["name"], "version": pkg["version"]} for pkg in self.packages]
        self.image = [{"type": "library", "name": "ca-certificates", "version": "fixture", "purl": "pkg:deb/debian/ca-certificates@fixture"}]
        self.write_bom("rust-workspace", self.rust)
        self.write_bom("container-image", self.image)
        (self.directory / "source-sha.txt").write_text(SHA + "\n")
        (self.directory / "image-id.txt").write_text("sha256:" + "b" * 64 + "\n")

    def write_bom(self, name, inventory):
        (self.directory / f"{name}.cdx.json").write_text(json.dumps({"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1, "components": inventory}))

    def validate(self):
        validator.validate(ROOT / "Cargo.lock", self.directory)

    def test_complete_lockfile_and_image_pass(self):
        self.validate()

    def test_nested_components_pass(self):
        self.write_bom("rust-workspace", [{"type": "application", "name": "workspace", "components": self.rust}])
        self.validate()

    def test_missing_dependency_fails(self):
        self.write_bom("rust-workspace", self.rust[1:])
        with self.assertRaisesRegex(ValueError, "omitted locked packages"):
            self.validate()

    def test_missing_workspace_member_fails(self):
        self.write_bom("rust-workspace", [pkg for pkg in self.rust if pkg["name"] != "two-bot-core"])
        with self.assertRaisesRegex(ValueError, "two-bot-core"):
            self.validate()

    def test_empty_or_wrong_format_inventory_fails(self):
        for name in ["rust-workspace", "container-image"]:
            with self.subTest(name=name):
                self.write_bom(name, [])
                with self.assertRaisesRegex(ValueError, "Empty SBOM"):
                    self.validate()
                self.write_bom(name, self.rust if name == "rust-workspace" else self.image)
        (self.directory / "container-image.cdx.json").write_text('{"components": [{"name": "fake"}]}')
        with self.assertRaisesRegex(ValueError, "Not a CycloneDX"):
            self.validate()

    def test_image_without_debian_packages_fails(self):
        self.write_bom("container-image", [{"name": "two-bot"}])
        with self.assertRaisesRegex(ValueError, "Debian runtime"):
            self.validate()

    def test_bad_provenance_fails(self):
        for name in ["source-sha.txt", "image-id.txt"]:
            with self.subTest(name=name):
                file = self.directory / name
                original = file.read_text()
                file.write_text("unknown")
                with self.assertRaisesRegex(ValueError, "invalid provenance"):
                    self.validate()
                file.write_text(original)

    def test_release_uses_published_tag_not_same_named_branch_or_new_main(self):
        tag_sha, branch_sha = "c" * 40, "d" * 40
        objects = {
            "ref/tags/v0.2.1": {"object": {"type": "commit", "sha": tag_sha}},
            "ref/heads/v0.2.1": {"object": {"type": "commit", "sha": branch_sha}},
        }
        with patch.object(selector, "github_object", side_effect=objects.__getitem__) as read:
            self.assertEqual(selector.select_target("false", "v0.2.1", SHA), {"ref": tag_sha, "tag": "v0.2.1"})
            self.assertEqual(selector.select_target("", "v0.2.1", SHA)["ref"], tag_sha)
            self.assertEqual(read.call_args_list, [unittest.mock.call("ref/tags/v0.2.1")] * 2)

    def test_annotated_tags_are_peeled_to_commit(self):
        with patch.object(selector, "github_object", side_effect=[
            {"object": {"type": "tag", "sha": "c" * 40}},
            {"object": {"type": "commit", "sha": "d" * 40}},
        ]) as read:
            self.assertEqual(selector.resolve_tag("v0.2.1"), "d" * 40)
            self.assertEqual(read.call_args.args, ("tags/" + "c" * 40,))

    def test_tag_resolution_rejects_invalid_noncommit_and_unbounded_objects(self):
        for obj in [{"type": "commit", "sha": "unknown"}, {"type": "tree", "sha": SHA}, {"type": "tag", "sha": SHA}]:
            with self.subTest(obj=obj), patch.object(selector, "github_object", return_value={"object": obj}):
                with self.assertRaises(ValueError):
                    selector.resolve_tag("v0.2.1")

    def test_tag_read_errors_are_not_retried_or_replaced(self):
        with patch.object(selector, "github_object", side_effect=subprocess.CalledProcessError(1, "gh")) as read:
            with self.assertRaises(subprocess.CalledProcessError):
                selector.resolve_tag("v0.2.1")
            self.assertEqual(read.call_count, 1)

    def test_upload_rejects_source_mismatch_and_moved_tag(self):
        with patch.object(selector, "resolve_tag", return_value=SHA):
            selector.verify_source("v0.2.1", SHA, self.directory / "source-sha.txt")
            with self.assertRaisesRegex(ValueError, "source"):
                selector.verify_source("v0.2.1", "c" * 40, self.directory / "source-sha.txt")
        with patch.object(selector, "resolve_tag", return_value="c" * 40):
            with self.assertRaisesRegex(ValueError, "tag"):
                selector.verify_source("v0.2.1", SHA, self.directory / "source-sha.txt")

    def test_release_verifies_source_and_tag_commit_before_upload(self):
        release = (ROOT / ".github/workflows/release.yml").read_text()
        attach = release.split("  attach-sbom:", 1)[1]
        self.assertIn("EXPECTED_SHA: ${{ needs.sbom-target.outputs.ref }}", attach)
        verification = attach.index("python3 scripts/release-sbom-target.py --verify-source sbom/source-sha.txt")
        self.assertLess(verification, attach.index('gh release upload "$RELEASE_TAG"'))

    def test_dry_run_cannot_publish_even_with_tag(self):
        self.assertEqual(selector.select_target("true", "v0.2.1", SHA), {"ref": SHA, "tag": ""})
        self.assertEqual(selector.select_target("true", "", SHA)["tag"], "")

    def test_invalid_tags_and_missing_outputs_fail_closed(self):
        for tag in ["", "main", "--clobber", "v0.2.1\ntag=main", "v0.2.1;echo unsafe"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                selector.select_target("false", tag, SHA)
        with self.assertRaises(ValueError):
            selector.select_target("true", "", "main")

    def test_real_target_cli_emits_empty_dry_run_tag(self):
        output = self.directory / "output"
        result = subprocess.run(["python3", str(ROOT / "scripts/release-sbom-target.py")], env={**os.environ, "DRY_RUN": "true", "RELEASE_TAG": "v0.2.1", "GITHUB_SHA": SHA, "GITHUB_OUTPUT": str(output)}, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output.read_text(), f"ref={SHA}\ntag=\n")

    def test_all_remote_workflow_actions_are_sha_pinned(self):
        for path in (ROOT / ".github/workflows").glob("*.yml"):
            for action in re.findall(r"^\s*-?\s*uses:\s*(\S+)", path.read_text(), re.MULTILINE):
                if not action.startswith("./"):
                    self.assertRegex(action, r"@[0-9a-f]{40}$", f"Unpinned action in {path}: {action}")

    def test_base_images_are_digest_pinned_and_dependabot_tracks_docker(self):
        images = re.findall(r"^FROM (\S+)", (ROOT / "Dockerfile").read_text(), re.MULTILINE)
        self.assertEqual(len(images), 2)
        for image in images:
            self.assertRegex(image, r"@sha256:[0-9a-f]{64}$")
        self.assertTrue(images[0].startswith("rust:1.94-bookworm@"))
        self.assertTrue(images[1].startswith("debian:bookworm-slim@"))
        self.assertIn("package-ecosystem: docker", (ROOT / ".github/dependabot.yml").read_text())

    def test_runtime_copies_trust_store_without_installing_openssl_helpers(self):
        dockerfile = (ROOT / "Dockerfile").read_text()
        builder, runtime = dockerfile.split(" AS runtime", 1)
        self.assertIn("apt-get install -y --no-install-recommends ca-certificates", builder)
        self.assertNotIn("apt-get", runtime)
        for path in ["/etc/ssl/certs", "/usr/share/ca-certificates"]:
            self.assertIn(f"COPY --from=builder {path}/ {path}/", runtime)
        self.assertIn("COPY --from=builder /usr/share/doc/ca-certificates/copyright", runtime)
        self.assertNotIn("--force-depends", dockerfile)
        self.assertNotIn("/var/lib/dpkg", dockerfile)

    def test_pr_dry_run_is_read_only_bounded_and_isolated(self):
        check = (ROOT / ".github/workflows/check.yml").read_text()
        supply = (ROOT / ".github/workflows/supply-chain.yml").read_text()
        self.assertIn("pull_request:", check)
        self.assertIn("name: PR SBOM dry-run", check)
        self.assertIn("ref: ${{ github.event.pull_request.head.sha || github.sha }}", check)
        self.assertNotIn("pull_request_target", check + supply)
        self.assertIn("timeout-minutes: 40", supply)
        self.assertIn("contents: read", supply)
        self.assertNotIn("secrets:", supply)
        self.assertIn("push: false", supply)
        self.assertIn("two-bot-next:scan-${{ github.run_id }}-${{ github.run_attempt }}", supply)
        self.assertEqual(supply.count("image-ref: ${{ env.IMAGE }}"), 2)
        self.assertIn('docker image rm "$IMAGE"', supply)
        self.assertEqual(supply.count("cache-dir: ${{ runner.temp }}/trivy-cache"), 4)
        for name in ["check", "release", "supply-chain"]:
            path = ROOT / ".github/workflows" / f"{name}.yml"
            self.assertNotIn("ubuntu-latest", path.read_text(), str(path))

    def test_candidate_preflight_cannot_replace_existing_gates(self):
        supply = (ROOT / ".github/workflows/supply-chain.yml").read_text()
        self.assertIn("python3 scripts/test-vulnerability-preflight.py", supply)
        preflight = supply.index("python3 scripts/vulnerability-preflight.py sbom")
        self.assertLess(supply.index("Collect exact-image applicability evidence"), preflight)
        self.assertLess(preflight, supply.index("Retain SBOMs and findings"))
        self.assertIn("sha256sum vulnerability-preflight.json", supply)
        self.assertEqual(supply.count("exit-code: '1'"), 2)
        self.assertNotIn("continue-on-error", supply)

    def test_native_selector_diagnostics_cannot_replace_vulnerability_gates(self):
        supply = (ROOT / ".github/workflows/supply-chain.yml").read_text()
        self.assertIn("python3 scripts/test-trivy-selector-probes.py", supply)
        selectors = supply.index("python3 scripts/probe-trivy-selectors.py sbom")
        self.assertLess(supply.index("python3 scripts/vulnerability-preflight.py sbom"), selectors)
        self.assertLess(selectors, supply.index("Retain SBOMs and findings"))
        self.assertIn("timeout-minutes: 4", supply)
        self.assertIn("sha256sum trivy-selector-probes.json", supply)
        self.assertEqual(supply.count("exit-code: '1'"), 2)
        self.assertNotIn("continue-on-error", supply)

    def test_required_check_rejects_every_non_success_scan_result(self):
        workflow = (ROOT / ".github/workflows/check.yml").read_text()
        job = workflow.split("\n  check:\n", 1)[1]
        self.assertIn("needs: [self-role-store, supply-chain]", job)
        self.assertIn("if: ${{ always() }}", job)
        guard = re.search(r"if: (needs\.self-role-store\.result != 'success'[^\n]*)\n\s+run: exit 1", job)
        self.assertIsNotNone(guard)
        self.assertEqual(guard[1], "needs.self-role-store.result != 'success' || needs.supply-chain.result != 'success'")
        for store in ["success", "failure", "skipped", "cancelled"]:
            for scan in ["success", "failure", "skipped", "cancelled"]:
                with self.subTest(store=store, scan=scan):
                    condition = guard[1].replace("needs.self-role-store.result", f"'{store}'").replace("needs.supply-chain.result", f"'{scan}'")
                    result = subprocess.run(["bash", "-c", f"if [[ {condition} ]]; then exit 1; fi"])
                    self.assertEqual(result.returncode, 0 if store == scan == "success" else 1)

    def test_shared_gate_and_dry_run_publication_guards(self):
        supply = (ROOT / ".github/workflows/supply-chain.yml").read_text()
        release = (ROOT / ".github/workflows/release.yml").read_text()
        check = (ROOT / ".github/workflows/check.yml").read_text()
        self.assertIn("uses: ./.github/workflows/supply-chain.yml", check)
        self.assertIn("uses: ./.github/workflows/supply-chain.yml", release)
        self.assertIn("inputs.dry_run != true", release)
        self.assertIn("needs.sbom-target.outputs.tag != ''", release)
        self.assertIn("needs.release-sbom.result == 'success'", release)
        self.assertIn("needs.release-please.outputs.release_created == 'true'", release)
        self.assertIn("ref: ${{ inputs.ref }}", supply)
        self.assertIn("persist-credentials: false", supply)
        self.assertEqual(supply.count("exit-code: '1'"), 2)
        self.assertEqual(supply.count("severity: HIGH,CRITICAL"), 2)
        self.assertEqual(supply.count("ignore-unfixed: false"), 2)
        self.assertEqual(supply.count("trivyignores: .trivyignore.yaml"), 2)
        self.assertNotIn("continue-on-error", supply)
        self.assertIn("steps.inventory.outcome == 'success'", supply)
        self.assertIn("if-no-files-found: error", supply)
        self.assertIn("sha256sum --check SHA256SUMS", release)
        # Current release deliberately accepts no vulnerabilities. If an
        # exception is proposed, update this fixture with its reviewed evidence.
        active = "\n".join(line for line in (ROOT / ".trivyignore.yaml").read_text().splitlines() if not line.startswith("#"))
        self.assertEqual(active.strip(), "vulnerabilities: []")


if __name__ == "__main__":
    unittest.main()
