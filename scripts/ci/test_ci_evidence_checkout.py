"""Exercise the downstream CI summary with local Git and synthetic Actions rows."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ".github/scripts/pr_evidence.py"
REPO = "TogetherWeOwn/two-bot-next"
HEAD = "a" * 40
RUN = f"{HEAD}\tpull_request\t.github/workflows/check.yml\t12\t2"
JOB = f"34\t{HEAD}\t12\t2\tsuccess\tci-ok"
URL = f"https://github.com/{REPO}/actions/runs/12"
SUMMARY = (
    f"## Actions evidence for `{HEAD}`\n\n"
    f"[Workflow run, attempt 2]({URL}/attempts/2)\n\n"
    f"- [ci-ok]({URL}/job/34) (success, attempt 2)\n"
    f"- [worker check]({URL}/job/33) (success, attempt 1)\n"
)


class CiEvidenceCheckoutTests(unittest.TestCase):
    def test_narrow_checkout_preserves_exact_attempt_summary_and_refusals(self):
        workflow = yaml.load(
            (ROOT / ".github/workflows/check.yml").read_text(), Loader=yaml.BaseLoader
        )
        job = workflow["jobs"]["ci-evidence"]
        self.assertEqual(job["needs"], "ci-ok")
        self.assertEqual(job["if"], "${{ !cancelled() && github.event_name == 'pull_request' }}")
        self.assertEqual(job["permissions"], {"contents": "read", "actions": "read"})
        checkout, publish = job["steps"]
        self.assertEqual(set(checkout), {"uses", "with"})
        self.assertTrue(checkout["uses"].startswith("actions/checkout@"))
        self.assertEqual(set(checkout["with"]),
                         {"sparse-checkout", "sparse-checkout-cone-mode", "persist-credentials"})
        self.assertEqual(checkout["with"]["sparse-checkout-cone-mode"], "false")
        self.assertEqual(checkout["with"]["persist-credentials"], "false")
        self.assertEqual(set(publish), {"env", "run"})
        self.assertEqual(publish["env"], {
            "GH_TOKEN": "${{ github.token }}",
            "EVIDENCE_HEAD": "${{ github.event.pull_request.head.sha }}",
        })

        def git(cwd, *args):
            subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, timeout=10)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "source"
            git(root, "init", "--quiet", "--template=", str(source))
            (source / SCRIPT).parent.mkdir(parents=True)
            (source / SCRIPT).write_bytes((ROOT / SCRIPT).read_bytes())
            for path in (".gitattributes", ".gitignore", "README.md", ".github/scripts/other.py"):
                (source / path).touch()
            git(source, "add", "--force", ".")
            git(source, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.com",
                "commit", "--no-gpg-sign", "--quiet", "-m", "Synthetic CI source")

            fake_gh = root / "gh"
            fake_gh.write_text(
                "#!/bin/sh\n"
                f'prefix=repos/{REPO}/actions/runs/12/attempts/2\n'
                'case "$1 $2" in\n'
                '  "api $prefix") printf "%s\\n" "$RUN_ROW" ;;\n'
                '  "api --paginate")\n'
                '    test "$3" = "$prefix/jobs?per_page=100" || exit 1\n'
                '    test -n "$JOB_ROWS" && printf "%s\\n" "$JOB_ROWS"\n'
                '    exit 0 ;;\n'
                '  *) exit 1 ;;\n'
                'esac\n'
            )
            fake_gh.chmod(0o700)
            env = {
                "PATH": str(root) + os.pathsep + os.environ["PATH"],
                "GITHUB_REPOSITORY": REPO,
                "GITHUB_EVENT_NAME": "pull_request",
                "EVIDENCE_HEAD": HEAD,
                "GITHUB_RUN_ID": "12",
                "GITHUB_RUN_ATTEMPT": "2",
                "GITHUB_STEP_SUMMARY": str(root / "summary"),
            }
            worker = f"33\t{HEAD}\t12\t1\tsuccess\tworker check"
            cases = (
                ("passing CI", RUN, f"{JOB}\n{worker}", SUMMARY, ""),
                ("failing CI", RUN, f"{JOB.replace('success', 'failure')}\n{worker}",
                 SUMMARY.replace("success, attempt 2", "failure, attempt 2"), ""),
                ("wrong head", RUN.replace(HEAD, "b" * 40), JOB, "",
                 "Actions run does not match the event head, workflow and attempt."),
                ("future job attempt", RUN, JOB.replace("\t2\t", "\t3\t"), "",
                 "Actions returned a job outside this head or attempt."),
                ("empty job inventory", RUN, "", "", "Actions returned no jobs for this attempt."),
            )
            for version in ("previous", "current"):
                worktree = root / version
                git(root, "clone", "--quiet", "--no-checkout", str(source), str(worktree))
                if version == "previous":
                    git(worktree, "sparse-checkout", "set", ".github/scripts")
                else:
                    # Mirror the pinned action's non-cone setup before checkout.
                    git(worktree, "config", "core.sparseCheckout", "true")
                    (worktree / ".git/info/sparse-checkout").write_text(
                        "\n" + checkout["with"]["sparse-checkout"]
                    )
                git(worktree, "checkout", "--quiet", "--force", "HEAD")
                if version == "current":
                    self.assertEqual(
                        {p.relative_to(worktree).as_posix() for p in worktree.rglob("*")
                         if p.is_file() and ".git" not in p.relative_to(worktree).parts},
                        {".gitattributes", ".gitignore", SCRIPT},
                    )
                for name, run, jobs, summary, error in cases:
                    with self.subTest(version=version, case=name):
                        summary_file = Path(env["GITHUB_STEP_SUMMARY"])
                        summary_file.unlink(missing_ok=True)
                        result = subprocess.run(
                            ["bash", "-eo", "pipefail", "-c", publish["run"]], cwd=worktree,
                            env={**env, "RUN_ROW": run, "JOB_ROWS": jobs}, text=True,
                            capture_output=True, timeout=10,
                        )
                        self.assertEqual(result.returncode, int(bool(error)), result.stderr)
                        self.assertEqual(result.stderr.strip(), error)
                        self.assertEqual(summary_file.read_text(), summary)
