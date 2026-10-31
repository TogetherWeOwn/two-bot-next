"""Every cargo-deny step must scan the whole workspace.

The root Cargo.toml is a package and the workspace. Without `--workspace`,
cargo-deny's graph holds only the root crate, so advisories, licenses, bans
and sources all pass on a crate with no dependencies.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github/workflows"
DENY_USES = re.compile(r"^(?P<indent>[ ]*)(?:-[ ]+)?uses:[ ]*EmbarkStudios/cargo-deny-action@")
ARGUMENTS = re.compile(r"^[ ]*arguments:[ ]*(?P<value>.*)$")


def deny_step_arguments(text):
    """Return the `with.arguments` value of each cargo-deny step, or None."""
    lines = text.splitlines()
    found = []
    for index, line in enumerate(lines):
        match = DENY_USES.match(line)
        if match is None:
            continue
        step_col = len(match.group("indent"))
        value = None
        for follow in lines[index + 1 :]:
            if not follow.strip() or follow.lstrip().startswith("#"):
                continue
            indent = len(follow) - len(follow.lstrip(" "))
            if indent < step_col or (indent == step_col and follow.lstrip().startswith("- ")):
                break
            arguments = ARGUMENTS.match(follow)
            if arguments:
                value = arguments.group("value").split(" #", 1)[0].strip()
        found.append(value)
    return found


def unscoped_steps(text):
    return [args for args in deny_step_arguments(text) if "--workspace" not in (args or "").split()]


class CargoDenyScopeTests(unittest.TestCase):
    def test_root_manifest_is_package_and_workspace(self):
        manifest = (ROOT / "Cargo.toml").read_text()
        self.assertRegex(manifest, r"(?m)^\[package\]$")
        self.assertRegex(manifest, r"(?m)^\[workspace\]$")

    def test_every_cargo_deny_step_scans_the_workspace(self):
        seen = {}
        for workflow in sorted(WORKFLOWS.glob("*.y*ml")):
            text = workflow.read_text()
            steps = deny_step_arguments(text)
            if steps:
                seen[workflow.name] = steps
                self.assertEqual(unscoped_steps(text), [], workflow.name)
        self.assertIn("check.yml", seen)
        self.assertIn("nightly.yml", seen)

    def test_removing_the_flag_is_caught(self):
        for name in ("check.yml", "nightly.yml"):
            text = (WORKFLOWS / name).read_text()
            mutated = text.replace("arguments: --workspace --all-features", "arguments: --all-features")
            self.assertNotEqual(mutated, text, name)
            self.assertTrue(unscoped_steps(mutated), name)
            dropped = re.sub(r"(?m)^[ ]*arguments: --workspace --all-features\n", "", text)
            self.assertEqual(unscoped_steps(dropped), [None], name)


if __name__ == "__main__":
    unittest.main()
