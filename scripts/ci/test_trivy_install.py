"""Offline verified-install, fail-closed transport and SBOM wiring regressions."""

import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / "scripts/install-trivy.sh"
DIGEST = "1816b632dfe529869c740c0913e36bd1629cb7688bd5634f4a858c1d57c88b75"
URL = "https://github.com/aquasecurity/trivy/releases/download/v0.69.3/trivy_0.69.3_Linux-64bit.tar.gz"


class TrivyInstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP"))
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.tools = self.root / "tools"
        self.tools.mkdir()
        self.scratch = self.root / "scratch"
        self.scratch.mkdir()
        self.path_file = self.root / "github-path"
        self.path_file.touch()
        self.archive = self.root / "fixture.tar.gz"
        self.marker = self.root / "executed"
        binary = b'#!/bin/sh\nprintf "Version: 0.69.3\\n"\nprintf executed > "$MARKER"\n'
        with tarfile.open(self.archive, "w:gz") as archive:
            member = tarfile.TarInfo("trivy")
            member.size = len(binary)
            archive.addfile(member, io.BytesIO(binary))
        # Substitute only the reviewed digest in a trusted test copy, never add
        # a production digest/URL override. Real sha256sum and tar are exercised.
        self.installer = self.tools / "install-trivy.sh"
        source = INSTALLER.read_text()
        self.assertIn(f"sha256={DIGEST}\n", source)
        self.installer.write_text(source.replace(DIGEST, hashlib.sha256(self.archive.read_bytes()).hexdigest()))
        self.env = {**os.environ, "RUNNER_TEMP": str(self.scratch),
                    "GITHUB_PATH": str(self.path_file), "MARKER": str(self.marker),
                    "FIXTURE_ARCHIVE": str(self.archive), "CALLS": str(self.root / "calls"),
                    "PATH": f"{self.tools}{os.pathsep}{os.environ['PATH']}"}
        # Startup hooks can replace PATH and bypass the offline transport stub.
        self.env.pop("BASH_ENV", None)
        self.env.pop("ENV", None)
        self.tool("curl", '''#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
args = sys.argv[1:]
pathlib.Path(os.environ["CALLS"]).write_text(json.dumps(args))
shutil.copyfile(os.environ["FIXTURE_ARCHIVE"], args[args.index("--output") + 1])
sys.exit(int(os.environ.get("CURL_EXIT", "0")))
''')
        self.tool("uname", '#!/bin/sh\ncase "$1" in -s) printf "Linux\\n";; -m) printf "x86_64\\n";; esac\n')

    def tool(self, name, source):
        path = self.tools / name
        path.write_text(source)
        path.chmod(0o755)

    def run_install(self):
        return subprocess.run(["bash", str(self.installer)], env=self.env,
                              capture_output=True, text=True, timeout=10)

    def assert_failed_closed(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.path_file.read_text(), "")
        self.assertFalse(self.marker.exists(), "unverified binary must never execute")
        self.assertEqual(list(self.scratch.iterdir()), [], "failed installation must remove its own scratch")

    def test_verified_install_publishes_only_fresh_binary(self):
        result = self.run_install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("OK", result.stdout)
        self.assertIn("Version: 0.69.3", result.stdout)
        self.assertTrue(self.marker.exists())
        directory = Path(self.path_file.read_text().strip())
        self.assertEqual(directory.parent, self.scratch)
        self.assertTrue((directory / "trivy").stat().st_mode & 0o111)
        self.assertEqual(list(self.scratch.iterdir()), [directory])

    def test_download_failure_does_not_hash_extract_execute_or_publish(self):
        for status in (6, 7, 22, 28, 63):
            with self.subTest(status=status):
                self.env["CURL_EXIT"] = str(status)
                result = self.run_install()
                self.assert_failed_closed(result)
                self.assertEqual(result.returncode, status)
                self.assertIn(f"download failed (exit {status})", result.stderr)
                self.assertNotIn("OK", result.stdout)

    def test_checksum_mismatch_never_extracts_or_executes(self):
        self.installer.write_text(INSTALLER.read_text())
        result = self.run_install()
        self.assert_failed_closed(result)
        self.assertIn("FAILED", result.stdout)

    def test_corrupt_archive_fails_after_verified_download(self):
        self.archive.write_bytes(b"not a gzip archive")
        self.installer.write_text(INSTALLER.read_text().replace(
            DIGEST, hashlib.sha256(self.archive.read_bytes()).hexdigest()))
        result = self.run_install()
        self.assert_failed_closed(result)
        self.assertIn("OK", result.stdout)

    def test_permission_failure_does_not_publish_path(self):
        self.tool("chmod", "#!/bin/sh\nexit 1\n")
        result = self.run_install()
        self.assert_failed_closed(result)

    def test_version_command_failure_does_not_publish_path(self):
        binary = b"#!/bin/sh\nexit 42\n"
        with tarfile.open(self.archive, "w:gz") as archive:
            member = tarfile.TarInfo("trivy")
            member.size = len(binary)
            archive.addfile(member, io.BytesIO(binary))
        self.installer.write_text(INSTALLER.read_text().replace(
            DIGEST, hashlib.sha256(self.archive.read_bytes()).hexdigest()))
        result = self.run_install()
        self.assert_failed_closed(result)
        self.assertEqual(result.returncode, 42)

    def test_unsupported_architecture_fails_without_download(self):
        self.tool("uname", '#!/bin/sh\nprintf "arm64\\n"\n')
        result = self.run_install()
        self.assert_failed_closed(result)
        self.assertIn("requires Linux/x86_64", result.stderr)
        self.assertFalse((self.root / "calls").exists())

    def test_transport_uses_pinned_https_asset_and_finite_caps(self):
        result = self.run_install()
        self.assertEqual(result.returncode, 0, result.stderr)
        args = json.loads((self.root / "calls").read_text())
        self.assertEqual(args[0], "-q", "runner curlrc must not override policy")
        self.assertEqual(args[-1], URL)
        for flag in ("--fail", "--silent", "--show-error", "--location", "--retry-all-errors"):
            self.assertIn(flag, args)
        for flag, value in {"--proto": "=https", "--proto-redir": "=https",
                            "--max-redirs": "3", "--connect-timeout": "10",
                            "--max-time": "120", "--retry": "2", "--retry-delay": "2",
                            "--retry-max-time": "300", "--max-filesize": "104857600"}.items():
            self.assertEqual(args[args.index(flag) + 1], value)
        source = INSTALLER.read_text()
        self.assertIn("timeout --kill-after=5s 360s curl -q", source)
        self.assertIn("%{http_code}", args[args.index("--write-out") + 1])
        self.assertNotIn("url_effective", source, "do not log signed redirect URLs")
        self.assertLess(source.index("sha256sum --check --strict"), source.index("tar -xOzf"))
        self.assertLess(source.index('"$bin/trivy" --version'), source.index('>> "$GITHUB_PATH"'))
        self.assertNotIn("--insecure", source)
        self.assertEqual(re.findall(r"^sha256=(.+)$", source, re.M), [DIGEST])

    def test_outer_timeout_failure_propagates_without_install(self):
        self.tool("timeout", '#!/bin/sh\nexit 124\n')
        result = self.run_install()
        self.assert_failed_closed(result)
        self.assertEqual(result.returncode, 124)
        self.assertIn("download failed (exit 124)", result.stderr)
        self.assertFalse((self.root / "calls").exists())

    def test_workflow_installs_before_untrusted_checkout_and_guards_runtime(self):
        workflow = yaml.load((ROOT / ".github/workflows/sbom.yml").read_text(), Loader=yaml.BaseLoader)
        steps = workflow["jobs"]["image"]["steps"]
        install = next(s for s in steps if s.get("id") == "trivy")
        self.assertEqual(install["run"], "bash scripts/install-trivy.sh")
        self.assertNotIn("if", install)
        self.assertNotIn("continue-on-error", install)
        checkout = next(s for s in steps if s.get("with", {}).get("ref") == "${{ inputs.ref }}")
        self.assertLess(steps.index(install), steps.index(checkout))
        scans = [s for s in steps if s.get("uses", "").startswith("aquasecurity/trivy-action@")]
        self.assertEqual(len(scans), 4)
        for scan in scans:
            self.assertGreater(steps.index(scan), steps.index(install))
            self.assertEqual(scan["uses"], "aquasecurity/trivy-action@ed142fd0673e97e23eac54620cfb913e5ce36c25")
            self.assertEqual(scan["with"]["version"], "v0.69.3")
            self.assertEqual(scan["with"]["skip-setup-trivy"], "true")
            self.assertEqual(scan["with"]["cache"], "false")
        runtime = scans[-1]
        self.assertEqual(runtime["if"], "${{ !cancelled() && steps.trivy.outcome == 'success' }}")
        for scan in scans[:-1]:
            self.assertNotIn("if", scan, "default success condition refuses a failed install")
        for gate in scans[2:]:
            self.assertEqual(gate["with"]["severity"], "HIGH,CRITICAL")
            self.assertEqual(gate["with"]["ignore-unfixed"], "false")
            self.assertEqual(gate["with"]["exit-code"], "1")


if __name__ == "__main__":
    unittest.main()
