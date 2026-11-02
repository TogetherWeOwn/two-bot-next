"""Production deploy build identity and the /readyz revision check.

`render` writes the Wrangler config for `wrangler deploy --config`: the checked-in
TOML with absolute paths and the three non-secret build arguments (`image_vars`)
that /readyz reports as `build_revision`, `build_id` and `deploy_id`. `readyz`
decides whether one /readyz answer proves the revision the workflow deployed or
rolled back to.

`readyz` refuses bodies over 64 KiB before decoding or parsing. It refuses
ambiguous or unsafe evidence before grading: duplicate JSON object keys,
nesting past the depth bound, and remote revision/build-ID
shapes outside `unknown`/40-hex/bounded digits-digits yield a fixed
single-line nonzero refusal that echoes no remote value.
"""

import argparse
import json
import os
import re
import sys
import time
import tomllib
from pathlib import Path

from build_identity import (
    DIGITS,
    SHA,
    UNSTAMPED,
    is_build_id_shape,
    is_build_revision_shape,
    is_deploy_id_shape,
    parse_deploy_id,
)

EXCERPT_BYTES = 2048
MAX_READYZ_BYTES = 64 * 1024

# Well below CPython's default recursion limit (1000): any report deeper
# than this is refused up front with an explicit stack instead of recursing.
MAX_READYZ_DEPTH = 100

# Strings are skipped whole, so brackets inside them never count. An
# unterminated string runs to the end, which keeps the scan linear.
JSON_TOKEN = re.compile(r'"(?:[^"\\]|\\.)*(?:"|\\?\Z)|[\[\]{}]', re.DOTALL)


def _json_depth_exceeds(text, limit):
    """Lexical nesting pre-scan, run before json.loads.

    The decoder's own RecursionError is no longer a reliable bound: newer
    CPython sizes it from the real C stack and decodes 10,000 levels.
    """
    depth = 0
    for match in JSON_TOKEN.finditer(text):
        token = match.group()
        if token in "[{":
            depth += 1
            if depth > limit:
                return True
        elif token in "]}":
            depth -= 1
    return False


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


def _too_deeply_nested(record, limit=MAX_READYZ_DEPTH):
    """Iterative depth check; True when nesting exceeds limit."""
    stack = [(record, 1)]
    while stack:
        obj, depth = stack.pop()
        if depth > limit:
            return True
        if isinstance(obj, dict):
            for value in obj.values():
                if isinstance(value, (dict, list)):
                    stack.append((value, depth + 1))
        elif isinstance(obj, list):
            for value in obj:
                if isinstance(value, (dict, list)):
                    stack.append((value, depth + 1))
    return False


def render(args):
    if not (is_build_revision_shape(args.sha) and DIGITS.fullmatch(args.run_id) and DIGITS.fullmatch(args.attempt)
            and is_deploy_id_shape(args.deploy_id)):
        sys.exit("render: needs a 40-hex SHA, numeric run id and attempt, and a canonical deploy ID")
    # The stamped ID must name this SHA: an ID computed for another commit
    # refuses here, not after the deploy it cannot match.
    if parse_deploy_id(args.deploy_id)[2] != args.sha[:7]:
        sys.exit("render: the deploy ID names a different commit")
    source = Path(args.config).resolve()
    config = tomllib.loads(source.read_text())
    if config.get("name") != "two-bot-next":
        sys.exit("render: the Wrangler config is not the two-bot-next Worker")
    containers = config.get("env", {}).get("production", {}).get("containers", [])
    if len(containers) != 1 or containers[0].get("class_name") != "TwoBotContainer" or containers[0].get("max_instances") != 1:
        sys.exit("render: production needs exactly one TwoBotContainer with max_instances = 1")
    # Wrangler validates unused environment definitions too. Resolve every
    # local container path against the source TOML before relocating the JSON,
    # then replace only production's build inputs with the admitted image.
    declared = [
        *config.get("containers", []),
        *(container for env in config.get("env", {}).values() for container in env.get("containers", [])),
    ]
    for container in declared:
        container["image"] = str((source.parent / container["image"]).resolve())
        container["image_build_context"] = str(source.parent.parent)
    artifact = getattr(args, "artifact", None)
    if artifact:
        from bot_promotion import configure_bundle, load_admitted

        try:
            admitted = load_admitted(artifact, args.sha, verify_modules=True)
            worker = admitted["receipt"]["worker"]
            production = config["env"]["production"]
            if (production.get("compatibility_date", config.get("compatibility_date"))
                    != worker["compatibility_date"]
                    or production.get("compatibility_flags", config.get("compatibility_flags", []))
                    != worker["compatibility_flags"]):
                raise ValueError("incompatible saved Worker")
            configure_bundle(config, Path(artifact).resolve().parent / "modules", worker["entrypoint"])
            containers[0]["image"] = admitted["production_image"]
            containers[0].pop("image_vars", None)
            containers[0].pop("image_build_context", None)
        except (OSError, ValueError):
            return reject("render: immutable promotion artifact refused")
    else:
        config["main"] = str((source.parent / config["main"]).resolve())
        containers[0]["image_vars"] = {"BOT_BUILD_REVISION": args.sha,
                                      "BOT_BUILD_ID": f"{args.run_id}-{args.attempt}",
                                      "BOT_DEPLOY_ID": args.deploy_id}
    # Worker-edge span lines log this SHA as `release` (same commit as the
    # container image). Generated-only: a committed value would go stale, so
    # the committed TOML must never carry it.
    production_vars = config["env"]["production"].setdefault("vars", {})
    if not isinstance(production_vars, dict):
        sys.exit("render: production vars have the wrong shape")
    production_vars["CONTAINER_RELEASE"] = args.sha
    Path(args.out).write_text(json.dumps(config, indent=2) + "\n")
    return 0


def reject(reason):
    print(reason)
    return 1


def _gateway_ready(report):
    """True when the component breakdown reports the gateway ready.

    The breakdown is untrusted service output: only an exact
    ["gateway", "ready"] pair counts, and nothing remote reaches the
    verdict text, so a hostile answer cannot forge a passing state.
    """
    components = report.get("components")
    if not isinstance(components, list):
        return False
    return any(
        isinstance(row, list) and row == ["gateway", "ready"] for row in components
    )


def readyz(args):
    if args.status not in ("200", "503"):
        return reject(f"/readyz answered {args.status}")
    # The expected deploy ID is optional during the stamping transition:
    # callers that predate it judge the legacy revision/build-ID shape
    # exactly as before (the protected body-cap suite pins those verdicts
    # byte-for-byte). Present callers always pass it — the workflow pins the
    # flag — and then it must be canonical and name the guarded SHA in both
    # modes. getattr: direct callers may predate the attribute itself.
    expected = getattr(args, "deploy_id", None)
    if expected is not None:
        try:
            _date, _n, expected_sha7 = parse_deploy_id(expected)
        except ValueError:
            return reject("readyz needs a canonical deploy ID to judge")
        if expected_sha7 != args.sha[:7]:
            return reject("the expected deploy ID names a different commit")
    expected_build = args.build_id
    artifact = getattr(args, "artifact", None)
    if artifact:
        from bot_promotion import load_admitted

        try:
            admitted = load_admitted(artifact, args.sha)
            version = getattr(args, "expected_worker_version", None)
            if (args.mode != "deploy" or not isinstance(version, str)
                    or re.fullmatch(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", version) is None
                    or version == admitted["receipt"]["staging"]["worker_version"]
                    or (response_version(args.headers) if getattr(args, "headers", None)
                        else getattr(args, "worker_version", None)) != version
                    or (getattr(args, "health_headers", None)
                        and response_version(args.health_headers) != version)
                    or getattr(args, "image", None) != admitted["production_image"]):
                raise ValueError("serving production identity mismatch")
            expected_build = admitted["receipt"]["build"]["build_id"]
            expected = admitted["receipt"]["build"]["deploy_id"]
        except (OSError, ValueError):
            return reject("readyz: immutable promotion or serving identity refused")
    try:
        with open(args.body, "rb") as handle:
            raw = handle.read(MAX_READYZ_BYTES + 1)
        # The sentinel detects a hidden suffix; size wins over all parser
        # verdicts, without decoding or echoing any oversized remote input.
        if len(raw) > MAX_READYZ_BYTES:
            return reject("/readyz answered over the body cap")
        text = raw.decode("utf-8")
        if _json_depth_exceeds(text, MAX_READYZ_DEPTH):
            return reject("/readyz JSON is too deeply nested")
        report = json.loads(text, object_pairs_hook=_reject_duplicate_keys)
    except DuplicateKeyError:
        return reject("/readyz carries duplicate JSON object keys")
    except RecursionError:
        # Decoder-level backstop behind the lexical pre-scan above.
        return reject("/readyz JSON is too deeply nested")
    except (OSError, ValueError):
        return reject("/readyz did not return JSON")
    if not isinstance(report, dict):
        return reject("/readyz JSON is not an object")
    if _too_deeply_nested(report):
        return reject("/readyz JSON is too deeply nested")
    revision, build_id = report.get("build_revision"), report.get("build_id")
    if not isinstance(revision, str) or not isinstance(build_id, str):
        return reject("/readyz has no build_revision and build_id")
    # Remote shapes are validated before any value enters diagnostics: only
    # the unstamped marker, a 40-char hex revision, or a bounded
    # digits-digits build ID can appear in output. Anything else (newline,
    # pipe, control, overlong, secret-shaped) yields a fixed refusal with no
    # echo, so a hostile answer cannot forge log/summary lines or leak into
    # evidence. The explicit unstamped rollback exception below is preserved,
    # as are valid historical numeric run-attempt build IDs.
    if revision != UNSTAMPED and not is_build_revision_shape(revision):
        return reject("/readyz build_revision has an unexpected shape")
    if build_id != UNSTAMPED and not is_build_id_shape(build_id):
        return reject("/readyz build_id has an unexpected shape")
    deploy_id = report.get("deploy_id")
    if args.mode == "deploy" and expected is not None:
        # A fresh build always reports its stamped ID; a missing key is a
        # previous container still serving, never a pass. Rollback mode —
        # and legacy callers without an expectation — ignore the reported
        # value entirely: the restored build predates stamping or carries
        # its original stamped ID (like build_id).
        if not isinstance(deploy_id, str):
            return reject("/readyz has no deploy_id")
        if deploy_id != UNSTAMPED and not is_deploy_id_shape(deploy_id):
            return reject("/readyz deploy_id has an unexpected shape")
    if revision == args.sha:
        if args.mode == "deploy" and build_id != expected_build:
            return reject(
                f"build_id {build_id} is not this run's build ({expected_build}): "
                "a previous container still serves this SHA"
            )
        if args.mode == "deploy" and expected is not None and deploy_id != expected:
            return reject(
                f"deploy_id {deploy_id} is not this run's deploy ({expected}): "
                "a previous container still serves this SHA"
            )
        # A parked gateway is never a pass: the release gate requires
        # /readyz 200 with gateway=ready, or it fails and the run rolls
        # back. A truthful 503 while the gateway reconnects keeps polling
        # in the workflow; it must never certify the deploy.
        if args.status != "200" or not _gateway_ready(report):
            return reject("gateway not ready: /readyz is not 200 with gateway ready; roll back")
        print(f"gateway ready, revision matches the SHA, build {build_id}")
        return 0
    if args.mode == "rollback" and revision == build_id == UNSTAMPED:
        # Pre-stamp identity is recorded, not verified: it still needs a live
        # 200 with gateway ready, so parked 503s never count as a stable pass
        # on a rollback to a build from before stamping.
        if args.status != "200" or not _gateway_ready(report):
            return reject("gateway not ready: /readyz is not 200 with gateway ready; roll back")
        print("pre-stamp version: build identity unknown, revision not verifiable (recorded; rollback not failed)")
        return 0
    if revision == UNSTAMPED:
        return reject("build_revision is unknown: this build was not stamped")
    return reject("build_revision does not match the SHA")


def response_version(path):
    with open(path, "rb") as handle:
        raw = handle.read(MAX_READYZ_BYTES + 1)
    if len(raw) > MAX_READYZ_BYTES:
        raise ValueError("response headers over cap")
    values = [line.split(b":", 1)[1].strip().decode("ascii")
              for line in raw.splitlines() if line.lower().startswith(b"x-two-worker-version:")]
    if len(values) != 1:
        raise ValueError("ambiguous response version")
    return values[0]


def _promoted_runtime_ready(client, url, version, receipt):
    """Confirm live Worker headers and original stamps without logging bodies."""
    for suffix in ("health", "readyz"):
        status, headers, body = client.request(f"{url}/{suffix}")
        if status != 200 or len(body) > MAX_READYZ_BYTES:
            return False
        # HTTPMessage retains duplicate fields; a last-wins header lookup
        # would let an ambiguous response authorize the promoted image.
        if hasattr(headers, "get_all"):
            versions = headers.get_all("x-two-worker-version", [])
        else:
            versions = [value for key, value in headers.items()
                        if key.lower() == "x-two-worker-version"]
        if versions != [version]:
            return False
        if suffix == "readyz":
            try:
                report = json.loads(body, object_pairs_hook=_reject_duplicate_keys)
            except (ValueError, UnicodeError, RecursionError):
                return False
            build = receipt["build"]
            if (not isinstance(report, dict) or _too_deeply_nested(report)
                    or report.get("build_revision") != receipt["source"]["sha"]
                    or report.get("build_id") != build["build_id"]
                    or report.get("deploy_id") != build["deploy_id"]
                    or not _gateway_ready(report)):
                return False
    return True


def serving(args):
    """Check identity before cold start; prove convergence only after readiness."""
    import worker_container_rollback as rollback
    from bot_promotion import PRODUCTION_APP, REGISTRY_HOST, load_admitted

    try:
        admitted = load_admitted(args.artifact, args.sha)
        if not is_deploy_id_shape(args.deploy_id) or parse_deploy_id(args.deploy_id)[2] != args.sha[:7]:
            raise ValueError("invalid production event ID")
        image = admitted["production_image"]
        account = os.environ.get("CLOUDFLARE_ACCOUNT_ID")
        expected_image = f"{REGISTRY_HOST}/{account}/{PRODUCTION_APP}@{admitted['receipt']['container']['digest']}"
        if not account or image != expected_image:
            raise ValueError("production image account mismatch")
        if (not isinstance(args.expected_worker_version, str)
                or rollback.VERSION_ID.fullmatch(args.expected_worker_version) is None
                or args.expected_worker_version == admitted["receipt"]["staging"]["worker_version"]):
            raise ValueError("invalid production version")
        preflight = getattr(args, "preflight", False)
        url = getattr(args, "url", None)
        if url:
            url = rollback.check_url("production", url)
        previous = None
        snapshot = getattr(args, "snapshot", None)
        if snapshot:
            with open(snapshot, "rb") as handle:
                raw = handle.read(MAX_READYZ_BYTES + 1)
            if len(raw) > MAX_READYZ_BYTES:
                raise ValueError("snapshot over cap")
            previous = json.loads(raw, object_pairs_hook=_reject_duplicate_keys)
            if (not isinstance(previous, dict)
                    or previous.get("worker") != rollback.WORKERS["production"]
                    or previous.get("application") != PRODUCTION_APP
                    or type(previous.get("has_previous")) is not bool):
                raise ValueError("snapshot identity mismatch")
        deadline = time.monotonic() + 600
        client = rollback.base.Client(account, os.environ.get("CLOUDFLARE_API_TOKEN"), deadline)
        lag_streak = 0
        lag_identity = None
        while time.monotonic() < deadline:
            version = rollback.serving_version(client, rollback.WORKERS["production"])
            if version != args.expected_worker_version:
                raise ValueError("production Worker drift")
            app = rollback.application_by_name(client, rollback.APPLICATIONS["production"])
            if previous and previous["has_previous"] and previous.get("application_id") != app["id"]:
                raise ValueError("production application drift")
            rows = rollback.app_rollouts(client, app["id"])
            row = None
            if rows:
                latest = max(rows, key=lambda row: rollback.base.timestamp(row.get("created_at")))
                row = rollback.base.mapping(client.api(
                    f"/containers/applications/{rollback.base.identifier(app['id'])}/rollouts/"
                    f"{rollback.base.identifier(latest.get('id'))}"))
                if row.get("id") != latest.get("id"):
                    raise ValueError("production rollout drift")
                if rollback.rollout_image(row, rollback.APPLICATIONS["production"]) != image:
                    # Only the explicitly snapshotted old rollout can be a
                    # stale listing. A different/newer foreign image refuses.
                    if (previous and previous["has_previous"]
                            and row["id"] == previous.get("serving_rollout_id")
                            and row["target_configuration"]["image"] == previous.get("serving_image")):
                        row = None
                    else:
                        raise ValueError("production rollout image drift")
            accepted = False
            if row is not None:
                target = rollback.base.number(row.get("target_version"))
                converged = rollback.base.converged(row, image, target)
                app_matches = (rollback.base.mapping(app.get("configuration")).get("image") == image
                               and app.get("version") == target)
                if preflight:
                    # Takeover leaves the process stopped. This proves only
                    # intended configuration, not running-instance readiness.
                    accepted = app_matches
                elif app_matches:
                    runtime_ready = (not url or _promoted_runtime_ready(
                        client, url, version, admitted["receipt"]))
                    lag = rollback.base.active_lag(row, image, target)
                    identity = (app["id"], row["id"], target)
                    if lag and url and runtime_ready:
                        lag_streak = lag_streak + 1 if lag_identity == identity else 1
                        lag_identity = identity
                    else:
                        lag_streak = 0
                        lag_identity = None
                    accepted = runtime_ready and (converged or lag_streak >= rollback.base.ACTIVE_LAG_CONFIRMATIONS)
            if not accepted:
                # Empty/stale listings and any unready poll break consecutive
                # confirmations, including when application state lags.
                if row is None or not app_matches:
                    lag_streak = 0
                    lag_identity = None
                time.sleep(min(5, max(0, deadline - time.monotonic())))
                continue
            # Re-read after every accepted observation: an intervening secret-
            # only Worker publish must never authorize the old version's bytes.
            if rollback.serving_version(client, rollback.WORKERS["production"]) != version:
                raise ValueError("production Worker drift")
            proof = {"worker_version": version, "production_image": image,
                     "production_deploy_id": args.deploy_id,
                     "rollout_id": row["id"], "target_version": target,
                     "receipt": admitted["receipt"]}
            if preflight:
                proof["phase"] = "identity"
            Path(args.out).write_text(json.dumps(proof, indent=2) + "\n")
            print("promoted production identity verified; readiness pending" if preflight
                  else "promoted production Worker and container digest verified")
            return 0
    except (OSError, ValueError, RecursionError, rollback.GateError):
        return reject("serving: promoted production identity refused")
    return reject("serving: promoted production identity refused; image did not converge")


def excerpt(args):
    try:
        with open(args.body, "rb") as handle:
            head = handle.read(EXCERPT_BYTES + 1)
    except OSError:
        print("no body was written")
        return 0
    suffix = " ...(truncated)" if len(head) > EXCERPT_BYTES else ""
    # repr escapes newlines and control bytes, so a remote body stays one log line.
    print(repr(head[:EXCERPT_BYTES]) + suffix)
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    commands = parser.add_subparsers(dest="command", required=True)

    render_cmd = commands.add_parser("render", help="write the Wrangler config with build identity")
    render_cmd.add_argument("--config", required=True)
    render_cmd.add_argument("--out", required=True)
    render_cmd.add_argument("--sha", required=True)
    render_cmd.add_argument("--run-id", required=True)
    render_cmd.add_argument("--attempt", required=True)
    render_cmd.add_argument("--deploy-id", required=True)
    render_cmd.add_argument("--artifact", help="admitted immutable staging artifact; never rebuild")

    readyz_cmd = commands.add_parser("readyz", help="judge one /readyz answer against the revision")
    readyz_cmd.add_argument("--status", required=True)
    readyz_cmd.add_argument("--body", required=True)
    readyz_cmd.add_argument("--sha", required=True)
    readyz_cmd.add_argument("--mode", choices=("deploy", "rollback"), required=True)
    # This run's `<run id>-<attempt>` (ignored in rollback mode, where the
    # serving version was built by an older run). Deploy mode requires it so a
    # same-SHA redeploy cannot pass on the previous container's answer.
    readyz_cmd.add_argument("--build-id", required=True)
    # This run's `YYYY.MM.DD.N+sha7` deploy ID. Deploy mode requires the
    # report to carry it; rollback mode ignores the reported value (the
    # restored build carries its original stamped ID) but still requires a
    # canonical expectation naming the guarded SHA. Absent, the legacy
    # revision/build-ID verdicts apply unchanged (stamping transition).
    readyz_cmd.add_argument("--deploy-id", required=False, default=None)
    readyz_cmd.add_argument("--artifact", help="judge original admitted artifact stamps")
    readyz_cmd.add_argument("--expected-worker-version")
    readyz_cmd.add_argument("--worker-version")
    readyz_cmd.add_argument("--image")
    readyz_cmd.add_argument("--headers", help="actual /readyz response headers")
    readyz_cmd.add_argument("--health-headers", help="actual /health response headers")

    serving_cmd = commands.add_parser("serving", help="verify promoted production control-plane identity")
    serving_cmd.add_argument("--sha", required=True)
    serving_cmd.add_argument("--deploy-id", required=True)
    serving_cmd.add_argument("--artifact", required=True)
    serving_cmd.add_argument("--expected-worker-version", required=True)
    serving_cmd.add_argument("--out", required=True)
    serving_cmd.add_argument("--preflight", action="store_true", help="identity only, before owned cold-start probes")
    serving_cmd.add_argument("--url", help="production origin for live exact-runtime confirmations")
    serving_cmd.add_argument("--snapshot", help="pre-deploy snapshot identifying an older rollout listing")

    excerpt_cmd = commands.add_parser("excerpt", help="print a remote body as one bounded escaped line")
    excerpt_cmd.add_argument("--body", required=True)

    args = parser.parse_args(argv)
    if args.command == "render":
        return render(args)
    if args.command == "excerpt":
        return excerpt(args)
    if args.command == "serving":
        return serving(args)
    return readyz(args)


if __name__ == "__main__":
    sys.exit(main())
