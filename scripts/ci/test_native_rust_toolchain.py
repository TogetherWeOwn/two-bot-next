"""Hermetic behavior tests for .github/scripts/install-rust-toolchain.sh.

`rustup`, `cargo` and `rustc` are shell shims on a PATH that holds nothing
else, so no test touches a real toolchain or the network.
"""

import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / ".github/scripts/install-rust-toolchain.sh"
BASH = shutil.which("bash")

SHIM = """#!{bash}
printf '%s %s\\n' "${{0##*/}}" "$*" >> "$FAKE_CALLS"
if [ "${{0##*/}}" = rustup ]; then
  printf 'permit=%s\\n' "${{RUSTUP_PERMIT_COPY_RENAME:-}}" >> "$FAKE_CALLS"
  case "$1" in
    toolchain)
      [ -z "${{FAKE_FAIL_INSTALL:-}}" ] || exit 1
      count=0
      [ ! -f "$FAKE_COUNT" ] || read -r count < "$FAKE_COUNT"
      count=$((count + 1))
      printf '%s\n' "$count" > "$FAKE_COUNT"
      if [ "$count" -le "${{FAKE_FAILURES:-0}}" ]; then
        printf '%s\n' "$FAKE_DIAGNOSTIC" >&2
        exit 42
      fi ;;
    default) [ -z "${{FAKE_FAIL_DEFAULT:-}}" ] || exit 1 ;;
  esac
fi
if [ "${{0##*/}}" = rustc ] && [ -n "${{FAKE_FAIL_RUSTC:-}}" ]; then exit 1; fi
exit 0
"""


class NativeRustToolchainTest(unittest.TestCase):
    def run_script(self, tools=("rustup", "cargo", "rustc"), env=None, unset=(), **flags):
        with tempfile.TemporaryDirectory(prefix="native-rust-") as tmp:
            tmp_path = Path(tmp)
            bin_dir = tmp_path / "bin"
            bin_dir.mkdir()
            for tool in tools:
                shim = bin_dir / tool
                shim.write_text(SHIM.format(bash=BASH))
                shim.chmod(shim.stat().st_mode | stat.S_IXUSR)
            for tool in ("mktemp", "rm", "grep", "tee"):
                (bin_dir / tool).symlink_to(shutil.which(tool))
            sleep = bin_dir / "sleep"
            sleep.write_text(f"#!{BASH}\nprintf 'sleep %s\\n' \"$*\" >> \"$FAKE_CALLS\"\n")
            sleep.chmod(sleep.stat().st_mode | stat.S_IXUSR)
            calls = tmp_path / "calls.txt"
            github_env = tmp_path / "github_env"
            calls.write_text("")
            github_env.write_text("")
            run_env = {
                "PATH": str(bin_dir),
                "HOME": tmp,
                "CARGO_HOME": "/fixture/cargo",
                "FAKE_CALLS": str(calls),
                "FAKE_COUNT": str(tmp_path / "count"),
                "GITHUB_ENV": str(github_env),
                "RUST_TOOLCHAIN": "1.99.1",
                "RUST_COMPONENTS": "",
            }
            run_env.update(env or {})
            for name in unset:
                run_env.pop(name, None)
            run_env.update({key: "1" for key, on in flags.items() if on})
            proc = subprocess.run(
                [BASH, str(SCRIPT)], env=run_env, capture_output=True, text=True, timeout=30, cwd=tmp,
            )
            return (
                proc,
                [line for line in calls.read_text().splitlines() if not line.startswith("permit=")],
                calls.read_text(),
                github_env.read_text().splitlines(),
            )

    def test_installs_pin_components_profile_default_and_version(self):
        proc, calls, raw, _ = self.run_script(env={"RUST_COMPONENTS": "rustfmt,clippy"})
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(calls, [
            "rustup toolchain install 1.99.1 --component rustfmt --component clippy"
            " --profile minimal --no-self-update --force-non-host",
            "rustup default 1.99.1 --force-non-host",
            "rustc +1.99.1 --version --verbose",
        ])
        self.assertIn("permit=1", raw)

    def test_empty_components_request_none(self):
        proc, calls, _, _ = self.run_script()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertNotIn("--component", calls[0])
        self.assertIn("--profile minimal", calls[0])

    def test_spaced_component_list_is_split(self):
        proc, calls, _, _ = self.run_script(env={"RUST_COMPONENTS": "rust-src, rustfmt"})
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("--component rust-src --component rustfmt --profile", calls[0])

    def test_nightly_pin_is_accepted(self):
        proc, calls, _, _ = self.run_script(env={"RUST_TOOLCHAIN": "nightly-2026-10-01"})
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("rustup default nightly-2026-10-01 --force-non-host", calls)

    def test_inexact_pins_fail_before_any_rustup_call(self):
        for pin in ("", "stable", "nightly", "1.99", "1.99.1.2", "1.99.1 ", "nightly-2026-10", "beta", "$(id)"):
            with self.subTest(pin=pin):
                proc, calls, _, github_env = self.run_script(env={"RUST_TOOLCHAIN": pin})
                self.assertNotEqual(proc.returncode, 0)
                self.assertIn("exact Rust toolchain", proc.stderr)
                self.assertFalse([c for c in calls if c.startswith("rustup")], calls)
                self.assertEqual(github_env, [])
        proc, calls, _, _ = self.run_script(unset=("RUST_TOOLCHAIN",))
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse([c for c in calls if c.startswith("rustup")])

    def test_install_failure_fails_without_default_or_env(self):
        proc, calls, _, github_env = self.run_script(FAKE_FAIL_INSTALL=True)
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(len([c for c in calls if c.startswith("rustup toolchain install")]), 1)
        self.assertFalse([c for c in calls if c.startswith("rustup default")])
        self.assertEqual(github_env, [])

    def test_checksum_retry_recovery_and_exhaustion(self):
        diagnostic = ("checksum failed for archive\n"
                      "this is likely due to an ongoing update of the official release server")
        for failures in (2, 5):
            with self.subTest(failures=failures):
                proc, calls, _, github_env = self.run_script(env={
                    "FAKE_FAILURES": str(failures), "FAKE_DIAGNOSTIC": diagnostic,
                    "RUST_COMPONENTS": "rustfmt,clippy",
                })
                installs = [c for c in calls if c.startswith("rustup toolchain install")]
                self.assertEqual(len(installs), min(failures + 1, 5))
                self.assertTrue(all(c == installs[0] for c in installs))
                self.assertEqual(calls.count("sleep 30"), min(failures, 4))
                self.assertEqual(proc.stdout.count(diagnostic), failures)
                self.assertEqual(proc.stderr.count("Retrying rustup"), min(failures, 4))
                if failures == 5:
                    self.assertEqual(proc.returncode, 42)
                    self.assertFalse(any(c.startswith("rustup default") for c in calls))
                    self.assertEqual(github_env, [])
                else:
                    self.assertEqual(proc.returncode, 0, proc.stderr)
                    self.assertTrue(any(c.startswith("rustup default") for c in calls))
                    self.assertTrue(github_env)

    def test_other_errors_do_not_retry(self):
        for diagnostic in ("unrelated failure", "checksum failed for archive",
                           "this is likely due to an ongoing update of the official release server"):
            with self.subTest(diagnostic=diagnostic):
                proc, calls, _, github_env = self.run_script(env={
                    "FAKE_FAILURES": "5", "FAKE_DIAGNOSTIC": diagnostic,
                })
                self.assertEqual(proc.returncode, 42)
                self.assertIn(diagnostic, proc.stdout)
                self.assertEqual(len([c for c in calls if c.startswith("rustup toolchain install")]), 1)
                self.assertNotIn("sleep 30", calls)
                self.assertFalse(any(c.startswith("rustup default") for c in calls))
                self.assertEqual(github_env, [])

    def test_default_and_version_failures_fail(self):
        for flag in ("FAKE_FAIL_DEFAULT", "FAKE_FAIL_RUSTC"):
            with self.subTest(flag=flag):
                proc, _, _, github_env = self.run_script(**{flag: True})
                self.assertNotEqual(proc.returncode, 0)
                self.assertEqual(github_env, [])

    def test_missing_tools_fail_without_installing(self):
        for missing in ("rustup", "cargo", "rustc"):
            with self.subTest(missing=missing):
                tools = tuple(t for t in ("rustup", "cargo", "rustc") if t != missing)
                proc, calls, _, github_env = self.run_script(tools=tools)
                self.assertNotEqual(proc.returncode, 0)
                self.assertFalse([c for c in calls if c.startswith("rustup toolchain")], calls)
                self.assertEqual(github_env, [])

    def test_cargo_env_defaults_are_fixed_strings(self):
        proc, _, _, github_env = self.run_script(unset=("CARGO_INCREMENTAL", "CARGO_TERM_COLOR"))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(github_env, ["CARGO_HOME=/fixture/cargo", "CARGO_INCREMENTAL=0", "CARGO_TERM_COLOR=always"])

    def test_existing_cargo_env_is_not_overridden_even_when_empty(self):
        proc, _, _, github_env = self.run_script(
            env={"CARGO_INCREMENTAL": "1", "CARGO_TERM_COLOR": ""})
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(github_env, ["CARGO_HOME=/fixture/cargo"])
        proc, _, _, github_env = self.run_script(env={"CARGO_INCREMENTAL": "1"})
        self.assertEqual(github_env, ["CARGO_HOME=/fixture/cargo", "CARGO_TERM_COLOR=always"])

    def test_cargo_home_default_custom_and_line_boundary(self):
        for value in ("/fixture/custom cargo", ""):
            proc, _, _, env = self.run_script(env={"CARGO_HOME": value})
            self.assertEqual(proc.returncode, 0, proc.stderr)
            if value:
                self.assertEqual(env[0], "CARGO_HOME=" + value)
            else:
                self.assertTrue(env[0].endswith("/.cargo"))
        for value in ("/home/a\nOTHER=value", "/home/a\rOTHER=value"):
            proc, calls, _, env = self.run_script(env={"CARGO_HOME": value})
            self.assertNotEqual(proc.returncode, 0)
            self.assertEqual(calls, [])
            self.assertEqual(env, [])

    def test_script_has_no_network_bootstrap_or_output_writes(self):
        text = SCRIPT.read_text()
        self.assertNotRegex(text, r"\b(curl|wget|sh\.rustup\.rs)\b")
        self.assertNotIn("GITHUB_OUTPUT", text)
        self.assertNotIn("GITHUB_PATH", text)
        self.assertNotIn("GITHUB_STATE", text)

    def test_all_installers_keep_their_components_and_checkout_order(self):
        # Unlisted workflows retain the action until their own migrations.
        expected = {
            "check.yml": {
                "check": "rustfmt, clippy", "rust-tests": "rustfmt, clippy", "coverage-plan": "",
                "coverage-shard": "rustfmt, clippy", "ignored-db-stores": "rustfmt, clippy",
                "ignored-db-runtime": "rustfmt, clippy", "moderation-db": "clippy", "self-role-store": "",
                "community-db": "clippy", "feeds-db": "clippy", "tickets-postgres": "", "fuzz-compile": "rust-src",
            },
            "helix-token-fuzz-smoke.yml": {"helix-token-smoke": "rust-src"},
            "vote-kick-fuzz-smoke.yml": {"vote-kick-smoke": "rust-src"},
            "nightly.yml": {job: "" for job in ("sweep-plan", "sweep-partition", "sweep-guarded", "sweep-docs")},
            "pipeline-benchmark.yml": {"benchmark": ""},
            "reusable-rust-nextest.yml": {"nextest": ""},
            "help-embed-synthetic.yml": {"help-embed-smoke": ""},
        }
        actual = {}
        for path in (ROOT / ".github/workflows").glob("*.yml"):
            doc = yaml.safe_load(path.read_text())
            for job_id, job in doc["jobs"].items():
                for index, step in enumerate(job.get("steps", [])):
                    if path.name in expected:
                        self.assertNotIn("dtolnay/rust-toolchain@", step.get("uses", ""))
                    if step.get("run") != "bash .github/scripts/install-rust-toolchain.sh":
                        continue
                    with self.subTest(workflow=path.name, job=job_id):
                        self.assertNotIn(job_id, actual.get(path.name, {}))
                        actual.setdefault(path.name, {})[job_id] = step["env"]["RUST_COMPONENTS"]
                        self.assertEqual(step["shell"], "bash")
                        self.assertNotIn("with", step)
                        if path.name in {"helix-token-fuzz-smoke.yml", "vote-kick-fuzz-smoke.yml"}:
                            self.assertEqual(step["env"]["RUST_TOOLCHAIN"], "nightly-2026-10-01")
                        self.assertTrue(any(s.get("uses", "").startswith("actions/checkout@")
                                            for s in job["steps"][:index]))
        self.assertEqual(actual, expected)

    def test_rust_action_inventory_is_closed(self):
        # Preserve the pinned one-off writer's historical provenance without
        # permitting another Rust action caller or an unreviewed pin change.
        expected = [("configuration-reference.yml", "configuration-reference",
                     "dtolnay/rust-toolchain@683b588c487a69d01f6c6ddf40f036ff682cc2c8")]

        def inventory(workflows):
            return sorted((name, job_id, step["uses"])
                          for name, doc in workflows.items()
                          for job_id, job in (doc.get("jobs") or {}).items()
                          for step in job.get("steps", [])
                          if str(step.get("uses", "")).lower().startswith("dtolnay/rust-toolchain@"))

        workflows = {path.name: yaml.safe_load(path.read_text()) or {}
                     for pattern in ("*.yml", "*.yaml")
                     for path in (ROOT / ".github/workflows").glob(pattern)}
        self.assertEqual(inventory(workflows), expected)
        extra = {"new.yml": {"jobs": {"new": {"steps": [{"uses": expected[0][2]}]}}}}
        self.assertNotEqual(inventory(workflows | extra), expected)
        self.assertNotEqual(inventory({}), expected)

    def test_no_workflow_consumes_installer_outputs(self):
        # The action exposed cachekey/name/... outputs; the native installer
        # has none, so no step may read outputs from an installer step.
        workflows = sorted((ROOT / ".github/workflows").glob("*.yml"))
        self.assertTrue(workflows)
        for path in workflows:
            doc = yaml.safe_load(path.read_text()) or {}
            for job_id, job in (doc.get("jobs") or {}).items():
                steps = job.get("steps") or []
                ids = {
                    step["id"] for step in steps
                    if "install-rust-toolchain.sh" in str(step.get("run", "")) and "id" in step
                }
                if not ids:
                    continue
                body = yaml.safe_dump(job)
                for step_id in ids:
                    with self.subTest(workflow=path.name, job=job_id, step=step_id):
                        refs = re.findall(rf"steps\.{re.escape(step_id)}\.outputs\.\w+", body)
                        self.assertEqual(refs, [])


if __name__ == "__main__":
    unittest.main()
