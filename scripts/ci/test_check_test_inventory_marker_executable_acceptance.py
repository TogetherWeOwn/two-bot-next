"""Protected acceptance tests: lane-presence markers must be executable.

The inventory HARD gate (``check-test-inventory.py``) requires four
lane-presence markers -- the fuzz build, the workspace lib/bins unit run,
the workspace doc-test run and an ``--all-targets`` build. These tests
pin, per marker, that the marker counts only as executable ``run:``
text:

- a marker that appears only in a comment (a YAML comment line, a
  trailing comment on an unrelated command, or a shell comment inside a
  ``run:`` block), only in a step or job ``name:``, only in an ``env:``
  value, or only in an ``echo``/``printf`` print MUST fail the gate with
  that marker's missing-lane error;
- the same marker in a real ``run:`` command (inline or block scalar,
  in ``check.yml`` or, where the gate already allows it, in
  ``nightly.yml``) passes;
- removing the real command while keeping the mention flips the
  verdict from pass to fail.

Every fixture carries the other markers as real commands plus the
blanket ``--test '*'`` run and a direct route for the ignored target,
so the only verdict that varies is the isolated marker's. Fixtures
build a miniature repository; no Cargo build, network or workflow
dispatch is involved.
"""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = Path(__file__).with_name("check-test-inventory.py")

import importlib.util

SPEC = importlib.util.spec_from_file_location("inventory_gate_marker_acceptance", SCRIPT)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)


# The four markers under test, as literal spec strings, each with the
# real command step a fixture uses and a fragment of the gate's
# missing-lane error for it.
MARKERS = {
    "fuzz_build": {
        "marker": "cargo +nightly-2026-10-01 fuzz build",
        "step": "      - run: cargo +nightly-2026-10-01 fuzz build\n",
        "error": "fuzz-compile job does not build",
    },
    "workspace_lib_bins": {
        "marker": "cargo test --workspace --lib --bins",
        "step": "      - run: cargo test --workspace --lib --bins --locked\n",
        "error": "workspace lib/bins unit run",
    },
    "workspace_doc": {
        "marker": "cargo test --workspace --doc",
        "step": "      - run: cargo test --workspace --doc --locked\n",
        "error": "workspace doc-test run",
    },
    "all_targets": {
        "marker": "--all-targets",
        "step": "      - run: cargo clippy --workspace --all-targets --locked\n",
        "error": "all-targets build",
    },
}

# Steps every fixture keeps so only the isolated marker's verdict
# varies: the blanket run covers the unignored target and the direct
# route covers the ignored one.
ALWAYS_STEPS = (
    "      - run: cargo test --workspace --test '*' --locked\n"
    "      - run: cargo test -p my-crate --test db_store --locked -- --ignored\n"
)


def mention_step(marker, kind):
    """A step whose text carries ``marker`` without executing it."""
    if kind == "yaml_comment":
        return f"      # lane command: {marker}\n      - run: true\n"
    if kind == "trailing_comment":
        return f"      - run: true # {marker}\n"
    if kind == "shell_comment_in_run_block":
        return f"      - run: |\n          # {marker}\n          true\n"
    if kind == "step_name":
        return f"      - name: Run {marker}\n        run: true\n"
    if kind == "step_env":
        return f'      - run: true\n        env:\n          CMD: "{marker}"\n'
    if kind == "echo":
        return f'      - run: echo "{marker}"\n'
    if kind == "printf_in_run_block":
        return "      - run: |\n          true\n" f"          printf '%s\\n' \"{marker}\"\n"
    raise AssertionError(f"unknown mention kind {kind!r}")


MENTION_KINDS = (
    "yaml_comment",
    "trailing_comment",
    "shell_comment_in_run_block",
    "step_name",
    "step_env",
    "echo",
    "printf_in_run_block",
)


def check_steps(replacements=None):
    """All marker steps as real commands, except any replaced marker."""
    replacements = replacements or {}
    steps = ""
    for key, spec in MARKERS.items():
        steps += replacements.get(key, spec["step"])
    return steps + ALWAYS_STEPS


def workflows(check_step_text, nightly_step_text="", job_name_line=""):
    check_yml = (
        "jobs:\n  check:\n"
        + job_name_line
        + "    steps:\n"
        + check_step_text
    )
    nightly_yml = (
        "jobs:\n  sweep:\n    steps:\n"
        "      - run: cargo test --workspace --locked\n"
        + nightly_step_text
    )
    return check_yml, nightly_yml


def make_root(check_yml, nightly_yml):
    tmp = tempfile.TemporaryDirectory()
    root = Path(tmp.name)
    files = {
        ".github/workflows/check.yml": check_yml,
        ".github/workflows/nightly.yml": nightly_yml,
        "fuzz/Cargo.toml": '[[bin]]\nname = "wiggle"\npath = "fuzz_targets/wiggle.rs"\n',
        "fuzz/fuzz_targets/wiggle.rs": "// target\n",
        "crates/my-crate/tests/plain.rs": "#[test]\nfn it() {}\n",
        "crates/my-crate/tests/db_store.rs": "#[ignore]\n#[test]\nfn it() {}\n",
    }
    for path, content in files.items():
        full = root / path
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(content)
    return tmp, root


def check_fixture(check_yml, nightly_yml):
    tmp, root = make_root(check_yml, nightly_yml)
    try:
        return gate.check(root)
    finally:
        tmp.cleanup()


class MentionOnlyMarkerFailsTests(unittest.TestCase):
    """Per marker: a non-executing mention is not lane presence."""

    def assert_marker_missing(self, key, check_yml, nightly_yml):
        errors = check_fixture(check_yml, nightly_yml)
        self.assertEqual(
            len(errors), 1,
            f"{key}: expected exactly its missing-lane error, got: {errors}")
        self.assertIn(MARKERS[key]["error"], errors[0])

    def test_mention_only_marker_fails_per_marker_and_kind(self):
        for key, spec in MARKERS.items():
            for kind in MENTION_KINDS:
                with self.subTest(marker=key, kind=kind):
                    steps = check_steps({key: mention_step(spec["marker"], kind)})
                    check_yml, nightly_yml = workflows(steps)
                    # Guard the fixture itself: the marker text is present
                    # in the raw workflow, so only executable-text
                    # matching can fail this fixture.
                    self.assertIn(spec["marker"], check_yml)
                    self.assert_marker_missing(key, check_yml, nightly_yml)

    def test_job_name_only_marker_fails_per_marker(self):
        for key, spec in MARKERS.items():
            with self.subTest(marker=key, kind="job_name"):
                steps = check_steps({key: "      - run: true\n"})
                check_yml, nightly_yml = workflows(
                    steps, job_name_line=f"    name: Lane for {spec['marker']}\n")
                self.assertIn(spec["marker"], check_yml)
                self.assert_marker_missing(key, check_yml, nightly_yml)

    def test_mention_in_nightly_does_not_cover_a_marker(self):
        # lib/bins, doc and --all-targets may live in either workflow,
        # but a mention in nightly is still only a mention.
        for key in ("workspace_lib_bins", "workspace_doc", "all_targets"):
            with self.subTest(marker=key, kind="nightly_echo"):
                spec = MARKERS[key]
                steps = check_steps({key: "      - run: true\n"})
                check_yml, nightly_yml = workflows(
                    steps, nightly_step_text=mention_step(spec["marker"], "echo"))
                self.assert_marker_missing(key, check_yml, nightly_yml)

    def test_removing_the_real_command_keeps_the_mention_and_flips(self):
        # Same fixture, same display name: only the executable command
        # is removed, so every marker's verdict must flip to missing.
        for key, spec in MARKERS.items():
            with self.subTest(marker=key):
                named_real = (
                    f"      - name: Run {spec['marker']}\n"
                    + spec["step"].replace("      - run:", "        run:", 1))
                check_yml, nightly_yml = workflows(check_steps({key: named_real}))
                self.assertEqual(check_fixture(check_yml, nightly_yml), [])

                named_mention = (
                    f"      - name: Run {spec['marker']}\n        run: true\n")
                check_yml, nightly_yml = workflows(check_steps({key: named_mention}))
                self.assert_marker_missing(key, check_yml, nightly_yml)


class ExecutableMarkerPassesTests(unittest.TestCase):
    """Positive controls: real ``run:`` commands stay green throughout."""

    def test_all_markers_as_inline_run_commands_pass(self):
        check_yml, nightly_yml = workflows(check_steps())
        self.assertEqual(check_fixture(check_yml, nightly_yml), [])

    def test_all_markers_inside_one_block_run_pass(self):
        block = "      - run: |\n" + "".join(
            "          " + spec["step"].split("run: ", 1)[1]
            for spec in MARKERS.values())
        steps = block + ALWAYS_STEPS
        check_yml, nightly_yml = workflows(steps)
        self.assertEqual(check_fixture(check_yml, nightly_yml), [])

    def test_lib_doc_and_all_targets_may_live_in_nightly(self):
        # Existing file scope is preserved: these three markers count
        # in either workflow, as long as the command is executable.
        nightly_steps = "".join(
            MARKERS[key]["step"]
            for key in ("workspace_lib_bins", "workspace_doc", "all_targets"))
        steps = check_steps({
            key: "      - run: true\n"
            for key in ("workspace_lib_bins", "workspace_doc", "all_targets")
        })
        check_yml, nightly_yml = workflows(steps, nightly_step_text=nightly_steps)
        self.assertEqual(check_fixture(check_yml, nightly_yml), [])

    def test_fuzz_build_only_in_nightly_still_fails(self):
        # Existing file scope is preserved: the fuzz build is required
        # in check.yml today; an executable nightly command alone does
        # not satisfy it.
        steps = check_steps({"fuzz_build": "      - run: true\n"})
        check_yml, nightly_yml = workflows(
            steps, nightly_step_text=MARKERS["fuzz_build"]["step"])
        errors = check_fixture(check_yml, nightly_yml)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn(MARKERS["fuzz_build"]["error"], errors[0])

    def test_mention_alongside_the_real_command_passes(self):
        steps = check_steps() + mention_step(
            MARKERS["workspace_lib_bins"]["marker"], "echo")
        check_yml, nightly_yml = workflows(steps)
        self.assertEqual(check_fixture(check_yml, nightly_yml), [])

    def test_real_tree_still_passes(self):
        # The real workflows carry every marker as a real command;
        # tightening to executable text must not orphan any of them.
        self.assertEqual(gate.check(ROOT), [])


class CliTests(unittest.TestCase):
    def run_cli(self, check_yml, nightly_yml):
        tmp, root = make_root(check_yml, nightly_yml)
        try:
            return subprocess.run(
                [sys.executable, str(SCRIPT), "--root", str(root)],
                capture_output=True, text=True, timeout=60)
        finally:
            tmp.cleanup()

    def test_cli_fails_and_names_the_echo_only_marker(self):
        key = "workspace_lib_bins"
        steps = check_steps({key: mention_step(MARKERS[key]["marker"], "echo")})
        check_yml, nightly_yml = workflows(steps)
        result = self.run_cli(check_yml, nightly_yml)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(MARKERS[key]["error"], result.stderr)

    def test_cli_passes_when_every_marker_is_a_real_command(self):
        check_yml, nightly_yml = workflows(check_steps())
        result = self.run_cli(check_yml, nightly_yml)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("every target is routed", result.stdout)


if __name__ == "__main__":
    unittest.main()
