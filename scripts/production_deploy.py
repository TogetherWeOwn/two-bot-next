"""Production deploy build identity and the /readyz revision check.

`render` writes the Wrangler config for `wrangler deploy --config`: the checked-in
TOML with absolute paths and the two non-secret build arguments (`image_vars`)
that /readyz reports as `build_revision` and `build_id`. `readyz` decides whether
one /readyz answer proves the revision the workflow deployed or rolled back to.
"""

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

SHA = re.compile(r"[0-9a-f]{40}")
DIGITS = re.compile(r"[0-9]+")
UNSTAMPED = "unknown"


def render(args):
    if not (SHA.fullmatch(args.sha) and DIGITS.fullmatch(args.run_id) and DIGITS.fullmatch(args.attempt)):
        sys.exit("render: needs a 40-hex SHA and numeric run id and attempt")
    source = Path(args.config).resolve()
    config = tomllib.loads(source.read_text())
    if config.get("name") != "two-bot-next":
        sys.exit("render: the Wrangler config is not the two-bot-next Worker")
    containers = config.get("env", {}).get("production", {}).get("containers", [])
    if len(containers) != 1 or containers[0].get("class_name") != "TwoBotContainer" or containers[0].get("max_instances") != 1:
        sys.exit("render: production needs exactly one TwoBotContainer with max_instances = 1")
    config["main"] = str((source.parent / config["main"]).resolve())
    declared = [
        *config.get("containers", []),
        *(container for env in config.get("env", {}).values() for container in env.get("containers", [])),
    ]
    for container in declared:
        container["image"] = str((source.parent / container["image"]).resolve())
        container["image_build_context"] = str(source.parent.parent)
    containers[0]["image_vars"] = {"BOT_BUILD_REVISION": args.sha, "BOT_BUILD_ID": f"{args.run_id}-{args.attempt}"}
    Path(args.out).write_text(json.dumps(config, indent=2) + "\n")
    return 0


def reject(reason):
    print(reason)
    return 1


def readyz(args):
    if args.status not in ("200", "503"):
        return reject(f"/readyz answered {args.status}")
    try:
        report = json.loads(Path(args.body).read_text())
    except (OSError, ValueError):
        return reject("/readyz did not return JSON")
    if not isinstance(report, dict):
        return reject("/readyz JSON is not an object")
    revision, build_id = report.get("build_revision"), report.get("build_id")
    if not isinstance(revision, str) or not isinstance(build_id, str):
        return reject("/readyz has no build_revision and build_id")
    if revision == args.sha:
        state = "gateway ready" if args.status == "200" else "gateway parked; deploy is healthy"
        print(f"{state}, revision matches the SHA, build {build_id}")
        return 0
    if args.mode == "rollback" and revision == build_id == UNSTAMPED:
        print("pre-stamp version: build identity unknown, revision not verifiable (recorded; rollback not failed)")
        return 0
    if revision == UNSTAMPED:
        return reject("build_revision is unknown: this build was not stamped")
    return reject("build_revision does not match the SHA")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    commands = parser.add_subparsers(dest="command", required=True)

    render_cmd = commands.add_parser("render", help="write the Wrangler config with build identity")
    render_cmd.add_argument("--config", required=True)
    render_cmd.add_argument("--out", required=True)
    render_cmd.add_argument("--sha", required=True)
    render_cmd.add_argument("--run-id", required=True)
    render_cmd.add_argument("--attempt", required=True)

    readyz_cmd = commands.add_parser("readyz", help="judge one /readyz answer against the revision")
    readyz_cmd.add_argument("--status", required=True)
    readyz_cmd.add_argument("--body", required=True)
    readyz_cmd.add_argument("--sha", required=True)
    readyz_cmd.add_argument("--mode", choices=("deploy", "rollback"), required=True)

    args = parser.parse_args(argv)
    if args.command == "render":
        return render(args)
    return readyz(args)


if __name__ == "__main__":
    sys.exit(main())
