"""Tests for scripts/production_hold_pending.py on a disposable local repo.

No network, service or database: each test builds a throwaway git repository.
"""

import os
import re
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import incident_issue
import production_hold_pending as pending

BASE_TS = 1_800_000_000


def git_env():
    """Fixture git environment: no inherited GIT_* routing/config, no global/system config."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env.update({"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"})
    return env


class Repo:
    def __init__(self, path):
        self.path = Path(path)
        self.n = 0
        self.times = {}  # commit number -> committer time actually recorded
        self.git("init", "-q", "-b", "main")

    def git(self, *args, check=True, input=None):
        env = git_env()
        return subprocess.run(
            ["git", "-C", str(self.path), "-c", "user.name=t",
             "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false",
             *args],
            check=check, capture_output=True, text=True, env=env, input=input,
            timeout=30).stdout.strip()

    def commit(self, files=None, remove=(), move=()):
        """Write a fixture commit with deterministic identity and committer time."""
        for src, dst in move:
            (self.path / dst).parent.mkdir(parents=True, exist_ok=True)
            self.git("mv", src, dst)
        for name in remove:
            self.git("rm", "-q", name)
        for name, content in (files or {}).items():
            target = self.path / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content)
            self.git("add", name)
        parent = self.git("rev-parse", "HEAD") if self.n else None
        self.n += 1
        recorded = BASE_TS + self.n * 100
        identity = f"t <t@example.invalid> {recorded} +0000"
        text = f"tree {self.git('write-tree')}\n"
        if parent is not None:
            text += f"parent {parent}\n"
        text += f"author {identity}\ncommitter {identity}\n\nc{self.n}\n"
        revision = self.git("hash-object", "-t", "commit", "-w", "--stdin", input=text)
        self.git("update-ref", "HEAD", revision)
        self.times[self.n] = recorded
        return revision


class PendingSinceTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.repo = Repo(tmp.name)
        self.base = self.repo.commit({"src/main.rs": "fn main() {}\n"})  # n=1

    def ts(self, n):
        return datetime.fromtimestamp(self.repo.times[n], tz=timezone.utc)

    def pend(self, main=None, promoted=None):
        main = main or self.repo.git("rev-parse", "HEAD")
        return pending.pending_since(promoted or self.base, main, self.repo.path)

    def test_same_commit_is_none(self):
        self.assertIsNone(self.pend())

    def test_docs_only_is_none(self):
        self.repo.commit({"docs/a.md": "x"})
        self.repo.commit({"README.md": "x", "LICENSE": "x", "ops/w.json": "{}"})
        self.assertIsNone(self.pend())

    def test_docs_then_runtime_then_docs_uses_runtime_clock(self):
        self.repo.commit({"docs/a.md": "x"})  # n=2
        self.repo.commit({"crates/a/lib.rs": "x"})  # n=3
        self.repo.commit({"docs/b.md": "x"})  # n=4
        self.assertEqual(self.pend(), self.ts(3))
        self.assertIsNotNone(self.pend().tzinfo)

    def test_first_of_several_runtime_commits_wins(self):
        self.repo.commit({"src/a.rs": "1"})  # n=2
        self.repo.commit({"src/b.rs": "2"})
        self.assertEqual(self.pend(), self.ts(2))

    def test_nested_readme_and_nested_docs_dir_deploy(self):
        self.repo.commit({"crates/x/README.md": "x"})
        self.assertEqual(self.pend(), self.ts(2))

    def test_nested_docs_not_under_root_docs_deploys(self):
        self.repo.commit({"wrangler/docs/a.md": "x"})
        self.assertEqual(self.pend(), self.ts(2))

    def test_mixed_docs_and_runtime_in_one_commit_deploys(self):
        self.repo.commit({"docs/a.md": "x", "Cargo.toml": "x"})
        self.assertEqual(self.pend(), self.ts(2))

    def test_rename_runtime_into_docs_counts_old_path(self):
        self.repo.commit(move=[("src/main.rs", "docs/main.md")])
        self.assertEqual(self.pend(), self.ts(2))

    def test_failed_early_runtime_commit_not_hidden_by_later_docs(self):
        early = self.repo.commit({"src/a.rs": "x"})
        self.repo.commit({"docs/a.md": "x"})
        self.assertEqual(self.pend(), self.ts(2))
        self.assertEqual(self.pend(main=early), self.ts(2))

    def test_large_patch_across_files_is_metadata_only(self):
        blob = "".join(f"{i}\n" for i in range(60000))
        files = {f"src/big{i}.rs": blob + str(i) for i in range(5)}
        self.repo.commit(files)
        total = len(blob) * 5
        self.assertGreater(total, 1024 * 1024)
        self.assertEqual(self.pend(), self.ts(2))

    def test_large_docs_patch_is_none(self):
        blob = "x" * 400_000
        self.repo.commit({f"docs/big{i}.md": blob + str(i) for i in range(4)})
        self.assertIsNone(self.pend())

    def test_empty_commit_is_conservatively_pending(self):
        self.repo.commit()
        self.assertEqual(self.pend(), self.ts(2))

    def test_ignore_list_matches_workflow(self):
        text = (ROOT / ".github/workflows/deploy-staging.yml").read_text()
        block = text.split("paths-ignore:")[1].split("workflow_dispatch:")[0]
        items = [m.strip().split(" #")[0].strip("'\"")
                 for m in re.findall(r"^\s+- (.+)$", block, re.M)]
        expected = {i for i in items if not i.endswith("/**") and i != "*.md"}
        prefixes = {i[:-2] for i in items if i.endswith("/**")}
        self.assertEqual(expected, set(pending.IGNORED_EXACT))
        self.assertEqual(prefixes, set(pending.IGNORED_PREFIXES))
        self.assertIn("*.md", items)

    def test_is_ignored_samples(self):
        for path in ("docs/a/b.md", "README.md", "ops/x.json", "LICENSE",
                     ".github/CODEOWNERS", ".github/ISSUE_TEMPLATE/b.yml"):
            self.assertTrue(pending.is_ignored(path), path)
        for path in ("crates/x/README.md", "src/lib.rs", ".github/workflows/a.yml",
                     "scripts/a.py", "docs", "Cargo.lock", "a/LICENSE"):
            self.assertFalse(pending.is_ignored(path), path)

    # Refusals: every unreadable or untrustworthy history raises, never None.

    def test_promoted_not_ancestor_refused(self):
        self.repo.git("checkout", "-q", "-b", "side", self.base)
        side = self.repo.commit({"docs/side.md": "x"})
        self.repo.git("checkout", "-q", "main")
        self.repo.commit({"src/a.rs": "x"})
        with self.assertRaises(incident_issue.IncidentError):
            self.pend(promoted=side)

    def test_main_behind_promoted_refused(self):
        head = self.repo.commit({"src/a.rs": "x"})
        with self.assertRaises(incident_issue.IncidentError):
            pending.pending_since(head, self.base, self.repo.path)

    def test_merge_commit_in_range_refused(self):
        self.repo.git("checkout", "-q", "-b", "side")
        self.repo.commit({"docs/side.md": "x"})
        self.repo.git("checkout", "-q", "main")
        self.repo.commit({"docs/main.md": "x"})
        self.repo.git("merge", "-q", "--no-ff", "-m", "m", "side")
        with self.assertRaises(incident_issue.IncidentError):
            self.pend()

    def test_promoted_only_on_second_parent_refused(self):
        self.repo.git("checkout", "-q", "-b", "side")
        side = self.repo.commit({"docs/side.md": "x"})
        self.repo.git("checkout", "-q", "main")
        self.repo.commit({"src/a.rs": "x"})
        self.repo.git("merge", "-q", "--no-ff", "-m", "m", "side")
        with self.assertRaises(incident_issue.IncidentError):
            self.pend(promoted=side)

    def test_missing_commit_refused(self):
        with self.assertRaises(incident_issue.IncidentError):
            pending.pending_since("c" * 40, self.base, self.repo.path)

    def test_shallow_checkout_refused(self):
        self.repo.commit({"src/a.rs": "x"})
        with tempfile.TemporaryDirectory() as tmp:
            clone = Path(tmp) / "c"
            subprocess.run(["git", "clone", "-q", "--depth=1",
                            f"file://{self.repo.path}", str(clone)],
                           check=True, capture_output=True, env=git_env(),
                           timeout=30)
            head = subprocess.run(["git", "-C", str(clone), "rev-parse", "HEAD"],
                                  check=True, capture_output=True, env=git_env(),
                                  text=True, timeout=30).stdout.strip()
            with self.assertRaises(incident_issue.IncidentError):
                pending.pending_since(self.base, head, clone)

    def test_malformed_shas_refused(self):
        for bad in ("", "abc", "A" * 40, "g" * 40, None, 7, self.base + "0",
                    "HEAD", "main"):
            with self.assertRaises(incident_issue.IncidentError):
                pending.pending_since(bad, self.base, self.repo.path)
            with self.assertRaises(incident_issue.IncidentError):
                pending.pending_since(self.base, bad, self.repo.path)

    def test_non_repository_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(incident_issue.IncidentError):
                pending.pending_since(self.base, self.base, tmp)

    def test_commit_cap(self):
        for i in range(4):
            self.repo.commit({"docs/a.md": str(i)})
        with mock.patch.object(pending, "MAX_COMMITS", 3):
            with self.assertRaises(incident_issue.IncidentError):
                self.pend()
        with mock.patch.object(pending, "MAX_COMMITS", 4):
            self.assertIsNone(self.pend())

    def test_output_cap(self):
        self.repo.commit({f"docs/{i:04d}.md": "x" for i in range(50)})
        with mock.patch.object(pending, "MAX_OUTPUT_BYTES", 64):
            with self.assertRaises(incident_issue.IncidentError):
                self.pend()

    def test_timeout_refused(self):
        head = self.repo.commit({"src/a.rs": "x"})
        proc = mock.Mock()
        proc.stdout.read.return_value = b"false\n"
        proc.wait.return_value = 0
        subprocess_double = mock.Mock(PIPE=-1, DEVNULL=-3)
        subprocess_double.Popen.return_value = proc
        timer = mock.Mock()

        def start_timer(interval, expire):
            timer.start.side_effect = expire
            return timer

        with mock.patch.object(pending, "subprocess", subprocess_double), \
                mock.patch.object(pending.threading, "Timer", side_effect=start_timer):
            with self.assertRaisesRegex(incident_issue.IncidentError,
                                        "^production hold history git timed out$"):
                self.pend(main=head)
        self.assertIn("--is-shallow-repository", subprocess_double.Popen.call_args.args[0])
        subprocess_double.Popen.assert_called_once()
        proc.kill.assert_called_once_with()
        proc.stdout.read.assert_called_once_with(pending.MAX_OUTPUT_BYTES + 1)
        proc.wait.assert_called_once_with()
        timer.cancel.assert_called_once_with()
        proc.stdout.close.assert_called_once_with()

    def test_missing_git_binary_refused(self):
        fake = mock.Mock(PIPE=-1, DEVNULL=-3)
        fake.Popen.side_effect = FileNotFoundError("git")
        with mock.patch.object(pending, "subprocess", fake):
            with self.assertRaises(incident_issue.IncidentError):
                self.pend()

    def test_errors_carry_no_child_output_or_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(incident_issue.IncidentError) as ctx:
                pending.pending_since(self.base, self.base, tmp)
        self.assertNotIn(tmp, str(ctx.exception))
        self.assertNotIn("fatal", str(ctx.exception))

    def test_malformed_chain_output_refused(self):
        self.repo.commit({"src/a.rs": "x"})
        with mock.patch.object(pending, "_git",
                               side_effect=[(0, b"false\n"), (0, b""), (0, b""),
                                            (0, b""), (0, b"not-a-sha\n")]):
            with self.assertRaises(incident_issue.IncidentError):
                self.pend()

    def test_malformed_path_data_refused(self):
        self.repo.commit({"src/a.rs": "x"})
        head = self.repo.git("rev-parse", "HEAD")
        real = pending._git

        def fake(root, args, **kw):
            if args[0] == "diff-tree":
                return 0, b"src/a.rs"  # missing NUL terminator
            return real(root, args, **kw)

        with mock.patch.object(pending, "_git", side_effect=fake):
            with self.assertRaises(incident_issue.IncidentError):
                self.pend(main=head)


class InheritedGitEnvTest(unittest.TestCase):
    """Inherited GIT_* variables must not reach fixture git; only sentinel repos are used."""

    def snapshot(self, repo):
        gitdir = repo.path / ".git"
        objects = sorted(str(p.relative_to(gitdir)) for p in (gitdir / "objects").rglob("*")
                         if p.is_file())
        index = gitdir / "index"
        return (repo.git("rev-parse", "HEAD"), index.read_bytes(), objects,
                sorted(p.name for p in repo.path.iterdir()))

    def test_inherited_git_routing_cannot_touch_sentinel(self):
        with tempfile.TemporaryDirectory() as sentinel_tmp, \
                tempfile.TemporaryDirectory() as fixture_tmp, \
                tempfile.TemporaryDirectory() as extra_tmp:
            sentinel = Repo(sentinel_tmp)
            sentinel.commit({"keep.txt": "sentinel"})
            before = self.snapshot(sentinel)
            gitdir = str(sentinel.path / ".git")
            poisoned = {
                "GIT_DIR": gitdir,
                "GIT_WORK_TREE": str(sentinel.path),
                "GIT_INDEX_FILE": gitdir + "/index",
                "GIT_OBJECT_DIRECTORY": gitdir + "/objects",
                "GIT_ALTERNATE_OBJECT_DIRECTORIES": extra_tmp,
                "GIT_COMMON_DIR": gitdir,
                "GIT_NAMESPACE": "poison",
                "GIT_CEILING_DIRECTORIES": fixture_tmp,
                "GIT_CONFIG_COUNT": "1",
                "GIT_CONFIG_KEY_0": "core.worktree",
                "GIT_CONFIG_VALUE_0": str(sentinel.path),
                "GIT_AUTHOR_NAME": "poison",
                "GIT_COMMITTER_DATE": "1 +0000",
            }
            with mock.patch.dict(os.environ, poisoned):
                env = git_env()
                self.assertEqual(
                    {k for k in env if k.startswith("GIT_")},
                    {"GIT_CONFIG_GLOBAL", "GIT_CONFIG_NOSYSTEM"})
                self.assertEqual(env["GIT_CONFIG_GLOBAL"], os.devnull)
                self.assertEqual(env["GIT_CONFIG_NOSYSTEM"], "1")
                fixture = Repo(fixture_tmp)
                fixture.commit({"src/a.rs": "1"})
                fixture.commit({"src/b.rs": "2"})
                fixture_head = fixture.git("rev-parse", "HEAD")
                self.assertEqual(
                    fixture.git("rev-parse", "--absolute-git-dir"),
                    str(Path(fixture_tmp, ".git").resolve()))
                clone = Path(fixture_tmp) / "clone"
                subprocess.run(["git", "clone", "-q", "--depth=1",
                                f"file://{fixture.path}", str(clone)],
                               check=True, capture_output=True, env=git_env(),
                               timeout=30)
            self.assertTrue((clone / ".git").is_dir())
            self.assertEqual(self.snapshot(sentinel), before)
            self.assertNotEqual(fixture_head, before[0])


if __name__ == "__main__":
    unittest.main()
