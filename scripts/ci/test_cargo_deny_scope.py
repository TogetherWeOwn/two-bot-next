"""Every cargo-deny step must scan the whole workspace.

The root Cargo.toml is a package and the workspace. Without `--workspace`,
cargo-deny's graph holds only the root crate, so advisories, licenses, bans
and sources all pass on a crate with no dependencies.
"""

from pathlib import Path
import re
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github/workflows"
DENY_ACTION = "EmbarkStudios/cargo-deny-action@"


def deny_step_arguments(text):
    """Return the `with.arguments` input of each cargo-deny step, or None.

    Only `with` supplies an action input; the same key under `env` does not.
    """
    workflow = yaml.safe_load(text) or {}
    found = []
    for job in (workflow.get("jobs") or {}).values():
        for step in job.get("steps") or []:
            if not str(step.get("uses", "")).startswith(DENY_ACTION):
                continue
            arguments = (step.get("with") or {}).get("arguments")
            found.append(None if arguments is None else str(arguments))
    return found


def unscoped_steps(text):
    return [args for args in deny_step_arguments(text) if "--workspace" not in (args or "").split()]


def deny_step(with_block="", env_block=""):
    return (
        "jobs:\n"
        "  check:\n"
        "    steps:\n"
        "      - uses: EmbarkStudios/cargo-deny-action@0000000000000000000000000000000000000000\n"
        f"{env_block}{with_block}"
    )


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
            mutated = re.sub(r"(?m)^([ ]*arguments:.*?)--workspace[ ]*", r"\1", text)
            self.assertNotEqual(mutated, text, name)
            self.assertTrue(unscoped_steps(mutated), name)
            dropped = re.sub(r"(?m)^[ ]*arguments:.*--workspace.*\n", "", text)
            self.assertNotEqual(dropped, text, name)
            self.assertEqual(unscoped_steps(dropped), [None], name)

    def test_flag_outside_with_is_caught(self):
        env_only = deny_step(env_block="        env:\n          arguments: --workspace --all-features\n")
        self.assertEqual(unscoped_steps(env_only), [None])
        both = deny_step(
            env_block="        env:\n          arguments: --workspace\n",
            with_block="        with:\n          arguments: --all-features\n",
        )
        self.assertEqual(unscoped_steps(both), ["--all-features"])
        scoped = deny_step(with_block="        with:\n          arguments: --workspace --all-features\n")
        self.assertEqual(unscoped_steps(scoped), [])


if __name__ == "__main__":
    unittest.main()
