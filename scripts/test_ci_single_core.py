"""Offline regressions for the opt-in single-core check workflow."""

import contextlib
import io
import os
from pathlib import Path
import tempfile
import textwrap
import unittest
from unittest import mock


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/check.yml"


def validation_script():
    workflow = WORKFLOW.read_text()
    marker = "      - name: Pin native tests to one CPU (loaded-runner validation)\n"
    step = workflow.split(marker, 1)[1].split("      # Restore on every run;", 1)[0]
    assert "if: github.event_name == 'workflow_dispatch' && inputs.single_core_tests" in step
    return textwrap.dedent(step.split("python3 - <<'PY'\n", 1)[1].rsplit("          PY", 1)[0])


class SingleCoreValidationTests(unittest.TestCase):
    def test_pins_test_runner_not_compilation_and_records_revision(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            env_file = Path(directory) / "github-env"
            env = {"GITHUB_ENV": str(env_file), "GITHUB_SHA": "fixture-revision"}
            output = io.StringIO()
            with (
                mock.patch.dict(os.environ, env),
                mock.patch("os.sched_getaffinity", return_value={2, 4}),
                mock.patch("shutil.which", return_value="/usr/bin/taskset"),
                mock.patch("subprocess.run") as run,
                mock.patch("subprocess.check_output", return_value="host: x86_64-unknown-linux-gnu\n") as version,
                mock.patch.object(Path, "read_text", return_value="fixture load"),
                contextlib.redirect_stdout(output),
            ):
                # Extract before mocking read_text so the workflow itself is real.
                exec(compile(SCRIPT, str(WORKFLOW), "exec"), {})
            run.assert_called_once_with(["/usr/bin/taskset", "-c", "2", "true"], check=True)
            version.assert_called_once_with(["rustc", "-vV"], text=True)
            self.assertEqual(
                env_file.read_text(),
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER=/usr/bin/taskset -c 2\n",
            )
            self.assertIn("fixture-revision", output.getvalue())
            self.assertIn("cpu pressure:", output.getvalue())

    def test_missing_taskset_fails_instead_of_claiming_load_validation(self):
        with mock.patch("shutil.which", return_value=None):
            with self.assertRaisesRegex(SystemExit, "requires taskset"):
                exec(compile(SCRIPT, str(WORKFLOW), "exec"), {})


SCRIPT = validation_script()

if __name__ == "__main__":
    unittest.main()
