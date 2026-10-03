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
  COMMITS                JSON array of {id, message} for push events
  REPO_PRIVATE           "true" | "false"; decides whether internal ticket references are allowed
  REQUIRE_CARD_REF       "true" | "false"; private repos only: demand `Refs: TOG-1234` in the body
  PR_STANDARDS_MODE      "warn" | "error"; level for the upstream body/branch/reference rules (default warn)
  INTERNAL_ID_PREFIXES   regex alternation of internal ticket prefixes (default TOG|PAP|PAPA)

Title format, an empty body and a missing required card reference are always errors: they are the
standard the org already enforces. Everything new is `PR_STANDARDS_MODE` so a repo can land the
gate in warn mode, clear its open PRs, then flip one line to `error`.
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
    ("Thinking Path", 3, False),
    ("Linked Issues or Issue Description", 1, True),
    ("What Changed", 1, False),
    ("Verification", 1, False),
    ("Risks", 1, False),
    ("Model Used", 1, False),
]

MODEL_PLACEHOLDERS = ("provider, model id", "your model", "<model>", "[provider")

DEDUP_CHECKBOX = re.compile(
    r"^\s*[-*]\s*\[\s*([ xX])\s*\][^\n]*search(?:ed)?[^\n]*(?:similar|duplicate|prior|related)[^\n]*\bprs?\b",
    re.I | re.M,
)

COMMENT = re.compile(r"<!--.*?-->", re.S)

# Instance-local references a public contributor cannot open (upstream "No Internal Issue References").
# The ticket-id pattern is built per call from INTERNAL_ID_PREFIXES; these are the fixed ones.
FIXED_INTERNAL = [
    (re.compile(r"\bagent://"), "an agent:// link"),
    (re.compile(r"(?:^|[\s(<\[])/[A-Z]{2,6}/(?:issues|agents|projects|approvals|runs)/"), "an instance UI link"),
    (re.compile(r"\b(?:localhost|127\.0\.0\.1)\b"), "a localhost URL"),
    (re.compile(r"\b(?:10\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3})\b"),
     "a private-network address"),
    (re.compile(r"\b[\w-]+(?:\.[\w-]+)*\.ts\.net\b"), "a tailnet hostname"),
    (re.compile(r"\binfextion\.net\b"), "the internal Paperclip host"),
]


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


def internal_hits(text, prefixes):
    text = strip_comments(text)
    hits = []
    m = re.search(rf"\b(?:{prefixes})-\d+\b", text)
    if m:
        hits.append(f"ticket id {m.group(0)}")
    for pat, label in FIXED_INTERNAL:
        if pat.search(text):
            hits.append(label)
    return hits


def check_pull_request(env):
    mode = "error" if env.get("PR_STANDARDS_MODE", "warn").strip().lower() == "error" else "warning"
    prefixes = env.get("INTERNAL_ID_PREFIXES") or "TOG|PAP|PAPA"
    private = env.get("REPO_PRIVATE", "true").strip().lower() == "true"
    title = (env.get("TITLE") or "").strip()
    body = env.get("BODY") or ""
    author = env.get("AUTHOR") or ""
    head_ref = env.get("HEAD_REF") or ""
    out = []

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
        out.append(Finding("error", "PR body", "The description is empty. Fill in the PR template: thinking path, "
                           "what changed, how it was verified, risks and the model used."))
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
            if heading == "Model Used" and any(p in content.lower() for p in MODEL_PLACEHOLDERS):
                out.append(Finding(mode, "PR template", "'## Model Used' still holds placeholder text. Name the "
                                   "model and version, or write 'None - human-authored'."))
            elif real_items(content) < minimum:
                need = f"at least {minimum} items" if minimum > 1 else "real content"
                out.append(Finding(mode, "PR template", f"'## {heading}' needs {need}."))
        if ptype and ptype not in LIGHT_TYPES:
            m = DEDUP_CHECKBOX.search(plain)
            if not m or m.group(1) == " ":
                out.append(Finding(mode, "Duplicate search", "Tick the checklist line saying you searched for "
                                   "duplicate or related PRs, and link any you found."))

    if private:
        if not generated and not re.search(r"\bTOG-\d+\b", body):
            level = "error" if env.get("REQUIRE_CARD_REF", "true").strip().lower() == "true" else "warning"
            out.append(Finding(level, "Card reference", "Add 'Refs: TOG-1234' to the PR body."))
    else:
        for where, text in (("title", title), ("body", body), ("branch name", head_ref)):
            hits = internal_hits(text, prefixes)
            if hits:
                out.append(Finding(mode, "Internal reference", f"The PR {where} holds {', '.join(hits)}. Public "
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
        errs = header_errors(subject)
        if errs:
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
