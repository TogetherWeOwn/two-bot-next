#!/usr/bin/env python3
"""Staging Discord-level smoke for the branded `/help` embeds.

The staging test bot posts the renderer payloads
(`crates/discord/src/bin/help_embed_smoke.rs`, built from the real
`help_response` / `help_detail_response` builders via `brand_embed()`) into
the dedicated staging test channel, reads both messages back through the
Discord REST API, and asserts the brand the members actually see:

  colour       every posted embed reads back `color == 0xFF0E52`
  footer       every posted embed reads back footer text `Together We Own`
               with the footer icon URL exactly the Icon 1 asset
  icon         the Icon 1 asset URL answers HTTP 200 as `image/png`
  cleanup      both test messages are deleted afterwards

The message ids and every assertion land in the evidence JSON and in
`$GITHUB_STEP_SUMMARY`, so TWO-1620-style asks for human screenshots are no
longer needed. A PASS means Discord served and returned the brand intact; a
FAIL names the assertion that did not hold.

Identity and fences (fail-closed, before any post):
  - TWO_HELP_EMBED_SMOKE_BOT_TOKEN: a dedicated test bot (never the staging
    product bot, never prod). The smoke posts and deletes only its own test
    messages; it never invokes slash commands (Discord only creates
    interactions for real users, so a bot token cannot).
  - HELP_EMBED_SMOKE_CHANNEL_ID: the dedicated staging test channel. The
    script GETs the channel first and refuses unless its guild is exactly
    TWO Staging; the live guild or anything else refuses with exit 2 and
    sends nothing.
  - The footer icon fetch never carries the bot token: the asset URL is
    public, and an Authorization header there would leak the token to a
    third party.

Exit codes: 0 every check passed (or SKIP while the smoke is not required
and not configured), 1 a check failed, 2 refused before contact (fence or
missing configuration when required).

Usage:
  TWO_HELP_EMBED_SMOKE_BOT_TOKEN=... python3 scripts/staging_help_embed_smoke.py \\
      --payloads payloads.json --evidence evidence.json
"""

import argparse
import json
import os
import sys
import urllib.error
import urllib.request

API = "https://discord.com/api/v10"
STAGING_GUILD_ID = "1545644954272137297"
LIVE_GUILD_ID = "326474832151838730"

EXPECTED_COLOR = 0xFF0E52
EXPECTED_FOOTER_TEXT = "Together We Own"
EXPECTED_ICON_URL = (
    "https://raw.githubusercontent.com/TogetherWeOwn/two-bot-next"
    "/main/assets/brand/two-icon-1-dark-512.png"
)

# Body-cap refusal contract (same shape as staging_probe.py MAX_BODY_BYTES and
# the voice synthetic BODY_CAP): every response read takes at most
# MAX_BODY_BYTES+1 bytes so a hidden suffix is detected. Any body longer than
# MAX_BODY_BYTES (64 KiB) is refused before decoding/parsing with a fixed
# message; exact-cap valid input passes, and the extra byte never reaches a
# successful JSON path. Refusals never echo the token, bodies, headers or URLs.
MAX_BODY_BYTES = 64 * 1024

REST_TIMEOUT_SECONDS = 15.0
ICON_TIMEOUT_SECONDS = 20.0


class Refusal(Exception):
    pass


class BodyTooLarge(ValueError):
    """A response body exceeds MAX_BODY_BYTES (fixed vocabulary only)."""


REDIRECT_STATUSES = (301, 302, 303, 307, 308)


class RedirectRefused(ValueError):
    """An authenticated read answered a redirect (fixed vocabulary only)."""


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


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Turn every redirect into an HTTPError without contacting its target."""

    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


def _open_no_redirect(request, timeout):
    """One token-bearing request that never follows a redirect.

    urllib's default redirect handler would replay the Authorization header
    to the redirect target; rejecting redirects at the opener means only the
    intended URL is ever contacted. A 3xx surfaces as HTTPError so each
    reader reports its fixed-vocabulary redirect refusal.
    """
    opener = urllib.request.build_opener(
        urllib.request.HTTPHandler(),
        urllib.request.HTTPSHandler(),
        urllib.request.HTTPErrorProcessor(),
        _NoRedirectHandler(),
    )
    return opener.open(request, timeout=timeout)


def _read_capped(response):
    """Read at most MAX_BODY_BYTES+1 bytes; refuse oversize bodies."""
    chunks = []
    remaining = MAX_BODY_BYTES + 1
    while remaining > 0:
        piece = response.read(remaining)
        if not piece:
            break
        chunks.append(piece)
        remaining -= len(piece)
    body = b"".join(chunks)
    if len(body) > MAX_BODY_BYTES:
        raise BodyTooLarge("response body exceeds the 64 KiB cap")
    return body


def _decode_json(body):
    try:
        text = body.decode("utf-8")
    except UnicodeDecodeError:
        raise Refusal("response body is not UTF-8 JSON")
    try:
        return json.loads(text, object_pairs_hook=_reject_duplicate_keys)
    except DuplicateKeyError:
        raise
    except ValueError:
        raise Refusal("response body is not JSON")


def _rest(token, method, path, payload=None):
    """One authenticated Discord REST call with redirect and body-cap fences."""
    data = None
    headers = {
        "Authorization": f"Bot {token}",
        "User-Agent": "two-bot-next-help-embed-smoke/1.0",
    }
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(
        API + path, data=data, headers=headers, method=method
    )
    try:
        with _open_no_redirect(request, REST_TIMEOUT_SECONDS) as response:
            status = response.status
            body = _read_capped(response)
    except urllib.error.HTTPError as error:
        if error.code in REDIRECT_STATUSES:
            raise RedirectRefused("authenticated read answered a redirect")
        raise Refusal("Discord REST call failed")
    return status, _decode_json(body)


def _fetch_icon():
    """GET the public footer icon with no credentials and body-cap fences."""
    request = urllib.request.Request(
        EXPECTED_ICON_URL,
        headers={"User-Agent": "two-bot-next-help-embed-smoke/1.0"},
        method="GET",
    )
    try:
        with _open_no_redirect(request, ICON_TIMEOUT_SECONDS) as response:
            status = response.status
            content_type = response.headers.get("Content-Type", "")
            _read_capped(response)
    except urllib.error.HTTPError as error:
        if error.code in REDIRECT_STATUSES:
            raise RedirectRefused("icon fetch answered a redirect")
        raise Refusal("icon fetch failed")
    return status, content_type


# --------------------------------------------------------------------------
# Pure verdict logic (offline-testable)
# --------------------------------------------------------------------------

def check(name, ok, detail):
    return {"check": name, "verdict": "PASS" if ok else "FAIL", "detail": detail}


def embed_facts(message):
    """Brand facts for the first embed of a read-back message (fixed shape)."""
    embeds = message.get("embeds") or []
    first = embeds[0] if embeds else {}
    footer = first.get("footer") or {}
    return {
        "embed_count": len(embeds),
        "color": first.get("color"),
        "footer_text": footer.get("text"),
        "footer_icon_url": footer.get("icon_url"),
    }


def evaluate(help_message, detail_message, icon_status, icon_content_type, meta):
    """Checks for one smoke run. ``meta`` carries the renderer provenance
    (``help_builder_embedded`` / ``help_detail_builder_embedded`` /
    ``help_topic``); every brand assertion below is absolute, never relative
    to what the renderer emitted."""
    out = []
    for label, message in (("help", help_message), ("help_detail", detail_message)):
        facts = embed_facts(message or {})
        out.append(check(
            f"{label}_posted",
            isinstance((message or {}).get("id"), str),
            f"{label} message id={(message or {}).get('id')!r}",
        ))
        out.append(check(
            f"{label}_color",
            facts["color"] == EXPECTED_COLOR,
            f"{label} embed color={facts['color']!r} (brand 0xFF0E52)",
        ))
        out.append(check(
            f"{label}_footer_text",
            facts["footer_text"] == EXPECTED_FOOTER_TEXT,
            f"{label} footer text={facts['footer_text']!r}",
        ))
        out.append(check(
            f"{label}_footer_icon_url",
            facts["footer_icon_url"] == EXPECTED_ICON_URL,
            f"{label} footer icon url matches the Icon 1 asset: "
            f"{facts['footer_icon_url'] == EXPECTED_ICON_URL}",
        ))
    icon_ok = icon_status == 200 and (icon_content_type or "").split(";")[0].strip() == "image/png"
    out.append(check(
        "footer_icon_fetch",
        icon_ok,
        f"Icon 1 asset fetch status={icon_status!r} "
        f"content_type={icon_content_type!r} (need 200 image/png)",
    ))
    out.append(check(
        "provenance_recorded",
        isinstance(meta, dict)
        and isinstance(meta.get("help_builder_embedded"), bool)
        and isinstance(meta.get("help_detail_builder_embedded"), bool),
        f"renderer provenance help_embedded={meta.get('help_builder_embedded')!r} "
        f"detail_embedded={meta.get('help_detail_builder_embedded')!r} "
        f"topic={meta.get('help_topic')!r}",
    ))
    return out


def summarize(checks, help_id, detail_id, topic, meta):
    """Human-readable `$GITHUB_STEP_SUMMARY` lines (ids and verdicts only)."""
    lines = ["## Help embed smoke (staging)", ""]
    verdict = {item["check"]: item["verdict"] for item in checks}
    for label, message_id in (("help", help_id), ("help_detail", detail_id)):
        parts = [f"{label}_posted", f"{label}_color", f"{label}_footer_text",
                 f"{label}_footer_icon_url"]
        states = ", ".join(f"{part[len(label) + 1:]}={verdict.get(part, '?')}" for part in parts)
        lines.append(f"- `{label}`" + (f" (`/{topic}`)" if label == "help_detail" else "")
                     + f" message `{message_id}`: {states}")
    lines.append(f"- footer icon fetch: {verdict.get('footer_icon_fetch', '?')}")
    lines.append(
        "- provenance: help_builder_embedded="
        f"{meta.get('help_builder_embedded')!r} "
        f"detail_builder_embedded={meta.get('help_detail_builder_embedded')!r}")
    failed = [item["check"] for item in checks if item["verdict"] == "FAIL"]
    if failed:
        lines += ["", f"FAILED: {', '.join(failed)}"]
    else:
        lines += ["", "All brand assertions passed."]
    return "\n".join(lines) + "\n"


# --------------------------------------------------------------------------
# Live run
# --------------------------------------------------------------------------

def _load_payloads(path):
    try:
        with open(path, "rb") as handle:
            body = handle.read(MAX_BODY_BYTES + 1)
    except OSError:
        raise Refusal("renderer payloads are unreadable")
    if len(body) > MAX_BODY_BYTES:
        raise BodyTooLarge("response body exceeds the 64 KiB cap")
    payloads = _decode_json(body)
    if not isinstance(payloads, dict):
        raise Refusal("renderer payloads have the wrong shape")
    return payloads


def _posted_embeds(payloads, key):
    case = payloads.get(key)
    if not isinstance(case, dict):
        raise Refusal("renderer payloads have the wrong shape")
    embeds = case.get("posted_embeds")
    if not isinstance(embeds, list) or not embeds:
        raise Refusal("renderer payloads have the wrong shape")
    return embeds


def run_smoke(token, channel_id, payloads):
    """Post, read back, assert and delete. Returns the evidence dict.

    Raises Refusal on fence violations and live-call failures with fixed
    vocabulary (never the token, bodies, headers or URLs). Deletion is best
    effort inside a finally: a failed delete is recorded, never silent.
    """
    if not channel_id.isdigit():
        raise Refusal("test channel id has the wrong shape")
    try:
        _status, channel = _rest(token, "GET", f"/channels/{channel_id}")
    except (Refusal, RedirectRefused, BodyTooLarge, DuplicateKeyError):
        raise
    except Exception:
        raise Refusal("Discord REST call failed")
    if not isinstance(channel, dict) or channel.get("guild_id") != STAGING_GUILD_ID:
        raise Refusal("test channel is not in TWO Staging")
    posted = []
    try:
        _status, created_help = _rest(
            token, "POST", f"/channels/{channel_id}/messages",
            {"embeds": _posted_embeds(payloads, "help"),
             "allowed_mentions": {"parse": []}},
        )
        help_id = created_help.get("id") if isinstance(created_help, dict) else None
        if not isinstance(help_id, str):
            raise Refusal("Discord REST call failed")
        posted.append(help_id)
        _status, created_detail = _rest(
            token, "POST", f"/channels/{channel_id}/messages",
            {"embeds": _posted_embeds(payloads, "help_detail"),
             "allowed_mentions": {"parse": []}},
        )
        detail_id = created_detail.get("id") if isinstance(created_detail, dict) else None
        if not isinstance(detail_id, str):
            raise Refusal("Discord REST call failed")
        posted.append(detail_id)
        _status, help_message = _rest(token, "GET", f"/channels/{channel_id}/messages/{help_id}")
        _status, detail_message = _rest(
            token, "GET", f"/channels/{channel_id}/messages/{detail_id}")
        try:
            icon_status, icon_content_type = _fetch_icon()
        except (Refusal, RedirectRefused, BodyTooLarge):
            raise
        except Exception:
            raise Refusal("icon fetch failed")
        detail_case = payloads.get("help_detail") or {}
        meta = {
            "help_builder_embedded": (payloads.get("meta") or {}).get("help_builder_embedded"),
            "help_detail_builder_embedded": (payloads.get("meta") or {}).get(
                "help_detail_builder_embedded"),
            "help_topic": detail_case.get("topic") if isinstance(detail_case, dict) else None,
        }
        checks = evaluate(help_message, detail_message, icon_status, icon_content_type, meta)
    finally:
        deletions = {}
        for message_id in posted:
            try:
                _rest(token, "DELETE", f"/channels/{channel_id}/messages/{message_id}")
                deletions[message_id] = "deleted"
            except Exception:
                deletions[message_id] = "delete_failed"
    for message_id, outcome in deletions.items():
        checks.append(check(f"cleanup_{message_id}", outcome == "deleted",
                            f"test message {message_id}: {outcome}"))
    evidence = {
        "channel_id": channel_id,
        "help_message_id": posted[0] if len(posted) > 0 else None,
        "help_detail_message_id": posted[1] if len(posted) > 1 else None,
        "meta": meta,
        "checks": checks,
    }
    return evidence


def main(argv=None):
    parser = argparse.ArgumentParser(description="Staging help-embed smoke.")
    parser.add_argument("--payloads", required=True, help="Renderer JSON from help-embed-smoke")
    parser.add_argument("--evidence", required=True, help="Evidence JSON to write")
    parser.add_argument("--channel-id", default=os.environ.get("HELP_EMBED_SMOKE_CHANNEL_ID", ""),
                        help="Staging test channel (default: env)")
    parser.add_argument("--required", default=os.environ.get("HELP_EMBED_SMOKE_REQUIRED", "false"),
                        help="true while the smoke gates staging (default: env or false)")
    parser.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY", ""),
                        help="Step summary file (default: env)")
    args = parser.parse_args(argv)

    token = os.environ.get("TWO_HELP_EMBED_SMOKE_BOT_TOKEN", "")
    if not token or not args.channel_id:
        if args.required == "true":
            print("::error::help embed smoke is required but the test bot token or "
                  "channel is not configured (provision TWO_HELP_EMBED_SMOKE_BOT_TOKEN "
                  "and HELP_EMBED_SMOKE_CHANNEL_ID; see docs/help-embed-smoke.md)", flush=True)
            return 2
        notice = ("help embed smoke is not configured yet (no test bot token or "
                  "channel); see docs/help-embed-smoke.md")
        print(f"::notice::{notice}", flush=True)
        evidence = {"verdict": "SKIP", "reason": "not-configured", "checks": []}
        with open(args.evidence, "w", encoding="utf-8") as handle:
            json.dump(evidence, handle, indent=2, sort_keys=True)
            handle.write("\n")
        if args.summary:
            with open(args.summary, "a", encoding="utf-8") as handle:
                handle.write("## Help embed smoke (staging)\n\nSKIP: not configured.\n")
        return 0

    try:
        payloads = _load_payloads(args.payloads)
    except (Refusal, BodyTooLarge, DuplicateKeyError) as error:
        print(f"::error::{error}", flush=True)
        return 2
    try:
        evidence = run_smoke(token, args.channel_id, payloads)
    except (Refusal, RedirectRefused, BodyTooLarge, DuplicateKeyError) as error:
        print(f"::error::{error}", flush=True)
        return 2
    checks = evidence.get("checks", [])
    failed = [item for item in checks if item.get("verdict") == "FAIL"]
    evidence["verdict"] = "FAIL" if failed else "PASS"
    with open(args.evidence, "w", encoding="utf-8") as handle:
        json.dump(evidence, handle, indent=2, sort_keys=True)
        handle.write("\n")
    if args.summary:
        with open(args.summary, "a", encoding="utf-8") as handle:
            handle.write(summarize(
                checks,
                evidence.get("help_message_id"),
                evidence.get("help_detail_message_id"),
                (evidence.get("meta") or {}).get("help_topic"),
                evidence.get("meta") or {},
            ))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
