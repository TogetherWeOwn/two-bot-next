#!/usr/bin/env python3
"""Promotion regression gate: the candidate on staging against production now.

Two steps in deploy-production, both before any production mutation:

  staging   (`staging-soak-evidence` job, after the soak) reads the staging
            user-flow probe receipt and staging's cumulative REST counters
            (the staging process starts at the candidate's deploy, so they
            cover its soak). Staging serving another build means the
            candidate's soak cannot be judged: an automatic run skips green
            (`staging-superseded`, the newer build promotes itself), a manual
            run refuses. Writes the compact staging evidence the next step reads.
  compare   (`production` job) reads the passive production probe receipt
            and production's cumulative REST counters as the baseline, then
            refuses when
              - a journey (probe check) that passes on production fails on
                staging for the candidate, or
              - staging's REST 5xx ratio exceeds max(1.5 x production's
                ratio, a 5% floor), with at least 3 staging 5xx responses.
            Fewer than 10 judged staging REST requests (or no staging bearer)
            records `insufficient_data`, never `pass`, mirroring the
            promotion error-rate gate; a production baseline below 10
            requests (or without a bearer) falls back to the floor alone.

Receipts carry check names, verdicts, integers, ratios and fixed codes only:
never probe bodies, URLs or credentials. Exit 0 passes (or skips), 1 refuses.
"""

import argparse
import json
import os
from pathlib import Path
import re
import sys

RATIO_MARGIN = 1.5
FLOOR_RATIO = 0.05
MIN_REQUESTS = 10  # REST_5XX_MIN_REQUESTS in wrangler/src/alert-rules.ts
MIN_5XX = 3
MAX_BYTES = 65536
SHA = re.compile(r"[0-9a-f]{40}")
CHECK_NAME = re.compile(r"[a-z_]{1,32}")


class GateError(ValueError):
    pass


def _reject_duplicate_keys(pairs):
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise GateError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


def parse(text):
    try:
        value = json.loads(text, object_pairs_hook=_reject_duplicate_keys)
    except (ValueError, RecursionError):
        raise GateError("unreadable") from None
    if not isinstance(value, dict):
        raise GateError("unreadable")
    return value


def read(path):
    """A receipt mapping, or None when the producing step wrote nothing."""
    try:
        raw = Path(path).read_bytes()
    except OSError:
        return None
    if len(raw) > MAX_BYTES:
        raise GateError("oversize")
    return parse(raw.decode("utf-8", "replace"))


def checks(receipt):
    """{check name: "pass"|"fail"} from a probe receipt; {} when absent."""
    found = {}
    for row in (receipt or {}).get("checks") or []:
        if isinstance(row, dict) and isinstance(row.get("check"), str) \
                and CHECK_NAME.fullmatch(row["check"]) and row.get("verdict") in ("pass", "fail"):
            found[row["check"]] = row["verdict"]
    return found


def counters(receipt):
    """(rest5xx, restTotal) when the scrape succeeded, else None."""
    receipt = receipt or {}
    total, errors = receipt.get("restTotal"), receipt.get("rest5xx")
    if receipt.get("available") is not True or type(total) is not int or type(errors) is not int \
            or not 0 <= errors <= total:
        return None
    return errors, total


def staging_step(probe, rest, sha, auto):
    """(skip code, staging evidence) for the candidate on staging."""
    if probe is None:
        # Only an answered probe can show staging moved on; no receipt never skips.
        raise GateError("no staging probe receipt")
    revision = probe.get("build_revision")
    if not isinstance(revision, str) or not SHA.fullmatch(revision):
        revision = None
    code = (rest or {}).get("code")
    evidence = {"sha": sha, "staging_build_revision": revision, "checks": checks(probe), "rest": None,
                "rest_code": code if isinstance(code, str) and CHECK_NAME.fullmatch(code) else None}
    pair = counters(rest)
    if pair is not None:
        evidence["rest"] = {"rest5xx": pair[0], "restTotal": pair[1]}
    if revision != sha:
        if auto:
            return "staging-superseded", evidence
        raise GateError("staging no longer serves the candidate; its soak cannot be judged")
    return "", evidence


def ratio(pair):
    return pair[0] / pair[1] if pair and pair[1] else 0.0


def compare(staging, prod_probe, prod_rest):
    """The regression verdict: candidate on staging against the production baseline."""
    staged = staging.get("checks") if isinstance(staging.get("checks"), dict) else {}
    baseline = checks(prod_probe)
    regressions = sorted(name for name, verdict in baseline.items()
                         if verdict == "pass" and staged.get(name) != "pass")
    rest = staging.get("rest") if isinstance(staging.get("rest"), dict) else None
    staging_pair = counters({"available": True, **rest}) if rest else None
    prod_pair = counters(prod_rest)
    prod_ratio = ratio(prod_pair) if prod_pair and prod_pair[1] >= MIN_REQUESTS else None
    threshold = max(RATIO_MARGIN * prod_ratio, FLOOR_RATIO) if prod_ratio is not None else FLOOR_RATIO
    if staging_pair is None or staging_pair[1] < MIN_REQUESTS:
        rate_verdict = "insufficient_data"
    elif staging_pair[0] >= MIN_5XX and ratio(staging_pair) > threshold:
        rate_verdict = "fail"
    else:
        rate_verdict = "pass"
    failed = bool(regressions) or rate_verdict == "fail"
    return {
        "sha": staging.get("sha"),
        "journeys": {"production": baseline, "staging": staged, "regressions": regressions,
                     "baseline": "available" if baseline else "unavailable"},
        "rest_5xx": {
            "staging": {"rest5xx": staging_pair[0], "restTotal": staging_pair[1],
                        "ratio": round(ratio(staging_pair), 6)} if staging_pair else None,
            "production": {"rest5xx": prod_pair[0], "restTotal": prod_pair[1],
                           "ratio": round(ratio(prod_pair), 6)} if prod_pair else None,
            "threshold": round(threshold, 6),
            "baseline": "available" if prod_ratio is not None else "floor_only",
            "verdict": rate_verdict,
        },
        "verdict": "fail" if failed else "pass",
        "code": ("journey_regression" if regressions
                 else "rest_5xx_regression" if rate_verdict == "fail" else "no_regression"),
    }


def _pair(side):
    return f"{side['rest5xx']}/{side['restTotal']}" if side else "n/a"


def summary_line(evidence):
    rate, journeys = evidence["rest_5xx"], evidence["journeys"]
    return (f"- Regression vs production: `{evidence['code']}`; journeys regressed: "
            f"{', '.join(journeys['regressions']) or 'none'} "
            f"(production baseline {journeys['baseline']}); REST 5xx staging {_pair(rate['staging'])}, "
            f"production {_pair(rate['production'])}, threshold {rate['threshold']:.2%} "
            f"({rate['baseline']}), rate verdict `{rate['verdict']}`\n")


def write_github(name, text):
    path = os.environ.get(name)
    if path:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(text)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    commands = parser.add_subparsers(dest="command", required=True)
    stage = commands.add_parser("staging", help="judge staging after the soak")
    stage.add_argument("--probe", required=True)
    stage.add_argument("--counters", required=True)
    stage.add_argument("--sha", required=True)
    stage.add_argument("--auto", choices=("true", "false"), required=True)
    stage.add_argument("--out", required=True)
    comp = commands.add_parser("compare", help="judge staging against the production baseline")
    comp.add_argument("--staging-evidence", required=True)
    comp.add_argument("--probe", required=True)
    comp.add_argument("--counters", required=True)
    comp.add_argument("--sha", required=True)
    comp.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    if not SHA.fullmatch(args.sha or ""):
        print("::error::Refusing promotion: the regression gate needs the guarded 40-hex SHA")
        return 1
    try:
        if args.command == "staging":
            skip, evidence = staging_step(read(args.probe), read(args.counters), args.sha,
                                          args.auto == "true")
            Path(args.out).write_text(json.dumps(evidence, sort_keys=True) + "\n")
            write_github("GITHUB_OUTPUT", f"skip={skip}\nevidence={json.dumps(evidence, sort_keys=True)}\n")
            print("staging soak evidence: " + json.dumps(evidence, sort_keys=True))
            if skip:
                write_github("GITHUB_STEP_SUMMARY",
                             f"- Skip: `{skip}`: staging now serves another build, which promotes itself; "
                             "nothing is deployed\n")
            return 0
        staging = parse(args.staging_evidence)
        if staging.get("sha") != args.sha:
            raise GateError("the staging evidence names another SHA")
        evidence = compare(staging, read(args.probe), read(args.counters))
    except GateError as error:
        print(f"::error::Refusing promotion: regression gate cannot judge ({error})")
        return 1
    Path(args.out).write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    print("promotion regression gate: " + json.dumps(evidence, sort_keys=True))
    write_github("GITHUB_STEP_SUMMARY", summary_line(evidence))
    if evidence["verdict"] != "pass":
        print(f"::error::Refusing promotion: the candidate regresses against production ({evidence['code']})")
        return 1
    if evidence["rest_5xx"]["verdict"] == "insufficient_data":
        print("::warning::regression gate: too few staging REST requests to judge the 5xx ratio; not a pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
