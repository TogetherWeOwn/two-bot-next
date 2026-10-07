#!/usr/bin/env python3
"""Read-only staging `events` read for the B2 soak (stdlib only; shells out to psql).

This is the processed side of docs/evidence-route.md step 2, run by the manual
`staging-events-read` workflow so that no agent ever holds a database
credential. It reads the committed funnel rows of ONE fixture member in ONE
window of at most 15 minutes and writes a sanitized artifact:

  {"schema_version": 1,
   "window": {"start": "...Z", "end": "...Z"},
   "row_count": <rows kept, at most 60>,
   "truncated": <more than 60 rows matched>,
   "rows": [{"ordinal": 1, "event_type": "...", "recorded_at": "...Z"}]}

The idempotency key (it embeds guild:member), the member, the guild, the
source and the metadata never enter the artifact, the log or the step summary.
The query does not even select the key, so it never reaches this process.

Fences, all before the first connection (exit 2, nothing was sent):
  * the window is UTC, `end > start`, at most 15 minutes, and already over;
  * the guild is the pinned TWO Staging id, checked with the same guard the
    Discord smokes use (the live guild and any other guild refuse);
  * the fixture member comes only from $STAGING_FIXTURE_MEMBER_ID (digits,
    15-22), never from an input, so a dispatcher cannot aim the job at another
    member;
  * the database login comes only from $TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL
    and must name role two_bot_events_ro on database two_bot at a host that is
    not production-like (the same rule as crates/cutover/src/staging_migrate.rs).
    Only sslmode, channel_binding and connect_timeout may ride on the URL, and
    the URL is turned into PG* environment variables for psql, so the password
    never appears in a process list.
After connecting, the server must also report current_user and
current_database() as that role and database before the rows are read.

Every statement runs in a READ ONLY transaction with a 10 s statement timeout.
psql's stderr is never echoed: a failure prints one fixed-vocabulary line.

Exit status: 0 read, 1 read failed, 2 refused by a fence.

Usage (the workflow supplies the environment):
  python3 scripts/staging_events_read.py \\
      --window-start 2026-10-07T14:00:00Z --window-end 2026-10-07T14:15:00Z \\
      --output staging-events-read-<run_id>.json
"""

import argparse
import csv
from datetime import datetime, timedelta, timezone
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
from urllib.parse import parse_qsl, unquote, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_automation_read_smoke as pins  # noqa: E402

SCHEMA_VERSION = 1
URL_ENV = "TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL"
MEMBER_ENV = "STAGING_FIXTURE_MEMBER_ID"
DB_ROLE = "two_bot_events_ro"
DB_NAME = "two_bot"
MAX_WINDOW = timedelta(minutes=15)
ROW_CAP = 60
QUERY_LIMIT = ROW_CAP + 1  # one extra row detects overflow
PSQL_TIMEOUT_SECONDS = 60

INSTANT_RE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]{1,3})?Z")
MEMBER_RE = re.compile(r"[0-9]{15,22}")
EVENT_TYPE_RE = re.compile(r"[a-z][a-z0-9_]{0,47}")
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
# docs/evidence-route.md step 2, selecting only what the artifact carries. The
# casts make the window compare identically whether the column is the legacy
# ISO text or the timestamptz the 0405 migration converts it to.
ROWS_SQL = (
    BEGIN
    + "SELECT event_type,\n"
    + "       to_char(recorded_at::timestamptz AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')\n"
    + "FROM events\n"
    + "WHERE guild_id = :'guild'\n"
    + "  AND member_id = :'member'\n"
    + "  AND recorded_at::timestamptz BETWEEN :'window_start'::timestamptz AND :'window_end'::timestamptz\n"
    + "ORDER BY id\n"
    + f"LIMIT {QUERY_LIMIT};\n"
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


def parse_window(start, end, now):
    first = parse_instant("window_start", start)
    last = parse_instant("window_end", end)
    if last <= first:
        raise Refused("refusing: window_end must be after window_start")
    if last - first > MAX_WINDOW:
        raise Refused("refusing: the window may be at most 15 minutes")
    if last > now:
        raise Refused("refusing: the window must already have ended")
    return format_instant(first), format_instant(last)


def fixture_member(env):
    value = env.get(MEMBER_ENV, "")
    if not value:
        raise Refused(f"refusing: no fixture member bound (${MEMBER_ENV})")
    if not MEMBER_RE.fullmatch(value):
        raise Refused(f"refusing: ${MEMBER_ENV} must be 15-22 digits")
    return value


def fence_guild(guild_id=None):
    guild_id = pins.STAGING_GUILD_ID if guild_id is None else guild_id
    try:
        return pins.guild_fence(guild_id)
    except pins.SmokeError as error:
        raise Refused(str(error))


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
    if "/" in host or "@" in host:
        raise Refused("refusing: the database host must be a bare name")
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
          "PGSSLMODE": "require", "PGCONNECT_TIMEOUT": "10"}
    if port is not None:
        pg["PGPORT"] = str(port)
    try:
        params = parse_qsl(parts.query, keep_blank_values=True, strict_parsing=bool(parts.query))
    except ValueError:
        raise Refused("refusing: the database login query is malformed")
    for key, value in params:
        if key not in URL_PARAMS:
            raise Refused(f"refusing: the database login may not set {key}")
        allowed = URL_PARAMS[key]
        if (allowed is not None and value not in allowed) or (allowed is None and not value.isdigit()):
            raise Refused(f"refusing: the database login has a bad {key}")
        pg[{"sslmode": "PGSSLMODE", "channel_binding": "PGCHANNELBINDING",
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
    return [row for row in csv.reader(io.StringIO(text)) if row]


def check_identity(output):
    rows = csv_rows(output)
    if rows != [[DB_ROLE, DB_NAME]]:
        raise Refused(f"refusing: the server does not report role {DB_ROLE} on database {DB_NAME}")


def parse_rows(output):
    rows = []
    for raw in csv_rows(output):
        if len(raw) != 2 or not EVENT_TYPE_RE.fullmatch(raw[0]) or not RECORDED_AT_RE.fullmatch(raw[1]):
            raise ReadFailed("a row had an unexpected shape; nothing was written")
        rows.append({"event_type": raw[0], "recorded_at": raw[1]})
    if len(rows) > QUERY_LIMIT:
        raise ReadFailed("more rows than the query limit; nothing was written")
    return rows


def build_artifact(window_start, window_end, rows):
    kept = rows[:ROW_CAP]
    return {
        "schema_version": SCHEMA_VERSION,
        "window": {"start": window_start, "end": window_end},
        "row_count": len(kept),
        "truncated": len(rows) > ROW_CAP,
        "rows": [{"ordinal": index, "event_type": row["event_type"], "recorded_at": row["recorded_at"]}
                 for index, row in enumerate(kept, start=1)],
    }


def artifact_errors(artifact):
    """The shape contract, used before every write and by the offline tests."""
    errors = []
    if set(artifact) != {"schema_version", "window", "row_count", "truncated", "rows"}:
        errors.append("top-level keys")
    if artifact.get("schema_version") != SCHEMA_VERSION:
        errors.append("schema_version")
    window = artifact.get("window")
    if not isinstance(window, dict) or set(window) != {"start", "end"} or not all(
            isinstance(v, str) and RECORDED_AT_RE.fullmatch(v) for v in window.values()):
        errors.append("window")
    rows = artifact.get("rows")
    if not isinstance(rows, list) or len(rows) > ROW_CAP or artifact.get("row_count") != len(rows):
        errors.append("rows/row_count")
        return errors
    if not isinstance(artifact.get("truncated"), bool):
        errors.append("truncated")
    for index, row in enumerate(rows, start=1):
        if (not isinstance(row, dict) or set(row) != {"ordinal", "event_type", "recorded_at"}
                or row["ordinal"] != index or not EVENT_TYPE_RE.fullmatch(str(row["event_type"]))
                or not RECORDED_AT_RE.fullmatch(str(row["recorded_at"]))):
            errors.append(f"row {index}")
    return errors


def summary_text(artifact):
    counts = {}
    for row in artifact["rows"]:
        counts[row["event_type"]] = counts.get(row["event_type"], 0) + 1
    lines = ["## staging events read",
             f"window: {artifact['window']['start']} to {artifact['window']['end']}",
             f"rows: {artifact['row_count']} (truncated: {str(artifact['truncated']).lower()})"]
    lines += [f"- {name}: {count}" for name, count in sorted(counts.items())]
    return "\n".join(lines) + "\n"


def read_events(window_start, window_end, env, psql):
    """Every fence, then the identity probe, then the rows. Returns the artifact."""
    member = fixture_member(env)
    guild = fence_guild()
    pg_env = psql_environment(database_target(env), env)
    if not psql:
        raise Refused("refusing: psql is not installed on this runner")
    check_identity(run_psql(psql, pg_env, IDENTITY_SQL))
    output = run_psql(psql, pg_env, ROWS_SQL, (("guild", guild), ("member", member),
                                              ("window_start", window_start), ("window_end", window_end)))
    artifact = build_artifact(window_start, window_end, parse_rows(output))
    if artifact_errors(artifact):
        raise ReadFailed("the artifact failed its own shape check; nothing was written")
    return artifact


def main(argv=None, env=None, now=None, psql=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--window-start", required=True)
    parser.add_argument("--window-end", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args(argv)
    env = os.environ if env is None else env
    now = now or datetime.now(timezone.utc)
    psql = shutil.which("psql") if psql is None else psql
    try:
        window_start, window_end = parse_window(args.window_start, args.window_end, now)
        artifact = read_events(window_start, window_end, env, psql)
    except Refused as error:
        print(str(error), file=sys.stderr)
        return 2
    except ReadFailed as error:
        print(f"failed: {error}", file=sys.stderr)
        return 1
    except Exception as error:  # noqa: BLE001 - a traceback could quote the member or the login
        print(f"failed: unexpected {type(error).__name__}; nothing was written", file=sys.stderr)
        return 1
    args.output.write_text(json.dumps(artifact, indent=2) + "\n", encoding="utf-8")
    summary = env.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write(summary_text(artifact))
    print(f"ok: {artifact['row_count']} rows, truncated={str(artifact['truncated']).lower()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
