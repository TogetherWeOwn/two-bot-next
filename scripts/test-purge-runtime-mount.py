"""Offline shell contracts for a narrowly guarded, ordinary apt mount purge."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/purge-runtime-mount.sh"
VERSION = "2.38.1-5+deb12u3"
MOUNT = f"mount\t{VERSION}\tamd64\tno\tinstall ok installed\n"
OTHER = f"util-linux\t{VERSION}\tamd64\tyes\tinstall ok installed\n"


class MountPurgeTests(unittest.TestCase):
    def run_fixture(self, mode="exact"):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")
        self.assertTrue(scratch, "Fake tools require run-owned scratch")
        with tempfile.TemporaryDirectory(dir=scratch) as temporary:
            directory = Path(temporary)
            tool = (
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "from pathlib import Path\n"
                "root = Path(os.environ['FIXTURE_DIRECTORY'])\n"
                "mode = os.environ['FIXTURE_MODE']\n"
                "name = Path(sys.argv[0]).name\n"
                "args = sys.argv[1:]\n"
                "with (root / 'calls.jsonl').open('a') as log:\n"
                "    log.write(json.dumps([name, *args]) + '\\n')\n"
                "assert os.environ['LC_ALL'] == 'C'\n"
                f"mount = {MOUNT!r}\n"
                f"other = {OTHER!r}\n"
                "purged = (root / 'purged').exists()\n"
                "if name == 'dpkg-query':\n"
                "    assert args[:1] == ['-W'] and args[1].startswith('-f=')\n"
                "    if args[2:] == ['mount']:\n"
                "        if mode == 'query-failure': sys.exit(11)\n"
                "        changes = {'essential': ('\\tno\\t', '\\tyes\\t'),\n"
                "                   'wrong-version': ('2.38.1-5+deb12u3', '0.0.0'),\n"
                "                   'wrong-arch': ('amd64', 'arm64'),\n"
                "                   'not-installed': ('install ok installed', 'deinstall ok config-files')}\n"
                "        if mode in changes: mount = mount.replace(*changes[mode])\n"
                "        print(mount, end='')\n"
                "    else:\n"
                "        assert len(args) == 2\n"
                "        if purged and mode == 'after-query-failure': sys.exit(12)\n"
                "        if not purged and mode == 'before-query-failure': sys.exit(13)\n"
                "        if not purged and mode != 'missing-mount-record': print(mount, end='')\n"
                "        if not purged and mode == 'duplicate-mount-record': print(mount, end='')\n"
                "        if purged and mode == 'other-package-changed': other = other.replace('2.38.1-5+deb12u3', '0.0.0')\n"
                "        if not (purged and mode == 'other-package-removed'): print(other, end='')\n"
                "        if purged and mode == 'mount-remains': print(mount, end='')\n"
                "elif name == 'apt-get':\n"
                "    assert args[:2] == ['-o', 'APT::Get::AutomaticRemove=false']\n"
                "    if '--simulate' in args:\n"
                "        assert args[2:] == ['--simulate', 'purge', 'mount']\n"
                "        if mode == 'simulation-failure': sys.exit(8)\n"
                "        actions = {'extra-purge': 'Purg util-linux [2.38.1]',\n"
                "                   'extra-remove': 'Remv util-linux [2.38.1]',\n"
                "                   'install': 'Inst another-package (1.0)',\n"
                "                   'configure': 'Conf another-package (1.0)',\n"
                "                   'duplicate': 'Purg mount [2.38.1]'}\n"
                "        print('Reading package lists...')\n"
                "        if mode != 'no-purge': print('Purg mount [2.38.1-5+deb12u3]')\n"
                "        if mode in actions: print(actions[mode])\n"
                "    elif 'purge' in args:\n"
                "        assert args[2:] == ['--yes', 'purge', 'mount']\n"
                "        if mode == 'purge-failure': sys.exit(9)\n"
                "        (root / 'purged').write_text('mount only')\n"
                "    else:\n"
                "        assert args[2:] == ['check']\n"
                "        if mode == 'initial-check-failure' and not purged: sys.exit(10)\n"
                "        if mode == 'final-check-failure' and purged: sys.exit(10)\n"
            )
            for name in ("dpkg-query", "apt-get"):
                executable = directory / name
                executable.write_text(tool)
                executable.chmod(0o700)
            result = subprocess.run(["/bin/sh", str(SCRIPT)], capture_output=True, text=True,
                                    env={"PATH": str(directory), "FIXTURE_DIRECTORY": str(directory),
                                         "FIXTURE_MODE": mode}, timeout=10)
            log = directory / "calls.jsonl"
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            return result, calls, (directory / "purged").exists()

    def test_only_mount_is_removed_and_database_checked(self):
        result, calls, purged = self.run_fixture()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(purged)
        apt = [call for call in calls if call[0] == "apt-get"]
        self.assertEqual([call[3:] for call in apt], [
            ["check"], ["--simulate", "purge", "mount"], ["--yes", "purge", "mount"], ["check"]])
        self.assertEqual(sum(call[0] == "dpkg-query" for call in calls), 3)

    def test_nonstock_or_essential_package_is_not_removed(self):
        for mode in ("essential", "wrong-version", "wrong-arch", "not-installed"):
            with self.subTest(mode=mode):
                result, calls, purged = self.run_fixture(mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("stock non-Essential mount", result.stderr)
                self.assertFalse(purged)
                self.assertFalse(any(call[0] == "apt-get" for call in calls))

    def test_any_additional_or_missing_plan_action_is_rejected_before_purge(self):
        for mode in ("extra-purge", "extra-remove", "install", "configure", "duplicate", "no-purge"):
            with self.subTest(mode=mode):
                result, calls, purged = self.run_fixture(mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("not exactly a mount-only purge", result.stderr)
                self.assertFalse(purged)
                self.assertFalse(any("--yes" in call for call in calls))

    def test_incomplete_or_duplicate_inventory_does_not_remove_packages(self):
        for mode in ("missing-mount-record", "duplicate-mount-record"):
            with self.subTest(mode=mode):
                result, _, purged = self.run_fixture(mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Incomplete or inconsistent package inventory", result.stderr)
                self.assertFalse(purged)

    def test_dpkg_and_apt_failures_are_preserved(self):
        for mode, status in (("query-failure", 11), ("simulation-failure", 8),
                             ("purge-failure", 9), ("after-query-failure", 12),
                             ("before-query-failure", 13)):
            with self.subTest(mode=mode):
                result, _, purged = self.run_fixture(mode)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(purged, mode == "after-query-failure")

    def test_other_package_or_remaining_mount_changes_fail_the_build(self):
        for mode in ("other-package-changed", "other-package-removed", "mount-remains"):
            with self.subTest(mode=mode):
                result, _, purged = self.run_fixture(mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("package inventory changed beyond mount", result.stderr)
                self.assertTrue(purged)

    def test_dependency_check_failures_are_not_ignored(self):
        for mode in ("initial-check-failure", "final-check-failure"):
            with self.subTest(mode=mode):
                result, _, purged = self.run_fixture(mode)
                self.assertEqual(result.returncode, 10, result.stderr)
                self.assertEqual(purged, mode == "final-check-failure")

    def test_shell_parses_without_running_package_tools(self):
        result = subprocess.run(["/bin/sh", "-n", str(SCRIPT)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        text = SCRIPT.read_text()
        for forbidden in ("--allow-remove-essential", "--force", "autoremove", "apt-get update",
                          "/var/lib/dpkg", "ignore-unfixed"):
            self.assertNotIn(forbidden, text)


if __name__ == "__main__":
    unittest.main()
