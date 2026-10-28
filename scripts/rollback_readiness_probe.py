#!/usr/bin/env python3
"""Read-only rollback-readiness probe for the B4 cutover (stdlib only).

Four checks, one PASS/FAIL line each; exit 1 if any fails, 0 if all pass:

  manifest       the newest `two-funnel-*.ndjson.gz` in the backup directory
                 (the drill's `ls -1t` selector) has a parseable v3/v4/v5/v6/v7/v8 manifest
                 and an end marker whose row total matches the manifest counts.
  sequences      every live serial/identity sequence, from a saved result of
                 SEQUENCES_QUERY, has a manifest high-water at or past its live
                 `last_value`. A restore restarts allocators from the archive,
                 so anything less rewinds them (docs/cutover.md, allocator gate).
  readyz         the staging Worker answers /readyz with the bot's truthful
                 component breakdown (wrangler/scripts/check-readyz.mjs).
  deploy-config  each named environment in wrangler.toml declares the required
                 TWO_* vars, non-empty (vars are not inherited).

The probe never writes, migrates or connects to a database. The live sequence
state is an operator-supplied file: run `--print-sequences-query` through the
approved read-only binding, never a URL argument. The only network call is one
GET to an origin matching the staging Worker pattern; anything else, including
a redirect, is refused before or instead of being followed. This is a screen,
not a restore verification: `two-bot restore --dry-run` remains the full
archive inspection (docs/backup.md).

Body-cap refusal contract: the transport reads at most READYZ_BODY_CAP+1
bytes on both the normal and HTTPError paths so a hidden suffix is detected.
Any body longer than READYZ_BODY_CAP (64 KiB) is refused before JSON parsing
with the fixed message "/readyz answered over the body cap"; exact-cap valid
input passes, and the extra byte never reaches a successful JSON path.
Refusals never echo the body, headers, URL or parse exceptions.

Input budgets: the gzip stream is metered against DECOMPRESSED_CAP (payload
plus newline bytes; exactly the cap passes, one byte more refuses, and reads
stop at the breach). The operator-supplied sequences file and wrangler config
are each read as bounded bytes before any decode, refusing past
SEQUENCES_FILE_CAP and CONFIG_FILE_CAP. The manifest createdAt is
shape- and length-validated before it can reach verdict output. All of these
refuse with fixed vocabulary naming no input content.

Usage:
  python3 scripts/rollback_readiness_probe.py --backup-dir DIR \\
      --sequences live-sequences.json [--staging-url URL] [--config wrangler.toml]
"""

import argparse
from dataclasses import dataclass
from datetime import date, time
import gzip
import http.client
import json
import os
from pathlib import Path
import re
import sys
import tomllib
import urllib.error
import urllib.request
from urllib.parse import urlsplit
import zlib

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CONFIG = ROOT / "wrangler/wrangler.toml"
ARCHIVE_PREFIX = "two-funnel-"
ARCHIVE_SUFFIX = ".ndjson.gz"
DUMP_VERSIONS = (3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16)
# Same budgets as the Rust reader (crates/core/src/backup/dump_file.rs),
# whose decoded budget equals its compressed budget and counts newlines.
COMPRESSED_CAP = 1 << 30
DECOMPRESSED_CAP = 1 << 30
LINE_CAP = 8 << 20
# Operator-supplied inputs: live sequence dumps are kilobytes and wrangler
# configs tens of kilobytes, so 1 MiB each is generous headroom that still
# bounds the gate's reads before any parse runs.
SEQUENCES_FILE_CAP = 1 << 20
CONFIG_FILE_CAP = 1 << 20
I64_MAX = (1 << 63) - 1
DEFAULT_ENVS = ("staging", "production")
# Required beyond the TWO_* keys of top-level [vars], which every named env
# must repeat (scripts/check-env-bindings.py EXTRA_REQUIRED_VARS).
REQUIRED_TWO_KEYS = ("TWO_GUILD_NAME",)
# The approved staging origin (wrangler/scripts/ownership-control.mjs).
STAGING_HOST = re.compile(r"two-bot-next-staging\.[a-z0-9-]+\.workers\.dev")
READYZ_TIMEOUT_SECONDS = 10
READYZ_BODY_CAP = 64 << 10
# Cloudflare rejects the default "Python-urllib/x.y" agent at the edge with 403
# (error 1010), so an explicit agent is required for the probe to reach the Worker.
USER_AGENT = "two-bot-next-staging-rollout/1.0"
# The dumper writes UTC createdAt ("...T00:00:00.000Z"); accept that shape with
# optional sub-second precision (nanoseconds at most) and numeric offsets, and
# bound the length well above any of those so hostile text never reaches output.
CREATED_AT_RE = re.compile(
    r"(?P<Y>\d{4})-(?P<m>\d{2})-(?P<d>\d{2})"
    r"T(?P<H>\d{2}):(?P<M>\d{2}):(?P<S>\d{2})"
    r"(?:\.(?P<frac>\d{1,9}))?(?:Z|(?P<oh>[+-]\d{2}):(?P<om>\d{2}))", re.ASCII)
CREATED_AT_MAX_LEN = 64
# Component names from crates/bot/src/server.rs; the drift test pins them to the source.
# Per-surface allowlist: exactly the four components the container serves.
# Anything else is refused with fixed vocabulary naming no component data.
READYZ_STATES = ("ready", "starting", "down")
READYZ_REQUIRED = ("process", "gateway", "database", "token_invalid")
READYZ_ALLOWED = frozenset({"process", "gateway", "database", "token_invalid"})
# Only the gateway parks: a 503 is accepted only with it starting or down. A database ping failure or a
# latched bot token is a fault for this process (docs/runbook.md, docs/rest-guard.md), never parked.
READYZ_PARKED = {"gateway": ("down", "starting")}
# Owned serial/identity sequences via pg_get_serial_sequence, plus the standalone
# guild_settings_version_seq (guarded by to_regclass), mirroring the restore
# (crates/core/src/backup/dump.rs). Also reports whether last_value is readable:
# pg_sequences reports NULL both for "never called" and "no privilege".
SEQUENCES_QUERY = """\
SELECT coalesce(json_agg(json_build_object(
         'table', t.table_name, 'column', t.column_name, 'sequence', t.seq,
         'last_value', s.last_value,
         'readable', has_sequence_privilege(t.seq, 'SELECT,USAGE'))
       ORDER BY t.table_name, t.column_name), '[]'::json)
FROM (SELECT table_name, column_name,
             pg_get_serial_sequence(
               quote_ident(table_schema) || '.' || quote_ident(table_name), column_name) AS seq
      FROM information_schema.columns
      WHERE table_schema = current_schema()
      UNION ALL
      SELECT 'guild_settings' AS table_name, 'version' AS column_name,
             format('%I.%I', current_schema(), 'guild_settings_version_seq') AS seq
      WHERE to_regclass('guild_settings_version_seq') IS NOT NULL) t
JOIN pg_sequences s ON format('%I.%I', s.schemaname, s.sequencename) = t.seq
WHERE t.seq IS NOT NULL;
"""


class ProbeError(Exception):
    """A check's one-line failure reason."""


@dataclass(frozen=True)
class Result:
    name: str
    ok: bool
    reason: str

    def line(self):
        return f"{'PASS' if self.ok else 'FAIL'} {self.name}: {self.reason}"


class DuplicateKeyError(ValueError):
    """A JSON object repeats a key; the message never carries key/value data."""


def _reject_duplicate_keys(pairs):
    """object_pairs_hook refusing repeats instead of last-wins.

    The json decoder calls this for every object at any depth, after
    escape decoding, so {"a": 1, "\\u0061": 2} counts as a repeat. The
    raised message is fixed vocabulary: it names no key, value or path.
    """
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise DuplicateKeyError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


def decode_strict(body):
    """Decode JSON while refusing duplicate object keys at any depth."""
    return json.loads(body, object_pairs_hook=_reject_duplicate_keys)


def is_int(value, low=0, high=I64_MAX):
    # JSON true/false decode to bool, a subclass of int; floats never count.
    return isinstance(value, int) and not isinstance(value, bool) and low <= value <= high


def latest_archive(backup_dir):
    if backup_dir is None:
        raise ProbeError("no backup directory given (--backup-dir or TWO_BACKUP_DIR)")
    directory = Path(backup_dir)
    if not directory.is_dir():
        raise ProbeError(f"backup directory {directory} does not exist")
    archives = [p for p in directory.iterdir()
                if p.name.startswith(ARCHIVE_PREFIX) and p.name.endswith(ARCHIVE_SUFFIX)
                and p.is_file()]
    if not archives:
        raise ProbeError(f"no {ARCHIVE_PREFIX}*{ARCHIVE_SUFFIX} archive in {directory}")
    return max(archives, key=lambda p: (p.stat().st_mtime_ns, p.name))


def read_line(stream):
    line = stream.readline(LINE_CAP + 1)
    if len(line) > LINE_CAP:
        raise ProbeError(f"a line exceeds the {LINE_CAP}-byte cap")
    return line


class BudgetedStream:
    """Counts decompressed bytes (payload plus newlines) as they are read."""

    def __init__(self, stream):
        self._stream = stream
        self.total = 0

    def readline(self, size):
        size = min(size, DECOMPRESSED_CAP - self.total + 1)
        line = self._stream.readline(size)
        self.total += len(line)
        if self.total > DECOMPRESSED_CAP:
            raise ProbeError("archive exceeds decompressed byte cap")
        return line


def valid_created_at(value):
    if not isinstance(value, str) or len(value) > CREATED_AT_MAX_LEN:
        return False
    match = CREATED_AT_RE.fullmatch(value)
    if not match:
        return False
    try:
        date(int(match["Y"]), int(match["m"]), int(match["d"]))
        time(int(match["H"]), int(match["M"]), int(match["S"]))
        if match["oh"] is not None and (abs(int(match["oh"])) > 23
                                      or int(match["om"]) > 59):
            return False
    except ValueError:
        return False
    return True


def validate_manifest(obj):
    """Mirror the Rust reader's manifest refusals, minus the table inventory."""
    if not isinstance(obj, dict) or obj.get("kind") != "manifest":
        raise ProbeError("first line is not a manifest")
    if not is_int(obj.get("version")) or obj["version"] not in DUMP_VERSIONS:
        raise ProbeError(f"manifest version {obj.get('version')!r} is not one of {DUMP_VERSIONS}")
    if not isinstance(obj.get("createdAt"), str):
        raise ProbeError("manifest has no createdAt timestamp")
    if not valid_created_at(obj["createdAt"]):
        raise ProbeError("manifest has invalid createdAt timestamp")
    if not is_int(obj.get("eventsSequence")):
        raise ProbeError("manifest has an invalid eventsSequence")
    migrations = obj.get("schemaMigrations")
    if not isinstance(migrations, list) or not all(isinstance(m, str) for m in migrations):
        raise ProbeError("manifest has an invalid schemaMigrations list")
    sequences = obj.get("sequences", {})
    if not isinstance(sequences, dict):
        raise ProbeError("manifest has invalid sequences")
    for name, mark in sequences.items():
        if not is_int(mark):
            raise ProbeError(f"manifest sequence {name} has an invalid high-water mark")
    tables = obj.get("tables")
    if not isinstance(tables, list) or not tables:
        raise ProbeError("manifest has no table list")
    names = set()
    for table in tables:
        name = table.get("name") if isinstance(table, dict) else None
        if not isinstance(name, str):
            raise ProbeError("manifest table has no name")
        if name in names:
            raise ProbeError(f"manifest table {name} is duplicated")
        names.add(name)
        columns = table.get("columns")
        if not isinstance(columns, list) or not all(isinstance(c, str) for c in columns):
            raise ProbeError(f"manifest table {name} has an invalid column list")
        if not is_int(table.get("count"), high=(1 << 64) - 1):
            raise ProbeError(f"manifest table {name} has an invalid row count")
    return obj


def read_manifest(path):
    """Parse the manifest, then stream to the end marker; rows are not decoded."""
    size = path.stat().st_size
    if size > COMPRESSED_CAP:
        raise ProbeError(f"{path.name} is {size} bytes, past the {COMPRESSED_CAP}-byte cap")
    try:
        with gzip.open(path, "rb") as raw:
            stream = BudgetedStream(raw)
            first = read_line(stream)
            while first and not first.strip():
                first = read_line(stream)
            if not first:
                raise ProbeError(f"{path.name} is empty")
            manifest = validate_manifest(decode_strict(first))
            last = b""
            while line := read_line(stream):
                if line.strip():
                    last = line
    except ProbeError:
        raise
    except DuplicateKeyError:
        raise ProbeError(f"{path.name} manifest line carries duplicate JSON object keys")
    except (OSError, EOFError, zlib.error) as e:
        raise ProbeError(f"{path.name} is not a complete gzip archive ({e.__class__.__name__})")
    except (ValueError, RecursionError):
        # ValueError also carries bad UTF-8 and the int-digit cap; refuse all as non-JSON.
        raise ProbeError(f"{path.name} manifest line is not JSON")
    try:
        end = decode_strict(last) if last else None
    except DuplicateKeyError:
        raise ProbeError(f"{path.name} end marker carries duplicate JSON object keys")
    except (ValueError, RecursionError):
        end = None
    if not isinstance(end, dict) or end.get("kind") != "end" or not is_int(end.get("rows"), high=(1 << 64) - 1):
        raise ProbeError(f"{path.name} has no end marker: truncated, treat it as lost")
    declared = sum(t["count"] for t in manifest["tables"])
    if end["rows"] != declared:
        raise ProbeError(f"{path.name} end marker says {end['rows']} rows, manifest counts {declared}")
    return manifest


def describe_manifest(archive, manifest):
    return (f"{archive.name} v{manifest['version']}, {len(manifest['tables'])} tables, "
            f"createdAt {manifest['createdAt']}, eventsSequence {manifest['eventsSequence']}")


def load_live_sequences(path):
    if path is None:
        raise ProbeError("no live sequences query result given (--sequences FILE)")
    try:
        with open(path, "rb") as f:
            data = f.read(SEQUENCES_FILE_CAP + 1)
    except OSError as e:
        raise ProbeError(f"cannot read {path} ({e.__class__.__name__})")
    if len(data) > SEQUENCES_FILE_CAP:
        raise ProbeError("sequences file exceeds byte cap")
    try:
        rows = decode_strict(data)
    except DuplicateKeyError:
        raise ProbeError(f"{path} carries duplicate JSON object keys")
    except (ValueError, RecursionError):
        # Excessively nested input exhausts the decoder's recursion budget.
        raise ProbeError(f"{path} is not JSON; expected the SEQUENCES_QUERY result")
    if not isinstance(rows, list):
        raise ProbeError(f"{path} is not a JSON array; expected the SEQUENCES_QUERY result")
    for row in rows:
        if (not isinstance(row, dict)
                or not all(isinstance(row.get(k), str) for k in ("table", "column", "sequence"))
                or not isinstance(row.get("readable"), bool)
                or not (row.get("last_value") is None or is_int(row["last_value"], low=-I64_MAX - 1))):
            raise ProbeError(f"{path} row {row!r:.80} does not match the SEQUENCES_QUERY shape")
    if not rows:
        raise ProbeError("live query returned no sequences: wrong database, schema or role")
    return rows


def check_sequences(manifest, rows):
    tables = {t["name"] for t in manifest["tables"]}
    marks = manifest.get("sequences", {})
    problems = []
    for row in rows:
        where = f"{row['table']}.{row['column']}"
        mark = marks.get(row["table"])
        if mark is None and (row["table"], row["column"]) == ("events", "id"):
            # MAX(events.id) at dump time (crates/core/src/backup/dump.rs).
            mark = manifest["eventsSequence"]
        if row["table"] not in tables:
            problems.append(f"{where} (table not in the backup)")
        elif not row["readable"]:
            problems.append(f"{where} (no SELECT/USAGE on {row['sequence']})")
        elif mark is None:
            problems.append(f"{where} (no manifest high-water)")
        elif row["last_value"] is not None and row["last_value"] > mark:
            problems.append(f"{where} (live {row['last_value']} > manifest {mark}, would rewind)")
    if problems:
        shown = ", ".join(problems[:3]) + (f", +{len(problems) - 3} more" if len(problems) > 3 else "")
        raise ProbeError(f"{len(problems)} of {len(rows)} live sequences not covered: {shown}")
    return f"manifest covers all {len(rows)} live sequences"


def staging_readyz_url(url):
    if not url:
        raise ProbeError("no staging Worker URL given (--staging-url or STAGING_WORKER_URL)")
    try:
        parts = urlsplit(url)
    except ValueError:
        raise ProbeError("refusing: not the two-bot-next-staging workers.dev origin")
    try:
        port = parts.port
    except ValueError:
        port = "invalid"
    if (parts.scheme != "https" or port is not None or parts.username or parts.password
            or not STAGING_HOST.fullmatch(parts.hostname or "")
            or parts.path not in ("", "/") or parts.query or parts.fragment):
        raise ProbeError("refusing: not the two-bot-next-staging workers.dev origin")
    return f"https://{parts.hostname}/readyz"


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None  # urllib then raises the 3xx as an HTTPError


def fetch(url):
    """One GET without redirects; returns (status, body bytes)."""
    opener = urllib.request.build_opener(_NoRedirect)
    try:
        with opener.open(urllib.request.Request(url, headers={"User-Agent": USER_AGENT},
                                               method="GET"),
                         timeout=READYZ_TIMEOUT_SECONDS) as response:
            return response.status, response.read(READYZ_BODY_CAP + 1)
    except urllib.error.HTTPError as e:
        return e.code, e.read(READYZ_BODY_CAP + 1)


def readyz_state(status, body):
    if len(body) > READYZ_BODY_CAP:
        raise ProbeError("/readyz answered over the body cap")
    try:
        report = decode_strict(body)
    except DuplicateKeyError:
        raise ProbeError(f"/readyz {status} carries duplicate JSON object keys")
    except (ValueError, RecursionError):
        # Excessively nested input exhausts the decoder's recursion budget
        # below READYZ_BODY_CAP; grade it as no breakdown, never a crash.
        report = None
    components = report.get("components") if isinstance(report, dict) else None
    if (not isinstance(components, list) or "error" in report
            or not all(isinstance(c, list) and len(c) == 2 and isinstance(c[0], str)
                       and c[1] in READYZ_STATES for c in components)):
        raise ProbeError(f"/readyz {status} without the bot's component breakdown "
                         "(ownership refusal or broken deploy)")
    state = dict(components)
    if len(state) != len(components):
        raise ProbeError(f"/readyz {status} repeats a component name")
    if any(name not in READYZ_ALLOWED for name in state):
        raise ProbeError(f"/readyz {status} carries an unknown component name")
    missing = [name for name in READYZ_REQUIRED if name not in state]
    if missing:
        raise ProbeError(f"/readyz {status} lacks {', '.join(missing)}")
    return state


def check_readyz(url, fetch_fn):
    endpoint = staging_readyz_url(url)
    try:
        status, body = fetch_fn(endpoint)
    except (OSError, http.client.HTTPException) as e:
        raise ProbeError(f"{endpoint} did not respond ({e.__class__.__name__})")
    state = readyz_state(status, body)
    not_ready = sorted((name, value) for name, value in state.items() if value != "ready")
    if status == 200 and not not_ready:
        return f"/readyz 200: all {len(state)} components ready"
    if status == 503 and not_ready and all(value in READYZ_PARKED.get(name, ())
                                           for name, value in not_ready):
        summary = ", ".join(f"{name} {value}" for name, value in not_ready)
        return f"/readyz 503: process ready, {summary} (parked, not E2E approval)"
    shown = ", ".join(f"{name} {value}" for name, value in not_ready or sorted(state.items()))
    raise ProbeError(f"/readyz {status} with unexpected components: {shown:.200}")


def check_deploy_config(config, envs, extra_keys):
    try:
        with open(config, "rb") as f:
            data = f.read(CONFIG_FILE_CAP + 1)
    except OSError as e:
        raise ProbeError(f"cannot read {config} ({e.__class__.__name__})")
    if len(data) > CONFIG_FILE_CAP:
        raise ProbeError("wrangler config exceeds byte cap")
    try:
        cfg = tomllib.loads(data.decode("utf-8"))
    except (ValueError, RecursionError):
        # tomllib raises a plain ValueError for integers past the digit cap.
        raise ProbeError(f"{config} is not valid TOML")
    required = sorted(set(REQUIRED_TWO_KEYS) | set(extra_keys)
                      | {k for k in cfg.get("vars", {}) if k.startswith("TWO_")})
    problems = []
    for env in envs:
        section = cfg.get("env", {}).get(env)
        if not isinstance(section, dict):
            problems.append(f"[env.{env}] is missing")
            continue
        variables = section.get("vars", {})
        missing = [k for k in required if not (isinstance(variables.get(k), str) and variables[k])]
        if missing:
            problems.append(f"[env.{env}.vars] lacks {', '.join(missing)}")
    if problems:
        raise ProbeError("; ".join(problems))
    return f"{', '.join(envs)} declare {', '.join(required)}"


def run(args, fetch_fn=fetch):
    results = []

    def record(name, check, *check_args):
        try:
            results.append(Result(name, True, check(*check_args)))
        except ProbeError as e:
            results.append(Result(name, False, str(e)))

    manifest = None
    try:
        archive = latest_archive(args.backup_dir)
        manifest = read_manifest(archive)
        results.append(Result("manifest", True, describe_manifest(archive, manifest)))
    except ProbeError as e:
        results.append(Result("manifest", False, str(e)))
    if manifest is None:
        results.append(Result("sequences", False, "no valid backup manifest to compare against"))
    else:
        record("sequences", lambda: check_sequences(manifest, load_live_sequences(args.sequences)))
    record("readyz", check_readyz, args.staging_url, fetch_fn)
    record("deploy-config", check_deploy_config, args.config, args.env or DEFAULT_ENVS, args.require)
    return results


def two_key(value):
    if not re.fullmatch(r"TWO_[A-Z0-9_]*[A-Z0-9]", value):
        raise argparse.ArgumentTypeError(f"{value!r} is not a TWO_* key")
    return value


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--backup-dir", default=os.environ.get("TWO_BACKUP_DIR"),
                        help="archive directory (default: $TWO_BACKUP_DIR)")
    parser.add_argument("--sequences", help="saved JSON result of --print-sequences-query")
    parser.add_argument("--staging-url", default=os.environ.get("STAGING_WORKER_URL"),
                        help="staging Worker origin (default: $STAGING_WORKER_URL)")
    parser.add_argument("--config", default=DEFAULT_CONFIG, help="wrangler.toml to read")
    parser.add_argument("--env", action="append", help="named environment to check "
                        f"(repeatable; default: {', '.join(DEFAULT_ENVS)})")
    parser.add_argument("--require", action="append", default=[], type=two_key,
                        help="additional required TWO_* var (repeatable)")
    parser.add_argument("--print-sequences-query", action="store_true",
                        help="print the read-only live sequences query and exit")
    return parser.parse_args(argv)


def main(argv=None, fetch_fn=fetch):
    args = parse_args(argv)
    if args.print_sequences_query:
        print(SEQUENCES_QUERY, end="")
        return 0
    results = run(args, fetch_fn)
    for result in results:
        print(result.line())
    failed = sum(not r.ok for r in results)
    print(f"rollback readiness: {len(results) - failed}/{len(results)} checks passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
