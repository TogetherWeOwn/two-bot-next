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

    def test_release_uses_published_tag_not_new_main(self):
        self.assertEqual(selector.select_target("false", "v0.2.1", SHA), {"ref": "v0.2.1", "tag": "v0.2.1"})
        self.assertEqual(selector.select_target("", "v0.2.1", SHA)["ref"], "v0.2.1")

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
