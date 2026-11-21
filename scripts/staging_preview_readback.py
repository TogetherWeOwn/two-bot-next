"""Read back the preview command during an exact staging rollout (GET only)."""

import json
import os
import signal
import time
from types import SimpleNamespace

import staging_automation_read_smoke as discord
import staging_health_contract_probe as health

NAME = "automod-preview"
OPTIONS = (
    ("text", 3, True, {"min_length": 1, "max_length": 2000}),
    ("mentions", 4, False, {"min_value": 0, "max_value": 100}),
    ("attachments", 3, False, {"max_length": 1000}),
)
BOUND_KEYS = ("min_length", "max_length", "min_value", "max_value")


class PreviewError(ValueError):
    """A refusal whose message is safe to log."""


def option_matches(option, name, kind, required, bounds):
    return (isinstance(option, dict) and option.get("name") == name
            and type(option.get("type")) is int and option["type"] == kind
            and option.get("required", False) is required
            and option.get("autocomplete", False) is False
            and all(option.get(key) in (None, []) for key in ("choices", "options", "channel_types"))
            and all(type(option.get(key)) is int and option[key] == bounds[key]
                    if key in bounds else option.get(key) is None for key in BOUND_KEYS))


def observe(args, token, *, clock=time.time, health_fetch=health.fetch, make_fetch=discord.make_fetch):
    guild = discord.guild_fence(args.guild_id)
    discord.require_token(token)
    health.staging_origin(args.staging_url)
    if not (health.is_build_revision_shape(args.expected_sha)
            and health.is_build_id_shape(args.build_id)):
        raise PreviewError("expected build identity is not canonical")

    def bounded(fetch, timeout):
        def get(url):
            if clock() * 1000 + timeout * 1000 >= args.expires_at:
                raise PreviewError("insufficient staging window for another GET")
            return fetch(url)
        return get

    def serving():
        _, results, report = health.run(args, bounded(health_fetch, health.TIMEOUT_SECONDS))
        if not all(result.ok for result in results) or health.canonical_build_identity(report) != (
            args.expected_sha, args.build_id
        ):
            raise PreviewError("staging readiness or exact serving build did not match")

    serving()
    fetch = bounded(make_fetch(token), discord.TIMEOUT_SECONDS)
    app = discord.check_identity(fetch)
    endpoint = f"{discord.API}/applications/{app}/guilds/{guild}/commands"
    entries = discord.get_json(fetch, endpoint)
    if not isinstance(entries, list):
        raise discord.SmokeError("staging command list is not a list")
    command_id, reason = discord.admit_entry(app, guild, entries, NAME)
    if command_id is None:
        raise PreviewError(reason)
    version = next(entry["version"] for entry in entries if isinstance(entry, dict)
                   and entry.get("id") == command_id)
    detail = discord.get_json(fetch, f"{endpoint}/{command_id}")
    if (not isinstance(detail, dict) or detail.get("id") != command_id
            or detail.get("version") != version or detail.get("name") != NAME
            or "type" not in detail or discord.identity_fault(detail, app, guild)):
        raise PreviewError("preview detail does not match the admitted command")
    options = detail.get("options")
    if (detail.get("default_member_permissions") != "32" or not isinstance(options, list)
            or len(options) != len(OPTIONS)
            or not all(option_matches(option, *expected) for option, expected in zip(options, OPTIONS))):
        raise PreviewError("preview detail does not match the Manage Server and option contract")
    serving()
    if clock() * 1000 >= args.expires_at:
        raise PreviewError("staging window expired before read-back completed")
    return {"build_revision": args.expected_sha, "build_id": args.build_id, "command": NAME,
            "command_id": command_id, "version": detail["version"], "registry": "pass",
            "transport": "GET-only", "invocation": False}


def window_expired(_signum, _frame):
    raise PreviewError("staging window expired before read-back completed")


def main():
    try:
        args = SimpleNamespace(
            expected_sha=os.environ.get("GITHUB_SHA", ""),
            build_id=f"{os.environ.get('GITHUB_RUN_ID', '')}-{os.environ.get('GITHUB_RUN_ATTEMPT', '')}",
            expires_at=int(os.environ.get("STAGING_USE_EXPIRES_AT", "")),
            staging_url=os.environ.get("STAGING_WORKER_URL"),
            guild_id=os.environ.get("DISCORD_STAGING_GUILD_ID"),
        )
    except ValueError:
        raise SystemExit("Preview read-back refused: invalid staging window expiry") from None
    # urllib's socket timeout is per operation, not a wall-clock deadline.
    previous_handler = signal.signal(signal.SIGALRM, window_expired)
    try:
        remaining = (args.expires_at - time.time() * 1000) / 1000
        if remaining <= 0:
            raise PreviewError("staging window already expired")
        signal.setitimer(signal.ITIMER_REAL, remaining)
        receipt = observe(args, os.environ.get("DISCORD_STAGING_BOT_TOKEN", ""))
        print(json.dumps(receipt, sort_keys=True))
    except (PreviewError, health.ProbeError, discord.SmokeError) as error:
        raise SystemExit(f"Preview read-back refused: {error}") from None
    except (ValueError, TypeError, OSError, OverflowError):
        raise SystemExit("Preview read-back refused: invalid transport or response") from None
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous_handler)


if __name__ == "__main__":
    main()
