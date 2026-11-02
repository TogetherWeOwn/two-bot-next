#!/usr/bin/env python3
"""PR standards gate: Conventional Commits title, upstream-shaped body, no internal refs on public repos.

Runs inside the existing required `pr-lint` job, so adopting it adds no new required check and
needs no ruleset change. Stdlib only. Adapted from the PR quality gates in paperclipai/paperclip
(`.github/scripts/check-pr-*.mjs`, MIT): same section names, same skip-by-type rules, same
"no internal issue references" rule. Greptile, the commitperclip bot, lockfile and AWS routing
parts of upstream are deliberately not adopted.

Environment contract (every value is a plain string):
  EVENT                  pull_request | push | workflow_dispatch   (anything but pull_request lints commits)
  TITLE, BODY, AUTHOR    pull request title, body, author login
  HEAD_REF               pull request head branch name
  CHANGED_FILES          newline-separated repository-relative paths changed by the PR.
                         Renames carry both the old and the new path (one per line);
                         deletes are listed like any other change. Absent or empty
                         skips the attestation gate (push events carry no PR diff;
                         the workflow fails the job when the PR file list cannot
                         be produced, so a PR cannot reach the gate without one).
  COMMITS                JSON array of {id, message} for push events
  REPO_PRIVATE           "true" | "false"; decides whether internal ticket references are allowed
  REQUIRE_CARD_REF       "true" | "false"; private repos only: demand `Refs: TOG-1234` in the body
  PR_STANDARDS_MODE      "warn" | "error"; level for the upstream body/branch/reference rules (default warn)
  INTERNAL_ID_PREFIXES   regex alternation of internal ticket prefixes (default TOG|PAP|PAPA)

Title format, an empty body and a missing required card reference are always errors: they are the
standard the org already enforces. On a public repo the strict title/body subset (tracker ids,
instance UI links, the internal host) is also always an error, regardless of PR_STANDARDS_MODE.
A PR whose changed paths hit the T0 risk floor must carry a complete T0 attestation
(`risk_tier: T0`, non-empty Claims and Tests subsections, checked D1–D8, no leftover
placeholders): that too is always an error, regardless of PR_STANDARDS_MODE, the same
way internal references already are. The floor runs before the dependency-bot exemption,
so bot PRs touching T0 paths need a human-written attestation; only generated release
PRs authored by a bot are exempt.
Only the required template sections (beyond an empty body), the advisory title/body remainder
(agent links, localhost, private-network addresses, tailnet names), branch names and commit
history follow PR_STANDARDS_MODE, so a repo can land the gate in warn mode, clear its open
PRs, then flip one line to `error`.
"""
import json
import os
import re
import sys
from dataclasses import dataclass

TYPES = "feat|fix|perf|refactor|test|docs|build|ci|chore|revert|style|security"
HEADER = re.compile(rf"^({TYPES})(\([A-Za-z0-9._/,-]+\))?!?: \S.*$")
HELP = (
    "Expected a Conventional Commits header, e.g. 'fix(auth): refuse expired sudo sessions' or "
    "'feat(events): shareable event page'. Allowed types: " + TYPES.replace("|", ", ") + "."
)

# Types that need neither a linked issue nor the duplicate-search tick (upstream SKIP_ISSUE_PREFIXES).
LIGHT_TYPES = {"docs", "chore", "build", "ci", "style", "test", "revert"}

REQUIRED_SECTIONS = [
    # (heading, minimum real items, skipped for LIGHT_TYPES)
    # Pipeline review 2026-10-10: only what a reviewer or a deterministic gate needs. The tracked card or
    # issue, and the test evidence. Thinking Path, What Changed, Risks, Model Used and the checklist ticks
    # are optional prose; the reviewers read the diff itself.
    ("Linked Issues or Issue Description", 1, True),
    ("Verification", 1, False),
]

COMMENT = re.compile(r"<!--.*?-->", re.S)

# Instance-local references a public contributor cannot open (upstream "No Internal Issue References").
# The ticket-id pattern is built per call from INTERNAL_ID_PREFIXES; these are the fixed ones.
#
# Forward control (public-text lint): on a public repo the PR title and body must not carry a
# strict tracker id, an instance UI link, or the internal host. Those three always fail the
# title/body check, regardless of PR_STANDARDS_MODE.
# The remaining fixed patterns (agent links, localhost, private-network addresses, tailnet names)
# stay at PR_STANDARDS_MODE: they are common in legitimate verification prose.
INSTANCE_UI = (re.compile(r'(?:^|[\s(<\[`"\'])/[A-Z]{2,6}/(?:issues|agents|projects|approvals|runs)/'), "an instance UI link")
HOST_INTERNAL = (re.compile(r"\binfextion\.net\b", re.I), "the internal Paperclip host")
STRICT_FIXED = [INSTANCE_UI, HOST_INTERNAL]
OTHER_FIXED = [
    (re.compile(r"\bagent://"), "an agent:// link"),
    (re.compile(r"\b(?:localhost|127\.0\.0\.1)\b"), "a localhost URL"),
    (re.compile(r"\b(?:10\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3})\b"),
     "a private-network address"),
    (re.compile(r"\b[\w-]+(?:\.[\w-]+)*\.ts\.net\b"), "a tailnet hostname"),
]
# Single source for the fixed patterns: the full scan is the strict subset plus
# the advisory remainder, interleaved to keep the long-standing finding order.
FIXED_INTERNAL = OTHER_FIXED[:1] + [INSTANCE_UI] + OTHER_FIXED[1:] + [HOST_INTERNAL]

# T0 risk floor: the path list from the Attestation section of
# .github/pull_request_template.md. Word/directory sets mirror the central
# TogetherWeOwn/.github attestation validator so the required gate and the
# shadow observation agree on what counts as T0.
T0_DIRECTORIES = frozenset({"migrations", "sql", "db", "database", "drizzle"})
T0_WORDS = frozenset({
    "auth", "authn", "authz", "authentication", "authorization", "oauth",
    "session", "sessions", "billing", "payment", "payments", "permission", "permissions",
    "gateway", "voice", "crypto", "vault",
    "secret", "secrets", "backup", "backups", "dump", "dumps",
    "grant", "grants", "role", "roles", "erasure", "ingest",
})
T0_PATH_PATTERNS = (
    re.compile(r"^\.github/workflows/deploy[^/]*(?:/.*)?$"),
    re.compile(r"(?:^|/)wrangler(?:[.-][^/]*)?\.(?:toml|jsonc?)$"),
    re.compile(r"^crates/cutover/"),
)
T0_DOC_SUFFIXES = (".md", ".rst", ".txt")
T0_RISK_TIER = re.compile(r"^risk_tier:\s*(T0|T1|T2)\s*$", re.I | re.M)
T0_CHECK_ITEM = re.compile(r"^-\s*\[( |x|X)\]\s*(D[1-8]):\s*(.+?)\s*$")
# Template placeholders either carry a space, a pipe or a slash
# (<secret sinks, ...>, <T0|T1|T2>, <path/to/test_file>) or are bare template
# tokens (<test_name>). Real generics such as Vec<u8> or Option<T> always have
# the `<` glued to an identifier, so the bare-token expression requires a
# non-identifier boundary before the `<`. Autolinks are stripped before
# matching so evidence links never count as placeholders; inline code is kept
# visible so backticked placeholders still fail while `Vec<u8>` passes via
# the boundary rule. Spaced generics such as `Result<T, E>` are excluded only
# when the `<` is glued to a type name and every blank sits right after a
# comma; a standalone `<command, result>` (after `=`, a space or line start)
# is an unfilled proof value, not a generic, so it still fails.
T0_PLACEHOLDER = re.compile(r"<[^<>\n]*[ |/][^<>\n]*>")
T0_BARE_PLACEHOLDER = re.compile(r"(?<![A-Za-z0-9_])<[A-Za-z][A-Za-z0-9_]*>")
T0_AUTOLINK = re.compile(r"<https?://[^<>\s]+>")
T0_CLAIM_REF = re.compile(r"\bC\d+\b", re.I)
T0_TEST_MAPPING = re.compile(r"\b(?:test|proof|failed_before)\s*=", re.I)


def t0_visible_text(text):
    """Attestation prose minus autolinks, for placeholder scans."""
    return T0_AUTOLINK.sub("", text or "")


def _looks_like_generic_list(before, content):
    """True for a comma-separated type list glued to a type name, such as the
    `<T, E>` in `Result<T, E>` or the turbofish in `Ok::<_, E>`: `before`
    (up to three chars ahead of `<`) ends an identifier or a nested generic,
    optionally through Rust `::`, a comma is present, and every blank sits
    right after one. Template placeholders either carry a blank inside a
    phrase (`<secret sinks, ...>`) or stand alone (`proof=<command, result>`,
    `note:<a, b>`), so they never match both halves at once."""
    if not re.search(r"[A-Za-z0-9_>\]](?:::)?$", before):
        return False
    blanks = [m.start() for m in re.finditer(r"\s", content)]
    return bool(blanks) and "," in content and all(
        content[pos - 1] == "," for pos in blanks)


def t0_has_placeholder(text):
    """True when visible attestation text still holds a template placeholder."""
    visible = t0_visible_text(text)
    for match in T0_PLACEHOLDER.finditer(visible):
        before = visible[max(0, match.start() - 3):match.start()]
        if not _looks_like_generic_list(before, match.group(0)[1:-1]):
            return True
    return bool(T0_BARE_PLACEHOLDER.search(visible))


def t0_path_words(component):
    """Split one path component the way the template defines words: punctuation,
    snake_case, kebab-case and CamelCase boundaries (auth.rs, gateway_backoff.rs,
    member-erasure.ts, SessionController.php)."""
    component = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1_\2", component)
    component = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", component)
    return set(re.split(r"[^a-z0-9]+", component.lower()))


def is_t0_path(path):
    """True when one changed path hits the T0 floor. Prose files (.md/.rst/.txt)
    are not modules, but still inherit a critical directory floor; substrings
    such as authors.py, voices/ and roleplay.ts do not match the word list."""
    if any(pat.search(path) for pat in T0_PATH_PATTERNS):
        return True
    parts = path.split("/")
    if any(part.lower() in T0_DIRECTORIES for part in parts[:-1]):
        return True
    components = parts[:-1] if path.lower().endswith(T0_DOC_SUFFIXES) else parts
    return any(t0_path_words(part) & T0_WORDS for part in components)


def attestation_findings(body, paths):
    """T0 attestation gate. Empty unless a changed path hits the T0 floor, in
    which case the body must declare `risk_tier: T0`, carry a `### Claims`
    subsection with C-numbered claims and a `### Tests` subsection with one
    test-mapping line per claim, and carry all of D1–D8 checked with real
    evidence (no leftover `<placeholder>` text anywhere in the Attestation
    section, backticked or bare). Every finding is an error: declaring a
    higher tier than the floor is fine, understating it is not. The gate
    checks the shape and presence of the claim-to-test mapping, not test
    execution, changed-file references or the truth of claims."""
    t0_paths = sorted({p for p in paths if is_t0_path(p)})
    if not t0_paths:
        return []
    sections = parse_sections(body)
    att = sections.get("attestation")
    if att is None:
        return [Finding("error", "Attestation",
                        f"This PR touches T0 paths ({', '.join(t0_paths[:3])}"
                        f"{', …' if len(t0_paths) > 3 else ''}) but the body has no "
                        "'## Attestation' section. Declare `risk_tier: T0` with the "
                        "claim-to-test mapping and checked D1–D8.")]
    out = []
    tier = T0_RISK_TIER.search(att)
    if tier is None or tier.group(1).upper() != "T0":
        out.append(Finding("error", "Attestation",
                           f"This PR touches T0 paths ({', '.join(t0_paths[:3])}"
                           f"{', …' if len(t0_paths) > 3 else ''}) but does not declare "
                           "`risk_tier: T0`. The floor is a minimum: declaring T0 for "
                           "a critical change on any path is always fine."))
    for heading, marker, hint in (
        ("Claims", T0_CLAIM_REF,
         "one behavior claim per C-number (C1, C2, ...)"),
        ("Tests", T0_TEST_MAPPING,
         "one test-mapping line per claim "
         "(test=... | failed_before=... | proof=...)"),
    ):
        m = re.search(rf"^###\s*{heading}\s*$", att, flags=re.M | re.I)
        chunk = re.split(r"^###\s+", att[m.end():], flags=re.M)[0] if m else ""
        if (m is None or real_items(t0_visible_text(chunk)) < 1
                or not marker.search(t0_visible_text(chunk))):
            out.append(Finding("error", "Attestation",
                               f"T0 PRs need a non-empty '### {heading}' subsection: "
                               f"{hint}."))
    items = {}
    for line in att.splitlines():
        m = T0_CHECK_ITEM.match(line.strip())
        if m:
            items[m.group(2).upper()] = (m.group(1).lower() == "x", m.group(3))
    bad = [f"D{i}" for i in range(1, 9)
           if f"D{i}" not in items or not items[f"D{i}"][0]
           or not items[f"D{i}"][1].strip()
           or t0_has_placeholder(items[f"D{i}"][1])]
    if bad:
        out.append(Finding("error", "Attestation",
                           f"T0 PRs need all of D1–D8 checked with evidence, not "
                           f"placeholders. Fix: {', '.join(bad)}."))
    # Leftover placeholders in the rest of the section (claims, test mapping):
    # the risk_tier and D-lines report their own findings above.
    rest = T0_RISK_TIER.sub("", re.sub(r"^-\s*\[[ xX]\]\s*D[1-8]:.*$", "", att,
                                            flags=re.M | re.I))
    if t0_has_placeholder(rest):
        out.append(Finding("error", "Attestation",
                           "The Attestation section still holds placeholder text "
                           "(<...>). Replace every placeholder with evidence or "
                           "not-applicable plus a concrete reason."))
    return out


@dataclass
class Finding:
    level: str  # "error" | "warning"
    title: str
    message: str


def pr_type(title):
    m = re.match(rf"^({TYPES})(?:\([^)]*\))?!?:", title or "")
    return m.group(1) if m else None


def header_errors(text):
    errs = []
    if not HEADER.match(text):
        errs.append("not a Conventional Commits header")
    if len(text) > 100:
        errs.append(f"{len(text)} chars (max 100)")
    if text.rstrip().endswith("."):
        errs.append("ends with a period")
    return errs


def strip_comments(text):
    return COMMENT.sub("", text or "")


def parse_sections(body):
    """Map normalised `## Heading` -> content. Comments are removed first."""
    sections, current, buf = {}, None, []
    for line in strip_comments(body).splitlines():
        m = re.match(r"^##\s+(.+?)\s*#*\s*$", line)
        if m:
            if current is not None:
                sections[current] = "\n".join(buf).strip()
            current, buf = m.group(1).strip().lower(), []
        elif current is not None:
            buf.append(line)
    if current is not None:
        sections[current] = "\n".join(buf).strip()
    return sections


def real_items(content):
    """Count meaningful items: bullets, quoted bullets or prose lines, minus placeholders."""
    n = 0
    for raw in content.splitlines():
        line = re.sub(r"^\s*(?:>\s*)*(?:[-*]\s+|\d+[.)]\s+)?", "", raw).strip()
        line = re.sub(r"^\[[ xX]\]\s*", "", line)
        if len(line) < 6 or line in {"-", "_No response_"}:
            continue
        if re.fullmatch(r"\[[^\]]*\]", line):  # `[Which subsystem is involved]`
            continue
        if line.endswith((" ...", " …")):  # `This pull request ...`
            continue
        n += 1
    return n


def internal_hits(text, prefixes, ignore_case=False):
    """Ticket ids stay case-sensitive in prose (`tog-1` is plain text); branch names are slugs, so pass ignore_case."""
    text = strip_comments(text)
    hits = []
    m = re.search(rf"\b(?:{prefixes})-\d+\b", text, re.I if ignore_case else 0)
    if m:
        hits.append(f"ticket id {m.group(0)}")
    for pat, label in FIXED_INTERNAL:
        if pat.search(text):
            hits.append(label)
    return hits


def strict_hits(text, prefixes):
    """Fail-closed subset for public title/body: tracker ids, instance UI links, internal host.

    Tracker ids stay case-sensitive in prose; the internal host matches in any case
    (hostnames are case-insensitive). Scans the raw text, including HTML comments:
    comment text is still stored and served. Returns the ticket-id label with the
    matched value and the other labels without echoing the matched value."""
    text = text or ""
    hits = []
    m = re.search(rf"\b(?:{prefixes})-\d+\b", text)
    if m:
        hits.append(f"ticket id {m.group(0)}")
    for pat, label in STRICT_FIXED:
        if pat.search(text):
            hits.append(label)
    return hits


def other_hits(text):
    """Advisory remainder for public title/body: agent links, localhost, private nets, tailnet."""
    text = strip_comments(text)
    return [label for pat, label in OTHER_FIXED if pat.search(text)]


def check_pull_request(env):
    mode = "error" if env.get("PR_STANDARDS_MODE", "warn").strip().lower() == "error" else "warning"
    prefixes = env.get("INTERNAL_ID_PREFIXES") or "TOG|PAP|PAPA"
    private = env.get("REPO_PRIVATE", "true").strip().lower() == "true"
    title = (env.get("TITLE") or "").strip()
    body = env.get("BODY") or ""
    author = env.get("AUTHOR") or ""
    head_ref = env.get("HEAD_REF") or ""
    out = []

    # T0 attestation floor first: it runs before the dependency-bot early
    # return, so a bot PR touching a T0 path still needs a (human-written)
    # attestation. Generated release PRs authored by a bot stay exempt
    # (automation cannot attest); a human branch wearing the release-please--
    # prefix is not exempt, since the caller controls the branch name.
    generated_release = head_ref.startswith("release-please--") and author.endswith("[bot]")
    if not generated_release:
        changed = [line.strip() for line in (env.get("CHANGED_FILES") or "").splitlines()
                   if line.strip()]
        out.extend(attestation_findings(body, changed))
    if author.endswith("[bot]") and author.startswith(("dependabot", "renovate")):
        if header_errors(title):
            out.append(Finding("warning", "PR title", f"{author} title '{title}' is not conventional; set "
                               "commit-message.prefix (e.g. chore(deps)) in the bot config."))
        return out
    generated = head_ref.startswith("release-please--")

    errs = header_errors(title)
    if errs:
        out.append(Finding("error", "PR title", f"'{title}': {'; '.join(errs)}. {HELP}"))

    plain = strip_comments(body)
    if len(re.sub(r"\s", "", plain)) < 40:
        out.append(Finding("error", "PR body", "The description is empty. Fill in the PR template: the linked card or issue and "
                           "how it was verified."))
    elif not generated:
        ptype = pr_type(title)
        light = ptype in LIGHT_TYPES
        sections = parse_sections(body)
        for heading, minimum, skip_light in REQUIRED_SECTIONS:
            if skip_light and light:
                continue
            content = sections.get(heading.lower())
            if content is None:
                out.append(Finding(mode, "PR template", f"Missing section '## {heading}'."))
                continue
            if real_items(content) < minimum:
                need = f"at least {minimum} items" if minimum > 1 else "real content"
                out.append(Finding(mode, "PR template", f"'## {heading}' needs {need}."))

    if private:
        if not generated and not re.search(r"\bTOG-\d+\b", body):
            level = "error" if env.get("REQUIRE_CARD_REF", "true").strip().lower() == "true" else "warning"
            out.append(Finding(level, "Card reference", "Add 'Refs: TOG-1234' to the PR body."))
    else:
        # Titles and bodies: the strict subset (tracker id, instance UI link, internal host)
        # always fails; the advisory remainder follows PR_STANDARDS_MODE. Branch names keep
        # the existing behavior; commit history is out of scope for this gate.
        for where, text in (("title", title), ("body", body)):
            strict = strict_hits(text, prefixes)
            if strict:
                out.append(Finding("error", "Internal reference",
                                   f"The PR {where} holds {', '.join(strict)}. Public "
                                   "repos carry no internal references: say it in plain words, link only public "
                                   "GitHub issues, and name the branch after the change (fix/short-slug)."))
            rest = other_hits(text)
            if rest:
                out.append(Finding(mode, "Internal reference",
                                   f"The PR {where} holds {', '.join(rest)}. Public "
                                   "repos carry no internal references: say it in plain words, link only public "
                                   "GitHub issues, and name the branch after the change (fix/short-slug)."))
        hits = internal_hits(head_ref, prefixes, ignore_case=True)
        if hits:
            out.append(Finding(mode, "Internal reference", f"The PR branch name holds {', '.join(hits)}. Public "
                               "repos carry no internal references: say it in plain words, link only public "
                               "GitHub issues, and name the branch after the change (fix/short-slug)."))
    return out


def check_commits(env):
    mode = "error" if env.get("PR_STANDARDS_MODE", "warn").strip().lower() == "error" else "warning"
    prefixes = env.get("INTERNAL_ID_PREFIXES") or "TOG|PAP|PAPA"
    private = env.get("REPO_PRIVATE", "true").strip().lower() == "true"
    out = []
    try:
        commits = json.loads(env.get("COMMITS") or "[]") or []
    except ValueError:
        commits = []
    for c in commits:
        message = c.get("message") or ""
        subject = message.splitlines()[0] if message else ""
        if subject.startswith(("Merge ", 'Revert "')):
            continue
        short = (c.get("id") or "")[:10]
        author_obj = c.get("author") or {}
        author = ""
        if isinstance(author_obj, dict):
            author = str(author_obj.get("username") or author_obj.get("login")
                         or author_obj.get("name") or "")
        elif isinstance(author_obj, str):
            author = author_obj
        is_bot = author.endswith("[bot]") and author.startswith(("dependabot", "renovate"))
        errs = header_errors(subject)
        if errs:
            if is_bot:
                out.append(Finding("warning", "Commit on main", f"{short} '{subject}': {'; '.join(errs)}. "
                                   "Bot commit; set commit-message.prefix (e.g. chore(deps)) in the bot config."))
            else:
                out.append(Finding("error", "Commit on main", f"{short} '{subject}': {'; '.join(errs)}. Merge PRs with "
                                   "squash and the PR title as the commit title."))
        if not private:
            hits = internal_hits(message, prefixes)
            if hits:
                out.append(Finding(mode, "Internal reference", f"Commit {short} holds {', '.join(hits)}. Public "
                                   "history carries no internal references."))
    return out


def evaluate(env):
    return check_pull_request(env) if env.get("EVENT") == "pull_request" else check_commits(env)


def _escape(text):
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def main(env=None):
    env = dict(os.environ) if env is None else env
    findings = evaluate(env)
    for f in findings:
        print(f"::{f.level} title={_escape(f.title)}::{_escape(f.message)}")
    if any(f.level == "error" for f in findings):
        return 1
    print("PR conventions OK" if not findings else "PR conventions OK (warnings above)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
