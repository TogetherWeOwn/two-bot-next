"""Check a draft PR description against local commits before publishing it."""

import argparse
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory


SCRIPTS = Path(__file__).resolve().parents[2] / ".github/scripts"
sys.path.insert(0, str(SCRIPTS))
import pr_evidence
import pr_standards


def check_evidence(body, files, head):
    try:
        import pr_attestation

        pr_evidence.validate(pr_attestation, body, files, lambda path: pr_evidence.source_blob(head, path))
    except ModuleNotFoundError:
        print("PR evidence dependencies are missing. See CONTRIBUTING.md for setup.", file=sys.stderr)
    except pr_evidence.EvidenceError as error:
        print(f"PR evidence: {error}", file=sys.stderr)
    except (OSError, SyntaxError, ValueError, subprocess.SubprocessError):
        print("Could not validate PR evidence or read an attested source.", file=sys.stderr)
    else:
        print("Local Verification and Attestation passed.")
        return 0
    return 1


def check_squash(title, body, head_ref, number):
    # Give the existing CLI the same snapshot, including any original CRLF.
    try:
        with TemporaryDirectory(prefix="pr-verification-") as directory:
            draft = Path(directory) / "body.md"
            draft.write_bytes(body.encode("utf-8"))
            env = dict(os.environ, TITLE=title, BODY_FILE=str(draft), HEAD_REF=head_ref, PR_NUMBER=number)
            sys.stdout.flush()
            return subprocess.run(
                ["node", str(SCRIPTS / "commit-parse-guard.cjs"), "pr"],
                env=env, timeout=30, check=False,
            ).returncode
    except (OSError, subprocess.SubprocessError):
        print("Could not run the local squash parser. See CONTRIBUTING.md for setup.", file=sys.stderr)
        return 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--title", required=True)
    parser.add_argument("--body-file", type=Path, required=True)
    parser.add_argument("--base", default="origin/main", help="local target ref or SHA (default: origin/main)")
    parser.add_argument("--pr-number", default="", help="existing PR number, if assigned")
    args = parser.parse_args(argv)
    if args.pr_number and not pr_evidence.NUMBER.fullmatch(args.pr_number):
        parser.error("--pr-number must be a positive integer")

    try:
        body = args.body_file.read_bytes().decode("utf-8")
    except (OSError, UnicodeError):
        parser.error("could not read --body-file as UTF-8")
    try:
        git = pr_evidence.command
        base = git("git", "rev-parse", "--verify", "--end-of-options", f"{args.base}^{{commit}}").strip()
        head_ref = git("git", "rev-parse", "--abbrev-ref", "HEAD").strip()
        head = git("git", "rev-parse", "HEAD").strip()
        files = pr_evidence.sources(base, head)
    except (OSError, ValueError, subprocess.SubprocessError):
        parser.error("could not read the committed PR diff from --base to HEAD")

    # Hosted standards normalizes line endings when it reads the body file.
    standards = pr_standards.main({
        "EVENT": "pull_request", "TITLE": args.title, "HEAD_REF": head_ref,
        "BODY": body.replace("\r\n", "\n").replace("\r", "\n"), "REPO_PRIVATE": "false",
        "CHANGED_FILES": "\n".join(file["filename"] for file in files),
    })
    evidence = check_evidence(body, files, head)
    squash = check_squash(args.title, body, head_ref, args.pr_number)
    if standards or evidence or squash:
        return 1
    print(f"Local PR preflight passed for {head} against {base}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
