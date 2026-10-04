"""Offline regressions for the opt-in single-core check workflow."""

import contextlib
import io
import os
from pathlib import Path
import re
import tempfile
import textwrap
import unittest
from unittest import mock


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/check.yml"


MARKER = "      - name: Pin native tests to one CPU (loaded-runner validation)\n"
# A step ends at the next list item or comment at step indent; blank lines
# inside its heredoc are indented deeper, so they never match.
STEP_END = re.compile(r"\n\n      (?:- |#)")


def pin_steps():
    """Every copy of the pin step, one per native-test lane."""
    return [STEP_END.split(part, 1)[0] for part in WORKFLOW.read_text().split(MARKER)[1:]]


def validation_script():
    step = pin_steps()[0]
    assert "if: github.event_name == 'workflow_dispatch' && inputs.single_core_tests" in step
    return textwrap.dedent(step.split("python3 - <<'PY'\n", 1)[1].rsplit("          PY", 1)[0])


class SingleCoreValidationTests(unittest.TestCase):
    def run_script(self, threads=None):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            env_file = Path(directory) / "github-env"
            env = {"GITHUB_ENV": str(env_file), "GITHUB_SHA": "fixture-revision"}
            if threads is not None:
                env["RUST_TEST_THREADS"] = threads
            output = io.StringIO()
            with (
                mock.patch.dict(os.environ, env, clear=True),
                mock.patch("os.sched_getaffinity", return_value={2, 4, 6, 8}),
                mock.patch("shutil.which", return_value="/usr/bin/taskset"),
                mock.patch("subprocess.run") as run,
                mock.patch("subprocess.check_output", side_effect=(
                    ["2\n", "host: x86_64-unknown-linux-gnu\n"] if threads is None
                    else ["host: x86_64-unknown-linux-gnu\n"]
                )) as check_output,
                mock.patch.object(Path, "read_text", return_value="fixture load"),
                contextlib.redirect_stdout(output),
            ):
                exec(compile(SCRIPT, str(WORKFLOW), "exec"), {})
            return env_file.read_text(), output.getvalue(), run.call_args_list, check_output.call_args_list

    def test_preserves_unpinned_rust_parallelism_not_just_affinity_count(self):
        env, output, runs, checks = self.run_script()
        self.assertEqual(env, "RUST_TEST_THREADS=2\n"
                         "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER=/usr/bin/taskset -c 2\n")
        self.assertEqual(runs[0].args[0][:4], ["rustc", "-", "--crate-name", "test_parallelism"])
        self.assertIn("std::thread::available_parallelism()", runs[0].kwargs["input"])
        self.assertTrue(runs[0].kwargs["check"])
        self.assertEqual(checks[0].args[0], [runs[0].args[0][-1]])
        self.assertEqual(runs[1], mock.call(["/usr/bin/taskset", "-c", "2", "true"], check=True))
        self.assertEqual(checks[1], mock.call(["rustc", "-vV"], text=True))
        self.assertIn("libtest threads: 2 (unpinned Rust available_parallelism)", output)
        self.assertIn("unpinned affinity: [2, 4, 6, 8]", output)
        self.assertIn("fixture-revision", output)
        self.assertIn("cpu pressure:", output)

    def test_honors_explicit_harness_threads_and_records_cli_override(self):
        env, output, runs, checks = self.run_script("7")
        self.assertEqual(env, "RUST_TEST_THREADS=7\n"
                         "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER=/usr/bin/taskset -c 2\n")
        self.assertEqual(runs, [mock.call(["/usr/bin/taskset", "-c", "2", "true"], check=True)])
        self.assertEqual(checks, [mock.call(["rustc", "-vV"], text=True)])
        self.assertIn("libtest threads: 7 (explicit RUST_TEST_THREADS)", output)
        self.assertIn("explicit --test-threads still wins", output)

    def test_invalid_thread_count_cannot_claim_concurrent_validation(self):
        for threads in ["0", "-1", "not-a-count", "2\n"]:
            with self.subTest(threads=threads), self.assertRaisesRegex(SystemExit, "positive RUST_TEST_THREADS"):
                self.run_script(threads)

    def test_every_native_test_lane_carries_the_same_pin(self):
        # The opt-in flake validation must cover every lane that runs native
        # test binaries: rust-tests and both database lanes, and nothing else.
        steps = pin_steps()
        self.assertEqual(len(steps), 3)
        self.assertEqual(len(set(steps)), 1, "the lanes' pin steps drifted apart")

    def test_missing_taskset_fails_instead_of_claiming_load_validation(self):
        with mock.patch("shutil.which", return_value=None):
            with self.assertRaisesRegex(SystemExit, "requires taskset"):
                exec(compile(SCRIPT, str(WORKFLOW), "exec"), {})


SCRIPT = validation_script()

if __name__ == "__main__":
    unittest.main()
