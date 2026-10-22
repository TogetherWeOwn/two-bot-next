#!/usr/bin/env python3
"""Lint risky Postgres migration statements (squawk subset).

Scans changed migration files for statements that take disruptive locks or
break running application code. Deterministic: standard library only, no
database, no network, no third-party packages. See docs/migrations.md.

Rules use squawk-compatible IDs so the findings map 1:1 onto a future
squawk binary or shared reusable workflow:
- adding-required-field: ADD COLUMN with NOT NULL and no DEFAULT.
- creating-index: CREATE INDEX without CONCURRENTLY.
- adding-check-constraint: ADD CONSTRAINT CHECK without NOT VALID.
- changing-column-type: ALTER COLUMN ... TYPE rewrites the table.
- dropping-object: DROP TABLE or DROP COLUMN breaks running code.

Advisory by default: findings print as ::warning annotations and exit 0.
--strict makes the first four rules blocking; dropping-object remains a
warning because coordinated removals still require a final DROP. Tooling
failures (unresolvable baseline, unreadable file) always exit 1 so blindness
can never read as clean.
"""

import argparse
import fnmatch
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_GLOB = "crates/*/migrations/*.sql"

# Bounded-subprocess policy (docs/bounded-subprocess-checks.md): every child
# wait is finite so a stalled child fails closed here instead of hanging
# until the external CI ceiling. Local Git reads answer in well under a
# second; 30s bounds the stall without touching valid runs. Both Git calls
# below share this one value.
GIT_TIMEOUT_SECONDS = 30

RULES = (
    "adding-required-field",
    "creating-index",
    "adding-check-constraint",
    "changing-column-type",
    "dropping-object",
)
STRICT_RULES = frozenset(RULES) - {"dropping-object"}

# These exact transactional indexes are narrowly waived: concurrent builds
# deadlocked CI's parallel disposable-database migrators (40P01) while they
# waited on SQLx's advisory lock with open snapshots, and CONCURRENTLY
# cannot run inside sqlx's migration transaction at all. The 0456/0457
# entries index a table created earlier in the same file: empty at first
# apply, and the IF NOT EXISTS re-apply is a no-op for both statements.
# Keep every other creating-index finding strict, including any other
# statement in these files.
TRANSACTIONAL_INDEX_WAIVERS = {
    (
        "crates/cutover/migrations/0444_event_store_expiry_index.sql",
        "create index if not exists idx_event_store_expiry_event_time_id "
        "on event_store (event_time, id)",
    ),
    (
        "crates/cutover/migrations/0445_event_store_expiry_class_index.sql",
        "create index if not exists idx_event_store_expiry_class_time_id "
        "on event_store (event_type, event_time, id)",
    ),
    (
        "crates/cutover/migrations/0456_admin_api_tokens.sql",
        "create index if not exists idx_admin_api_tokens_live "
        "on admin_api_tokens (revoked_at, expires_at)",
    ),
    (
        "crates/store/migrations/0457_admin_api_tokens.sql",
        "create index if not exists idx_admin_api_tokens_live "
        "on admin_api_tokens (revoked_at, expires_at)",
    ),
}

ADVICE = {
    "adding-required-field": (
        "ADD COLUMN with NOT NULL and no DEFAULT rewrites the table under an "
        "exclusive lock; add the column nullable with a DEFAULT (or backfill), "
        "then SET NOT NULL in a later migration"
    ),
    "creating-index": (
        "CREATE INDEX without CONCURRENTLY blocks writes on existing tables; "
        "use CREATE INDEX CONCURRENTLY in a separate migration run outside "
        "a transaction (sqlx no-transaction). Indexes on the same qualified "
        "relation created unconditionally earlier in the same file target "
        "an empty table and are not flagged; CREATE TABLE IF NOT EXISTS "
        "earns no such exemption"
    ),
    "adding-check-constraint": (
        "CHECK constraint without NOT VALID scans and locks the table; "
        "add it as NOT VALID, then VALIDATE CONSTRAINT separately"
    ),
    "changing-column-type": (
        "ALTER COLUMN ... TYPE rewrites the table; prefer an additive "
        "migration (new column, backfill, swap)"
    ),
    "dropping-object": (
        "DROP TABLE / DROP COLUMN breaks running application code; "
        "coordinate removal across deploys instead of dropping in one step"
    ),
}

ADD_KEYWORD = re.compile(r"\bADD\s+", re.IGNORECASE)
ADD_NON_COLUMN_REST = re.compile(
    r"(?:CONSTRAINT|PRIMARY|UNIQUE|FOREIGN|CHECK)\b", re.IGNORECASE
)
COLUMN_PREFIX = re.compile(
    r"(?:COLUMN\s+)?(?:IF\s+NOT\s+EXISTS\s+)?", re.IGNORECASE
)
NOT_NULL = re.compile(r"\bNOT\s+NULL\b", re.IGNORECASE)
DEFAULT = re.compile(r"\bDEFAULT\b", re.IGNORECASE)
CREATE_INDEX = re.compile(r"\bCREATE\s+(?:UNIQUE\s+)?INDEX\b", re.IGNORECASE)
CONCURRENTLY = re.compile(r"\bCONCURRENTLY\b", re.IGNORECASE)
ADD_CHECK = re.compile(r"\bADD\s+(?:CONSTRAINT\s+\S+\s+)?CHECK\b", re.IGNORECASE)
NOT_VALID = re.compile(r"\bNOT\s+VALID\b", re.IGNORECASE)
ALTER_TYPE = re.compile(
    r"\bALTER\s+COLUMN\s+\S+\s+(?:SET\s+DATA\s+)?TYPE\b"
    r"|\bALTER\s+(?!COLUMN\b|TABLE\b)\S+\s+(?:SET\s+DATA\s+)?TYPE\b",
    re.IGNORECASE,
)
DROP_OBJECT = re.compile(r"\bDROP\s+(?:TABLE|COLUMN)\b", re.IGNORECASE)
ALTER_TABLE_PREFIX = re.compile(
    r"^\s*ALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?\S+(?:\s*\*)?\s+",
    re.IGNORECASE,
)
BARE_DROP = re.compile(r"^\s*DROP\s+(?!CONSTRAINT\b)", re.IGNORECASE)
CREATE_TABLE = re.compile(
    r"\bCREATE\s+(?:(?:GLOBAL|LOCAL)\s+)?(?:TEMP(?:ORARY)?\s+|UNLOGGED\s+)?"
    r"TABLE\s+(?P<conditional>IF\s+NOT\s+EXISTS\s+)?(?:ONLY\s+)?"
    r'(?P<name>"[^"]+"|[A-Za-z_][A-Za-z0-9_\.]*)',
    re.IGNORECASE,
)
INDEX_TARGET = re.compile(
    r"\bCREATE\s+(?:UNIQUE\s+)?INDEX\s+(?:CONCURRENTLY\s+)?"
    r"(?:IF\s+NOT\s+EXISTS\s+)?"
    r'(?:(?:"[^"]+"|[A-Za-z_][A-Za-z0-9_\.]*)\s+)?'
    r"ON\s+(?:ONLY\s+)?"
    r'(?P<name>"[^"]+"|[A-Za-z_][A-Za-z0-9_\.]*)',
    re.IGNORECASE,
)
DOLLAR_OPEN = re.compile(r"\$[A-Za-z_][A-Za-z0-9_]*\$|\$\$")


def _normalize_part(part):
    """Normalise one dot-separated identifier part from blanked() output.

    blanked() maps every quoted identifier to a placeholder: simple
    lowercase names (``"guilds"``) become ``_qq_guilds_`` (the same
    placeholder for the same name) while every other quoted name becomes
    a case-sensitive ``_qq_<n>_`` token. Placeholders never match keyword
    regexes (``\\b`` cannot fire inside ``_..._``), so quoted keyword
    names such as ``"default"`` stay inert. Lowercasing folds unquoted
    names as Postgres does, and stripping the wrapper restores equality:
    ``_qq_guilds_`` compares equal to unquoted ``guilds``, while a
    case-sensitive ``_qq_<n>_`` never equals a folded unquoted name.
    """
    base = part.strip().lower()
    match = re.fullmatch(r"_qq_(.+)_", base)
    return match.group(1) if match else base


def normalize_relation(raw):
    """Return the (schema, table) identity of a possibly qualified relation.

    The same-file empty-table exemption belongs to the exact relation
    that was created, schema and table together: ``scratch.items`` must
    never exempt an index on ``public.items``, which may already hold
    data. An unqualified name means the ``public`` schema (the one
    justified compatibility, in either direction); any other explicitly
    named schema is a different relation. Names with more than two
    parts keep every part, so an ambiguous identity only ever matches
    itself exactly and never broadens the exemption.
    """
    parts = tuple(_normalize_part(part) for part in raw.strip().split("."))
    if len(parts) == 1:
        return ("public", parts[0])
    return parts


class SquawkError(ValueError):
    pass


def blanked(text):
    """Return text with string, quoted-identifier, dollar-quoted and comment
    spans replaced by spaces (newlines preserved), so keyword matching and
    statement splitting never fire inside literals or comments.

    Every quoted identifier maps to a placeholder so a quoted keyword name
    such as ``"default"`` never reads as a live keyword. Simple lowercase
    names (``"guilds"``) emit ``_qq_guilds_`` (the same placeholder for the
    same name) so they compare equal to the unquoted form once
    _normalize_part() strips the wrapper, as Postgres folds unquoted names
    to lowercase. Every other quoted identifier maps to a unique
    placeholder per distinct name (``_qq_<n>_``), so two different quoted
    table names never collapse to one token while the same quoted name
    stays equal to itself.
    """
    out = []
    n = len(text)
    quoted_ids = {}
    i = 0
    while i < n:
        two = text[i : i + 2]
        if two == "--":
            end = text.find("\n", i)
            end = n if end < 0 else end
            for char in text[i:end]:
                out.append("\n" if char == "\n" else " ")
            i = end
        elif two == "/*":
            end = text.find("*/", i + 2)
            end = n if end < 0 else end + 2
            for char in text[i:end]:
                out.append("\n" if char == "\n" else " ")
            i = end
        elif text[i] == "'":
            # Postgres E'...' strings use backslash escapes (\' does not end
            # the literal); ordinary '...' strings do not. A bare E prefix
            # that is itself part of an identifier (e.g. ELSE'...') is not
            # an escape prefix: the E must stand alone.
            is_escape = (
                i > 0
                and text[i - 1] in "Ee"
                and (i - 1 == 0 or not (text[i - 2].isalnum() or text[i - 2] == "_"))
            )
            begin = i - 1 if is_escape else i
            if is_escape:
                out.pop()
            j = i + 1
            while j < n:
                if is_escape and text[j] == "\\" and j + 1 < n:
                    j += 2
                    continue
                if text[j] == "'":
                    if text[j + 1 : j + 2] == "'":
                        j += 2
                        continue
                    j += 1
                    break
                j += 1
            for char in text[begin:j]:
                out.append("\n" if char == "\n" else " ")
            i = j
        elif text[i] == '"':
            begin = i
            j = i + 1
            while j < n:
                if text[j] == '"':
                    if text[j + 1 : j + 2] == '"':
                        j += 2
                        continue
                    j += 1
                    break
                j += 1
            raw = text[begin:j]
            inner = raw[1:-1] if len(raw) >= 2 else ""
            if re.fullmatch(r"[a-z_][a-z0-9_]*", inner or ""):
                out.append(f"_qq_{inner}_")
            else:
                if raw not in quoted_ids:
                    quoted_ids[raw] = f"_qq_{len(quoted_ids)}_"
                out.append(quoted_ids[raw])
                out.append("\n" * raw.count("\n"))
            i = j
        elif text[i] == "$":
            match = DOLLAR_OPEN.match(text, i)
            if match:
                tag = match.group()
                close = text.find(tag, match.end())
                end = n if close < 0 else close + len(tag)
                for char in text[i:end]:
                    out.append("\n" if char == "\n" else " ")
                i = end
            else:
                out.append(text[i])
                i += 1
        else:
            out.append(text[i])
            i += 1
    return "".join(out)


def statements(text):
    """Yield (line, sql) for each ;-terminated statement in cleaned text."""
    cleaned = blanked(text)
    start = 0
    for match in re.finditer(";", cleaned):
        chunk = cleaned[start : match.start()]
        if chunk.strip():
            offset = start
            while offset < match.start() and cleaned[offset] in " \t\n\r":
                offset += 1
            yield cleaned.count("\n", 0, offset) + 1, chunk.strip()
        start = match.end()
    tail = cleaned[start:].strip()
    if tail:
        yield cleaned.count("\n", 0, start) + 1, tail


def split_actions(sql):
    """Split one statement on top-level commas (paren depth 0).

    Lets per-action rules scope NOT NULL / DEFAULT to the ADD that owns
    them instead of the whole statement.
    """
    parts, depth, current = [], 0, []
    for char in sql:
        if char == "(":
            depth += 1
        elif char == ")":
            depth = max(0, depth - 1)
        if char == "," and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(char)
    parts.append("".join(current))
    return parts


def top_level(text):
    """Return text with parenthesised spans blanked (newlines preserved).

    Shares split_actions()' depth counter so NOT NULL / DEFAULT inside
    CHECK (...) or type modifiers such as numeric(10,2) never read as
    column-level clauses. DEFAULT (expr) keeps its DEFAULT keyword at
    depth 0, so real expression defaults still count.
    """
    out, depth = [], 0
    for char in text:
        if char == "(":
            depth += 1
            out.append(" ")
        elif char == ")":
            depth = max(0, depth - 1)
            out.append(" ")
        elif depth > 0:
            out.append("\n" if char == "\n" else " ")
        else:
            out.append(char)
    return "".join(out)


REFERENTIAL_SET = re.compile(
    r"\bON\s+(?:DELETE|UPDATE)\s+SET\s+(?:NULL|DEFAULT)\b", re.IGNORECASE
)


def has_required_field(sql):
    """True when an ADD [COLUMN] action adds NOT NULL without a DEFAULT."""
    for action in split_actions(sql):
        match = ADD_KEYWORD.search(action)
        if not match:
            continue
        rest = action[match.end() :]
        if ADD_NON_COLUMN_REST.match(rest):
            continue
        rest = COLUMN_PREFIX.sub("", rest, count=1)
        if not rest.strip():
            continue
        scoped = REFERENTIAL_SET.sub(" ", top_level(rest))
        if NOT_NULL.search(scoped) and not DEFAULT.search(scoped):
            return True
    return False


def has_check_constraint(sql):
    """True when an ADD CONSTRAINT CHECK action lacks NOT VALID of its own.

    NOT VALID is scoped per comma-separated action so a NOT VALID on one
    action never silences a validated CHECK on another action in the same
    ALTER TABLE statement.
    """
    for action in split_actions(sql):
        if ADD_CHECK.search(action) and not NOT_VALID.search(action):
            return True
    return False


def index_on_existing_table(sql, created_tables):
    """True when a non-concurrent index targets a table outside the file's
    earlier CREATE TABLEs (empty same-file tables are safe to index)."""
    if not CREATE_INDEX.search(sql) or CONCURRENTLY.search(sql):
        return False
    match = INDEX_TARGET.search(sql)
    if not match:
        return True
    return normalize_relation(match.group("name")) not in created_tables


def has_dropping_object(sql):
    """True when a DROP TABLE/COLUMN action drops a column or table.

    The COLUMN keyword is optional in Postgres (``DROP [COLUMN]``), so a
    per-action check covers the bare spelling as well as the explicit
    ``DROP TABLE`` / ``DROP COLUMN`` forms. Only ALTER TABLE statements
    take the bare path, so standalone ``DROP INDEX`` / ``DROP TRIGGER``
    stay clean. ``DROP CONSTRAINT`` and ``ALTER COLUMN ... DROP NOT
    NULL/DEFAULT/IDENTITY`` never start an action with DROP, so they stay
    clean.
    """
    if DROP_OBJECT.search(sql):
        return True
    if not re.match(r"^\s*ALTER\s+TABLE\b", sql, re.IGNORECASE):
        return False
    actions = split_actions(sql)
    for index, action in enumerate(actions):
        candidate = action
        if index == 0:
            candidate = ALTER_TABLE_PREFIX.sub("", candidate, count=1)
        if BARE_DROP.match(candidate):
            return True
    return False


def lint_statement(sql, created_tables=frozenset()):
    """Return the list of rule IDs violated by one cleaned statement."""
    found = []
    if has_required_field(sql):
        found.append("adding-required-field")
    if index_on_existing_table(sql, created_tables):
        found.append("creating-index")
    if has_check_constraint(sql):
        found.append("adding-check-constraint")
    if ALTER_TYPE.search(sql):
        found.append("changing-column-type")
    if has_dropping_object(sql):
        found.append("dropping-object")
    return found


def is_waived_finding(path, rule, sql):
    """Match only the explicitly reviewed transactional index statements."""
    if rule != "creating-index":
        return False
    normalized = re.sub(r"\s+", " ", sql).strip().lower()
    return (path, normalized) in TRANSACTIONAL_INDEX_WAIVERS


def lint_text(text, path=None):
    """Return [(line, rule)] findings for one migration file's contents.

    Unconditional CREATE TABLE names accumulate in statement order so an
    index on a table created earlier in the same file (still empty) is not
    flagged, while an index on any other table still is. Conditional
    CREATE TABLE IF NOT EXISTS is a no-op when the relation already
    exists, so it proves nothing about emptiness and never qualifies the
    relation for the exemption. Waivers apply only when a path is given.
    """
    findings = []
    created = set()
    for line, sql in statements(text):
        for rule in lint_statement(sql, created):
            if path is None or not is_waived_finding(path, rule, sql):
                findings.append((line, rule))
        for match in CREATE_TABLE.finditer(sql):
            if match.group("conditional") is None:
                created.add(normalize_relation(match.group("name")))
    return findings


def matches_any(path, patterns):
    return any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns)


def discover_all(root, globs, excludes):
    found = []
    for pattern in globs:
        for file in sorted(root.glob(pattern)):
            if file.is_file():
                relative = file.relative_to(root).as_posix()
                if not matches_any(relative, excludes):
                    found.append(relative)
    return found


def git(root, *args):
    try:
        result = subprocess.run(
            ["git", "-C", str(root), *args], capture_output=True,
            timeout=GIT_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        raise SquawkError(
            f"cannot read Git baseline: git {' '.join(args)} timeout after "
            f"{GIT_TIMEOUT_SECONDS}s; next: re-run the gate; if the stall "
            "repeats, inspect the git invocation and runner state") from error
    except OSError as error:
        raise SquawkError(
            "cannot read Git baseline: git spawn failed "
            f"({error}); next: re-run the gate on a runner with git "
            "installed") from error
    if result.returncode:
        raise SquawkError(
            f"cannot read Git baseline: git {' '.join(args)} failed with "
            f"exit status {result.returncode}; next: verify the baseline "
            "ref exists and is fetchable, then re-run the gate")
    return result.stdout.decode().split("\0")


def changed_paths(root, base_ref):
    try:
        resolved = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "--verify", "--end-of-options", f"{base_ref}^{{commit}}"],
            capture_output=True, timeout=GIT_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        raise SquawkError(
            f"cannot resolve Git baseline {base_ref!r}: rev-parse timeout "
            f"after {GIT_TIMEOUT_SECONDS}s; next: re-run the gate; if the "
            "stall repeats, inspect the git invocation and runner "
            "state") from error
    except OSError as error:
        raise SquawkError(
            f"cannot resolve Git baseline {base_ref!r}: rev-parse spawn "
            f"failed ({error}); next: re-run the gate on a runner with git "
            "installed") from error
    if resolved.returncode:
        raise SquawkError(
            f"unresolvable Git baseline {base_ref!r}; next: verify the "
            "baseline ref exists and is fetchable, then re-run the gate")
    records = git(root, "diff", "--name-only", "-z", f"{base_ref}...HEAD", "--")
    return [record for record in records if record]


def select_files(root, globs, excludes, base_ref):
    if base_ref is None:
        return discover_all(root, globs, excludes)
    # Intersect full-mode glob expansion with the changed paths so diff mode
    # and full mode agree on `**` semantics (Path.glob matches a root-level
    # `migrations/` for `**/migrations/*.sql` while fnmatch needs a leading
    # segment, and fnmatch `*` crosses `/` while glob does not).
    candidates = set(discover_all(root, globs, excludes))
    changed = set(changed_paths(root, base_ref))
    return sorted(candidates & changed)


def check(root, globs, excludes, base_ref):
    findings = []
    for relative in select_files(root, globs, excludes, base_ref):
        try:
            text = (root / relative).read_text()
        except OSError as error:
            raise SquawkError(f"{relative}: {error}") from error
        findings.extend(
            (relative, line, rule) for line, rule in lint_text(text, relative)
        )
    return findings


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument(
        "--migration-glob",
        action="append",
        default=None,
        help=(
            "repeatable glob for migration files "
            f"(default: {DEFAULT_GLOB}); passing it replaces the default"
        ),
    )
    parser.add_argument(
        "--exclude",
        action="append",
        default=[],
        help="repeatable fnmatch exclusion applied after the globs",
    )
    parser.add_argument("--base-ref", default=None, help="Git baseline; lint files changed since it (default: lint all files)")
    parser.add_argument(
        "--strict",
        action="store_true",
        help="exit 1 on the four blocking rules; dropping-object remains advisory",
    )
    args = parser.parse_args(argv)
    globs = args.migration_glob if args.migration_glob else [DEFAULT_GLOB]
    try:
        findings = check(args.root, globs, args.exclude, args.base_ref)
    except (SquawkError, OSError) as error:
        print(f"squawk gate: {error}", file=sys.stderr)
        return 1
    for relative, line, rule in findings:
        level = "error" if args.strict and rule in STRICT_RULES else "warning"
        print(f"::{level} file={relative},line={line}::[squawk:{rule}] {ADVICE[rule]}")
    blocking = args.strict and any(
        rule in STRICT_RULES for _, _, rule in findings
    )
    if findings:
        files = sorted({relative for relative, _, _ in findings})
        print(
            f"squawk gate: {len(findings)} finding(s) in {len(files)} file(s)"
            + ("" if blocking else " (advisory)")
        )
    else:
        print("squawk gate: no risky migration statements")
    return 1 if blocking else 0


if __name__ == "__main__":
    sys.exit(main())
