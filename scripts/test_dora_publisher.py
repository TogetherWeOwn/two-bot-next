"""Execute the workflow publisher offline with disposable Git repositories.

Only the named workflow run block is executed: no copied publication commands,
GitHub calls, credentials, or dependency on the repository's current Git refs.
"""

import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/dora-metrics.yml"
BRANCH = "refs/heads/automation/dora-metrics"
SYNTHETIC_TOKEN = "offline-publisher-fixture-not-a-credential"


def publisher_script():
    """Extract the literal shell belonging to the named workflow step."""
    lines = WORKFLOW.read_text(encoding="utf-8").splitlines(keepends=True)
    marker = "      - name: Commit the weekly DORA file\n"
    if lines.count(marker) != 1:
        raise AssertionError("expected exactly one named DORA publication step")
    start = lines.index(marker) + 1
    end = next((i for i in range(start, len(lines))
                if lines[i].startswith("      - ")), len(lines))
    step = lines[start:end]
    run = "        run: |\n"
    if step.count(run) != 1:
        raise AssertionError("expected a literal publication run block")
    body = []
    for line in step[step.index(run) + 1:]:
        if line.strip() and not line.startswith("          "):
            break
        body.append(line)
    script = textwrap.dedent("".join(body))
    if not script.strip() or "${{" in script:
        raise AssertionError("publication shell must be nonempty and directly executable")
    return script


class PublisherReplayTests(unittest.TestCase):
    def setUp(self):
        scratch = (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
                   or os.environ.get("RUNNER_TEMP"))
        temporary = tempfile.TemporaryDirectory(prefix="dora-publisher-test-", dir=scratch)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "home"
        self.runner = self.root / "runner"
        self.hooks = self.root / "empty-hooks"
        self.templates = self.root / "empty-templates"
        for directory in (self.home, self.runner, self.hooks, self.templates):
            directory.mkdir()
        # Construct, never copy, the child environment. In particular no inherited
        # GH/GIT credentials, config overrides, shell startup hooks, or proxy vars.
        self.env = {
            "PATH": os.defpath,
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home),
            "LC_ALL": "C",
            "TMPDIR": str(self.root),
            "RUNNER_TEMP": str(self.runner),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_SYSTEM": os.devnull,
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_TEMPLATE_DIR": str(self.templates),
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_ASKPASS": "/bin/false",
            "SSH_ASKPASS": "/bin/false",
            "GIT_SSH_COMMAND": "/bin/false",
            "GIT_ALLOW_PROTOCOL": "file",
            "GH_TOKEN": SYNTHETIC_TOKEN,
        }
        self.script = publisher_script()
        self.remote = self.root / "remote.git"
        self.git(self.root, "init", "--bare", "--initial-branch=main", str(self.remote))
        self.disable_hooks(self.remote)
        self.seed = self.root / "seed"
        self.git(self.root, "init", "--initial-branch=main", str(self.seed))
        self.disable_hooks(self.seed)
        self.git(self.seed, "config", "user.name", "Offline fixture")
        self.git(self.seed, "config", "user.email", "fixture@example.invalid")
        (self.seed / "README.md").write_bytes(b"Untouched main tree.\n")
        self.git(self.seed, "add", "README.md")
        self.git(self.seed, "commit", "-m", "test: seed local publisher fixture")
        self.git(self.seed, "remote", "add", "origin", str(self.remote))
        self.git(self.seed, "push", "origin", "HEAD:refs/heads/main")
        self.main_sha = self.git(self.remote, "rev-parse", "refs/heads/main").stdout.strip()
        self.main_tree = self.git(self.remote, "rev-parse", "refs/heads/main^{tree}").stdout
        self.checkouts = []

    def git(self, directory, *args):
        result = subprocess.run(["git", *args], cwd=directory, env=self.env,
                                capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 0,
                         (result.stdout + result.stderr).decode(errors="replace"))
        return result

    def disable_hooks(self, directory):
        self.git(directory, "config", "core.hooksPath", str(self.hooks))

    def snapshot(self, checkout):
        return tuple(self.git(checkout, *args).stdout for args in (
            ("rev-parse", "HEAD"), ("symbolic-ref", "HEAD"), ("write-tree",),
            ("diff", "--binary"), ("diff", "--cached", "--binary"),
            ("status", "--porcelain=v1", "--untracked-files=all"),
        ))

    def publish(self, week, version):
        checkout = self.root / f"checkout-{len(self.checkouts)}"
        self.git(self.root, "clone", "--branch", "main", str(self.remote), str(checkout))
        self.disable_hooks(checkout)
        self.assertEqual(self.git(checkout, "rev-parse", "HEAD").stdout.strip(), self.main_sha)
        artifacts = {
            f"ops/metrics/dora-{week}.json":
                f'{{"week": "{week}", "fixture": "{version}"}}\n'.encode(),
            f"ops/metrics/dora-{week}.md":
                f"# DORA {week}\n\nFixture: {version} — byte-intact.\n".encode(),
        }
        for name, content in artifacts.items():
            path = checkout / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        before = self.snapshot(checkout)
        result = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail",
                                 "-c", self.script], cwd=checkout,
                                env={**self.env, "WEEK": week},
                                capture_output=True, timeout=30)
        self.checkouts.append((checkout, before, artifacts))
        return result, artifacts

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0,
                         (result.stdout + result.stderr).decode(errors="replace"))

    def publication_tip(self):
        return self.git(self.remote, "rev-parse", BRANCH).stdout

    def assert_published(self, expected):
        paths = self.git(self.remote, "ls-tree", "-r", "--name-only", BRANCH).stdout
        self.assertEqual(set(paths.decode().splitlines()), {"README.md", *expected})
        for name, content in expected.items():
            self.assertEqual(self.git(self.remote, "show", f"{BRANCH}:{name}").stdout, content)
        self.assertEqual(self.git(self.remote, "show", f"{BRANCH}:README.md").stdout,
                         b"Untouched main tree.\n")

    def assert_preserved(self, published=True):
        self.assertEqual(self.git(self.remote, "rev-parse", "refs/heads/main").stdout.strip(),
                         self.main_sha)
        self.assertEqual(self.git(self.remote, "rev-parse", "refs/heads/main^{tree}").stdout,
                         self.main_tree)
        refs = self.git(self.remote, "for-each-ref", "--format=%(refname)", "refs/heads").stdout
        self.assertEqual(set(refs.decode().splitlines()),
                         {"refs/heads/main", BRANCH} if published else {"refs/heads/main"})
        for checkout, before, artifacts in self.checkouts:
            with self.subTest(checkout=checkout.name):
                self.assertEqual(self.snapshot(checkout), before,
                                 "publisher changed original checkout HEAD/index/worktree")
                for name, content in artifacts.items():
                    self.assertEqual((checkout / name).read_bytes(), content)
        for config in [self.remote / "config", self.seed / ".git/config",
                       *(checkout / ".git/config" for checkout, _, _ in self.checkouts)]:
            with self.subTest(config=str(config.relative_to(self.root))):
                content = config.read_bytes().lower()
                self.assertNotIn(SYNTHETIC_TOKEN.encode(), content)
                self.assertNotIn(b"extraheader", content)
                self.assertNotIn(b"authorization", content)
                self.assertNotIn(b"credential", content)

    def reject_pushes(self):
        hooks = self.root / "reject-hooks"
        hooks.mkdir()
        hook = hooks / "pre-receive"
        hook.write_text("#!/bin/sh\nprintf '%s\\n' 'offline fixture rejects push' >&2\nexit 1\n",
                        encoding="utf-8")
        hook.chmod(0o700)
        self.git(self.remote, "config", "core.hooksPath", str(hooks))

    def test_first_publish_creates_only_automation_branch(self):
        result, artifacts = self.publish("2026-W43", "first")
        self.assert_success(result)
        self.assertEqual(self.git(self.remote, "rev-list", "--count",
                                  f"refs/heads/main..{BRANCH}").stdout, b"1\n")
        self.assert_published(artifacts)
        self.assert_preserved()

    def test_changed_same_week_from_fresh_main_replaces_both_files(self):
        first, _ = self.publish("2026-W43", "first")
        self.assert_success(first)
        original_tip = self.publication_tip()
        second, replacement = self.publish("2026-W43", "replacement")
        self.assert_success(second)
        self.assertNotEqual(self.publication_tip(), original_tip)
        self.assertEqual(self.git(self.remote, "rev-parse", f"{BRANCH}^").stdout, original_tip)
        self.assert_published(replacement)
        self.assert_preserved()

    def test_new_week_from_fresh_main_retains_older_week(self):
        first, older = self.publish("2026-W43", "older")
        self.assert_success(first)
        original_tip = self.publication_tip()
        second, newer = self.publish("2026-W44", "newer")
        self.assert_success(second)
        self.assertEqual(self.git(self.remote, "rev-parse", f"{BRANCH}^").stdout, original_tip)
        self.assert_published({**older, **newer})
        self.assert_preserved()

    def test_identical_output_is_noop_without_new_remote_commit(self):
        first, artifacts = self.publish("2026-W43", "identical")
        self.assert_success(first)
        original_tip = self.publication_tip()
        count = self.git(self.remote, "rev-list", "--count", BRANCH).stdout
        second, _ = self.publish("2026-W43", "identical")
        self.assert_success(second)
        self.assertEqual(self.publication_tip(), original_tip)
        self.assertEqual(self.git(self.remote, "rev-list", "--count", BRANCH).stdout, count)
        self.assert_published(artifacts)
        self.assert_preserved()

    def test_rejected_update_returns_nonzero_and_preserves_evidence(self):
        first, original = self.publish("2026-W43", "accepted")
        self.assert_success(first)
        original_tip = self.publication_tip()
        self.reject_pushes()
        refused, _ = self.publish("2026-W43", "refused")
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(b"pre-receive hook declined", refused.stderr)
        self.assertEqual(self.publication_tip(), original_tip)
        self.assert_published(original)
        self.assert_preserved()

    def test_rejected_first_publish_does_not_create_remote_branch(self):
        self.reject_pushes()
        refused, _ = self.publish("2026-W43", "refused")
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(b"pre-receive hook declined", refused.stderr)
        self.assert_preserved(published=False)


if __name__ == "__main__":
    unittest.main()
