"""Every docs script/workflow path citation resolves to a real file.

Scans every markdown file in the repo (``docs/**/*.md``, root docs, nested
READMEs) for bare ``scripts/...``, ``wrangler/scripts/...``,
``.github/workflows/...`` and ``crates/...`` code spans
(``.github/scripts/...`` included as the same class) and fails on missing
files. Only fenced blocks are skipped: inline spans are operator reading,
so a stale path there is a stale doc. Code spans may wrap across hard-wrapped
lines, so scanning joins consecutive prose lines before pairing backticks.

Run: python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
"""

import bisect
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
_EXCLUDED_PARTS = {".git", ".paperclip", "target", "node_modules"}


def _markdown_corpus():
    """Tracked markdown plus the generated command reference when present.

    ``git ls-files`` works offline and limits the corpus to files the PR can
    change, so untracked scratch notes or build output never fail the suite
    and excluded directories are never walked.
    """
    try:
        out = subprocess.run(
            ["git", "ls-files", "-z", "--", "*.md"],
            cwd=ROOT,
            capture_output=True,
            check=True,
        ).stdout.decode("utf-8")
        paths = {ROOT / p for p in out.split("\0") if p}
        generated = ROOT / "docs/generated/commands.md"
        if generated.is_file():
            paths.add(generated)
        return sorted(paths)
    except (OSError, subprocess.CalledProcessError):
        return sorted(
            p
            for p in ROOT.rglob("*.md")
            if not (set(p.relative_to(ROOT).parts) & _EXCLUDED_PARTS)
        )


DOCS = _markdown_corpus()

# Longest prefix first so `.github/scripts/x` never degrades to `scripts/x`.
# An optional `./` is accepted before any prefix (mid-span `python3 ./scripts/x`
# as well as span-start `./scripts/x`); the lookbehind still rejects
# repo-prefixed forms like `TogetherWeOwn/.github/...`. The token class includes
# glob characters so `_is_pattern` sees the whole citation instead of a
# truncated stem (`scripts/test_*.py` must skip, not report `scripts/test_`).
PATH_RE = re.compile(
    r"(?<![A-Za-z0-9_./-])"
    r"(?:\./)?"
    r"(?P<path>(?:wrangler/scripts/|\.github/scripts/|\.github/workflows/"
    r"|scripts/|crates/)[A-Za-z0-9_./*?\[\]{}<>+-]+)"
)
BACKTICK_RUN = re.compile(r"`+")
# CommonMark forbids backticks in a backtick fence info string: a line like
# ``` `x` ``` stays a paragraph with inline code, it must not swallow the rest
# of the doc as a fenced block. Tilde fences keep the old shape.
FENCE_OPEN = re.compile(r"(`{3,}(?=[^`]*$)|~{3,})")


def _code_spans(joined):
    """(open offset, span content) pairing same-length backtick runs.

    A backtick string closes only on the next run of equal length, so triple
    backticks in prose (`` ``` `x` ``` ``) form one span instead of shifting
    the parity of every later single-backtick citation on the joined block.
    """
    runs = [
        (match.start(), match.end(), len(match.group(0)))
        for match in BACKTICK_RUN.finditer(joined)
    ]
    spans = []
    i = 0
    while i < len(runs):
        open_start, open_end, open_len = runs[i]
        closer = None
        for j in range(i + 1, len(runs)):
            if runs[j][2] == open_len:
                closer = j
                break
        if closer is None:
            i += 1
            continue
        close_start, _, _ = runs[closer]
        spans.append((open_start, joined[open_end:close_start]))
        i = closer + 1
    return spans
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
    # Cut redirect-attached suffixes (`scripts/x.py>out.log` cites `scripts/x.py`)
    # and angle-wrapped citations (`<scripts/x.py>`) at the first bracket, but
    # only when a filename character precedes it: placeholder-first forms like
    # `crates/<crate>/...` keep their bracket and still skip as patterns.
    for i, char in enumerate(token):
        if char in "<>":
            if i > 0 and re.match(r"[A-Za-z0-9_.-]", token[i - 1]):
                token = token[:i]
            break
    return token.rstrip(".,;)]}")


def _is_pattern(path):
    return any(char in GLOB_CHARS for char in path) or PLACEHOLDER in path


def _cited_paths(text):
    """(line number, raw span, cleaned path) for every bare path citation.

    Code spans are paired across consecutive prose lines so hard-wrapped spans
    (`` `scripts/foo.py\\nrender` ``) and parity-shifted closers (a wrapped span
    closing mid-line before a ``crates/...`` citation) are still scanned.
    Blocks break on blank lines and fence gaps so stray backticks never pair
    across paragraphs or fenced blocks.
    """
    found = []
    prose = list(_prose_lines(text))
    block: list[tuple[int, str]] = []
    prev: int | None = None

    def flush() -> None:
        if not block:
            return
        joined = "\n".join(line for _, line in block)
        starts: list[int] = []
        offset = 0
        for _, line in block:
            starts.append(offset)
            offset += len(line) + 1
        for open_at, span in _code_spans(joined):
            idx = bisect.bisect_right(starts, open_at) - 1
            number = block[idx][0]
            cleaned_span = span.strip()
            if cleaned_span.startswith("./"):
                cleaned_span = cleaned_span[2:]
            display = " ".join(span.split())
            for path_match in PATH_RE.finditer(" " + cleaned_span):
                path = _clean(path_match.group("path"))
                if path and not _is_pattern(path):
                    found.append((number, display, path))
        block.clear()

    for number, line in prose:
        if line.strip() == "":
            flush()
            prev = number
            continue
        if prev is not None and number != prev + 1:
            flush()
        block.append((number, line))
        prev = number
    flush()
    return found


def _problems(doc, text, root=ROOT, allowlist=ALLOWLIST):
    try:
        label = doc.relative_to(root)
    except ValueError:
        label = Path(doc.name)
    found = []
    for number, span, path in _cited_paths(text):
        if path in allowlist:
            continue
        if not (root / path).exists():
            found.append(f"{label}:{number}: `{span}` -> missing file `{path}`")
    return found


class DocsPathConformance(unittest.TestCase):
    def test_corpus_is_found(self):
        self.assertIn(ROOT / "docs" / "stream-funnel.md", DOCS)
        self.assertIn(ROOT / "CONTRIBUTING.md", DOCS)
        self.assertIn(ROOT / "docs" / "testing" / "retention-runtime.md", DOCS)
        self.assertIn(ROOT / "fuzz" / "README.md", DOCS)
        self.assertGreater(len(DOCS), 200)

    def test_wrapped_corpus_citations_are_scanned(self):
        prod = (ROOT / "docs" / "production-deploy.md").read_text(encoding="utf-8")
        prod_cited = _cited_paths(prod)
        self.assertIn(
            "scripts/production_deploy.py", [path for _, _, path in prod_cited]
        )
        runbook = (ROOT / "docs" / "runbook.md").read_text(encoding="utf-8")
        runbook_cited = _cited_paths(runbook)
        self.assertIn(
            "scripts/ci/staging_schema_gate.py",
            [path for _, _, path in runbook_cited],
        )
        smoke = (ROOT / "docs" / "smoke-expected-responses.md").read_text(
            encoding="utf-8"
        )
        smoke_cited = _cited_paths(smoke)
        router_citations = [
            n for n, _, p in smoke_cited if p.startswith("crates/core/src/router")
        ]
        self.assertGreaterEqual(
            len(router_citations), 6, "parity-shifted router citations unscanned"
        )

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
            (root / ".github" / "workflows").mkdir(parents=True)
            (root / "wrangler" / "scripts").mkdir(parents=True)
            (root / "scripts" / "present.py").write_text("print(1)\n", encoding="utf-8")
            (root / "crates" / "table.rs").write_text("a\nb\n", encoding="utf-8")
            (root / ".github" / "scripts" / "real.py").write_text("x\n", encoding="utf-8")
            (root / ".github" / "workflows" / "real.yml").write_text("x\n", encoding="utf-8")
            (root / "wrangler" / "scripts" / "present.py").write_text("x\n", encoding="utf-8")
            doc = root / "docs" / "fixture.md"
            allowlist = {"scripts/retired.py": "test fixture"}
            # One stale pin per PATH_RE branch so a dropped prefix goes red.
            stale_cases = {
                "scripts": ("witness (`scripts/absent-scripts.py`)\n", "scripts/absent-scripts.py"),
                "wrangler": (
                    "witness (`wrangler/scripts/absent-wrangler.py`)\n",
                    "wrangler/scripts/absent-wrangler.py",
                ),
                "github-scripts": (
                    "witness (`.github/scripts/absent-ghscripts.py`)\n",
                    ".github/scripts/absent-ghscripts.py",
                ),
                "workflows": (
                    "witness (`.github/workflows/absent-workflow.yml`)\n",
                    ".github/workflows/absent-workflow.yml",
                ),
                "crates": ("witness (`crates/absent-crate.rs`)\n", "crates/absent-crate.rs"),
                "dotted span-start": (
                    "witness (`./scripts/absent-dotted.py`)\n",
                    "scripts/absent-dotted.py",
                ),
                "dotted mid-span command": (
                    "run (`python3 ./scripts/absent-dotted-cmd.py`)\n",
                    "scripts/absent-dotted-cmd.py",
                ),
                "wrapped span": (
                    "run `scripts/absent-wrapped.py\nwith args`\n",
                    "scripts/absent-wrapped.py",
                ),
                "parity-shifted closer": (
                    "note `long expected\nresponse.` (`scripts/absent-parity.py`)\n",
                    "scripts/absent-parity.py",
                ),
                "fence info with backticks": (
                    "``` `x` ```\nwitness (`scripts/absent-fence.py`)\n",
                    "scripts/absent-fence.py",
                ),
                "redirect-attached out": (
                    "run (`python3 scripts/absent-redirect.py>out.log`)\n",
                    "scripts/absent-redirect.py",
                ),
                "redirect-attached in": (
                    "run (`python3 scripts/absent-redirect-in.py<in.txt`)\n",
                    "scripts/absent-redirect-in.py",
                ),
                "angle-wrapped": (
                    "witness (`<scripts/absent-angle.py>`)\n",
                    "scripts/absent-angle.py",
                ),
                "brace-wrapped": (
                    "witness (`{scripts/absent-brace.py}`)\n",
                    "scripts/absent-brace.py",
                ),
                "nested double span": (
                    "note `` `scripts/absent-double.py` ``\n",
                    "scripts/absent-double.py",
                ),
                "nested triple span": (
                    "note ``` `scripts/absent-triple.py` ```\n",
                    "scripts/absent-triple.py",
                ),
                "unpaired backtick across blank line": (
                    "stray ` backtick\n\nwitness (`scripts/absent-para.py`)\n",
                    "scripts/absent-para.py",
                ),
            }
            for name, (snippet, expected) in stale_cases.items():
                with self.subTest(name):
                    problems = _problems(doc, snippet, root=root, allowlist=allowlist)
                    self.assertEqual(len(problems), 1, problems)
                    self.assertIn(expected, problems[0])
            live = (
                "present (`scripts/present.py`)\n"
                "with args (`scripts/present.py --flag`)\n"
                "mid-span command (`node scripts/present.py 1`)\n"
                "dotted mid-span command (`python3 ./scripts/present.py`)\n"
                "rust item (`crates/table.rs::tests::foo`)\n"
                "line list (`crates/table.rs:1-2,3`)\n"
                "dotted (`./scripts/present.py`)\n"
                "github scripts (`.github/scripts/real.py`)\n"
                "workflows (`.github/workflows/real.yml`)\n"
                "wrangler (`wrangler/scripts/present.py`)\n"
                "glob (`crates/*/migrations`)\n"
                "partial-segment glob (`scripts/test_*.py`)\n"
                "placeholder-first (`crates/<crate>/migrations`)\n"
                "brace glob (`scripts/{a,b}.py`)\n"
                "wrapped live `scripts/present.py\nwith args`\n"
                "\n"
                "parity live `long expected\nresponse.` (`crates/table.rs`)\n"
                "retired (`scripts/retired.py`)\n"
                "```python\n"
                "`scripts/absent.py`\n"
                "```\n"
            )
            self.assertEqual(_problems(doc, live, root=root, allowlist=allowlist), [])
            # Wrapped citations must be extracted, not merely invisible: both the
            # wrapped-live path and the parity-live path appear in `_cited_paths`.
            cited = [path for _, _, path in _cited_paths(live)]
            self.assertIn("scripts/present.py", cited)
            self.assertIn("crates/table.rs", cited)


if __name__ == "__main__":
    unittest.main()
