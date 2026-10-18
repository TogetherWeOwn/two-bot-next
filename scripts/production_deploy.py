"""Production deploy build identity and the /readyz revision check.

`render` writes the Wrangler config for `wrangler deploy --config`: the checked-in
TOML with absolute paths and the two non-secret build arguments (`image_vars`)
that /readyz reports as `build_revision` and `build_id`. `readyz` decides whether
one /readyz answer proves the revision the workflow deployed or rolled back to.

`readyz` refuses ambiguous or unsafe evidence before grading: duplicate JSON
object keys, nesting past the depth bound, and remote revision/build-ID
shapes outside `unknown`/40-hex/bounded digits-digits yield a fixed
single-line nonzero refusal that echoes no remote value.
"""

import argparse
import json
import sys
import tomllib
from pathlib import Path

from build_identity import (
    DIGITS,
    SHA,
    UNSTAMPED,
    is_build_id_shape,
    is_build_revision_shape,
)

EXCERPT_BYTES = 2048

# Well below CPython's default recursion limit (1000): any report deeper
# than this is refused up front with an explicit stack instead of recursing.
MAX_READYZ_DEPTH = 100


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
    if not (is_build_revision_shape(args.sha) and DIGITS.fullmatch(args.run_id) and DIGITS.fullmatch(args.attempt)):
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
    try:
        report = json.loads(
            Path(args.body).read_text(), object_pairs_hook=_reject_duplicate_keys
        )
    except DuplicateKeyError:
        return reject("/readyz carries duplicate JSON object keys")
    except RecursionError:
        # Decoder-level backstop: input deeper than the C decoder's own
        # stack raises here before _too_deeply_nested ever sees it.
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
    if revision == args.sha:
        if args.mode == "deploy" and build_id != args.build_id:
            return reject(
                f"build_id {build_id} is not this run's build ({args.build_id}): "
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

    readyz_cmd = commands.add_parser("readyz", help="judge one /readyz answer against the revision")
    readyz_cmd.add_argument("--status", required=True)
    readyz_cmd.add_argument("--body", required=True)
    readyz_cmd.add_argument("--sha", required=True)
    readyz_cmd.add_argument("--mode", choices=("deploy", "rollback"), required=True)
    # This run's `<run id>-<attempt>` (ignored in rollback mode, where the
    # serving version was built by an older run). Deploy mode requires it so a
    # same-SHA redeploy cannot pass on the previous container's answer.
    readyz_cmd.add_argument("--build-id", required=True)

    excerpt_cmd = commands.add_parser("excerpt", help="print a remote body as one bounded escaped line")
    excerpt_cmd.add_argument("--body", required=True)

    args = parser.parse_args(argv)
    if args.command == "render":
        return render(args)
    if args.command == "excerpt":
        return excerpt(args)
    return readyz(args)


if __name__ == "__main__":
    sys.exit(main())
