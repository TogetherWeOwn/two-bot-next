"""Offline regressions for the auditable runtime-crate image evidence checks."""

import copy
import importlib.util
import json
import os
import re
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent
ROOT = SCRIPTS.parent
VALIDATOR = SCRIPTS / "validate-auditable-image.py"
SCANNER = SCRIPTS / "scan-auditable-image.sh"
IMAGE_ID = "sha256:" + "a" * 64
SOURCE_SHA = "b" * 40
BINARY = "/home/nonroot/two-bot"
LOCK = """version = 4

[[package]]
name = "two-bot"
version = "0.1.0"

[[package]]
name = "tokio"
version = "1.40.0"

[[package]]
name = "libc"
version = "0.2.150"

[[package]]
name = "dev-only-crate"
version = "9.9.9"
"""

spec = importlib.util.spec_from_file_location("validate_auditable_image", VALIDATOR)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def artifact(name, version, **over):
    value = {
        "name": name, "version": version, "type": "rust-crate",
        "foundBy": "cargo-auditable-binary-cataloger",
        "locations": [{"path": BINARY}],
    }
    value.update(over)
    return value


def inventory():
    return {
        "source": {"type": "image", "metadata": {
            "imageID": IMAGE_ID, "userInput": IMAGE_ID,
            "os": "linux", "architecture": "amd64"}},
        "descriptor": {"name": "syft", "version": "1.56.0"},
        "artifacts": [artifact("two-bot", "0.1.0"), artifact("tokio", "1.40.0"),
                      artifact("libc", "0.2.150")],
    }


def report(matches=()):
    return {
        "descriptor": {"name": "grype", "version": "0.121.1"},
        "source": {"type": "image", "target": {"imageID": IMAGE_ID, "userInput": IMAGE_ID}},
        "matches": list(matches), "ignoredMatches": [],
    }


class ValidatorTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        (self.dir / "image-id.txt").write_text(IMAGE_ID + "\n")
        (self.dir / "source-sha.txt").write_text(SOURCE_SHA + "\n")
        (self.dir / "Cargo.lock").write_text(LOCK)
        self.write(inventory(), report())

    def write(self, inv=None, rep=None):
        if inv is not None:
            (self.dir / "runtime-crates.syft.json").write_text(json.dumps(inv))
        if rep is not None:
            (self.dir / "runtime-crates.grype.json").write_text(json.dumps(rep))

    def fails(self):
        with self.assertRaises((ValueError, KeyError, TypeError)):
            module.validate(self.dir)

    def test_valid_binds_provenance_and_keeps_dev_only_lockfile_extra(self):
        result = module.validate(self.dir)
        self.assertEqual(result["image_id"], IMAGE_ID)
        self.assertEqual(result["config_id"], IMAGE_ID)
        self.assertEqual(result["source_sha"], SOURCE_SHA)
        self.assertEqual(result["runtime_crates"], 3)
        self.assertEqual(result["grype_matches"], 0)
        self.assertEqual(result["suppressed_count"], 0)
        self.assertEqual(result["lockfile_only"],
                         [{"name": "dev-only-crate", "version": "9.9.9"}])
        self.assertEqual(set(result["sha256"]), {
            "Cargo.lock", "runtime-crates.syft.json", "runtime-crates.grype.json"})

    def assert_partition(self, result, runtime, locked):
        def tuples(rows):
            return {(row["name"], row["version"]) for row in rows}

        r = tuples(result["runtime_crate_tuples"])
        l = tuples(result["locked_crate_tuples"])
        o = tuples(result["lockfile_only"])
        self.assertEqual(r, runtime)
        self.assertEqual(l, locked)
        self.assertLessEqual(r, l)
        self.assertEqual(o, l - r)
        self.assertFalse(r & o)
        self.assertEqual(r | o, l)
        self.assertEqual(result["runtime_crates"], len(r))
        self.assertEqual(result["locked_crates"], len(l))
        self.assertEqual(result["runtime_completeness"], "not_proven")
        for key in ("runtime_crate_tuples", "locked_crate_tuples", "lockfile_only"):
            rows = [(row["name"], row["version"]) for row in result[key]]
            self.assertEqual(rows, sorted(set(rows)))

    def test_receipt_preserves_exact_separate_runtime_and_lockfile_views(self):
        runtime = {("two-bot", "0.1.0"), ("tokio", "1.40.0"), ("libc", "0.2.150")}
        self.assert_partition(module.validate(self.dir), runtime,
                              runtime | {("dev-only-crate", "9.9.9")})

    def test_partition_deduplicates_tuples_but_preserves_multiple_versions(self):
        inv = inventory()
        inv["artifacts"].extend([artifact("tokio", "1.40.0"), artifact("tokio", "1.39.0")])
        self.write(inv)
        (self.dir / "Cargo.lock").write_text(LOCK + '\n[[package]]\nname = "tokio"\n'
                                           'version = "1.39.0"\n\n[[package]]\n'
                                           'name = "tokio"\nversion = "1.40.0"\n')
        runtime = {("two-bot", "0.1.0"), ("tokio", "1.40.0"), ("tokio", "1.39.0"),
                   ("libc", "0.2.150")}
        self.assert_partition(module.validate(self.dir), runtime,
                              runtime | {("dev-only-crate", "9.9.9")})

    def test_empty_complement_is_valid_but_does_not_prove_runtime_completeness(self):
        inv = inventory()
        inv["artifacts"].append(artifact("dev-only-crate", "9.9.9"))
        self.write(inv)
        locked = {("two-bot", "0.1.0"), ("tokio", "1.40.0"), ("libc", "0.2.150"),
                  ("dev-only-crate", "9.9.9")}
        result = module.validate(self.dir)
        self.assert_partition(result, locked, locked)
        self.assertEqual(result["lockfile_only"], [])

    def test_containerd_manifest_id_binds_to_native_config_digest(self):
        inv, rep = inventory(), report()
        config_id = "sha256:" + "c" * 64
        inv["source"]["metadata"].update(
            imageID=config_id, repoDigests=["two-bot-next@" + IMAGE_ID])
        rep["source"]["target"]["imageID"] = config_id
        self.write(inv, rep)
        result = module.validate(self.dir)
        self.assertEqual(result["image_id"], IMAGE_ID)
        self.assertEqual(result["config_id"], config_id)
        for over in ({"repoDigests": []}, {"repoDigests": ["other@" + config_id]},
                     {"repoDigests": ["not-a-digest"]}, {"repoDigests": IMAGE_ID},
                     {"userInput": config_id}, {"imageID": "invalid"}):
            with self.subTest(over=over):
                wrong = copy.deepcopy(inv)
                wrong["source"]["metadata"].update(over)
                self.write(wrong, rep)
                self.fails()
        self.write(inv, rep)
        rep["source"]["target"]["imageID"] = IMAGE_ID
        self.write(rep=rep)
        self.fails()

    def test_synthetic_advisory_matches_runtime_tuple_without_suppression(self):
        match = {"vulnerability": {"id": "SYNTHETIC-0001"},
                 "artifact": artifact("tokio", "1.40.0")}
        self.write(rep=report([match]))
        result = module.validate(self.dir)
        self.assertEqual(result["grype_matches"], 1)
        self.assertEqual(result["suppressed_count"], 0)

    def test_match_outside_runtime_inventory_fails(self):
        for art in (artifact("dev-only-crate", "9.9.9"), artifact("tokio", "1.0.0"),
                    artifact("tokio", "1.40.0", type="deb")):
            self.write(rep=report([{"artifact": art}]))
            self.fails()

    def test_suppressed_or_malformed_matches_fail(self):
        rep = report()
        rep["ignoredMatches"] = [{"artifact": artifact("tokio", "1.40.0")}]
        self.write(rep=rep)
        self.fails()
        rep = report()
        rep["matches"] = {}
        self.write(rep=rep)
        self.fails()

    def test_report_must_be_for_this_inventory_and_tool(self):
        rep = report()
        rep["source"]["target"]["imageID"] = "sha256:" + "c" * 64
        self.write(rep=rep)
        self.fails()
        rep = report()
        rep["source"]["target"]["userInput"] = "mutable-tag:latest"
        self.write(rep=rep)
        self.fails()
        rep = report()
        rep["source"] = {"type": "sbom-file", "target": "/x/runtime-crates.syft.json"}
        self.write(rep=rep)
        self.fails()
        rep = report()
        rep["descriptor"]["version"] = "0.0.1"
        self.write(rep=rep)
        self.fails()

    def mutate_inventory(self, fn):
        inv = copy.deepcopy(inventory())
        fn(inv)
        self.write(inv)
        self.fails()

    def test_inventory_rejections(self):
        cases = {
            "empty": lambda i: i.update(artifacts=[]),
            "incomplete": lambda i: i.update(artifacts=i["artifacts"][:2]),
            "wrong image": lambda i: i["source"]["metadata"].update(imageID="sha256:" + "c" * 64),
            "wrong input": lambda i: i["source"]["metadata"].update(userInput="mutable:latest"),
            "not image": lambda i: i["source"].update(type="directory"),
            "wrong arch": lambda i: i["source"]["metadata"].update(architecture="arm64"),
            "wrong os": lambda i: i["source"]["metadata"].update(os="windows"),
            "wrong tool": lambda i: i["descriptor"].update(name="other"),
            "wrong version": lambda i: i["descriptor"].update(version="1.0.0"),
            "wrong location": lambda i: i["artifacts"][0].update(locations=[{"path": "/usr/bin/x"}]),
            "extra location": lambda i: i["artifacts"][0]["locations"].append({"path": "/x"}),
            "no location": lambda i: i["artifacts"][0].update(locations=[]),
            "unknown cataloger": lambda i: i["artifacts"][0].update(foundBy="other"),
            "unknown type": lambda i: i["artifacts"][0].update(type="deb"),
            "unknown crate": lambda i: i["artifacts"].append(artifact("ghost", "1.0.0")),
            "wrong crate version": lambda i: i["artifacts"].append(artifact("tokio", "0.0.1")),
            "empty version": lambda i: i["artifacts"][0].update(version=""),
            "non-string name": lambda i: i["artifacts"][0].update(name=1),
            "missing field": lambda i: i.pop("descriptor"),
        }
        for label, fn in cases.items():
            with self.subTest(label):
                self.mutate_inventory(fn)

    def test_provenance_rejections(self):
        for name, text in (("image-id.txt", "sha256:short"), ("image-id.txt", "A" * 71),
                           ("source-sha.txt", "abc"), ("source-sha.txt", "B" * 40)):
            with self.subTest(name=name, text=text):
                original = (self.dir / name).read_text()
                (self.dir / name).write_text(text)
                self.fails()
                (self.dir / name).write_text(original)

    def test_bad_json_duplicate_keys_and_non_objects_fail(self):
        for text in ("{", "[]", "null", '{"a": 1, "a": 2}', '{"a": NaN}'):
            with self.subTest(text=text):
                (self.dir / "runtime-crates.syft.json").write_text(text)
                self.fails()
        self.write(inventory())
        (self.dir / "runtime-crates.grype.json").write_text('{"matches": [], "matches": []}')
        self.fails()

    def test_oversized_and_missing_evidence_fail(self):
        for name in ("runtime-crates.syft.json", "runtime-crates.grype.json"):
            with self.subTest(name=name):
                path = self.dir / name
                original = path.read_bytes()
                with path.open("wb") as handle:
                    handle.truncate(module.CAP + 1)
                self.fails()
                path.write_bytes(original)
                path.unlink()
                self.fails()
                path.write_bytes(original)

    def test_lockfile_oversized_or_malformed_fails(self):
        (self.dir / "Cargo.lock").write_text("not [toml")
        self.fails()
        (self.dir / "Cargo.lock").write_text("# " + "x" * (module.sbom.TEXT_CAP + 1))
        self.fails()

    def run_cli(self):
        return subprocess.run([sys.executable, "-I", str(VALIDATOR), str(self.dir)],
                              capture_output=True, text=True, check=False)

    def test_cli_pass_writes_check_and_fail_is_nonzero_and_quiet(self):
        done = self.run_cli()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("not a vulnerability verdict", done.stdout)
        check = json.loads((self.dir / "runtime-crates-check.json").read_text())
        self.assertEqual(check["suppressed_count"], 0)
        (self.dir / "runtime-crates.syft.json").write_text("{")
        done = self.run_cli()
        self.assertEqual(done.returncode, 1)
        self.assertEqual(done.stderr.strip(), "Runtime crate inventory validation failed")
        self.assertEqual(done.stdout, "")


class ScannerTests(unittest.TestCase):
    def make_env(self, root, syft_rc=0, grype_rc=0):
        bin_dir = root / "bin"
        bin_dir.mkdir()

        def tool(name, body):
            path = bin_dir / name
            path.write_text("#!/usr/bin/env bash\n" + body)
            path.chmod(path.stat().st_mode | stat.S_IXUSR)

        tool("curl", 'out=""; while [ $# -gt 0 ]; do [ "$1" = -o ] && out=$2; shift; done; : > "$out"\n')
        tool("sha256sum", "cat >/dev/null\n")
        tool("timeout", "shift\nexec \"$@\"\n")
        tool("tar", 'dest=""; prev=""; for a in "$@"; do [ "$prev" = -C ] && dest=$a; prev=$a; done\n'
                    'last=${@: -1}\n'
                    'if [ "$last" = syft ]; then body=$STUB_SYFT; else body=$STUB_GRYPE; fi\n'
                    'printf "#!/usr/bin/env bash\\n%s\\n" "$body" > "$dest/$last"\n'
                    'chmod +x "$dest/$last"\n')
        env = {k: v for k, v in os.environ.items() if k not in ("BASH_ENV", "ENV")}
        env.update(PATH=f"{bin_dir}:{os.environ['PATH']}",
                   RUNNER_TEMP=str(root / "tmp"))
        # Both native tools infer the config format from its filename suffix.
        config_check = (
            'config=""; prev=""; for a in "$@"; do '
            '[ "$prev" = --config ] && config=$a; prev=$a; done\n'
            '[[ "$config" = *.yaml ]] && grep -Fxq "{}" "$config" || exit 64\n'
            'printf "%s\\n" "$config" >> "$STUB_CONFIG_LOG"\n')
        env["STUB_CONFIG_LOG"] = str(root / "config-paths")
        # Stub bodies: syft writes the -o target; grype prints JSON.
        env["STUB_SYFT"] = config_check + (
            'for a in "$@"; do case $a in syft-json=*) echo "{}" > "${a#syft-json=}";; esac; done; '
            f"exit {syft_rc}")
        env["STUB_GRYPE"] = config_check + f"echo '{{}}'; exit {grype_rc}"
        (root / "tmp").mkdir()
        return env

    def run_scan(self, syft_rc=0, grype_rc=0, image_id=IMAGE_ID):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        out = root / "out"
        out.mkdir()
        (out / "image-id.txt").write_text(image_id + "\n")
        env = self.make_env(root, syft_rc, grype_rc)
        done = subprocess.run(["bash", str(SCANNER), str(out)], env=env,
                              capture_output=True, text=True, check=False)
        return done, out, root

    def test_success_writes_both_native_reports_and_cleans_scratch(self):
        done, out, root = self.run_scan()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertTrue((out / "runtime-crates.syft.json").is_file())
        self.assertTrue((out / "runtime-crates.grype.json").is_file())
        self.assertEqual(list((root / "tmp").iterdir()), [])

    def test_scanners_receive_explicit_empty_yaml_configuration(self):
        done, _, root = self.run_scan()
        self.assertEqual(done.returncode, 0, done.stderr)
        configs = (root / "config-paths").read_text().splitlines()
        self.assertEqual(len(configs), 2)
        self.assertEqual(configs[0], configs[1])
        config = Path(configs[0])
        self.assertEqual(config.suffix, ".yaml")
        self.assertEqual(config.parent.parent, root / "tmp")
        self.assertFalse(config.exists())

    def test_syft_failure_propagates_and_skips_grype(self):
        done, out, _ = self.run_scan(syft_rc=7)
        self.assertEqual(done.returncode, 7)
        self.assertFalse((out / "runtime-crates.grype.json").exists())

    def test_grype_failure_propagates(self):
        done, _, _ = self.run_scan(grype_rc=5)
        self.assertEqual(done.returncode, 5)

    def test_malformed_image_id_is_refused(self):
        done, out, _ = self.run_scan(image_id="sha256:nothex")
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse((out / "runtime-crates.syft.json").exists())


class TrivyInstallerTests(unittest.TestCase):
    def installer(self):
        text = (ROOT / ".github/workflows/sbom.yml").read_text()
        title = "      - name: Install verified pinned Trivy\n"
        self.assertIn(title, text)
        step = text.split(title, 1)[1].split("\n      - ", 1)[0]
        self.assertIn("        id: trivy\n", step)
        self.assertIn("        run: bash scripts/install-trivy.sh\n", step)
        return (ROOT / "scripts/install-trivy.sh").read_text()

    def run_installer(self, download_status=0, checksum_status=0):
        script = self.installer()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            tools = root / "tools"
            tools.mkdir()
            for command, body in {
                    "curl": 'printf "curl %s\\n" "$*" >> "$LOG"; exit "$DOWNLOAD_STATUS"',
                    "sha256sum": 'while IFS= read -r line; do printf "sha256sum %s\\n" "$line" >> "$LOG"; done; exit "$CHECKSUM_STATUS"',
                    "tar": 'printf "tar %s\\n" "$*" >> "$LOG"; printf "#!/bin/sh\\nprintf \'Version: 0.69.3\\\\n\'\\n"',
                    "uname": 'case "$1" in -s) printf "Linux\\n";; -m) printf "x86_64\\n";; esac'}.items():
                path = tools / command
                path.write_text("#!/bin/bash\n" + body + "\n")
                path.chmod(0o755)
            env = {k: v for k, v in os.environ.items() if k not in ("BASH_ENV", "ENV")}
            env.update(PATH=f"{tools}:{os.environ['PATH']}", RUNNER_TEMP=tmp,
                       GITHUB_PATH=str(root / "github-path"), LOG=str(root / "calls"),
                       DOWNLOAD_STATUS=str(download_status), CHECKSUM_STATUS=str(checksum_status))
            result = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", script],
                                    env=env, text=True, capture_output=True, timeout=10)
            self.assertTrue((root / "calls").exists(), result.stderr)
            calls = (root / "calls").read_text()
            published = (root / "github-path").exists()
            self.assertFalse(list(root.glob("trivy-download.*")))
            return result, calls, published

    def test_verified_direct_download_is_published_after_checksum_and_extraction(self):
        result, calls, published = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(published)
        self.assertIn("https://github.com/aquasecurity/trivy/releases/download/v0.69.3/trivy_0.69.3_Linux-64bit.tar.gz", calls)
        self.assertIn("1816b632dfe529869c740c0913e36bd1629cb7688bd5634f4a858c1d57c88b75", calls)
        self.assertLess(calls.index("sha256sum"), calls.index("tar "))
        script = self.installer()
        self.assertLess(script.index("tar -xOzf"), script.index('>> "$GITHUB_PATH"'))
        self.assertLess(script.index('"$bin/trivy" --version'), script.index('>> "$GITHUB_PATH"'))
        self.assertNotIn("get.trivy.dev", script)

    def test_download_failure_is_not_extracted_or_published(self):
        result, calls, published = self.run_installer(download_status=6)
        self.assertEqual(result.returncode, 6)
        self.assertNotIn("sha256sum", calls)
        self.assertNotIn("tar ", calls)
        self.assertFalse(published)

    def test_checksum_failure_is_not_extracted_or_published(self):
        result, calls, published = self.run_installer(checksum_status=1)
        self.assertEqual(result.returncode, 1)
        self.assertNotIn("tar ", calls)
        self.assertFalse(published)


class StaticWiringTests(unittest.TestCase):
    def setUp(self):
        self.scanner = SCANNER.read_text()
        self.dockerfile = (ROOT / "Dockerfile").read_text()

    def test_scanner_pins_and_policy(self):
        for needle in (
                "syft/releases/download/v1.56.0/syft_1.56.0_linux_amd64.tar.gz",
                "1c2493cfcf52d3367d6cb25780d9190c111c2219c34c820574c43e8bae15c528",
                "grype/releases/download/v0.121.1/grype_0.121.1_linux_amd64.tar.gz",
                "bc3ea3f9e6fd56ac781c08c6c5e614271b79f438200757f6932085c19b0a4657",
                "--override-default-catalogers cargo-auditable-binary-cataloger",
                '--config "$scratch/config.yaml"', "set -euo pipefail", "sha256sum --check"):
            self.assertIn(needle, self.scanner)
        code = "\n".join(l for l in self.scanner.splitlines() if not l.lstrip().startswith("#"))
        for forbidden in ("--fail-on", "--ignore", "ignore-states"):
            self.assertNotIn(forbidden, code)
        self.assertEqual(self.scanner.count("sha256sum --check"), 2)
        self.assertIsNone(re.search(r"\|\s*(ba)?sh\b", code))

    def test_validator_pins_match_scanner(self):
        text = VALIDATOR.read_text()
        self.assertIn('"1.56.0"', text)
        self.assertIn('"0.121.1"', text)
        self.assertIn("v1.56.0", self.scanner)
        self.assertIn("v0.121.1", self.scanner)

    def test_dockerfile_installs_verified_cargo_auditable_and_uses_it(self):
        self.assertIn("cargo-auditable/releases/download/v0.7.7/", self.dockerfile)
        d = self.dockerfile
        # Installed after the real-source COPY so the dependency layer replays unchanged.
        self.assertGreater(d.index("cargo-auditable/releases"), d.index("COPY . ."))
        self.assertLess(d.index("install -m 0755"), d.index("cargo auditable build"))
        for sha in ("7676443367c5e33d9fb22acef63cccb5e9202de12dae1b0579995eb3e2bc7a18",
                    "c324af56990601354bbf40080f582ba46f8ec8b807a1388e25bb7e429d285c18"):
            self.assertIn(sha, self.dockerfile)
        self.assertIn("sha256sum --check", self.dockerfile)
        self.assertIn("cargo auditable build --release --locked", self.dockerfile)
        self.assertNotIn("RUN cargo build --release", self.dockerfile)
        self.assertLess(self.dockerfile.index("sha256sum --check"),
                        self.dockerfile.index("install -m 0755"))


    def workflow_jobs(self):
        text = (ROOT / ".github/workflows/sbom.yml").read_text()
        head, sep, verify = text.partition("\n  verify:\n")
        self.assertTrue(sep)
        self.assertIn("\n  image:\n", head)
        return head, verify

    @staticmethod
    def code(text):
        return "\n".join(l for l in text.splitlines() if not l.lstrip().startswith("#"))

    def test_workflow_fixtures_run_before_untrusted_checkout(self):
        image, _ = self.workflow_jobs()
        image = self.code(image)
        fixtures = image.index("python3 -m unittest discover -s scripts -p 'test_auditable_image.py'")
        self.assertLess(fixtures, image.index("ref: ${{ inputs.ref }}"))
        self.assertEqual(image.count("inputs.ref"), 1)
        # Nothing local is executed after the untrusted checkout in this job.
        self.assertNotIn("python3 scripts/", image[image.index("inputs.ref"):])
        self.assertNotIn("bash scripts/", image[image.index("inputs.ref"):])

    def test_workflow_installs_trivy_before_untrusted_checkout_and_skips_redirect(self):
        image, _ = self.workflow_jobs()
        image = self.code(image)
        self.assertLess(image.index("name: Install verified pinned Trivy"), image.index("inputs.ref"))
        self.assertEqual(image.count("skip-setup-trivy: true"), 4)
        self.assertIn("id: trivy\n        run: bash scripts/install-trivy.sh", image)
        self.assertIn("!cancelled() && steps.trivy.outcome == 'success'", image)
        self.assertNotIn('rm -rf "$RUNNER_TEMP/trivy-bin"', image)

    def test_workflow_scan_and_validator_run_only_in_trusted_verify_job(self):
        image, verify = self.workflow_jobs()
        image, verify = self.code(image), self.code(verify)
        for needle in ("scan-auditable-image.sh", "validate-auditable-image.py"):
            self.assertNotIn(needle, image)
            self.assertEqual(verify.count(needle), 1)
        self.assertNotIn("inputs.ref", verify)
        self.assertNotIn("path: source", verify)
        self.assertLess(verify.index("docker load -i sbom/image.tar"),
                        verify.index("scan-auditable-image.sh"))
        self.assertLess(verify.index("scan-auditable-image.sh"),
                        verify.index("validate-auditable-image.py"))

    def test_workflow_validator_precedes_lockfile_removal(self):
        _, verify = self.workflow_jobs()
        verify = self.code(verify)
        self.assertEqual(verify.count("rm sbom/Cargo.lock"), 1)
        self.assertLess(verify.index("python3 scripts/validate-auditable-image.py sbom"),
                        verify.index("rm sbom/Cargo.lock"))

    def test_workflow_checksums_keep_release_line_and_separate_diagnostics(self):
        _, verify = self.workflow_jobs()
        original = "(cd sbom && sha256sum *.cdx.json source-sha.txt image-id.txt > SHA256SUMS)"
        self.assertEqual(verify.count(original), 1)
        self.assertEqual(verify.count("> SHA256SUMS"), 1)
        extra = ("(cd sbom && sha256sum runtime-crates*.json source-sha.txt "
                 "image-id.txt > runtime-crates.sha256)")
        self.assertEqual(verify.count(extra), 1)
        self.assertEqual(verify.count("runtime-crates.sha256"), 1)
        self.assertLess(verify.index(extra), verify.index("rm sbom/Cargo.lock"))

    def test_workflow_native_failure_keeps_existing_evidence_path(self):
        _, verify = self.workflow_jobs()
        steps = verify.split("\n      - name: ")
        for title in ("Validate native runtime crate evidence",
                      "Validate SBOM inventories and record checksums"):
            step = next(s for s in steps if s.startswith(title))
            self.assertIn("if: ${{ !cancelled() }}", step)
            self.assertNotIn("continue-on-error", step)
        for title in ("Collect exact-image applicability evidence",
                      "Bind findings to exact installed tuples"):
            step = next(s for s in steps if s.startswith(title))
            self.assertIn("!cancelled() && steps.inventory.outcome == 'success'", step)

    def test_workflow_keeps_both_trivy_high_critical_gates(self):
        image, _ = self.workflow_jobs()
        steps = image.split("\n      - name: ")
        for title, output in (
                ("Gate Rust dependencies (HIGH/CRITICAL, including unfixed)",
                 "sbom/rust-vulnerabilities.json"),
                ("Gate runtime image (HIGH/CRITICAL, including unfixed)",
                 "sbom/image-vulnerabilities.json")):
            matching = [s for s in steps if s.startswith(title)]
            self.assertEqual(len(matching), 1, title)
            step = matching[0]
            for needle in ("uses: aquasecurity/trivy-action@", "severity: HIGH,CRITICAL",
                           "ignore-unfixed: false", "trivyignores: .trivyignore.yaml",
                           f"output: {output}", "exit-code: '1'"):
                self.assertIn(needle, step, title)
            self.assertNotIn("continue-on-error", step)
            self.assertNotIn("ignore-unfixed: true", step)


if __name__ == "__main__":
    unittest.main()
