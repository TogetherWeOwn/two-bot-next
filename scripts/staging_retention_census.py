#!/usr/bin/env python3
"""Read-only staging event-store retention census (stdlib only; shells out to psql).

Manual evidence for the event-store retention expiry (PR #1006): it reports
whether the expiry migrations and indexes are live, how many capture rows sit
on each side of the implementation cutoffs, and whether a bounded cohort of
must-preserve rows is byte-identical before and after one natural ~900 s
DryRun interval. Run by the manual `staging-retention-census` workflow so no
agent ever holds a database credential. Never runs automatically, never
invokes retention, never activates Execute, never changes a mode, and has no
production path.

Three pinned aggregate groups, counts/digests/timestamps only:
  A. migration ledger rows 444-445 from `_sqlx_migrations` (the two expiry
     migrations; 446 lives only in the store chain's `_two_bot_migrations`
     ledger and 447 is unrelated, so neither is queried here) plus presence
     of the two expiry indexes on `event_store` (the class index is expected
     present and the time-only index absent: migration 0445 drops it);
  B. eligible-old vs fresh counts by retention family (`PRESENCE_UPDATE`,
     `VOICE_STATE_UPDATE`, `VOICE_SERVER_UPDATE` at 30 days,
     `GUILD_AUDIT_LOG_ENTRY_CREATE` at 45 days) with the implementation
     cutoffs (`now() + 900s - window`, equality inclusive, per
     `crates/cutover/src/retention.rs`). The buckets are pre-exclusion upper
     bounds: the queued-unguarded-guild and eligible-deletion-pair
     exclusions of the per-class lifecycle are not applied here, so no
     bucket is called "would-delete";
  C. bounded stable-cohort digest (at most 1000 rows, sha256 of
     `id|event_time`) captured before AND after one in-job 900 s sleep that
     observes a natural DryRun interval. Both captures reuse one
     server-anchored window, so the bounds cannot move between them. An
     empty cohort, zero candidates, or a missing pgcrypto digest is a
     reported limitation, never a PASS.

Fences, all before the first connection (exit 2, nothing was sent):
  * the database login comes only from $TWO_BOT_STAGING_PLAN_DATABASE_URL
    and must be a bare login on the pinned staging database at the pinned
    staging host. The login is a separate identity holding only
    two_bot_migrator_ro (that group is NOLOGIN, so the binding username is
    never the group name itself). The URL port is omitted or 5432, pooler
    hosts and pooler-style usernames are refused, and a production-like host
    or database refuses. Only sslmode, channel_binding and connect_timeout
    may ride on the URL; psql always uses verify-full with the runner's
    system CA bundle, so the certificate must authenticate that pinned
    hostname. The URL is turned into PG* environment variables for psql, so
    the password never appears in a process list;
  * the expected host, database and branch pins come only from the
    non-secret workflow vars $TWO_BOT_STAGING_HOST,
    $TWO_BOT_STAGING_DATABASE and $TWO_BOT_STAGING_BRANCH_ID, never derived
    from the URL. A PlanetScale host requires a bare branch pin matching
    the binding username's `{login}.{branch_id}` suffix.
Every connection then runs SET ROLE two_bot_migrator_ro before its READ ONLY
transaction (mirroring the plan binary's per-connection role); a denied SET
ROLE refuses. After connecting, the server must also report current_user as
that role and current_database() as the pinned database, and must report
that the login holds no membership in two_bot_migrator, before any census
query runs.

Every statement runs in a READ ONLY transaction with a statement and lock
timeout. No DDL runs anywhere, including no CREATE EXTENSION: when pgcrypto
is absent the cohort reports count/min/max without a digest. psql's stderr
is never echoed: a failure prints one fixed-vocabulary line. CSV must parse
strictly; every count must be a non-negative integer, every digest 64 hex
chars, every timestamp a real instant.

The report prints to stdout and the step summary (log only, no artifact):
migration and index presence, per-family counts, both cohort captures with
their observation timestamps, and the digest comparison. No raw rows,
member IDs, payloads, or connection strings ever print.

Exit status: 0 census complete (limitations included), 1 census failed,
2 refused by a fence.

Usage (the workflow supplies the environment):
  python3 scripts/staging_retention_census.py [--json]

Agent result schema and offline verification: docs/ci-security.md,
"Agent observation contract". JSON changes output only, never query scope or mode.
"""

import argparse
import csv
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
import io
import json
import os
import re
import shutil
import subprocess
import sys
import time
from urllib.parse import parse_qsl, unquote, urlsplit


URL_ENV = "TWO_BOT_STAGING_PLAN_DATABASE_URL"
HOST_ENV = "TWO_BOT_STAGING_HOST"
DATABASE_ENV = "TWO_BOT_STAGING_DATABASE"
BRANCH_ENV = "TWO_BOT_STAGING_BRANCH_ID"
DB_ROLE = "two_bot_migrator_ro"
MIGRATOR_ROLE = "two_bot_migrator"
SYSTEM_CA_BUNDLE = "/etc/ssl/certs/ca-certificates.crt"
PSQL_TIMEOUT_SECONDS = 60
# One natural retention interval (RETENTION_CADENCE_SECONDS in retention.rs).
SLEEP_SECONDS = 900
# One expiry batch (MAX_RAW_EXPIRY_ROWS_PER_BATCH in retention.rs).
COHORT_LIMIT = 1000
MIGRATION_VERSIONS = (444, 445)
CLASS_INDEX = "idx_event_store_expiry_class_time_id"
TIME_INDEX = "idx_event_store_expiry_event_time_id"
FAMILIES = ("PRESENCE_UPDATE", "VOICE_STATE_UPDATE", "VOICE_SERVER_UPDATE",
            "GUILD_AUDIT_LOG_ENTRY_CREATE")

COUNT_RE = re.compile(r"[0-9]+")
DIGEST_RE = re.compile(r"[0-9a-f]{64}")
# PostgreSQL ISO timestamps: microsecond precision, explicit numeric zone or Z.
# Full shape admission precedes calendar/offset validation; no text is echoed.
TIMESTAMP_RE = re.compile(
    r"([0-9]{4}-[0-9]{2}-[0-9]{2}[ T][0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]{1,6})?)"
    r"(Z|([+-])([0-9]{2})(?::?([0-9]{2})(?::([0-9]{2}))?)?)")
# libpq parameters a login URL may carry. Anything else (host=, options=,
# service=, passfile=, ...) could redirect the connection or change the session.
URL_PARAMS = {
    "sslmode": {"require", "verify-ca", "verify-full"},
    "channel_binding": {"require", "prefer", "disable"},
    "connect_timeout": None,  # digits, checked below
}
# The only environment psql inherits besides the PG* values built from the URL.
INHERITED_ENV = ("PATH", "HOME", "LANG", "LC_ALL", "TMPDIR")

# Every psql invocation is its own session, so every script re-asserts the
# read-only group before the transaction (mirroring the plan binary's
# per-connection SET ROLE in staging_migrate.rs): a login that is not a
# member of the group fails on this line, before anything is read.
SET_ROLE = f"SET ROLE {DB_ROLE};\n"
READ_ONLY_5S = (SET_ROLE + "BEGIN READ ONLY;\nSET LOCAL statement_timeout = '5s';\n"
                "SET LOCAL lock_timeout = '5s';\n")
READ_ONLY_15S = (SET_ROLE + "BEGIN READ ONLY;\nSET LOCAL statement_timeout = '15s';\n"
                 "SET LOCAL lock_timeout = '5s';\n")
# session_user is unaffected by SET ROLE, so the membership flag tests the
# login itself (mirroring refuse_migrator_member_plan_login): a login that
# could also SET ROLE to the migrator group is not a read-only credential.
IDENTITY_SQL = (READ_ONLY_5S + "SELECT current_user, current_database(), "
                f"pg_has_role(session_user, '{MIGRATOR_ROLE}', 'MEMBER');\nCOMMIT;\n")
# Capability probe, not census evidence: pgcrypto's digest() decides whether
# the cohort carries a digest or count/min/max only. Casts keep the probe
# independent of argument-type inference.
PROBE_SQL = (READ_ONLY_5S + "SELECT encode(digest('retention-census-probe'::text, "
             "'sha256'::text), 'hex');\nCOMMIT;\n")
MIGRATIONS_SQL = (
    READ_ONLY_5S
    + "SELECT version, installed_on, success\n"
    + "  FROM _sqlx_migrations\n"
    + " WHERE version IN (444, 445)\n"
    + " ORDER BY version;\n"
    + "COMMIT;\n"
)
INDEXES_SQL = (
    READ_ONLY_5S
    + "SELECT indexname, indexdef\n"
    + "  FROM pg_indexes\n"
    + " WHERE schemaname = 'public' AND tablename = 'event_store'\n"
    + "   AND indexname IN ('idx_event_store_expiry_class_time_id',"
    + "'idx_event_store_expiry_event_time_id');\n"
    + "COMMIT;\n"
)
# Eligible-old vs fresh counts with the implementation cutoffs
# (cutoff = now() + 900s - window, equality inclusive). Pre-exclusion upper
# bounds: lifecycle exclusions are not applied, so no bucket is "would-delete".
COUNTS_SQL = (
    READ_ONLY_15S
    + "WITH cutoffs AS (\n"
    + "  SELECT now() + make_interval(secs => 900) - make_interval(days => 30)"
    + " AS presence_voice_cutoff,\n"
    + "         now() + make_interval(secs => 900) - make_interval(days => 45)"
    + " AS audit_cutoff\n"
    + ")\n"
    + "SELECT event_type,\n"
    + "       count(*) FILTER (WHERE event_time <= CASE WHEN event_type = "
    + "'GUILD_AUDIT_LOG_ENTRY_CREATE'\n"
    + "         THEN (SELECT audit_cutoff FROM cutoffs)\n"
    + "         ELSE (SELECT presence_voice_cutoff FROM cutoffs) END) AS eligible_old,\n"
    + "       count(*) FILTER (WHERE event_time > CASE WHEN event_type = "
    + "'GUILD_AUDIT_LOG_ENTRY_CREATE'\n"
    + "         THEN (SELECT audit_cutoff FROM cutoffs)\n"
    + "         ELSE (SELECT presence_voice_cutoff FROM cutoffs) END) AS fresh,\n"
    + "       count(*) AS total\n"
    + "  FROM event_store\n"
    + " WHERE event_type IN ('PRESENCE_UPDATE','VOICE_STATE_UPDATE',"
    + "'VOICE_SERVER_UPDATE','GUILD_AUDIT_LOG_ENTRY_CREATE')\n"
    + " GROUP BY event_type ORDER BY event_type;\n"
    + "COMMIT;\n"
)
# One server epoch, read before the first capture, anchors both cohort
# captures to a single frozen window. A now() bound would move ~900 s
# between the captures and the oldest-1000 cohort would drift with it, so a
# live predicate cannot be compared before-vs-after.
# floor(), not a bare cast: PostgreSQL rounds a float/numeric epoch to the
# nearest bigint, so without the floor the upper bound could land up to 0.5 s
# after the anchor read and admit a sleep-time insert into the after capture.
ANCHOR_SQL = (READ_ONLY_5S + "SELECT floor(extract(epoch FROM now()))::bigint;\nCOMMIT;\n")
# Frozen-window margin: the lower bound stays fresher than every retention
# cutoff until the after-capture (lookahead 900 s + one sleep + margin);
# the upper bound excludes rows inserted during the sleep.
COHORT_MARGIN_SECONDS = 900 + SLEEP_SECONDS + 120


def cohort_predicate(anchor_epoch):
    """The shared frozen cohort window over one integer anchor literal."""
    if isinstance(anchor_epoch, bool) or not isinstance(anchor_epoch, int):
        raise ReadFailed("a row had an unexpected shape; nothing was reported")
    return (
        "    FROM event_store\n"
        "   WHERE event_type IN ('PRESENCE_UPDATE','VOICE_STATE_UPDATE',"
        "'VOICE_SERVER_UPDATE','GUILD_AUDIT_LOG_ENTRY_CREATE')\n"
        f"     AND event_time > to_timestamp({anchor_epoch}) + make_interval(secs => "
        f"{COHORT_MARGIN_SECONDS}) - make_interval(days => 30)\n"
        f"     AND event_time <= to_timestamp({anchor_epoch})\n"
        "   ORDER BY event_time, id\n"
        f"   LIMIT {COHORT_LIMIT}\n"
    )


def cohort_sql(anchor_epoch):
    return (
        READ_ONLY_15S
        + "WITH cohort AS (\n"
        + "  SELECT id, event_time\n"
        + cohort_predicate(anchor_epoch)
        + ")\n"
        + "SELECT count(*) AS cohort_count,\n"
        # id::text: PostgreSQL has no bigint || text operator, so the bare
        # concatenation errors (42883) on the BIGSERIAL schema. Same values in,
        # same digest shape out; the digest is only compared before-vs-after.
        + "       encode(digest(string_agg(id::text || '|' || event_time::text, ','"
        + " ORDER BY event_time, id), 'sha256'), 'hex') AS cohort_digest,\n"
        + "       min(event_time) AS cohort_min_event_time,\n"
        + "       max(event_time) AS cohort_max_event_time\n"
        + "  FROM cohort;\n"
        + "COMMIT;\n"
    )


def cohort_nodigest_sql(anchor_epoch):
    # pgcrypto-absent fallback: same cohort, no digest. Reported as a limitation.
    return (
        READ_ONLY_15S
        + "WITH cohort AS (\n"
        + "  SELECT id, event_time\n"
        + cohort_predicate(anchor_epoch)
        + ")\n"
        + "SELECT count(*) AS cohort_count,\n"
        + "       min(event_time) AS cohort_min_event_time,\n"
        + "       max(event_time) AS cohort_max_event_time\n"
        + "  FROM cohort;\n"
        + "COMMIT;\n"
    )


class Refused(Exception):
    """A fence refused the run before any census query ran."""


class ReadFailed(Exception):
    """The census itself failed; the message is fixed vocabulary, never psql output."""


def format_instant(moment):
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{moment.microsecond // 1000:03d}Z"


def prod_like(name):
    return "prod" in name.lower()


def branch_id_valid(value):
    """Bare branch-id shape (alphanumerics, dash, underscore, at most 63)."""
    return (isinstance(value, str) and 0 < len(value) <= 63
            and all(ch.isascii() and (ch.isalnum() or ch in "-_") for ch in value))


def database_target(env):
    """Turn the login URL into PG* variables plus the verified pins.

    Refuses anything but the read-only staging login on the independently
    pinned host/database/branch. Returns (pg, host_pin, database_pin).
    """
    url = env.get(URL_ENV, "")
    if not url:
        raise Refused(f"refusing: no database login bound (${URL_ENV})")
    host_pin = (env.get(HOST_ENV, "") or "").strip()
    database_pin = (env.get(DATABASE_ENV, "") or "").strip()
    branch_pin = (env.get(BRANCH_ENV, "") or "").strip()
    for label, pin in (("host", host_pin), ("database", database_pin)):
        if not pin or prod_like(pin) or "@" in pin or "/" in pin:
            raise Refused(f"refusing: the pinned staging {label} is empty, "
                          "production-like or not a bare name")
    if "-pooler" in host_pin.lower():
        raise Refused("refusing: the pinned staging host is a pooler endpoint")
    if branch_pin:
        if not branch_id_valid(branch_pin):
            raise Refused("refusing: the pinned staging branch is not a bare branch pin")
    elif host_pin.lower().endswith(".psdb.cloud"):
        raise Refused("refusing: PlanetScale staging hosts require a branch pin")
    try:
        parts = urlsplit(url)
        port = parts.port
        host = parts.hostname
    except ValueError:
        raise Refused("refusing: the database login is not a valid postgres URL")
    if parts.scheme not in ("postgres", "postgresql") or not host \
            or "," in parts.netloc.rpartition("@")[2]:
        raise Refused("refusing: the database login must be one postgres:// host")
    if port not in (None, 5432):
        raise Refused("refusing: the database port must be the direct 5432 endpoint")
    if "/" in host or "@" in host:
        raise Refused("refusing: the database host must be a bare name")
    if "-pooler" in host.lower():
        raise Refused("refusing: pooler endpoints are refused; use the direct endpoint")
    if host.lower() != host_pin.lower():
        raise Refused("refusing: the database host does not match the pinned staging endpoint")
    if prod_like(host):
        raise Refused("refusing: the database host is production-like")
    user = unquote(parts.username or "")
    password = unquote(parts.password or "")
    database = unquote(parts.path.lstrip("/").split("?", 1)[0])
    if "|" in user:
        raise Refused("refusing: pooler-style usernames are refused; use the direct role login")
    if not user:
        raise Refused("refusing: the database login names no user")
    # The login name itself is not pinned (mirroring the plan binary): any
    # bare login passes here, and the server-side guards prove it holds only
    # the read-only group (SET ROLE must succeed, current_user must be the
    # group, session_user must not hold the migrator group).
    if host.lower().endswith(".psdb.cloud"):
        login, dot, suffix = user.rpartition(".")
        if not dot or not login or not suffix or suffix.lower() != branch_pin.lower():
            raise Refused("refusing: the database binding does not match the verified branch")
    elif branch_pin:
        raise Refused("refusing: a branch pin only applies to PlanetScale staging hosts")
    if not password:
        raise Refused("refusing: the database login carries no password")
    if prod_like(database):
        raise Refused("refusing: the database name is production-like")
    if database != database_pin:
        raise Refused("refusing: the database does not match the pinned staging database")
    pg = {"PGHOST": host, "PGUSER": user, "PGPASSWORD": password, "PGDATABASE": database,
          "PGSSLMODE": "verify-full", "PGSSLROOTCERT": SYSTEM_CA_BUNDLE,
          "PGCONNECT_TIMEOUT": "10"}
    if port is not None:
        pg["PGPORT"] = str(port)
    try:
        params = parse_qsl(parts.query, keep_blank_values=True, strict_parsing=bool(parts.query))
    except ValueError:
        raise Refused("refusing: the database login query is malformed")
    for key, value in params:
        if key not in URL_PARAMS:
            raise Refused("refusing: the database login may not set that query parameter")
        allowed = URL_PARAMS[key]
        if (allowed is not None and value not in allowed) or (allowed is None and not value.isdigit()):
            raise Refused(f"refusing: the database login has a bad {key}")
        if key == "sslmode":
            # Upgrade every accepted mode to authenticated hostname verification.
            continue
        pg[{"channel_binding": "PGCHANNELBINDING",
            "connect_timeout": "PGCONNECT_TIMEOUT"}[key]] = value
    return pg, host_pin, database_pin


def psql_environment(pg, env):
    return {**{key: env[key] for key in INHERITED_ENV if key in env}, **pg}


def classify_failure(stderr):
    """Map psql's stderr to a fixed phrase; the text itself may quote values."""
    text = stderr.lower()
    for needle, phrase in (
            ("password authentication failed", "authentication failed"),
            ("permission denied", "permission denied for the read-only role"),
            ("statement timeout", "statement timeout"),
            ("could not translate host", "could not resolve the database host"),
            ("connection refused", "could not connect"),
            ("could not connect", "could not connect"),
            ("timeout expired", "could not connect"),
            ("ssl", "tls negotiation failed")):
        if needle in text:
            return phrase
    return "psql failed"


def psql_command(psql):
    return [psql, "-X", "-q", "-t", "--csv", "-v", "ON_ERROR_STOP=1", "-f", "-"]


def run_psql(psql, pg_env, script):
    try:
        done = subprocess.run(psql_command(psql), input=script, capture_output=True, text=True,
                              env=pg_env, timeout=PSQL_TIMEOUT_SECONDS, check=False)
    except subprocess.TimeoutExpired:
        raise ReadFailed("psql timed out")
    except OSError:
        raise ReadFailed("psql could not start")
    if done.returncode != 0:
        raise ReadFailed(f"{classify_failure(done.stderr)} (psql exit {done.returncode})")
    return done.stdout


def probe_pgcrypto(psql, pg_env):
    """True when digest() runs; False only when the function is undefined.

    Any other failure is a real read failure, not an absent extension.
    """
    try:
        done = subprocess.run(psql_command(psql), input=PROBE_SQL, capture_output=True,
                              text=True, env=pg_env, timeout=PSQL_TIMEOUT_SECONDS,
                              check=False)
    except subprocess.TimeoutExpired:
        raise ReadFailed("psql timed out")
    except OSError:
        raise ReadFailed("psql could not start")
    if done.returncode == 0:
        return True
    lowered = done.stderr.lower()
    if "digest" in lowered and "does not exist" in lowered:
        return False
    raise ReadFailed(f"{classify_failure(done.stderr)} (psql exit {done.returncode})")


def csv_rows(text):
    try:
        return [row for row in csv.reader(io.StringIO(text), strict=True) if row]
    except csv.Error:
        raise ReadFailed("a row had an unexpected shape; nothing was reported") from None


def parse_count(value):
    if not isinstance(value, str) or not COUNT_RE.fullmatch(value):
        raise ReadFailed("a row had an unexpected shape; nothing was reported")
    try:
        return int(value)
    except ValueError:
        raise ReadFailed("a row had an unexpected shape; nothing was reported") from None


def timestamp_instant(value):
    match = TIMESTAMP_RE.fullmatch(value) if isinstance(value, str) else None
    if match is None:
        raise ReadFailed("a row had an unexpected shape; nothing was reported")
    local, _, sign, hours, minutes, seconds = match.groups()
    hour, minute, second = int(hours or 0), int(minutes or 0), int(seconds or 0)
    # PostgreSQL numeric displacements are less than 16 hours.
    if hour > 15 or minute > 59 or second > 59:
        raise ReadFailed("a row had an unexpected shape; nothing was reported")
    offset = timedelta(hours=hour, minutes=minute, seconds=second)
    if sign == "-":
        offset = -offset
    try:
        return datetime.fromisoformat(local).replace(
            tzinfo=timezone(offset)).astimezone(timezone.utc)
    except (ValueError, OverflowError):
        raise ReadFailed("a row had an unexpected shape; nothing was reported") from None


def parse_timestamp(value):
    instant = timestamp_instant(value)
    return instant.isoformat(sep=" ").removesuffix("+00:00") + "+00"


def check_identity(output, database_pin):
    rows = csv_rows(output)
    if len(rows) != 1 or len(rows[0]) != 3 or rows[0][2] not in ("t", "f"):
        raise ReadFailed("a row had an unexpected shape; nothing was reported")
    if rows[0][:2] != [DB_ROLE, database_pin]:
        raise Refused(f"refusing: the server does not report role {DB_ROLE} "
                      "on the pinned staging database")
    if rows[0][2] == "t":
        raise Refused(f"refusing: the plan login holds the {MIGRATOR_ROLE} group; "
                      f"the plan binding must be a login that holds only {DB_ROLE}")


def parse_migrations(output):
    """Map each ledger version to (installed_on, success) or None when absent."""
    found = {}
    for raw in csv_rows(output):
        if len(raw) != 3 or not COUNT_RE.fullmatch(raw[0]) or raw[2] not in ("t", "f"):
            raise ReadFailed("a row had an unexpected shape; nothing was reported")
        version = parse_count(raw[0])
        if version not in MIGRATION_VERSIONS or version in found:
            raise ReadFailed("a row had an unexpected shape; nothing was reported")
        found[version] = (parse_timestamp(raw[1]), raw[2] == "t")
    return {version: found.get(version) for version in MIGRATION_VERSIONS}


def parse_indexes(output):
    """Presence per expected expiry index; definitions are validated, never printed."""
    present = set()
    for raw in csv_rows(output):
        if len(raw) != 2 or raw[0] not in (CLASS_INDEX, TIME_INDEX) or not raw[1]:
            raise ReadFailed("a row had an unexpected shape; nothing was reported")
        present.add(raw[0])
    return {CLASS_INDEX: CLASS_INDEX in present, TIME_INDEX: TIME_INDEX in present}


def parse_counts(output):
    """Per-family (eligible_old, fresh, total); absent families are zero rows."""
    found = {}
    for raw in csv_rows(output):
        if len(raw) != 4 or raw[0] not in FAMILIES or raw[0] in found:
            raise ReadFailed("a row had an unexpected shape; nothing was reported")
        eligible, fresh, total = (parse_count(raw[1]), parse_count(raw[2]), parse_count(raw[3]))
        if eligible + fresh != total:
            raise ReadFailed("a row had an unexpected shape; nothing was reported")
        found[raw[0]] = (eligible, fresh, total)
    return {family: found.get(family, (0, 0, 0)) for family in FAMILIES}


def parse_cohort(output, with_digest):
    """(count, digest_or_None, min_or_None, max_or_None) for one capture."""
    rows = csv_rows(output)
    if len(rows) != 1:
        raise ReadFailed("a row had an unexpected shape; nothing was reported")
    raw = rows[0]
    if with_digest and len(raw) != 4 or not with_digest and len(raw) != 3:
        raise ReadFailed("a row had an unexpected shape; nothing was reported")
    count = parse_count(raw[0])
    if count > COHORT_LIMIT:
        raise ReadFailed("a row had an unexpected shape; nothing was reported")
    rest = raw[1:] if with_digest else (None, *raw[1:])
    digest, minimum, maximum = rest[0], rest[1], rest[2]
    if count == 0:
        if digest or minimum or maximum:
            raise ReadFailed("a row had an unexpected shape; nothing was reported")
        return 0, None, None, None
    if not minimum or not maximum:
        raise ReadFailed("a row had an unexpected shape; nothing was reported")
    minimum, maximum = parse_timestamp(minimum), parse_timestamp(maximum)
    if timestamp_instant(minimum) > timestamp_instant(maximum):
        raise ReadFailed("a row had an unexpected shape; nothing was reported")
    if with_digest:
        if not digest or not DIGEST_RE.fullmatch(digest):
            raise ReadFailed("a row had an unexpected shape; nothing was reported")
        return count, digest, minimum, maximum
    return count, None, minimum, maximum


def parse_anchor(output):
    """One server epoch; the frozen cohort-window anchor."""
    rows = csv_rows(output)
    if len(rows) != 1 or len(rows[0]) != 1:
        raise ReadFailed("a row had an unexpected shape; nothing was reported")
    return parse_count(rows[0][0])


@dataclass(frozen=True)
class Observation:
    id: str
    code: str
    value: str | bool | dict | None
    next_action: str

    def document(self):
        return {"id": self.id, "code": self.code, "value": self.value,
                "next_action": self.next_action}


@dataclass(frozen=True)
class Limitation:
    id: str
    code: str
    next_action: str

    def document(self):
        return {"id": self.id, "code": self.code, "next_action": self.next_action}


@dataclass(frozen=True)
class CensusError:
    code: str
    retryable: bool
    next_action: str

    def document(self):
        return {"code": self.code, "retryable": self.retryable,
                "next_action": self.next_action}


@dataclass(frozen=True)
class CensusResult:
    status: str
    observations: tuple[Observation, ...] = ()
    limitations: tuple[Limitation, ...] = ()
    error: CensusError | None = None

    def document(self):
        # Explicit allowlist: never serialize exceptions, environments or raw rows.
        return {"schema_version": 1, "status": self.status,
                "observations": [row.document() for row in self.observations],
                "limitations": [row.document() for row in self.limitations],
                "error": self.error.document() if self.error else None}


def cohort_value(capture):
    count, digest, minimum, maximum = capture
    return {"count": count, "digest": digest, "minimum": minimum, "maximum": maximum}


def build_result(observed_before, observed_after, migrations, indexes, counts,
                 before, after, pgcrypto):
    """Project admitted aggregates only; never infer execution from observations."""
    values = (
        ("observed_before", observed_before, "Compare capture times with the frozen window."),
        ("observed_after", observed_after, "Compare capture times with the frozen window."),
        ("migrations", {str(version): None if migrations[version] is None else
                        {"installed_on": migrations[version][0],
                         "success": migrations[version][1]}
                        for version in MIGRATION_VERSIONS},
         "Inspect missing or unsuccessful ledger entries; do not apply from this report."),
        ("indexes", {name: indexes[name] for name in (CLASS_INDEX, TIME_INDEX)},
         "Compare index presence with the expiry migration contract."),
        ("counts", {family: dict(zip(("eligible_old", "fresh", "total"), counts[family]))
                    for family in FAMILIES},
         "Treat eligible_old as a pre-exclusion upper bound, not a deletion count."),
        ("cohort_before", cohort_value(before), "Compare only these frozen-window captures."),
        ("cohort_after", cohort_value(after), "Compare only these frozen-window captures."),
    )
    observations = [Observation(key, "observed", value, action) for key, value, action in values]
    limitations = []
    if before[0] == 0 or after[0] == 0:
        limitations.append(Limitation("empty_cohort", "empty_cohort",
                                      "Preservation is unobserved; obtain a nonempty cohort."))
    if not pgcrypto:
        limitations.append(Limitation("pgcrypto_absent", "pgcrypto_absent",
                                      "Preservation is unobserved; no digest is available."))
    if before[0] and after[0] and pgcrypto:
        observations.append(Observation(
            "digest_match", "observed", before[1] == after[1],
            "Investigate mismatches; equality alone does not prove retention execution."))
    if all(total == 0 for _, _, total in counts.values()):
        limitations.append(Limitation("empty_families", "empty_families",
                                      "Obtain nonempty family evidence before drawing conclusions."))
    for key in ("mode", "counters"):
        observations.append(Observation(key, "unobserved", None,
                                        "Obtain separate runtime evidence; never infer execution here."))
    return CensusResult("observed", tuple(observations), tuple(limitations))


def render_text(result):
    """Render the same named values/codes/actions as the optional JSON document."""
    lines = ["## staging retention census"]
    if result.error:
        error = result.error
        return (f"{error.code}: retryable={str(error.retryable).lower()} "
                f"next_action: {error.next_action}\n")
    for row in result.observations:
        key, value = row.id, row.value
        if key == "migrations":
            ledger = []
            for version, entry in value.items():
                if entry is None:
                    ledger.append(f"{version} not applied")
                else:
                    success = 'success' if entry['success'] else 'FAILURE'
                    ledger.append(f"{version} applied {success} {entry['installed_on']}")
            text = "migrations: " + "; ".join(ledger)
        elif key == "indexes":
            text = "indexes: " + ", ".join(
                f"{name} {'present' if present else 'absent'}" for name, present in value.items())
        elif key == "counts":
            text = "counts: pre-exclusion upper bounds; lifecycle exclusions not applied"
        elif key in ("cohort_before", "cohort_after"):
            text = "{} (cohort {}: count={} digest={} min={} max={})".format(
                key, key.removeprefix("cohort_"), value["count"], value["digest"] or "-",
                value["minimum"] or "-", value["maximum"] or "-")
        elif key in ("mode", "counters"):
            text = f"{key}: not observed by this read path"
        else:
            text = f"{key}: {str(value).lower() if isinstance(value, bool) else value}"
        lines.append(f"{text} [code={row.code}; next_action: {row.next_action}]")
        if key == "counts":
            for family, buckets in value.items():
                lines.append(f"{family}: eligible_old={buckets['eligible_old']} "
                             f"fresh={buckets['fresh']} total={buckets['total']}")
    phrases = {
        "empty_cohort": "empty cohort; preservation unobserved",
        "pgcrypto_absent": "pgcrypto absent; cohort has count/min/max without a digest",
        "empty_families": "no rows in the retention families",
    }
    for row in result.limitations:
        lines.append(f"limitation: {phrases[row.code]} [id={row.id}; code={row.code}; "
                     f"next_action: {row.next_action}]")
    lines.append("mode/counters: not observed by this read path")
    return "\n".join(lines) + "\n"


def format_report(observed_before, observed_after, migrations, indexes, counts,
                  before, after, pgcrypto):
    return render_text(build_result(observed_before, observed_after, migrations, indexes,
                                    counts, before, after, pgcrypto))


def run_census(env, psql, clock, sleep_fn, *, structured=False):
    """Every fence, then A/B/C-before, one sleep, C-after; no partial result."""
    pg, _, database_pin = database_target(env)
    pg_env = psql_environment(pg, env)
    if not psql:
        raise Refused("refusing: psql is not installed on this runner")
    try:
        identity_output = run_psql(psql, pg_env, IDENTITY_SQL)
    except ReadFailed as error:
        # The identity script runs first and does nothing but SET ROLE plus
        # one read, so a permission denial here is a non-member login, not a
        # missing grant: refuse like the plan binary's set-role-denied cause.
        if "permission denied" in str(error):
            raise Refused(f"refusing: the plan login cannot activate role {DB_ROLE} "
                          "(SET ROLE denied)") from None
        raise
    check_identity(identity_output, database_pin)
    pgcrypto = probe_pgcrypto(psql, pg_env)
    migrations = parse_migrations(run_psql(psql, pg_env, MIGRATIONS_SQL))
    indexes = parse_indexes(run_psql(psql, pg_env, INDEXES_SQL))
    counts = parse_counts(run_psql(psql, pg_env, COUNTS_SQL))
    anchor = parse_anchor(run_psql(psql, pg_env, ANCHOR_SQL))
    cohort_query = cohort_sql(anchor) if pgcrypto else cohort_nodigest_sql(anchor)
    observed_before = format_instant(clock())
    before = parse_cohort(run_psql(psql, pg_env, cohort_query), pgcrypto)
    if not structured:
        print(f"sleeping {SLEEP_SECONDS}s to observe one natural DryRun interval",
              flush=True)
    sleep_fn(SLEEP_SECONDS)
    observed_after = format_instant(clock())
    after = parse_cohort(run_psql(psql, pg_env, cohort_query), pgcrypto)
    result = build_result(observed_before, observed_after, migrations, indexes,
                          counts, before, after, pgcrypto)
    return result if structured else render_text(result)


def failure_result(code):
    actions = {
        "refused": "Correct the staging binding or identity fence before retrying.",
        "read_failed": "Inspect the read-only runner and admitted result shapes before retrying.",
        "publication_failed": "Correct the summary/output destination before rerunning the census.",
    }
    return CensusResult(code, error=CensusError(code, code != "refused", actions[code]))


def main(argv=None, env=None, clock=None, psql=None, sleep_fn=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--json", action="store_true", help="emit one versioned observation document")
    args = parser.parse_args(argv)
    env = os.environ if env is None else env
    clock = clock or (lambda: datetime.now(timezone.utc))
    sleep_fn = time.sleep if sleep_fn is None else sleep_fn
    publishing = False
    legacy_error = None
    try:
        psql = shutil.which("psql") if psql is None else psql
        result = run_census(env, psql, clock, sleep_fn, structured=True)
        publishing = True
        report = render_text(result)
        output = json.dumps(result.document(), sort_keys=True) + "\n" if args.json else report
        summary = env.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as handle:
                handle.write(report)
        print(output, end="")
        if not args.json:
            print("ok: census complete")
        return 0
    except Refused as error:
        result = failure_result("refused")
        # Refused messages are fixed vocabulary; text mode keeps the cause.
        legacy_error = f"{error}\n" + render_text(result)
    except ReadFailed as error:
        result = failure_result("read_failed")
        # ReadFailed messages are fixed vocabulary, never psql output.
        legacy_error = f"failed: {error}\n" + render_text(result)
    except Exception as error:  # a traceback or OS message could quote the login
        result = failure_result("publication_failed" if publishing else "read_failed")
        if not publishing and type(error) is ValueError:
            legacy_error = "failed: unexpected ValueError; nothing was reported\n"
    if args.json:
        print(json.dumps(result.document(), sort_keys=True))
    else:
        print(legacy_error or render_text(result), end="", file=sys.stderr)
    return 2 if result.status == "refused" else 1


if __name__ == "__main__":
    sys.exit(main())
