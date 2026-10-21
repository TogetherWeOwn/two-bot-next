"""Every docs script/workflow path citation resolves to a real file.

Scans every ``docs/*.md`` plus the root docs for bare ``scripts/...``,
``wrangler/scripts/...``, ``.github/workflows/...`` and ``crates/...`` code
spans (``.github/scripts/...`` included as the same class) and fails on
missing files. Only fenced blocks are skipped: inline spans are operator
reading, so a stale path there is a stale doc.

Run: python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
"""

import re
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOCS = sorted((ROOT / "docs").glob("*.md")) + sorted(ROOT.glob("*.md"))

# Longest prefix first so `.github/scripts/x` never degrades to `scripts/x`.
PATH_RE = re.compile(
    r"(?<![A-Za-z0-9_./-])"
    r"(?P<path>(?:wrangler/scripts/|\.github/scripts/|\.github/workflows/"
    r"|scripts/|crates/)[A-Za-z0-9_./-]+)"
)
CODE_SPAN = re.compile(r"`([^`]+)`")
FENCE_OPEN = re.compile(r"(`{3,}|~{3,})")
GLOB_CHARS = set("*?[]{}<>")
PLACEHOLDER = "NNNN"

# Verified-intentional cites of files that do not exist in this repo. Each
# entry must still be cited and still be missing (see the tightness test):
# a fixed doc or a newly created file means the entry comes out.
ALLOWLIST = {
    # Legacy two-bot sources, cited as provenance, never this repo's files.
    "scripts/preflight.ts": "legacy two-bot preflight source (preflight.md)",
    "scripts/roster.ts": "legacy two-bot alert command (raid-port.md)",
    "scripts/unknown-attribution.ts": "legacy two-bot report helper (classifier-golden.md)",
    # Retired bootstrap: removed in 6032fc72a, cited as historical record.
    "scripts/migrate-release-notes.cjs": "retired release-notes bootstrap (parity.md, soak-checklist.md)",
    # Dropped staging-only runtime, re-create on demand per parity.md 9.3.
    "scripts/automod-export.ts": "dropped staging-only export script (parity.md)",
    # Org-level target, not this repo (this repo pilots reusable-rust-nextest.yml).
    ".github/workflows/rust-nextest.yml": "org .github target (ci-speed-rollout.md)",
    # Planned slice, not yet implemented (doc says "not part of this PR").
    "crates/core/tests/fixtures/internal-action-signing-v2.json": (
        "planned v2 fixture (internal-action-signing-v2.md)"
    ),
}


def _prose_lines(text):
    fence = None
    for number, line in enumerate(text.splitlines(), start=1):
        if fence is None:
            opened = FENCE_OPEN.match(line.lstrip())
            if opened:
                fence = opened.group(1)
            else:
                yield number, line
        elif re.fullmatch(re.escape(fence[0]) + "{%d,}" % len(fence), line.strip()):
            fence = None


def _clean(token):
    """First path token of a span fragment: no args, items or line refs."""
    if token.startswith("./"):
        token = token[2:]
    token, _, _ = token.partition("::")
    token = token.split()[0] if token.split() else ""
    match = re.match(r"(?P<path>.+?):\d[\d,\-]*$", token)
    if match:
        token = match.group("path")
    return token.rstrip(".,;)]")


def _is_pattern(path):
    return any(char in GLOB_CHARS for char in path) or PLACEHOLDER in path


def _cited_paths(text):
    """(line number, raw span, cleaned path) for every bare path citation."""
    found = []
    for number, line in _prose_lines(text):
        for span in CODE_SPAN.findall(line):
            cleaned_span = span.strip()
            if cleaned_span.startswith("./"):
                cleaned_span = cleaned_span[2:]
            for match in PATH_RE.finditer(" " + cleaned_span):
                path = _clean(match.group("path"))
                if path and not _is_pattern(path):
                    found.append((number, span.strip(), path))
    return found


def _problems(doc, text, root=ROOT, allowlist=ALLOWLIST):
    found = []
    for number, span, path in _cited_paths(text):
        if path in allowlist:
            continue
        if not (root / path).exists():
            found.append(f"{doc.name}:{number}: `{span}` -> missing file `{path}`")
    return found


class DocsPathConformance(unittest.TestCase):
    def test_corpus_is_found(self):
        self.assertIn(ROOT / "docs" / "stream-funnel.md", DOCS)
        self.assertIn(ROOT / "CONTRIBUTING.md", DOCS)
        self.assertGreater(len(DOCS), 200)

    def test_every_docs_path_citation_resolves_or_is_allowlisted(self):
        problems = []
        for doc in DOCS:
            problems += _problems(doc, doc.read_text(encoding="utf-8"))
        self.assertEqual(problems, [])

    def test_allowlist_stays_tight(self):
        cited = set()
        for doc in DOCS:
            cited.update(path for _, _, path in _cited_paths(doc.read_text(encoding="utf-8")))
        for path, reason in ALLOWLIST.items():
            with self.subTest(path=path):
                self.assertIn(path, cited, f"uncited allowlist entry ({reason}): remove it")
                self.assertFalse(
                    (ROOT / path).exists(),
                    f"allowlisted `{path}` now exists ({reason}): remove it",
                )

    def test_stream_funnel_cites_the_live_env_bindings_checker(self):
        text = (ROOT / "docs" / "stream-funnel.md").read_text(encoding="utf-8")
        self.assertIn("`wrangler/scripts/check-env-bindings.py`", text)
        bare = [line for line in text.splitlines() if "`scripts/check-env-bindings.py`" in line]
        self.assertEqual(bare, [])

    def test_the_checker_reports_stale_citations_and_accepts_live_ones(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "docs").mkdir()
            (root / "scripts").mkdir()
            (root / "crates").mkdir()
            (root / ".github" / "scripts").mkdir(parents=True)
            (root / "scripts" / "present.py").write_text("print(1)\n", encoding="utf-8")
            (root / "crates" / "table.rs").write_text("a\nb\n", encoding="utf-8")
            (root / ".github" / "scripts" / "real.py").write_text("x\n", encoding="utf-8")
            doc = root / "docs" / "fixture.md"
            stale = "witness (`scripts/absent.py`)\n"
            live = (
                "present (`scripts/present.py`)\n"
                "with args (`scripts/present.py --flag`)\n"
                "mid-span command (`node scripts/present.py 1`)\n"
                "rust item (`crates/table.rs::tests::foo`)\n"
                "line list (`crates/table.rs:1-2,3`)\n"
                "dotted (`./scripts/present.py`)\n"
                "github scripts (`.github/scripts/real.py`)\n"
                "glob (`crates/*/migrations`)\n"
                "retired (`scripts/retired.py`)\n"
                "```\n"
                "`scripts/absent.py`\n"
                "```\n"
            )
            allowlist = {"scripts/retired.py": "test fixture"}
            problems = _problems(doc, stale, root=root, allowlist=allowlist)
            self.assertEqual(len(problems), 1)
            self.assertIn("scripts/absent.py", problems[0])
            self.assertEqual(_problems(doc, live, root=root, allowlist=allowlist), [])


if __name__ == "__main__":
    unittest.main()
