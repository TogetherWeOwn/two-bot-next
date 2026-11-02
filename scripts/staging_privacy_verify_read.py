#!/usr/bin/env python3
"""Read-only staging privacy-verification read for one fixture pair (stdlib only; shells out to psql).

This is the narrowed six-table collector for the privacy staging fixture,
run by the manual `staging-privacy-verify-read` workflow so that no agent
ever holds a database credential. It reads all-time per-pair counts for ONE
fixture member in ONE staging guild (alias `g` or `h`) and writes a
sanitized artifact:

  {"schema_version": 1,
   "guild": "g",
   "collected_at": "...Z",
   "counts": {"events": 0, "members": 0, "invite_snapshots": 0,
              "community_facts": 0, "privacy_opt_outs": 0,
              "privacy_opt_out_audit": 0},
   "sweep_extra": {"events": 0, "community_facts": 0},
   "audit": {"truncated": false,
             "rows": [{"ordinal": 1, "actor": "...", "opted_out_at": "...Z"}]}}

`counts` answers purge verification (the four purge tables), suppression
presence (`privacy_opt_outs`, 0 or 1) and the receipt count
(`privacy_opt_out_audit`). `audit.rows` carries the receipt actor labels and
timestamps behind a small cap; the count stays exact regardless.
`sweep_extra` counts rows the purge would delete beyond direct ownership:
rows matching the reviewed `member_erasure` manifest predicates whose owner
column is not the fixture member. A zero sweep proves the purge touches only
directly owned rows; a nonzero sweep means the baseline cannot confirm
attribution from counts and must stop, never pass by substitution.

The member, the guild pins, the sources and the metadata never enter the
artifact, the log or the step summary. The artifact names the guild by alias
only. The equality-count queries do not even select the match columns, so
content and metadata never reach this process; the sweep queries match on
`metadata`/`source_event_id`/`idempotency_key` but project only COUNT(*).

The 15-minute window of the events-smoke read does NOT apply here: purge
verification needs all-time pair counts. Narrowness comes instead from the
fixed pair predicates, the six-table scope, the row cap on receipts, and a
sanity ceiling on every count.

Fences, all before the first connection (exit 2, nothing was sent):
  * the guild alias is exactly `g` or `h`. `g` maps to the pinned TWO
    Staging id through the same guard the Discord smokes use (the live guild
    and any other guild refuse, and a repinned constant is caught). `h`
    maps to $STAGING_PRIVACY_VERIFY_H_GUILD_ID, which stays unset until the
    second guild's pin is recorded and verified not-live through the normal
    config path: an unset, malformed, live, or staging-equal H pin refuses;
  * the fixture member comes only from $STAGING_FIXTURE_MEMBER_ID (digits,
    15-22), never from an input, so a dispatcher cannot aim the job at another
    member;
  * every count must parse as an integer within its sanity ceiling (below);
    an out-of-range count fails closed before an artifact is written;
  * every receipt actor must mirror the service's actor shape (nonempty,
    at most 128 bytes, no control characters) AND exactly match a reviewed,
    identity-free fixture label (`member:self-serve` or
    `operator:fixture-check`). Every receipt timestamp must be a real UTC
    instant; anything else fails closed, including the overflow witness;
  * the database login comes only from
    $TWO_BOT_STAGING_PRIVACY_RO_DATABASE_URL and must name role
    two_bot_privacy_verify_ro on database two_bot at the exact host pinned
    in $STAGING_PRIVACY_VERIFY_EXPECTED_HOST. The URL port is omitted or
    5432, and a production-like host also refuses. Only sslmode,
    channel_binding and connect_timeout may ride on the URL; psql always uses
    verify-full with the runner's system CA bundle, so the certificate must
    authenticate that pinned hostname. The URL is turned into PG* environment
    variables for psql, so the password never appears in a process list.
After connecting, the server must also report current_user and
current_database() as that role and database before any count is read.

Every statement runs in a READ ONLY transaction with a 10 s statement timeout.
psql's stderr is never echoed: a failure prints one fixed-vocabulary line.
CSV must parse strictly. The artifact rechecks every count, the sweep, the
alias and all kept rows before publication.

Sanity caps, with justification:
  * AUDIT_ROW_CAP = 10 (+1 overflow witness): one receipt exists per fresh
    purge, and the fixture sequence performs a handful of purges per window;
    rows beyond 10 set `truncated` instead of growing the artifact, while the
    receipt count stays exact.
  * COUNT_SANITY_MAX = 1000000 per count query: a single fixture pair's
    all-time rows in any one staging table cannot approach one million (the
    fixture seeds bounded activity over days); a larger count means the
    predicates matched beyond the pair, so the read fails closed instead of
    publishing a misleading artifact. The offending count is never printed.

Exit status: 0 read, 1 read failed, 2 refused by a fence.

Usage (the workflow supplies the environment):
  python3 scripts/staging_privacy_verify_read.py \\
      --guild g --output staging-privacy-verify-read-<run_id>.json
"""

import argparse
import csv
from datetime import datetime, timezone
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import unicodedata
from urllib.parse import parse_qsl, unquote, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_automation_read_smoke as pins

SCHEMA_VERSION = 1
URL_ENV = "TWO_BOT_STAGING_PRIVACY_RO_DATABASE_URL"
HOST_ENV = "STAGING_PRIVACY_VERIFY_EXPECTED_HOST"
MEMBER_ENV = "STAGING_FIXTURE_MEMBER_ID"
H_GUILD_ENV = "STAGING_PRIVACY_VERIFY_H_GUILD_ID"
DB_ROLE = "two_bot_privacy_verify_ro"
DB_NAME = "two_bot"
SYSTEM_CA_BUNDLE = "/etc/ssl/certs/ca-certificates.crt"
GUILD_ALIASES = ("g", "h")
# The four opt-out purge tables (crates/cutover/src/privacy.rs PURGE_TABLES),
# then suppression presence and the receipt table.
COUNT_TABLES = ("events", "members", "invite_snapshots", "community_facts",
                "privacy_opt_outs", "privacy_opt_out_audit")
# Owner columns for the equality pair predicates. invite_snapshots
# attributes by inviter, community_facts by actor; the rest by member.
OWNER_COLUMN = {"events": "member_id", "members": "member_id",
                "invite_snapshots": "inviter_id", "community_facts": "actor_id",
                "privacy_opt_outs": "member_id",
                "privacy_opt_out_audit": "member_id"}
# Only events and community_facts match beyond direct ownership (serialized
# metadata / source-event / idempotency-key mentions). The members and
# invite_snapshots manifest predicates ARE the equality predicates, so no
# beyond-ownership match exists there by construction and no sweep runs.
SWEEP_TABLES = ("events", "community_facts")
AUDIT_ROW_CAP = 10
AUDIT_QUERY_LIMIT = AUDIT_ROW_CAP + 1  # one extra row detects overflow
COUNT_SANITY_MAX = 1_000_000
ACTOR_MAX_BYTES = 128
# Caller-supplied attribution is untrusted even when its service shape is valid.
# Publish only these identity-free fixture labels; additions need code review,
# not a dispatcher input or an environment binding.
PUBLISHABLE_ACTORS = frozenset({"member:self-serve", "operator:fixture-check"})
PSQL_TIMEOUT_SECONDS = 60

INSTANT_RE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]{1,3})?Z")
MEMBER_RE = re.compile(r"[0-9]{15,22}")
COUNT_RE = re.compile(r"[0-9]+")
RECORDED_AT_RE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z")
# libpq parameters a login URL may carry. Anything else (host=, options=,
# service=, passfile=, ...) could redirect the connection or change the session.
URL_PARAMS = {
    "sslmode": {"require", "verify-ca", "verify-full"},
    "channel_binding": {"require", "prefer", "disable"},
    "connect_timeout": None,  # digits, checked below
}
# The only environment psql inherits besides the PG* values built from the URL.
INHERITED_ENV = ("PATH", "HOME", "LANG", "LC_ALL", "TMPDIR")

BEGIN = "BEGIN READ ONLY;\nSET LOCAL statement_timeout = '10s';\n"
IDENTITY_SQL = BEGIN + "SELECT current_user, current_database();\nCOMMIT;\n"


def count_sql(table):
    """All-time equality pair count. Projects COUNT(*) only: the match
    columns never leave the server."""
    return (BEGIN + f"SELECT COUNT(*) FROM {table}\n"
            + f"WHERE guild_id = :'guild' AND {OWNER_COLUMN[table]} = :'member';\n"
            + "COMMIT;\n")


# Beyond-ownership sweep: the reviewed member_erasure manifest predicate
# ($1/$2 translated to psql :'guild'/:'member'), restricted to rows whose
# owner column is not the fixture member. IS DISTINCT FROM keeps NULL-owner
# rows in the sweep instead of dropping them through a NULL comparison.
# Projects COUNT(*) only: metadata, source-event and idempotency-key values
# never leave the server. Any drift from
# crates/cutover/src/member_erasure_plan.json must fail the offline
# conformance test and be re-reviewed here, never auto-adopted.
SWEEP_SQL = {
    "events": (
        BEGIN
        + "SELECT COUNT(*) FROM events\n"
        + "WHERE guild_id = :'guild'\n"
        + "  AND (member_id = :'member'\n"
        + "       OR metadata ~ ('(^|[^0-9])' || :'member' || '([^0-9]|$)')\n"
        + "       OR idempotency_key ~ ('(^|[^0-9])' || :'member' || '([^0-9]|$)'))\n"
        + "  AND member_id IS DISTINCT FROM :'member';\n"
        + "COMMIT;\n"
    ),
    "community_facts": (
        BEGIN
        + "SELECT COUNT(*) FROM community_facts\n"
        + "WHERE guild_id = :'guild'\n"
        + "  AND (actor_id = :'member'\n"
        + "       OR metadata ~ ('(^|[^0-9])' || :'member' || '([^0-9]|$)')\n"
        + "       OR source_event_id ~ ('(^|[^0-9])' || :'member' || '([^0-9]|$)')\n"
        + "       OR idempotency_key ~ ('(^|[^0-9])' || :'member' || '([^0-9]|$)'))\n"
        + "  AND actor_id IS DISTINCT FROM :'member';\n"
        + "COMMIT;\n"
    ),
}
# Receipt actor labels plus database timestamps, newest last, capped with an
# overflow witness. Scope columns stay in the predicate, never the projection.
AUDIT_ROWS_SQL = (
    BEGIN
    + "SELECT actor,\n"
    + "       to_char(opted_out_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')\n"
    + "FROM privacy_opt_out_audit\n"
    + "WHERE guild_id = :'guild'\n"
    + "  AND member_id = :'member'\n"
    + "ORDER BY id\n"
    + f"LIMIT {AUDIT_QUERY_LIMIT};\n"
    + "COMMIT;\n"
)


class Refused(Exception):
    """A fence refused the run before any row was read."""


class ReadFailed(Exception):
    """The read itself failed; the message is fixed vocabulary, never psql output."""


def format_instant(moment):
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{moment.microsecond // 1000:03d}Z"


def parse_instant(label, value):
    if not isinstance(value, str) or not INSTANT_RE.fullmatch(value):
        raise Refused(f"refusing: {label} must be a UTC instant like 2026-10-07T14:00:00Z")
    whole, _, fraction = value[:-1].partition(".")
    try:
        moment = datetime.strptime(whole, "%Y-%m-%dT%H:%M:%S")
    except ValueError:
        raise Refused(f"refusing: {label} is not a real UTC instant")
    millis = int((fraction or "0").ljust(3, "0"))
    return moment.replace(tzinfo=timezone.utc, microsecond=millis * 1000)


def fixture_member(env):
    value = env.get(MEMBER_ENV, "")
    if not value:
        raise Refused(f"refusing: no fixture member bound (${MEMBER_ENV})")
    if not MEMBER_RE.fullmatch(value):
        raise Refused(f"refusing: ${MEMBER_ENV} must be 15-22 digits")
    return value


def fence_guild(alias, env):
    """Map the g/h alias to its pinned guild id, refusing anything else.

    `g` reuses the TWO Staging pin through the Discord smokes' guard, so a
    repinned constant or the live guild refuses. `h` reads only the H pin
    binding, which stays unset until the second guild is recorded and
    verified not-live through the normal config path."""
    if alias not in GUILD_ALIASES:
        raise Refused("refusing: guild must be g or h")
    if alias == "g":
        try:
            return pins.guild_fence(pins.STAGING_GUILD_ID)
        except pins.SmokeError as error:
            raise Refused(str(error))
    pin = env.get(H_GUILD_ENV, "")
    if not pin:
        raise Refused(f"refusing: the second staging guild is not established (${H_GUILD_ENV})")
    if not pins.is_snowflake(pin):
        raise Refused(f"refusing: ${H_GUILD_ENV} must be a canonical snowflake")
    if pin == pins.LIVE_GUILD_ID:
        raise Refused("refusing: live guild id must never be read")
    if pin == pins.STAGING_GUILD_ID:
        raise Refused("refusing: the second staging guild must differ from the staging guild")
    return pin


def prod_like(name):
    return "prod" in name.lower()


def database_target(env):
    """Turn the login URL into PG* variables, refusing anything but the staging read-only login."""
    url = env.get(URL_ENV, "")
    if not url:
        raise Refused(f"refusing: no database login bound (${URL_ENV})")
    try:
        parts = urlsplit(url)
        port = parts.port
        host = parts.hostname
    except ValueError:
        raise Refused("refusing: the database login is not a valid postgres URL")
    if parts.scheme not in ("postgres", "postgresql") or not host or "," in parts.netloc.rpartition("@")[2]:
        raise Refused("refusing: the database login must be one postgres:// host")
    if port not in (None, 5432):
        raise Refused("refusing: the database port must be the PostgreSQL endpoint port")
    if "/" in host or "@" in host:
        raise Refused("refusing: the database host must be a bare name")
    expected_host = env.get(HOST_ENV, "")
    if (not expected_host or not expected_host.isascii() or expected_host != expected_host.strip()
            or any(character in expected_host for character in "/@,?#")):
        raise Refused(f"refusing: no valid staging host pin bound (${HOST_ENV})")
    if host.lower() != expected_host.lower():
        raise Refused("refusing: the database host does not match the pinned staging endpoint")
    if prod_like(host):
        raise Refused("refusing: the database host is production-like")
    user = unquote(parts.username or "")
    password = unquote(parts.password or "")
    database = unquote(parts.path.lstrip("/"))
    if user != DB_ROLE:
        raise Refused(f"refusing: the login must be role {DB_ROLE}")
    if not password:
        raise Refused("refusing: the database login carries no password")
    if prod_like(database):
        raise Refused("refusing: the database name is production-like")
    if database != DB_NAME:
        raise Refused(f"refusing: the database must be {DB_NAME}")
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
    return pg


def psql_environment(pg, env):
    return {**{key: env[key] for key in INHERITED_ENV if key in env}, **pg}


def classify_failure(stderr):
    """Map psql's stderr to a fixed phrase; the text itself may quote query values."""
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


def run_psql(psql, pg_env, script, variables=()):
    command = [psql, "-X", "-q", "-t", "--csv", "-v", "ON_ERROR_STOP=1"]
    for name, value in variables:
        command += ["-v", f"{name}={value}"]
    command += ["-f", "-"]
    try:
        done = subprocess.run(command, input=script, capture_output=True, text=True,
                              env=pg_env, timeout=PSQL_TIMEOUT_SECONDS, check=False)
    except subprocess.TimeoutExpired:
        raise ReadFailed("psql timed out")
    except OSError:
        raise ReadFailed("psql could not start")
    if done.returncode != 0:
        raise ReadFailed(f"{classify_failure(done.stderr)} (psql exit {done.returncode})")
    return done.stdout


def csv_rows(text):
    try:
        return [row for row in csv.reader(io.StringIO(text), strict=True) if row]
    except csv.Error:
        raise ReadFailed("a row had an unexpected shape; nothing was written") from None


def recorded_instant(value):
    """Parse the exact UTC millisecond form emitted by the query, or return None."""
    if not isinstance(value, str) or not RECORDED_AT_RE.fullmatch(value):
        return None
    try:
        return parse_instant("opted_out_at", value)
    except Refused:
        return None


def check_identity(output):
    rows = csv_rows(output)
    if rows != [[DB_ROLE, DB_NAME]]:
        raise Refused(f"refusing: the server does not report role {DB_ROLE} on database {DB_NAME}")


def parse_count(output):
    """One COUNT(*) row, one integer, inside the sanity ceiling. Never echoes."""
    rows = csv_rows(output)
    if len(rows) != 1 or len(rows[0]) != 1 or not COUNT_RE.fullmatch(rows[0][0]):
        raise ReadFailed("a count had an unexpected shape; nothing was written")
    try:
        count = int(rows[0][0])
    except ValueError:
        raise ReadFailed("a count had an unexpected shape; nothing was written") from None
    if count > COUNT_SANITY_MAX:
        raise ReadFailed("a count exceeded its sanity cap; nothing was written")
    return count


def valid_actor(value):
    """Mirror the service's actor shape (crates/cutover/src/privacy.rs
    valid_actor): nonempty, at most 128 bytes, no control characters."""
    return (isinstance(value, str) and bool(value.strip())
            and len(value.encode("utf-8")) <= ACTOR_MAX_BYTES
            and not any(unicodedata.category(char) == "Cc" for char in value))


def parse_audit_rows(output, forbidden_actors=()):
    rows = []
    for raw in csv_rows(output):
        if len(raw) != 2 or not valid_actor(raw[0]) or recorded_instant(raw[1]) is None:
            raise ReadFailed("a row had an unexpected shape; nothing was written")
        if raw[0] in forbidden_actors:
            raise ReadFailed("an audit row named the fixture subject; nothing was written")
        if raw[0] not in PUBLISHABLE_ACTORS:
            raise ReadFailed("an audit actor was not approved; nothing was written")
        rows.append({"actor": raw[0], "opted_out_at": raw[1]})
    if len(rows) > AUDIT_QUERY_LIMIT:
        raise ReadFailed("more rows than the query limit; nothing was written")
    return rows


def build_artifact(alias, collected_at, counts, sweep_extra, audit_rows):
    kept = audit_rows[:AUDIT_ROW_CAP]
    return {
        "schema_version": SCHEMA_VERSION,
        "guild": alias,
        "collected_at": collected_at,
        "counts": {table: counts[table] for table in COUNT_TABLES},
        "sweep_extra": {table: sweep_extra[table] for table in SWEEP_TABLES},
        "audit": {
            "truncated": len(audit_rows) > AUDIT_ROW_CAP,
            "rows": [{"ordinal": index, "actor": row["actor"], "opted_out_at": row["opted_out_at"]}
                     for index, row in enumerate(kept, start=1)],
        },
    }


def artifact_errors(artifact):
    """The shape and count/alias/row contract, checked before every write."""
    errors = []
    if set(artifact) != {"schema_version", "guild", "collected_at", "counts",
                         "sweep_extra", "audit"}:
        errors.append("top-level keys")
    if artifact.get("schema_version") != SCHEMA_VERSION:
        errors.append("schema_version")
    if artifact.get("guild") not in GUILD_ALIASES:
        errors.append("guild")
    if recorded_instant(artifact.get("collected_at")) is None:
        errors.append("collected_at")
    counts = artifact.get("counts")
    if (not isinstance(counts, dict) or set(counts) != set(COUNT_TABLES)
            or any(not isinstance(count, int) or isinstance(count, bool)
                   or not 0 <= count <= COUNT_SANITY_MAX for count in counts.values())):
        errors.append("counts")
    sweep = artifact.get("sweep_extra")
    if (not isinstance(sweep, dict) or set(sweep) != set(SWEEP_TABLES)
            or any(not isinstance(count, int) or isinstance(count, bool)
                   or not 0 <= count <= COUNT_SANITY_MAX for count in sweep.values())):
        errors.append("sweep_extra")
    audit = artifact.get("audit")
    rows = audit.get("rows") if isinstance(audit, dict) else None
    if not isinstance(audit, dict) or set(audit) != {"truncated", "rows"}:
        errors.append("audit")
        return errors
    if not isinstance(audit.get("truncated"), bool):
        errors.append("truncated")
    if not isinstance(rows, list) or len(rows) > AUDIT_ROW_CAP:
        errors.append("audit rows")
        return errors
    for index, row in enumerate(rows, start=1):
        if (not isinstance(row, dict) or set(row) != {"ordinal", "actor", "opted_out_at"}
                or row["ordinal"] != index or not valid_actor(row["actor"])
                or row["actor"] not in PUBLISHABLE_ACTORS
                or recorded_instant(row["opted_out_at"]) is None):
            errors.append(f"audit row {index}")
    return errors


def summary_text(artifact):
    lines = ["## staging privacy verify read",
             f"guild: {artifact['guild']}",
             f"collected_at: {artifact['collected_at']}"]
    lines += [f"- {table}: {artifact['counts'][table]}" for table in COUNT_TABLES]
    lines += [f"- sweep_extra {table}: {artifact['sweep_extra'][table]}" for table in SWEEP_TABLES]
    audit = artifact["audit"]
    lines.append(f"audit rows: {len(audit['rows'])} (truncated: {str(audit['truncated']).lower()})")
    return "\n".join(lines) + "\n"


def read_verify(alias, env, psql, now):
    """Every fence, then the identity probe, then the counts. Returns the artifact."""
    member = fixture_member(env)
    guild = fence_guild(alias, env)
    pg_env = psql_environment(database_target(env), env)
    if not psql:
        raise Refused("refusing: psql is not installed on this runner")
    check_identity(run_psql(psql, pg_env, IDENTITY_SQL))
    variables = (("guild", guild), ("member", member))
    counts = {table: parse_count(run_psql(psql, pg_env, count_sql(table), variables))
              for table in COUNT_TABLES}
    sweep_extra = {table: parse_count(run_psql(psql, pg_env, SWEEP_SQL[table], variables))
                   for table in SWEEP_TABLES}
    # The audit writer accepts a caller-supplied actor, so a fixture run could
    # store the member or guild id as the actor. Rejecting those values keeps
    # the retained artifact subject-free instead of publishing an id as a label.
    audit_rows = parse_audit_rows(run_psql(psql, pg_env, AUDIT_ROWS_SQL, variables),
                                  forbidden_actors={member, guild})
    artifact = build_artifact(alias, format_instant(now), counts, sweep_extra, audit_rows)
    if artifact_errors(artifact):
        raise ReadFailed("the artifact failed its own shape check; nothing was written")
    return artifact


def main(argv=None, env=None, now=None, psql=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--guild", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args(argv)
    env = os.environ if env is None else env
    now = now or datetime.now(timezone.utc)
    psql = shutil.which("psql") if psql is None else psql
    try:
        artifact = read_verify(args.guild, env, psql, now)
    except Refused as error:
        print(str(error), file=sys.stderr)
        return 2
    except ReadFailed as error:
        print(f"failed: {error}", file=sys.stderr)
        return 1
    # Broad by design, with no traceback: it could quote the member or the login.
    except Exception as error:
        print(f"failed: unexpected {type(error).__name__}; nothing was written", file=sys.stderr)
        return 1
    args.output.write_text(json.dumps(artifact, indent=2) + "\n", encoding="utf-8")
    summary = env.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write(summary_text(artifact))
    audit = artifact["audit"]
    print(f"ok: guild={artifact['guild']} audit_rows={len(audit['rows'])} "
          f"truncated={str(audit['truncated']).lower()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
