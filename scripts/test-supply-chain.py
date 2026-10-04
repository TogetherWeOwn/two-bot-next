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

    def test_all_workflow_container_and_service_images_are_digest_pinned(self):
        # Dependabot's docker ecosystem scans Dockerfiles only, so a floating
        # tag in a workflow container/service would never be bumped or noticed.
        for path in (ROOT / ".github/workflows").glob("*.yml"):
            for image in re.findall(r"^[ \t]+image:[ \t]+(\S+)", path.read_text(), re.MULTILINE):
                self.assertRegex(image, r"@sha256:[0-9a-f]{64}$", f"Unpinned image in {path}: {image}")

    def test_base_images_are_digest_pinned_and_dependabot_tracks_docker(self):
        images = re.findall(r"^FROM (\S+)", (ROOT / "Dockerfile").read_text(), re.MULTILINE)
        self.assertEqual(len(images), 2)
        for image in images:
            self.assertRegex(image, r"@sha256:[0-9a-f]{64}$")
        self.assertTrue(images[0].startswith("rust:1.94-trixie@"))
        self.assertTrue(images[1].startswith("gcr.io/distroless/cc-debian13:nonroot@"))
        self.assertIn("package-ecosystem: docker", (ROOT / ".github/dependabot.yml").read_text())

    def test_distroless_runtime_installs_nothing_and_runs_as_nonroot(self):
        dockerfile = (ROOT / "Dockerfile").read_text()
        runtime = dockerfile.split(" AS runtime", 1)[1]
        # No shell exists in the runtime base: no RUN, package manager or
        # purge step may appear, and trust data comes from the base itself.
        self.assertNotRegex(runtime, r"(?m)^RUN ")
        self.assertNotIn("apt-get", dockerfile)
        self.assertNotIn("useradd", dockerfile)
        self.assertNotIn("--force-depends", dockerfile)
        self.assertNotIn("/var/lib/dpkg", dockerfile)
        self.assertIn("USER 65532:65532", runtime)
        self.assertLess(runtime.index("USER 65532:65532"), runtime.index("ENTRYPOINT"))
        self.assertIn('ENTRYPOINT ["/home/nonroot/two-bot"]', runtime)
        self.assertIn('CMD ["/home/nonroot/two-bot", "--healthcheck"]', runtime)
        self.assertEqual(re.findall(r"(?m)^COPY .*", runtime),
                         ["COPY --from=builder --chown=65532:65532 /app/target/release/two-bot ./two-bot"])
        supply = (ROOT / ".github/workflows/sbom.yml").read_text()
        self.assertEqual(supply.count("exit-code: '1'"), 2)
        self.assertEqual(supply.count("ignore-unfixed: false"), 2)
        self.assertEqual(supply.count("trivyignores: .trivyignore.yaml"), 2)
        self.assertNotIn("continue-on-error", supply)
        self.assertRegex((ROOT / ".trivyignore.yaml").read_text(), r"(?m)^vulnerabilities: \[\]$")

    def test_pr_dry_run_is_read_only_bounded_and_isolated(self):
        check = (ROOT / ".github/workflows/check.yml").read_text()
        supply = (ROOT / ".github/workflows/sbom.yml").read_text()
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
        self.assertEqual(supply.count("cache: false"), 4)
        # Folded pr-lint + gitleaks gate (TOG-11810) shares its filename
        # with the pre-fold SBOM workflow; assert on our renamed file plus
        # the callers that reference it.
        for name in ["check", "release", "sbom", "supply-chain"]:
            path = ROOT / ".github/workflows" / f"{name}.yml"
            # Comments may name the overflow example; no runs-on may use it.
            code = "\n".join(
                line for line in path.read_text().splitlines()
                if not line.lstrip().startswith("#")
            )
            # Only the documented routing expression may name a hosted runner.
            routed = "'[\"ubuntu-latest\"]'"
            self.assertNotIn("ubuntu-latest", code.replace(routed, ""), str(path))

    def test_no_shared_caches(self):
        # The reusable workflow can run in default-branch context against an
        # arbitrary inputs.ref (release dispatch), so no cross-run cache may
        # be written or read: a poisoned Trivy DB or layer cache could make
        # the HIGH/CRITICAL gate pass. Trivy downloads a fresh DB every run;
        # per-run caches live only under runner.temp.
        supply = (ROOT / ".github/workflows/sbom.yml").read_text()
        for shared in ("cache-from", "cache-to", "two-bot-sbom-amd64", "cache: true"):
            self.assertNotIn(shared, supply)
        self.assertEqual(supply.count("cache: false"), 4)

    def test_no_local_script_runs_after_untrusted_checkout(self):
        # CodeQL cache-poisoning gate: a job reachable from workflow_dispatch
        # (release.yml -> sbom.yml) that checks out inputs.ref must not execute
        # a local script afterwards. The fixtures step runs from the trusted
        # root checkout BEFORE the `source` checkout; validation, evidence and
        # preflight run in `verify`, which never checks out inputs.ref and
        # receives the lockfile, provenance, BOMs and image through the
        # sbom-partial artifact.
        supply = (ROOT / ".github/workflows/sbom.yml").read_text()
        self.assertIn("ref: ${{ inputs.ref }}", supply)
        self.assertIn("python3 scripts/test-supply-chain.py", supply)
        self.assertLess(supply.index("python3 scripts/test-supply-chain.py"),
                        supply.index("ref: ${{ inputs.ref }}"))
        image = supply.split("  verify:", 1)[0]
        self.assertNotIn("python3 scripts/validate-sbom.py", image)
        self.assertNotIn("python3 scripts/runtime-image-evidence.py", image)
        self.assertNotIn("python3 scripts/vulnerability-preflight.py", image)
        verify = supply.split("  verify:", 1)[1]
        self.assertNotIn("inputs.ref", verify)
        self.assertIn("sbom-partial", image)
        self.assertIn("sbom-partial", verify)
        # The internal handoff artifact is per-attempt (same
        # run_id/run_attempt scheme as the scanned image tag): v4 artifacts
        # are immutable and same-named uploads coexist, so a bare-name
        # download on a `rerun failed jobs` retry can resolve to the previous
        # attempt's BOMs/image and fail validation (or validate stale BOMs).
        attempt_handoff = "name: sbom-partial-${{ github.run_id }}-${{ github.run_attempt }}"
        self.assertIn(attempt_handoff, image)
        self.assertIn(attempt_handoff, verify)
        for line in supply.splitlines():
            if line.lstrip().startswith("#"):
                continue
            self.assertNotEqual(line.strip(), "name: sbom-partial",
                                "bare sbom-partial artifact name reintroduces cross-attempt reuse on reruns")
        # The verify job reads the exact inputs.ref lockfile handed over in
        # the artifact, never its own checkout's lockfile; the handoff copy
        # is removed before checksums so release assets stay pinned.
        self.assertIn("python3 scripts/validate-sbom.py sbom/Cargo.lock sbom", verify)
        self.assertIn("cp source/Cargo.lock sbom/Cargo.lock", image)
        self.assertIn("rm sbom/Cargo.lock", verify)
        # Verify still runs after a failed gate (needs without a
        # needs-referencing if would skip on failure) but not on cancellation.
        self.assertIn("needs.image.result == 'success' || needs.image.result == 'failure'", verify)
        self.assertIn("!cancelled()", verify)

    def test_candidate_preflight_cannot_replace_existing_gates(self):
        supply = (ROOT / ".github/workflows/sbom.yml").read_text()
        self.assertIn("python3 scripts/test-vulnerability-preflight.py", supply)
        preflight = supply.index("python3 scripts/vulnerability-preflight.py sbom")
        self.assertLess(supply.index("Collect exact-image applicability evidence"), preflight)
        self.assertLess(preflight, supply.index("Retain SBOMs and findings"))
        self.assertIn("sha256sum vulnerability-preflight.json", supply)
        self.assertEqual(supply.count("exit-code: '1'"), 2)
        self.assertNotIn("continue-on-error", supply)

    def test_retired_bookworm_diagnostics_are_gone(self):
        supply = (ROOT / ".github/workflows/sbom.yml").read_text()
        for retired in ("probe-trivy-selectors.py", "test-trivy-selector-probes.py",
                        "purge-runtime-mount", "trivy-selector-probes.json"):
            self.assertNotIn(retired, supply)
            self.assertNotIn(retired, (ROOT / "Dockerfile").read_text())
        for path in ("probe-trivy-selectors.py", "test-trivy-selector-probes.py",
                     "purge-runtime-mount.sh", "test-purge-runtime-mount.py"):
            self.assertFalse((ROOT / "scripts" / path).exists(), path)

    def test_required_check_rejects_every_non_success_scan_result(self):
        workflow = (ROOT / ".github/workflows/check.yml").read_text()
        job = workflow.split("\n  check:\n", 1)[1]
        # CI standard rule 6 (TOG-14881): the supply-chain gate is
        # selector-aware like the self-role/parity gates -- it rejects a
        # failed, skipped or cancelled scan when `supply` was selected, and
        # only tolerates the skip the selector itself produced (a `skipped`
        # result with `supply == 'false'`). No other exception may hide a
        # scan result.
        self.assertIn("needs: [self-role-store, supply-chain, parity-docs, job-inputs]", job)
        self.assertIn("if: ${{ always() }}", job)
        guard = re.search(r"- name: require supply-chain gate to pass\n\s+if: ([^\n]*)\n\s+run: exit 1", job)
        self.assertIsNotNone(guard)
        self.assertEqual(guard[1], "needs.supply-chain.result != 'success' "
                                   "&& needs.job-inputs.outputs.supply != 'false'")
        for scan, selected, fail in [
                ("success", "true", False), ("failure", "true", True),
                ("skipped", "true", True), ("cancelled", "true", True),
                ("success", "false", False), ("failure", "false", False),
                ("skipped", "false", False), ("cancelled", "false", False)]:
            with self.subTest(scan=scan, selected=selected):
                condition = guard[1].replace(
                    "needs.supply-chain.result", f"'{scan}'").replace(
                    "needs.job-inputs.outputs.supply", f"'{selected}'")
                result = subprocess.run(["bash", "-c", f"if [[ {condition} ]]; then exit 1; fi"])
                self.assertEqual(result.returncode, 1 if fail else 0)

    def test_shared_gate_and_dry_run_publication_guards(self):
        supply = (ROOT / ".github/workflows/sbom.yml").read_text()
        release = (ROOT / ".github/workflows/release.yml").read_text()
        check = (ROOT / ".github/workflows/check.yml").read_text()
        self.assertIn("uses: ./.github/workflows/sbom.yml", check)
        self.assertIn("uses: ./.github/workflows/sbom.yml", release)
        self.assertNotIn("uses: ./.github/workflows/supply-chain.yml", check + release)
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
