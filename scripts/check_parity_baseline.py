#!/usr/bin/env python3
"""Check parity ancestry, complete source ledger and Git-resolved Markdown links.

No HTTP requests or application imports: supply a full, read-only legacy clone.
The historical range start can be off main after a rewrite; no linked SHA can.
"""

import argparse
from collections import Counter
import json
from pathlib import Path
import re
import subprocess
import sys
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
SHA = re.compile(r"[0-9a-f]{40}")
LINK = re.compile(r"\[[^\]\n]*\]\(([^\s)]+)\)")
BEGIN = "<!-- post-freeze-ledger:start -->"
END = "<!-- post-freeze-ledger:end -->"


class ParityError(ValueError):
    pass


def git(repo, *args):
    result = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, check=False
    )
    if result.returncode:
        raise ParityError(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout.strip()


def ancestor(repo, revision, main):
    git(repo, "merge-base", "--is-ancestor", revision, main)


def check(root, legacy_repo, main_ref="main"):
    config = json.loads((root / "docs/parity-baseline.json").read_text())
    if config.get("version") != 1 or config.get("repository") != "TogetherWeOwn/two-bot":
        raise ParityError("unsupported parity baseline manifest")
    for key in ("baseline", "historyStart", "registrySnapshot"):
        if not SHA.fullmatch(config.get(key, "")):
            raise ParityError(f"{key} must be a full lowercase commit SHA")
    baseline = config["baseline"]
    snapshot = config["registrySnapshot"]
    main = git(legacy_repo, "rev-parse", "--verify", f"{main_ref}^{{commit}}")
    for revision in (baseline, snapshot):
        ancestor(legacy_repo, revision, main)
    # Establish why the old golden can retain exactly the same command payload.
    if git(legacy_repo, "rev-parse", config["historyStart"] + ":src") != git(
        legacy_repo, "rev-parse", snapshot + ":src"
    ):
        raise ParityError("rewritten registry snapshot differs from historical src tree")
    fixture = json.loads((root / "crates/core/tests/fixtures/legacy_registry.json").read_text())
    if fixture.get("revision") != snapshot:
        raise ParityError("registry golden revision differs from registrySnapshot")

    matrix_path = root / "docs/parity.md"
    matrix = matrix_path.read_text()
    source_link = f"https://github.com/{config['repository']}/tree/{baseline}"
    if source_link not in matrix:
        raise ParityError("parity matrix must link the manifest baseline")
    if matrix.count(BEGIN) != 1 or matrix.count(END) != 1:
        raise ParityError("expected exactly one delimited post-freeze ledger")
    ledger = matrix.split(BEGIN, 1)[1].split(END, 1)[0]
    rows = []
    for line in ledger.splitlines():
        if not line.startswith("| ["):
            continue
        columns = [column.strip() for column in line.strip("|").split("|")]
        if len(columns) != 5:
            raise ParityError("ledger rows must have commit, area, change, status, disposition")
        commit, area, change, status, disposition = columns
        match = re.fullmatch(
            r"\[([0-9a-f]{7,40})\]\(https://github.com/TogetherWeOwn/two-bot/commit/([0-9a-f]{40})\)",
            commit,
        )
        if not match or not match[2].startswith(match[1]):
            raise ParityError(f"invalid ledger commit link: {commit}")
        if not area or not change or not disposition:
            raise ParityError(f"incomplete ledger row: {match[1]}")
        has_card = re.search(r"\[(TOG-\d+)\]\(/TOG/issues/\1\)", disposition)
        has_drop = disposition.startswith("drop: ") and len(disposition) > 12
        if status not in {"ported", "carded", "dropped", "gap"}:
            raise ParityError(f"unknown status: {status}")
        if status == "carded" and not has_card:
            raise ParityError(f"carded row needs a card: {match[1]}")
        if status == "dropped" and not has_drop:
            raise ParityError(f"dropped row needs an explicit drop reason: {match[1]}")
        if status == "gap" and not (has_card or has_drop):
            raise ParityError(f"gap needs a card or explicit drop reason: {match[1]}")
        rows.append(match[2])
    counts = Counter(rows)
    if any(count != 1 for count in counts.values()):
        raise ParityError("duplicate legacy commit in ledger")
    expected = set(git(
        legacy_repo, "log", "--format=%H", config["historyStart"] + ".." + baseline,
        "--", "src/",
    ).splitlines())
    if set(rows) != expected:
        raise ParityError(
            f"ledger coverage mismatch: missing={sorted(expected - set(rows))}, "
            f"extra={sorted(set(rows) - expected)}"
        )

    checked_revisions = {baseline, snapshot}
    links = set(LINK.findall(matrix))
    legacy_links = 0
    for href in sorted(links):
        url = urlsplit(href)
        if url.netloc == "github.com" and url.path.startswith("/TogetherWeOwn/two-bot/"):
            parts = url.path.split("/", 5)
            if len(parts) < 5 or parts[3] not in {"blob", "tree", "commit"}:
                raise ParityError(f"unversioned legacy link: {href}")
            revision = parts[4]
            if not SHA.fullmatch(revision):
                raise ParityError(f"legacy links require full SHAs: {href}")
            if revision not in checked_revisions:
                ancestor(legacy_repo, revision, main)
                checked_revisions.add(revision)
            if parts[3] in {"blob", "tree"} and len(parts) == 6:
                path = unquote(parts[5])
                kind = git(legacy_repo, "cat-file", "-t", f"{revision}:{path}")
                if kind != ("blob" if parts[3] == "blob" else "tree"):
                    raise ParityError(f"wrong Git object type: {href}")
                if url.fragment:
                    lines = re.fullmatch(r"L(\d+)(?:-L(\d+))?", url.fragment)
                    if not lines or kind != "blob":
                        raise ParityError(f"invalid legacy line anchor: {href}")
                    start, end = int(lines[1]), int(lines[2] or lines[1])
                    size = len(git(legacy_repo, "show", f"{revision}:{path}").splitlines())
                    if not 1 <= start <= end <= size:
                        raise ParityError(f"legacy line anchor outside file: {href}")
            legacy_links += 1
        elif not url.scheme and not href.startswith(("/", "#")):
            if not (matrix_path.parent / unquote(url.path)).exists():
                raise ParityError(f"missing relative link: {href}")
    return {"baseline": baseline, "legacyMain": main, "ledgerRows": len(rows),
            "legacyLinks": legacy_links, "reachableRevisions": len(checked_revisions)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--legacy-repo", type=Path, required=True)
    parser.add_argument("--main-ref", default="main")
    args = parser.parse_args()
    try:
        result = check(ROOT, args.legacy_repo, args.main_ref)
    except (ParityError, OSError, json.JSONDecodeError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    print("PASS: " + json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
