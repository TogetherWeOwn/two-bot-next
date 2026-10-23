#!/usr/bin/env python3
"""Weekly DORA delivery metrics from production Deployments (docs/dora-metrics.md).

Every Monday the dora-metrics workflow runs this over a 30-day window and
commits one JSON file plus a short markdown summary to ops/metrics/. The four
numbers track the pipeline goal's G2-G5 targets:

- G2 change failure rate: failed prod deploys / resolved prod deploys (<5%),
- G3 recovery: failed deploy -> restored service via rollback or the next
  green deploy (auto <5 min, worst <30),
- G4 merge-to-prod lead time p50/p90 (p50 <60 min, p90 <3 h),
- G5 throughput: deploys/day plus commits-per-deploy (<=15).

Sources are the Deployments API (environment=production) plus commit, compare
and Actions-run enrichment. Only rows carrying a canonical deploy ID
(`YYYY.MM.DD.N+sha7`) count: GitHub auto-creates Deployment rows for
`environment:` jobs that carry no ID and whose SHA is not the deployed commit
(the same exclusion digest_resolve.py applies). The per-deploy `deployments`
block in the JSON records every input ID so the numbers spot-check against
the API.

Core reads (the deployment listing, latest statuses) fail closed with fixed
vocabulary, never echoed bodies. Enrichment reads (commit dates, compare
counts, run verdicts) degrade loudly: the deploy keeps a null field and the
metric counts it as unresolved, so a thin week reads as thin, never as zero.
"""

import argparse
from datetime import datetime, timedelta, timezone
import json
import math
import os
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_identity


SCHEMA_VERSION = 1
WINDOW_DAYS = 30
DEPLOYMENTS_PER_PAGE = 100
DEPLOYMENTS_MAX_PAGES = 10
GH_TIMEOUT = 30

TARGET_CFR_LT = 0.05
TARGET_RECOVERY_AUTO_MINUTES_LT = 5
TARGET_RECOVERY_WORST_MINUTES_LT = 30
TARGET_LEAD_P50_MINUTES_LT = 60
TARGET_LEAD_P90_MINUTES_LT = 180
TARGET_COMMITS_PER_DEPLOY_MAX_LE = 15
TARGET_DEPLOYS_NOTE = ">=1/hour when green work exists"

FAILED_STATES = ("failure", "error")
RUN_URL = re.compile(r"/actions/runs/(\d+)(?:/|$)")
WEEK_LABEL = re.compile(r"[0-9]{4}-W[0-9]{2}")


class DoraError(Exception):
    """Fixed diagnostic class only; never constructed from upstream text."""


def parse_moment(raw):
    """An ISO-8601 timestamp as an aware UTC datetime; DoraError unless zoned."""
    if not isinstance(raw, str) or not raw:
        raise DoraError("upstream timestamp missing")
    try:
        moment = datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except ValueError:
        raise DoraError("upstream timestamp unreadable") from None
    if moment.tzinfo is None:
        raise DoraError("upstream timestamp carries no time zone")
    return moment.astimezone(timezone.utc)


def format_moment(moment):
    """Second-precision Zulu spelling for report timestamps."""
    return moment.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def week_label(moment):
    """The ISO `YYYY-Www` label for a week boundary moment."""
    year, week, _day = moment.isocalendar()
    return f"{year}-W{week:02d}"


def gh_api_get(path, **fields):
    """One read-only `gh api` call; the parsed JSON, or a fixed refusal."""
    field_args = []
    for key, value in fields.items():
        field_args += ["-f", f"{key}={value}"]
    try:
        # Keep the executable/method fixed and the endpoint out of option parsing.
        result = subprocess.run(
            ["gh", "api", "--method", "GET", *field_args, "--", path],
            shell=False, capture_output=True, text=True, timeout=GH_TIMEOUT,
        )
    except (OSError, subprocess.SubprocessError):
        raise DoraError("github api unavailable") from None
    if result.returncode != 0:
        raise DoraError("github api refused the read")
    try:
        return json.loads(result.stdout)
    except ValueError:
        raise DoraError("github api returned invalid JSON") from None


def list_production_deployments(repo, start):
    """Deploy-ID production rows at or after `start`, newest first.

    Stops at the first deploy-ID row older than the window (the API returns
    newest first), skipping automatic rows to find the boundary predecessor.
    The caller must verify its success before using it for commits-per-deploy.
    Automatic `environment:` rows (no deploy ID) are counted, never scored.
    """
    rows, automatic, boundary = [], 0, None
    for page in range(1, DEPLOYMENTS_MAX_PAGES + 1):
        batch = gh_api_get(f"repos/{repo}/deployments",
                           environment="production",
                           per_page=DEPLOYMENTS_PER_PAGE, page=page)
        if not isinstance(batch, list):
            raise DoraError("production deployments unreadable")
        for row in batch:
            if not isinstance(row, dict):
                raise DoraError("production deployments unreadable")
            try:
                created = parse_moment(row.get("created_at"))
            except DoraError:
                raise DoraError("production deployments unreadable") from None
            if created < start:
                if build_identity.is_deploy_id_shape(row.get("description")):
                    return rows, automatic, row
                continue
            if build_identity.is_deploy_id_shape(row.get("description")):
                rows.append(row)
            else:
                automatic += 1
        if len(batch) < DEPLOYMENTS_PER_PAGE:
            return rows, automatic, boundary
    raise DoraError("too many production deployments to count this window")


def latest_status(repo, deployment_id):
    """The newest status of one deployment: (state, at, log_url) or None.

    None means the deployment carries no status yet (a `record` that posted
    the row but never its verdict): the deploy lands in the unknown bucket,
    excluded from the failure rate but visible in the inputs.
    """
    statuses = gh_api_get(f"repos/{repo}/deployments/{deployment_id}/statuses", per_page=1)
    if not isinstance(statuses, list):
        raise DoraError("deployment statuses unreadable")
    if not statuses:
        return None
    if not isinstance(statuses[0], dict):
        raise DoraError("deployment statuses unreadable")
    first = statuses[0]
    state = first.get("state")
    if not isinstance(state, str) or not state:
        raise DoraError("deployment statuses unreadable")
    try:
        at = parse_moment(first.get("created_at"))
    except DoraError:
        raise DoraError("deployment statuses unreadable") from None
    log_url = first.get("log_url")
    return state, at, log_url if isinstance(log_url, str) else ""


def parse_run_id(log_url):
    """The Actions run id behind a status log URL, or None when absent."""
    if not log_url:
        return None
    match = RUN_URL.search(log_url)
    return int(match.group(1)) if match else None


def fetch_run(repo, run_id):
    """A run's (conclusion, head_sha), or None when the read fails."""
    try:
        run = gh_api_get(f"repos/{repo}/actions/runs/{run_id}")
    except DoraError:
        return None
    if not isinstance(run, dict):
        return None
    conclusion = run.get("conclusion")
    head_sha = run.get("head_sha")
    if not isinstance(conclusion, str) or not isinstance(head_sha, str):
        return None
    return conclusion, head_sha


def fetch_commit_at(repo, sha):
    """A commit's committer date (when it landed), or None when unreadable."""
    try:
        commit = gh_api_get(f"repos/{repo}/commits/{sha}")
    except DoraError:
        return None
    if not isinstance(commit, dict):
        return None
    inner = commit.get("commit")
    if not isinstance(inner, dict):
        return None
    committer = inner.get("committer")
    if not isinstance(committer, dict):
        return None
    try:
        return parse_moment(committer.get("date"))
    except DoraError:
        return None


def fetch_ahead_by(repo, base, head):
    """Commits `head` is ahead of `base`, or None when the compare fails."""
    try:
        compared = gh_api_get(f"repos/{repo}/compare/{base}...{head}")
    except DoraError:
        return None
    if not isinstance(compared, dict):
        return None
    ahead = compared.get("ahead_by")
    return ahead if type(ahead) is int and ahead >= 0 else None


def fetch_is_rollback(repo, deployment_id):
    """Whether a deployment record restores an older build; None when unreadable."""
    try:
        deployment = gh_api_get(f"repos/{repo}/deployments/{deployment_id}")
    except DoraError:
        return None
    if not isinstance(deployment, dict):
        return None
    payload = deployment.get("payload")
    if payload is None:
        return False
    if isinstance(payload, str):
        try:
            payload = json.loads(payload)
        except ValueError:
            return None
    if not isinstance(payload, dict):
        return None
    return payload.get("rollback") is True


def percentile_nearest_rank(values, percent):
    """The nearest-rank percentile of a non-empty sorted list."""
    rank = int(math.ceil(percent / 100 * len(values))) - 1
    return values[min(max(rank, 0), len(values) - 1)]


def resolve_deployments(repo, rows):
    """One audit record per deployment row, oldest first.

    Returns (records, unresolved_runs) where each record carries the status
    verdict plus every enrichment the API reads could resolve (nulls where
    they could not). Records sort oldest first so recovery and throughput
    scans read forward in time.
    """
    records = []
    for row in sorted(rows, key=lambda r: (r.get("created_at", ""), r.get("id", 0))):
        sha = row.get("sha")
        if not isinstance(sha, str) or not sha:
            raise DoraError("production deployments unreadable")
        status = latest_status(repo, row.get("id"))
        record = {
            "id": row.get("id"),
            "sha": sha,
            "deploy_id": row.get("description"),
            "created_at": format_moment(parse_moment(row.get("created_at"))),
            "state": status[0] if status else "unknown",
            "status_at": format_moment(status[1]) if status else None,
            "log_url": status[2] if status else "",
            "run_id": None,
            "run_conclusion": None,
            "run_head_sha": None,
            "run_verified": False,
            "commit_at": None,
            "lead_minutes": None,
            "commits_since_prev": None,
        }
        run_id = parse_run_id(record["log_url"]) if status else None
        if run_id is not None:
            record["run_id"] = run_id
            fetched = fetch_run(repo, run_id)
            if fetched is not None:
                record["run_conclusion"], record["run_head_sha"] = fetched
                record["run_verified"] = True
        if record["state"] == "success":
            commit_at = fetch_commit_at(repo, sha)
            if commit_at is not None:
                record["commit_at"] = format_moment(commit_at)
                lead = (status[1] - commit_at).total_seconds() / 60
                record["lead_minutes"] = round(max(lead, 0.0), 1)
        records.append(record)
    return records


def score_throughput(repo, records, boundary_sha):
    """Commits-per-deploy across successive successes; returns unresolved count."""
    unresolved, prev_sha = 0, boundary_sha
    for record in records:
        if record["state"] != "success":
            continue
        if prev_sha is None:
            unresolved += 1
        elif prev_sha == record["sha"]:
            record["commits_since_prev"] = 0
        else:
            ahead = fetch_ahead_by(repo, prev_sha, record["sha"])
            if ahead is None:
                unresolved += 1
            else:
                record["commits_since_prev"] = ahead
        prev_sha = record["sha"]
    return unresolved


def score_recovery(repo, records):
    """Recovery incidents for every failed deploy; (incidents, unrecovered, unverified)."""
    incidents, unrecovered, unverified = [], [], 0
    successes = [r for r in records if r["state"] == "success"]
    for failed in (r for r in records if r["state"] in FAILED_STATES):
        restorer = next((s for s in successes
                         if s["status_at"] is not None and failed["status_at"] is not None
                         and s["status_at"] >= failed["status_at"]
                         and s["id"] != failed["id"]), None)
        if restorer is None:
            unrecovered.append({"deployment_id": failed["id"], "sha": failed["sha"],
                                "failed_at": failed["status_at"]})
            continue
        rollback = fetch_is_rollback(repo, restorer["id"])
        if rollback is None:
            unverified += 1
            mechanism = "unverified"
        else:
            mechanism = "rollback" if rollback else "next_green"
        minutes = round((parse_moment(restorer["status_at"])
                         - parse_moment(failed["status_at"])).total_seconds() / 60, 1)
        incidents.append({"failed_deployment_id": failed["id"],
                          "failed_at": failed["status_at"],
                          "restored_by_deployment_id": restorer["id"],
                          "restored_at": restorer["status_at"],
                          "recovery_minutes": max(minutes, 0.0),
                          "mechanism": mechanism})
    return incidents, unrecovered, unverified


def build_report(repo, records, automatic, window, recovery, throughput_unresolved,
                 generated_at, generator):
    """The schema-versioned report dict from resolved per-deploy records.

    `recovery` is the (incidents, unrecovered, unverified) triple from
    score_recovery; `throughput_unresolved` counts deploys whose compare
    count the API could not resolve.
    """
    incidents, unrecovered, unverified = recovery
    resolved = [r for r in records if r["state"] in ("success", *FAILED_STATES)]
    failed = [r for r in resolved if r["state"] in FAILED_STATES]
    successes = [r for r in resolved if r["state"] == "success"]
    in_flight = [r for r in records if r["state"] not in ("success", *FAILED_STATES, "unknown")]
    unknown = [r for r in records if r["state"] == "unknown"]

    rate = round(len(failed) / len(resolved), 4) if resolved else None
    g2 = {"failed": len(failed), "total": len(resolved), "rate": rate,
          "target_lt": TARGET_CFR_LT,
          "pass": (rate < TARGET_CFR_LT) if rate is not None else None,
          "unknown_state": len(unknown), "in_flight": len(in_flight),
          "excluded_automatic_rows": automatic}

    minutes = sorted(i["recovery_minutes"] for i in incidents)
    worst = minutes[-1] if minutes else None
    mean = round(sum(minutes) / len(minutes), 1) if minutes else None
    rollbacks = [i for i in incidents if i["mechanism"] == "rollback"]
    g3 = {"incidents": incidents, "recovered": len(incidents),
          "worst_minutes": worst, "mean_minutes": mean,
          "target_auto_lt": TARGET_RECOVERY_AUTO_MINUTES_LT,
          "target_worst_lt": TARGET_RECOVERY_WORST_MINUTES_LT,
          "worst_pass": (worst < TARGET_RECOVERY_WORST_MINUTES_LT) if worst is not None else None,
          "rollback_recoveries": len(rollbacks),
          "rollback_all_under_5": (all(i["recovery_minutes"]
                                       < TARGET_RECOVERY_AUTO_MINUTES_LT for i in rollbacks)
                                   if rollbacks else None),
          "unverified_mechanism": unverified, "unrecovered": unrecovered}

    leads = sorted(r["lead_minutes"] for r in successes if r["lead_minutes"] is not None)
    p50 = percentile_nearest_rank(leads, 50) if leads else None
    p90 = percentile_nearest_rank(leads, 90) if leads else None
    g4 = {"p50_minutes": p50, "p90_minutes": p90, "method": "nearest-rank", "n": len(leads),
          "unresolved_commits": len(successes) - len(leads),
          "target_p50_lt": TARGET_LEAD_P50_MINUTES_LT,
          "target_p90_lt": TARGET_LEAD_P90_MINUTES_LT,
          "p50_pass": (p50 < TARGET_LEAD_P50_MINUTES_LT) if p50 is not None else None,
          "p90_pass": (p90 < TARGET_LEAD_P90_MINUTES_LT) if p90 is not None else None}

    per_day = {}
    for record in successes:
        day = record["status_at"][:10]
        per_day[day] = per_day.get(day, 0) + 1
    commits = [r["commits_since_prev"] for r in successes
               if r["commits_since_prev"] is not None]
    mean_per_day = round(len(successes) / window["days"], 2) if window["days"] else 0.0
    g5 = {"successful_deploys": len(successes), "per_day": per_day,
          "mean_per_day": mean_per_day, "target_note": TARGET_DEPLOYS_NOTE,
          "commits_per_deploy": {
              "mean": round(sum(commits) / len(commits), 1) if commits else None,
              "max": max(commits) if commits else None,
              "target_max_le": TARGET_COMMITS_PER_DEPLOY_MAX_LE,
              "pass": ((max(commits) <= TARGET_COMMITS_PER_DEPLOY_MAX_LE)
                       if commits else None),
              "unresolved": throughput_unresolved}}

    return {"schema_version": SCHEMA_VERSION, "week": window["label"],
            "window_days": window["days"],
            "window": {"start": window["start"], "end": window["end"]},
            "generated_at": generated_at,
            "generator": generator, "repository": repo,
            "g2_change_failure_rate": g2, "g3_recovery_minutes": g3,
            "g4_lead_time_minutes": g4, "g5_throughput": g5,
            "deployments": records}


def verdict_line(name, value, target, passed):
    """One markdown table row; nulls read as `n/a`, never as passing."""
    if value is None:
        return f"| {name} | n/a | {target} | n/a |"
    if passed is None:
        return f"| {name} | {value} | {target} | n/a |"
    mark = "pass" if passed else "FAIL"
    return f"| {name} | {value} | {target} | {mark} |"


def render_markdown(report):
    """The short human summary committed beside the JSON."""
    g2 = report["g2_change_failure_rate"]
    g3 = report["g3_recovery_minutes"]
    g4 = report["g4_lead_time_minutes"]
    g5 = report["g5_throughput"]
    cpd = g5["commits_per_deploy"]
    lines = [
        f"# DORA metrics {report['week']}",
        "",
        f"Window: {report['window']['start']} .. {report['window']['end']} "
        f"({report['window_days']} days, production Deployments).",
        "",
        "| Metric | Value | Target | Verdict |",
        "| --- | --- | --- | --- |",
        verdict_line("G2 change failure rate",
                     f"{g2['failed']}/{g2['total']} = {g2['rate']:.1%}"
                     if g2["rate"] is not None else None,
                     f"<{g2['target_lt']:.0%}", g2["pass"]),
        verdict_line("G3 worst recovery",
                     f"{g3['worst_minutes']} min over {g3['recovered']} recovered"
                     if g3["worst_minutes"] is not None else None,
                     f"<{g3['target_worst_lt']} min", g3["worst_pass"]),
        verdict_line("G4 lead p50 / p90",
                     f"{g4['p50_minutes']} / {g4['p90_minutes']} min (n={g4['n']})"
                     if g4["p50_minutes"] is not None else None,
                     f"<{g4['target_p50_lt']} / <{g4['target_p90_lt']} min",
                     (g4["p50_pass"] and g4["p90_pass"])
                     if g4["p50_pass"] is not None and g4["p90_pass"] is not None else None),
        verdict_line("G5 deploys/day", str(g5["mean_per_day"]),
                     g5["target_note"], None),
        verdict_line("G5 commits/deploy (max)",
                     str(cpd["max"]) if cpd["max"] is not None else None,
                     f"<={cpd['target_max_le']}", cpd["pass"]),
        "",
    ]
    notes = []
    if g3["unrecovered"]:
        ids = ", ".join(str(u["deployment_id"]) for u in g3["unrecovered"])
        notes.append(f"Unrecovered failures need a human look: deployment {ids}.")
    if g2["unknown_state"]:
        notes.append(f"{g2['unknown_state']} deployment(s) carry no status yet "
                     "(excluded from the failure rate).")
    if g2["in_flight"]:
        notes.append(f"{g2['in_flight']} deployment(s) still in flight "
                     "(excluded from the failure rate).")
    if g4["unresolved_commits"]:
        notes.append(f"{g4['unresolved_commits']} lead time(s) lack a commit date.")
    if cpd["unresolved"]:
        notes.append(f"{cpd['unresolved']} deploy(s) lack a compare count.")
    if notes:
        lines += ["Notes:"] + [f"- {note}" for note in notes] + [""]
    lines += [f"Raw inputs (deployment IDs, run IDs): `dora-{report['week']}.json`.",
              f"Generated by {report['generator']['workflow']} "
              f"run {report['generator']['run_id']} at {report['generated_at']}."]
    return "\n".join(lines) + "\n"


def repo_from_env(explicit):
    """The owner/name repo from the flag or the runner environment."""
    repo = explicit or os.environ.get("GITHUB_REPOSITORY", "") or os.environ.get("GH_REPO", "")
    if not repo or "/" not in repo:
        raise DoraError("needs GITHUB_REPOSITORY owner/name")
    return repo


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--window-days", type=int, default=WINDOW_DAYS)
    parser.add_argument("--out-dir", default="ops/metrics")
    parser.add_argument("--repo", default=None)
    parser.add_argument("--now", default=None,
                        help="UTC ISO-8601 window end override (default: now; tests only)")
    parser.add_argument("--week", default=None,
                        help="week label override YYYY-Www (default: ISO week of --now)")
    args = parser.parse_args(argv)

    try:
        if args.window_days < 1 or args.window_days > 366:
            raise DoraError("window must be 1 to 366 days")
        repo = repo_from_env(args.repo)
        end = parse_moment(args.now) if args.now else datetime.now(timezone.utc)
        start = end - timedelta(days=args.window_days)
        label = args.week or week_label(end)
        if WEEK_LABEL.fullmatch(label) is None:
            raise DoraError("week label must be YYYY-Www")

        rows, automatic, boundary = list_production_deployments(repo, start)
        records = resolve_deployments(repo, rows)
        boundary_sha = None
        if isinstance(boundary, dict):
            boundary_status = latest_status(repo, boundary.get("id"))
            sha = boundary.get("sha")
            if boundary_status and boundary_status[0] == "success" and isinstance(sha, str) and sha:
                boundary_sha = sha
        throughput_unresolved = score_throughput(repo, records, boundary_sha)
        incidents, unrecovered, unverified = score_recovery(repo, records)

        window = {"label": label, "days": args.window_days,
                  "start": format_moment(start), "end": format_moment(end)}
        generator = {"workflow": "dora-metrics",
                     "run_id": os.environ.get("GITHUB_RUN_ID", ""),
                     "sha": os.environ.get("GITHUB_SHA", "")}
        generated_at = format_moment(datetime.now(timezone.utc))
        report = build_report(repo, records, automatic, window,
                              (incidents, unrecovered, unverified),
                              throughput_unresolved, generated_at, generator)

        out_dir = Path(args.out_dir)
        out_dir.mkdir(parents=True, exist_ok=True)
        json_path = out_dir / f"dora-{label}.json"
        md_path = out_dir / f"dora-{label}.md"
        json_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        md_path.write_text(render_markdown(report), encoding="utf-8")

        output = os.environ.get("GITHUB_OUTPUT", "")
        if output:
            with open(output, "a", encoding="utf-8") as handle:
                handle.write(f"week={label}\n")
        g2, g3, g4, g5 = (report["g2_change_failure_rate"], report["g3_recovery_minutes"],
                          report["g4_lead_time_minutes"], report["g5_throughput"])
        print(f"week {label}: CFR {g2['failed']}/{g2['total']}, "
              f"worst recovery {g3['worst_minutes']} min, "
              f"lead p50/p90 {g4['p50_minutes']}/{g4['p90_minutes']} min, "
              f"{g5['mean_per_day']} deploys/day -> {json_path}")
        return 0
    except DoraError as error:
        print(f"dora_metrics failed: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
