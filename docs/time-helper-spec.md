# Time: private, copyable Discord timestamps

Status: **Declined: no build** (owner decision 2026-11-01). This doc stays as
the record; reopen only with a named host problem. Original proposal: written
2026-10-25. No command, registration, help behavior, storage, network call or
live message is added. Merging this file does not authorize a build or rollout.
This file is the repository source of truth; decision records bind its commit
and SHA-256. Revisions require renewed alignment before build cards.

## 1. Decision: keep no-build as the default

Consumer: an organizer who wants a localized time in an ordinary message,
without scheduling a bot post. This is a usefulness hypothesis, not evidence of
customer demand. Compare these choices before spending a build slot:

| Choice | Benefit / cost | Recommendation |
| --- | --- | --- |
| No build; manually use [HammerTime](https://hammertime.cyou/) | Already offers rendered examples and copyable syntax; no TWO maintenance. The member independently chooses the external site; TWO does not send inputs there | **Default and smallest useful option today** |
| Help-only guidance | A short private help example could explain offsets and point to HammerTime; no conversion in TWO, but still needs help changes and review | Choose only if discoverability, rather than conversion, is the actual need |
| One private stateless `/time` helper | Keeps conversion in Discord, explicit offset only, one private result; adds parser, registry, runtime and test obligations | Conditional alternative specified below; not selected for implementation |

Read 2026-10-25: HammerTime's “Chat syntax / Example result” table includes
`t T d D f F R` **and** `s S`. The
[Discord timestamp reference](https://docs.discord.com/developers/reference#message-formatting)
also documents those nine styles, seconds since the Unix epoch, viewer timezone
and locale, and default `f`. The seven-style premise is therefore narrower
than today's vendor surface. This proposal deliberately allowlists only the
seven requested styles; `s` and `S` refuse until a revised contract is accepted.
Neither source establishes TWO demand or a Discord numeric range guarantee.
No installation, outbound conversion API, spend or publication experiment is
proposed. Third-party privacy and availability were not audited.

## 2. Verified baseline and future integration seam

Inspected base: `c3af7333df80a766f5d4c229506a3bce53680828`.

- `crates/core/src/scheduled.rs:111-126` parses explicit-offset RFC3339 into
  milliseconds, then requires a future instant within 365 days. **Do not reuse
  that validator unchanged:** this helper accepts epoch zero, past times and
  times beyond a year; it has no clock dependency or scheduled-message body.
- `crates/core/src/commands.rs:404-440` builds the merged registry, first name
  wins, maximum 100 commands. Builtin dispatch precedes custom rows
  (`crates/core/src/router.rs:846-931`): `/time` could shadow a custom command.
- `crates/core/src/router.rs:773-789` applies guild, feature and permission
  checks. `crates/bot/src/activation.rs:87-108` only narrows configured gates.
  Do not give the helper `/help`'s always-on activation exception.
- `crates/discord/src/interactions.rs:113-128` supports ephemeral text replies;
  `crates/discord/src/message_safety.rs:14-20` suppresses mentions. The
  [reply lifecycle](interaction-replies.md) fixes privacy at ACK, not at edit.
- Shared [admission](automation-admission.md) is bounded concurrency, not a
  per-minute cooldown (`crates/bot/src/interaction_admission.rs:27-32`).
  Schedule option decoding rejects duplicate recognized fields but ignores
  unknowns (`crates/bot/src/schedule_runtime.rs:814-861`); the stricter policy
  below must be tested, not assumed from reuse.

No runtime, guild, database or deployment was inspected for this design.
[Command publication](command-publish.md) replaces the entire guild registry;
publishing is not handler wiring or activation approval.

## 3. Conditional helper: input and arithmetic

Proposed syntax: `/time at:<instant> style:<code>`. `at` is required; omitted
`style` means `f`. Both are flat string options. No subcommands, autocomplete,
buttons, modal, attachment, prefix form or free-text body.

1. Apply existing configured-guild, activation-constrained **automations** gate
   and runtime permission checks before parsing. Proposed permission is Open
   (self-service to a member invoking in the configured guild, no manager role),
   not a bypass for DM, foreign-guild or disabled-feature traffic. Missing or
   foreign guild is ignored as today; other denials stay private. A build must
   wire this placement explicitly, not broaden live capability clearance.
2. Decode the actual option list, not a lossy map: at most two entries; only
   `at` and `style`; no duplicates, nested values, non-string types or extras
   (including `public`, `channel`, `send`, `dry-run`, timezone and scheduling
   flags). Structural failures are `INVALID_OPTIONS`. Missing `at` is
   `MISSING_AT`. Order of entries does not affect the result.
3. `at` is 1–64 ASCII bytes, with no trimming or Unicode normalization. Accept
   the strict RFC3339 profile `YYYY-MM-DDTHH:MM:SS[.1-9 digits](Z|±HH:MM)`:
   uppercase `T`/`Z`, four-digit year 0001–9999, Gregorian valid dates, hours
   00–23, minutes/seconds 00–59, offsets up to ±23:59. No spaces, local zone
   names, natural language, epoch-number input, date-only or leap second `:60`.
   A syntactically complete local datetime without an offset gets
   `OFFSET_REQUIRED`; `-00:00` gets `OFFSET_UNKNOWN` (unknown is not UTC).
   `Z` and `+00:00` are equivalent. Other invalid syntax/date/length is
   `INVALID_INSTANT`. Never infer locale, DST or a stored profile timezone.
4. Convert with checked integer arithmetic. The **unrounded UTC instant** must
   be in `[1970-01-01T00:00:00Z, 10000-01-01T00:00:00Z)`; UTC normalization
   overflow, earlier instants (including epoch minus 0.1 second), and UTC year
   10000 refuse as `UNSUPPORTED_INSTANT`. This is a proposed TWO bound, not a
   claimed Discord client limit. Output seconds are 0–253402300799 inclusive.
   Drop fractional seconds by flooring the accepted nonnegative instant;
   never round to nearest or round up. No floating-point conversion. Canonical
   UTC is the **same rounded instant as the token**, `YYYY-MM-DDTHH:MM:SSZ`.
   Say “Fractional seconds discarded” if any fractional digits were supplied,
   including all-zero fractions. Equivalent offsets produce identical UTC,
   epoch seconds and tokens for the same style.
5. Validate `style`: exactly one case-sensitive ASCII character from this table;
   empty, multiple characters, whitespace or anything else is `INVALID_STYLE`.
   Input-time errors precede style errors; use this fixed validation order.

| Code | Meaning | Literal example (seconds = 1800018000) |
| --- | --- | --- |
| `t` | Short time | `<t:1800018000:t>` |
| `T` | Time with seconds | `<t:1800018000:T>` |
| `d` | Short date | `<t:1800018000:d>` |
| `D` | Long date | `<t:1800018000:D>` |
| `f` | Date and short time (default) | `<t:1800018000:f>` |
| `F` | Full date and short time | `<t:1800018000:F>` |
| `R` | Relative time, changes as the viewer's clock advances | `<t:1800018000:R>` |

Offsets identify an instant, not a timezone rule. The repeated wall-clock time
`2026-11-01T01:30:00-04:00` differs by one hour from `...-05:00`; both are valid.
The helper does not decide whether a numeric offset matches a named DST zone.

## 4. Private result, no-effect guarantees and bounded delivery

For `2027-01-15T18:30:00+05:30`, default style, the proposed private text is:

````text
UTC: 2027-01-15T13:00:00Z
Preview: <t:1800018000:f>
Copy: `<t:1800018000:f>`
Nothing saved or scheduled. This is a time preview, not an event invitation or a delivery promise.
````

The unescaped preview tag is rendered by the **Discord client**, never by an
external service. The literal token is wrapped in inline code, so it remains
copyable as `<t:seconds:style>` without Markdown delimiters. Build both solely
from validated integer seconds and the allowlisted character; never interpolate
raw input, option names, labels, mentions or parser error text. No HTML entities
in the token, no code-fence injection, no embeds or link previews. Every accepted
style shows explicit canonical UTC even if its preview omits date or seconds.
Client locale, timezone, clock and supported date rendering vary; UTC and the
literal remain authoritative if a preview is unavailable. Do not promise a
particular localized phrase or that someone has been invited/reminded.

All success, correction, busy and internal-error replies must be ephemeral at
initial ACK, mention-disabled (`parse/roles/users` empty, `replied_user=false`),
plain text and at most 1,000 ASCII bytes. Use the existing two-second ACK-start
budget and shared lifecycle; no public fallback, DM, delayed public follow-up
or channel-send retry. If the private callback fails, report only a bounded
transport outcome internally; do not claim delivery or try a public channel.

The pure converter has **zero** DB reads/writes, audit writes, HTTP/DNS calls,
channel operations, event/calendar reads, queue operations or profile state.
The runtime may send the one ephemeral Discord interaction response through
existing transport; that required callback is the only network effect, not an
external conversion fetch. Existing generic admission/lifecycle telemetry can
remain, but must not log raw input, canonical UTC, tokens, user content or
create a new audit record. Do not invoke the schedule runtime or store.

Preserve existing per-member in-flight limits (3), ordinary interaction slots
(16), privileged reserved slots (8; **not available to this Open command**),
busy-reply slots (8), transport rate limits and backoff. No new persistent or
per-minute rate-limit state. Busy may be privately refused or dropped if its
reply lane is full; it is not a delivered-result promise. Work per invocation
is constant bounded input/formatting, no tasks or unbounded retries.
Repeated calls are idempotent/no-effect conversions; relative previews may
change visually. There is no separate apply or dry-run mode: every invocation
is already a no-effect preview, and forged mode flags refuse.

## 5. Stable future agent check and corrections

**Proposed contract, not an executable CLI/API today.** A future offline check
must exercise the same pure converter as Discord, with an array preserving
option multiplicity. It accepts synthetic fixtures, no credentials, DB or
network; `--help` must link this file. The Discord help entry, if selected for
build, must use the same option/style definitions and say private/no schedule.
Do not maintain a separate parser in a CLI or duplicate the allowlist in help.

Versioned success example (the future check adds fixture id and exact code
SHA; no generated time, user ID, guild ID or raw input echo):

```json
{"schema":"two.time.check.v1","fixture_id":"offset-eq","code_sha":"<40-hex>","outcome":"ok","code":"OK","retryable":false,"next_action":"copy_token","canonical_utc":"2027-01-15T13:00:00Z","epoch_seconds":1800018000,"style":"f","token":"<t:1800018000:f>","fraction_discarded":false,"effects":{"db_reads":0,"db_writes":0,"audit_writes":0,"external_requests":0,"channel_sends":0}}
```

Failure keeps `schema`, `fixture_id`, `code_sha`, `outcome` (`refused` or
`unavailable`), `code`, `retryable`, `next_action`, and effects; it **omits**
canonical UTC, seconds, style and token. All human corrections are fixed text,
never a rejected value. These codes/actions are closed for v1:

| Code | Private correction / meaning | retryable | next_action |
| --- | --- | --- | --- |
| `MISSING_AT` | Supply at, e.g. 2027-01-15T13:00:00Z | false | `supply_at` |
| `INVALID_OPTIONS` | Use at once and optionally style once; remove other options | false | `fix_options` |
| `OFFSET_REQUIRED` | Add Z for UTC or a numeric offset such as +05:30 | false | `add_offset` |
| `OFFSET_UNKNOWN` | Replace unknown -00:00 with the actual offset or Z | false | `specify_known_offset` |
| `INVALID_INSTANT` | Use a real RFC3339 date/time with seconds and explicit offset, at most 64 ASCII bytes | false | `fix_instant` |
| `UNSUPPORTED_INSTANT` | Choose a UTC instant from 1970 through 9999; negative timestamps are unsupported | false | `choose_supported_instant` |
| `INVALID_STYLE` | Choose t, T, d, D, f, F or R; omit for f | false | `choose_style` |
| `DISABLED` | Helper unavailable in this guild; nothing saved | false | `use_manual_timestamp` |
| `FORBIDDEN` | Invocation not permitted; no conversion result | false | `use_allowed_guild_context` |
| `BUSY` | Busy; nothing saved. Try again later | true | `retry_later` |
| `INTERNAL` | Preview unavailable; nothing saved | true | `retry_later` |

Offline converter exits 0 for success, 2 for validation refusal, 1 for internal
failure; a fixture-suite runner exits 0 only when every expected case matches.
Runtime fences are adapter tests, not a reason to make the converter query
permissions. Existing ignored foreign-guild behavior need not send a correction.

## 6. Offline fixture recipe and required acceptance matrix

The following recipe is runnable **now** from the repo root, stdlib only. It
checks this document's arithmetic and all seven token shapes; it does **not**
implement `/time`, validate rejected inputs or prove private runtime behavior.
A future build must turn the matrix below into adapter/converter tests, and
register any new test target in existing CI. No live messages or DB needed.

```sh
set -e
python3 - <<'PY'
from datetime import datetime, timezone
from pathlib import Path
import re
text = Path('docs/time-helper-spec.md').read_text()
rows = [
    ('epoch', '1970-01-01T00:00:00Z', 0, '1970-01-01T00:00:00Z'),
    ('utc', '2027-01-15T13:00:00Z', 1800018000, '2027-01-15T13:00:00Z'),
    ('offset-eq', '2027-01-15T18:30:00+05:30', 1800018000, '2027-01-15T13:00:00Z'),
    ('fraction', '2027-01-15T13:00:00.999999Z', 1800018000, '2027-01-15T13:00:00Z'),
    ('dst-a', '2026-11-01T01:30:00-04:00', 1793511000, '2026-11-01T05:30:00Z'),
    ('dst-b', '2026-11-01T01:30:00-05:00', 1793514600, '2026-11-01T06:30:00Z'),
    ('upper', '9999-12-31T23:59:59Z', 253402300799, '9999-12-31T23:59:59Z'),
]
epoch = datetime(1970, 1, 1, tzinfo=timezone.utc)
for case, raw, seconds, canonical in rows:
    utc = datetime.fromisoformat(raw.replace('Z', '+00:00')).astimezone(timezone.utc)
    delta = utc - epoch
    assert delta.days * 86400 + delta.seconds == seconds, case
    assert utc.replace(microsecond=0).isoformat().replace('+00:00', 'Z') == canonical, case
    for style in 'tTdDfFR':
        token = f'<t:{seconds}:{style}>'
        assert re.fullmatch(r'<t:[0-9]{1,12}:[tTdDfFR]>', token), case
assert set(re.findall(r'^\| `([tTdDfFR])` \|', text, re.M)) == set('tTdDfFR')
print('PASS: 7 arithmetic fixtures, 49 token shapes, 7 documented styles; runtime not tested')
PY
python3 scripts/check_docs_index.py --json
```

| Stable fixture IDs | Future assertions (not run in this proposal) |
| --- | --- |
| `style-t/T/d/D/f/F/R`, `default-f`, `offset-eq` | All seven styles; omitted default; same instant via Z, +00:00, +05:30 and negative offset gives byte-identical UTC/token |
| `dst-a/b`, `past`, `beyond-year` | Explicit repeated DST hour differs by 3600 seconds; past 2000 and future 2040 accepted regardless of current date |
| `fraction-1/9/zero`, `fraction-too-long` | .1, .999999999 and .000 floor, flag true; tenth fractional digit invalid; UTC/token match |
| `epoch`, `negative`, `negative-fraction`, `upper`, `utc-overflow` | Zero accepted; 1969-12-31T23:59:59Z and ...59.9Z unsupported; upper fraction accepted; 9999-12-31T23:59:59-00:01 unsupported |
| `offset-missing/unknown`, `leap-good/bad`, `leap-second` | Missing offset corrected; -00:00 rejected; 2028-02-29 accepted, 2027-02-29 invalid, :60 invalid |
| `missing`, `duplicate-at/style`, `unknown`, `wrong-type`, `nested`, `oversize`, `unicode`, `style-invalid` | Structural error before semantic error; no lossy-map duplicate acceptance; 65-byte/non-ASCII input invalid; s/S, q, empty and forged mention/code-fence styles invalid |
| `copy-safe`, `repeat`, `transport-fail` | Exact literal with no entities; private rendered tag outside code and literal inside; no mentions/raw echo; repeat deterministic; callback failure never falls back to a public send |
| `dm`, `foreign`, `disabled`, `activation-denied`, `no-member`, `busy` | Existing router behavior preserved, no privileged lane consumption or widening; denied result has no token; no conversion before applicable gates |
| `no-effects`, `collision`, `capacity` | Spies assert zero DB/network/channel/queue effects except one private callback; custom /time collision refuses rollout; 100-command union does not silently drop/replace a command |

For future runtime evidence, count the private callback separately from
`external_requests` (external conversion/dependency calls) and `channel_sends`.
A JSON effects claim is not proof: require fail-on-use spies and captured ACK
flags/allowed_mentions for success and every refusal, including busy/errors.
Visual localized rendering remains unverified until approved staging acceptance.

## 7. Exact-spec intent gate; no automatic build

Keep two independent records:

1. **Design/no-build acceptance:** bind the exact file commit, SHA-256 and review
   revision. Acceptance may retain manual HammerTime/no-build without attesting
   that a community host was consulted. It authorizes no implementation.
2. **Factual host alignment:** actual community-host intent is **not recorded**
   in this proposal. Record an authorized host's intent source, date and chosen
   alternative separately; a design acceptance cannot manufacture that evidence.

Before any build card, require both exact-spec scope acceptance and the CEO's
intent review with the community host, selecting **help-only** or **helper**.
An author cannot self-approve. Those alternatives need separate bounded scope
acceptance; retaining **no-build** does not satisfy either build prerequisite.
Default if unresolved at the 2026-10-31 review: retain manual HammerTime/no-build,
no activation; expiry is not consent. Roadmap priority, code review and a merged
design are not demand/alignment. No help/helper implementation, build card,
spend or publication is authorized by design/no-build acceptance alone.

This proposal changes no security boundary; **no CISO approval is claimed or
required for the docs-only change**. If the chosen design later introduces a
new boundary (external service, persistence, wider permission/activation,
public send or event-data access), stop and obtain the actual CISO disposition
against that changed contract, not a boilerplate approval request here.

If helper is selected, before publication: inspect the full enabled registry
including custom commands for `/time` collision and the 100-command ceiling;
refuse publication on a collision, never rename/drop custom content silently.
Bind parser, permission, router, feature/activation and single runtime owner;
update generated command/help references and every registry/count pin. Stage
only after exact-head tests, CI and independent review, with a whole-registry
dry-run/diff, health checks and rollback. No production tests or migrations.
Rollback uses the approved prior registry/artifact without deleting data.
No build tasks, staging sends or publication are authorized by this document.
