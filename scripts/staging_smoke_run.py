#!/usr/bin/env python3
"""Read-mostly staging smoke run that writes one staging E2E run record (stdlib only).

One run, one record (docs/staging-e2e-run-record.md). The run composes the
existing read-only probes into a single fenced pass over the deployed staging
build and the TWO Staging guild:

  health      GET /health   exact liveness shape
  readyz      GET /readyz   200 only when every component is ready
  build       readyz build identity matches the tested revision
  identity    GET /users/@me   the token belongs to the staging application
  registry    guild command list + per-command resource read for the core
              surfaces (/rank, /leaderboard, /help), then the rest of the
              compiled publish set

It is read-mostly by construction: only GET requests, no request bodies, and
nothing is created or changed in the guild. It does NOT invoke a slash
command. Discord creates interactions only for real users and the Worker has
no HTTP interaction ingress, so a bot token cannot prove a reply. The offline
reply contract lives in crates/discord/src/staging_slash_smoke.rs; real
invocation stays a human-tester step (docs/staging-slash-smoke.md).

Fences, all before the first request:
  * the live guild id, any other guild id and a missing guild id refuse
    (exit 2); there is no override;
  * only an https://two-bot-next-staging.<sub>.workers.dev origin is fetched;
  * the staging bot token comes only from $DISCORD_STAGING_BOT_TOKEN and is
    never printed, logged or written to the record. A 401 refuses with a fixed
    message; no substitute credential is ever tried.

Exit status: 0 PASS, 1 NEEDS WORK, 2 refused by a fence (nothing was sent).

Usage:
  STAGING_WORKER_URL=https://two-bot-next-staging.<sub>.workers.dev \\
  DISCORD_STAGING_BOT_TOKEN=... DISCORD_STAGING_GUILD_ID=... \\
      python3 scripts/staging_smoke_run.py --tester "QA" [--expected-sha SHA] \\
          [--deploy-run-id ID] [--record FILE]
"""

import argparse
from dataclasses import dataclass, field
import json
import os
from pathlib import Path
import re
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_run_record  # noqa: E402
import staging_automation_read_smoke as discord_read  # noqa: E402
import staging_health_contract_probe as health_probe  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
PUBLISH_SET = ROOT / "crates/core/tests/fixtures/staging_published_commands.json"
# The "always" rows of docs/staging-e2e-command-matrix.md: published with no
# staging env gate, so a missing one is a failure, not an unset flag. The unit
# tests cross-check this tuple against the matrix doc.
CORE_SURFACES = ("rank", "leaderboard", "help")
SHA_RE = re.compile(r"[0-9a-f]{40}")
BUILD_ID_RE = re.compile(r"([0-9]+)-[0-9]+")
RUN_ID_RE = re.compile(r"[0-9]+")

# Opaque failure-signature IDs (docs/staging-slash-smoke.md "Failure signatures").
SIGNATURE_HEALTH = "SMOKE-HEALTH-FAIL"
SIGNATURE_READYZ = "SMOKE-READYZ-NOT-READY"
SIGNATURE_CHECKPOINT_READ = "SMOKE-READYZ-CHECKPOINT-READ-FAILED"
SIGNATURE_BUILD = "SMOKE-BUILD-MISMATCH"
SIGNATURE_DISCORD = "SMOKE-DISCORD-REFUSED"
SIGNATURE_MISSING = "SMOKE-REGISTRY-MISSING"
SIGNATURE_DETAIL = "SMOKE-REGISTRY-DETAIL-MISMATCH"


class Refused(Exception):
    """A fence refused the run before any request was sent."""


@dataclass
class Row:
    name: str
    result: str  # pass | fail | skipped
    expected: str
    actual: str
    started: str
    duration_ms: int
    signature: str | None = None

    def line(self):
        return f"{self.result.upper():7} {self.name}: {self.actual}"

    def record(self):
        row = {"name": self.name, "started_utc": self.started,
               "duration_ms": self.duration_ms, "result": self.result,
               "expected": self.expected, "actual": self.actual}
        if self.result == "fail":
            row["failure_signature"] = self.signature or "SMOKE-UNCLASSIFIED"
        return row


@dataclass
class Run:
    rows: list = field(default_factory=list)
    durations: dict = field(default_factory=dict)
    worker_version: str | None = None
    build_revision: str | None = None
    build_id: str | None = None
    extra_live: int = 0


def utc(timestamp=None):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(timestamp))


def fences(args, token):
    """Every refusal, evaluated before the first request. Returns the origin."""
    # The guild fence runs first: the live guild must refuse even when other
    # configuration is also missing, and without any request.
    try:
        discord_read.guild_fence(args.guild_id)
        discord_read.require_token(token)
    except discord_read.SmokeError as error:
        raise Refused(str(error))
    try:
        origin = health_probe.staging_origin(args.staging_url)
    except health_probe.ProbeError as error:
        raise Refused(str(error))
    if args.expected_sha and not SHA_RE.fullmatch(args.expected_sha):
        raise Refused("refusing: --expected-sha must be 40 lowercase hex characters")
    if args.deploy_run_id and not RUN_ID_RE.fullmatch(args.deploy_run_id):
        raise Refused("refusing: --deploy-run-id must be digits")
    return origin


def timed_fetch(inner, run):
    """Wrap a health-probe fetch so each GET's wall time is kept by path."""

    def fetch(url):
        started = time.monotonic()
        try:
            status, headers, body = inner(url)
        finally:
            run.durations[url.rsplit("/", 1)[-1]] = int((time.monotonic() - started) * 1000)
        if url.endswith("/readyz"):
            version = health_probe.header(headers, "x-two-worker-version")
            run.worker_version = version if isinstance(version, str) and version else None
        return status, headers, body

    return fetch


def health_rows(args, origin, run, fetch_fn):
    """health / readyz / build rows from the existing contract probe."""
    probe_args = argparse.Namespace(staging_url=origin, expected_sha=args.expected_sha)
    started = utc()
    _, results, report = health_probe.run(probe_args, timed_fetch(fetch_fn, run))
    if isinstance(report, dict):
        run.build_revision = report.get("build_revision")
        run.build_id = report.get("build_id")
    meta = {
        "health": ("GET /health", "200 {\"status\":\"ok\"}", "health", SIGNATURE_HEALTH),
        "readyz": ("GET /readyz", "200, every component ready", "readyz", SIGNATURE_READYZ),
        "build": ("readyz build identity", "build_revision is the tested revision",
                  "readyz", SIGNATURE_BUILD),
    }
    for result in results:
        name, expected, timing_key, signature = meta[result.name]
        if result.checkpoint_read_failed:
            signature = SIGNATURE_CHECKPOINT_READ
        run.rows.append(Row(name, "pass" if result.ok else "fail", expected, result.reason,
                            started, run.durations.get(timing_key, 0),
                            None if result.ok else signature))


def read_publish_set():
    document = json.loads(PUBLISH_SET.read_text(encoding="utf-8"))
    builtins = document["builtins_in_publish_order"]
    if not isinstance(builtins, list) or not all(isinstance(n, str) for n in builtins):
        raise ValueError("publish set fixture lost builtins_in_publish_order")
    return builtins


def discord_rows(guild, run, fetch_fn):
    """identity, then the core surfaces' list+resource reads, then the rest."""
    builtins = read_publish_set()
    clock = {"started": utc(), "t0": time.monotonic()}

    def mark():
        clock["started"], clock["t0"] = utc(), time.monotonic()

    def elapsed():
        return int((time.monotonic() - clock["t0"]) * 1000)

    mark()
    try:
        app_id = discord_read.check_identity(fetch_fn)
        entries = discord_read.get_json(
            fetch_fn, f"{discord_read.API}/applications/{app_id}/guilds/{guild}/commands")
        if not isinstance(entries, list):
            raise discord_read.SmokeError("command list answered 200 without a list")
    except discord_read.SmokeError as error:
        run.rows.append(Row("identity and command list", "fail",
                            "staging application reads its guild command list",
                            str(error), clock["started"], elapsed(), SIGNATURE_DISCORD))
        return
    run.rows.append(Row("identity and command list", "pass",
                        "staging application reads its guild command list",
                        f"{len(entries)} guild commands visible", clock["started"], elapsed()))

    live_names = {c.get("name") for c in entries if isinstance(c, dict)}
    run.extra_live = len(live_names - set(builtins))
    for name in CORE_SURFACES:
        mark()
        try:
            result = discord_read.check_command(fetch_fn, app_id, guild, entries, name)
        except discord_read.SmokeError as error:
            run.rows.append(Row(f"/{name} (registry read)", "fail",
                                "registered, resource scoped to the staging guild",
                                str(error), clock["started"], elapsed(), SIGNATURE_DISCORD))
            continue
        signature = SIGNATURE_MISSING if "not registered" in result.reason else SIGNATURE_DETAIL
        run.rows.append(Row(f"/{name} (registry read)", "pass" if result.ok else "fail",
                            "registered, resource scoped to the staging guild",
                            result.reason, clock["started"], elapsed(),
                            None if result.ok else signature))
    for name in builtins:
        if name in CORE_SURFACES:
            continue
        mark()
        present = name in live_names
        run.rows.append(Row(f"/{name} (registry read)", "pass" if present else "skipped",
                            "published when its staging env gate is on",
                            "registered in the staging guild" if present else
                            "not published in the staging guild (gate off or publish pending)",
                            clock["started"], elapsed()))


def build_record(args, run, started, ended):
    """Assemble the run record, or None when no tested revision is knowable."""
    revision = args.expected_sha or (
        run.build_revision if SHA_RE.fullmatch(run.build_revision or "") else None)
    if revision is None:
        return None
    deploy_run = args.deploy_run_id
    if not deploy_run and revision == run.build_revision:
        # The build id names the run that deployed the serving build, so it
        # only answers for the recorded revision when that is the serving one.
        match = BUILD_ID_RE.fullmatch(run.build_id or "")
        deploy_run = match.group(1) if match else None
    if deploy_run is None:
        return None
    failed = [r for r in run.rows if r.result == "fail"]
    skipped = [r for r in run.rows if r.result == "skipped"]
    passed = [r for r in run.rows if r.result == "pass"]
    scope = ("Read-mostly staging smoke: health contract, build identity and guild "
             "command registry. No slash command was invoked.")
    if failed:
        summary = (f"{len(failed)} of {len(run.rows)} checks failed "
                   f"({', '.join(r.name for r in failed)}); fix and re-run. {scope}")
    else:
        summary = (f"{len(passed)} checks passed, {len(skipped)} unpublished gated surface(s) "
                   f"skipped. {scope}")
    follow_ups = [f"{r.name}: {r.actual}" for r in failed + skipped]
    deployment = {"revision": revision, "deploy_staging_run_id": deploy_run}
    if run.worker_version:
        deployment["worker_version"] = run.worker_version
    return {
        "schema_version": 1,
        "run": {"id": f"staging-smoke-{started.replace(':', '').replace('-', '')}",
                "plan_ref": scope},
        "environment": {"target": "staging", "guild_id": args.guild_id, "worker_env": "staging"},
        "deployment": deployment,
        "tester": {"identity": args.tester},
        "window": {"start_utc": started, "end_utc": ended},
        "commands": [row.record() for row in run.rows],
        "cleanup": {"result": "not_applicable",
                    "notes": "Read-only: GET requests only; nothing was created or changed."},
        "verdict": {"disposition": "NEEDS WORK" if failed else "PASS",
                    "summary": summary, "follow_up_refs": follow_ups},
    }


def validate_record(record):
    schema = json.loads(check_run_record.DEFAULT_SCHEMA.read_text(encoding="utf-8"))
    return check_run_record.validate(record, schema)


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--staging-url", default=os.environ.get("STAGING_WORKER_URL"),
                        help="staging Worker origin (default: $STAGING_WORKER_URL)")
    parser.add_argument("--guild-id", default=os.environ.get("DISCORD_STAGING_GUILD_ID"),
                        help="staging guild id (default: $DISCORD_STAGING_GUILD_ID)")
    parser.add_argument("--expected-sha", default=os.environ.get("EXPECTED_SHA"),
                        help="deployed commit SHA under test; the build row fails on mismatch")
    parser.add_argument("--deploy-run-id", default=os.environ.get("DEPLOY_STAGING_RUN_ID"),
                        help="deploy-staging run id (default: the readyz build_id prefix when the "
                             "recorded revision is the serving build)")
    parser.add_argument("--tester", default="staging_smoke_run.py (read-only)",
                        help="role or handle recorded as the tester; never a credential")
    parser.add_argument("--record", default="staging-smoke-run-record.json",
                        help="run record path written when a tested revision is known")
    return parser.parse_args(argv)


def main(argv=None, health_fetch=None, discord_fetch=None):
    args = parse_args(argv)
    token = os.environ.get("DISCORD_STAGING_BOT_TOKEN", "")
    try:
        origin = fences(args, token)
    except Refused as error:
        print(f"staging smoke run refused: {error}")
        return 2
    run = Run()
    started = utc()
    health_rows(args, origin, run, health_fetch or health_probe.fetch)
    discord_rows(args.guild_id, run, discord_fetch or discord_read.make_fetch(token))
    ended = utc()
    for row in run.rows:
        print(row.line())
    failed = sum(row.result == "fail" for row in run.rows)
    print(f"staging smoke run: {len(run.rows) - failed}/{len(run.rows)} checks without failure; "
          "no slash command invoked")
    record = build_record(args, run, started, ended)
    if record is None:
        print("staging smoke run: no run record written (no tested revision or deploy run id "
              "known: pass --expected-sha and --deploy-run-id; the serving build's run id "
              "only counts for the serving revision)")
        return 1
    errors = validate_record(record)
    if errors:
        for error in errors:
            print(f"staging smoke run: record invalid: {error}")
        return 1
    try:
        Path(args.record).write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    except OSError as error:
        print(f"staging smoke run: record not written ({error.__class__.__name__})")
        return 1
    print(f"staging smoke run: {record['verdict']['disposition']}; record {args.record}")
    return 0 if record["verdict"]["disposition"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
