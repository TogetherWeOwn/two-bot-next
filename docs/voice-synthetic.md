# Staging voice synthetic

An automated test bot verifies the temp-voice room lifecycle on the deployed
staging bot, so nobody has to join a voice channel to check a voice change.

## What it checks

`scripts/staging_voice_synthetic.py` connects a dedicated test bot to the
Discord gateway, joins the staging **Create a Lobby** creator (the only
creator) and asserts:

| Check | Pass condition |
|---|---|
| create | a new room appears in the creator's category |
| moved | the test bot lands in that new room after joining the creator |
| name | the room is created already carrying a template name, not the `<display>'s room` fallback |
| position | the room is directly below its creator, or below the creator's existing rooms still open when the slot is read — a predecessor the empty-room sweep deleted mid-run does not anchor the slot (matching the placement planner) |
| owner | the room has a member overwrite for the joiner allowing View, Connect and Manage Channels |
| status | a voice-channel status line is set on the room |
| rejoin | the test bot hops from that room straight back into the creator, and a second new room appears and it is moved into that room (a member must never stay in the lobby) |
| delete | after the test bot leaves, every room it was moved into (the first room and the rejoin room) is deleted within the empty grace |
| rename | the creator is renamed before the join and restored after; the post-rename join must still move the test bot into a new room (SKIP while the test bot may not rename — see below) |
| limit | the room's `user_limit` PATCHes to a probed value and back, each write verified by read-back (FAIL on 403, SKIP on 429) |
| privacy | read-only checks confirm the room's @everyone overwrite does not deny Connect (the public baseline: a Connect deny is the `/private` marker; an inherited Connect allow stays public), the staging product bot has effective View Channel and Manage Roles (the `/private` overwrite precondition), and no member overwrite grants Manage Roles (FAIL on any mismatch or read error) |
| name_panel | the room's name after the rename cycle still renders from its template, not the fallback (what the `/name` panel's Current-name line shows) |
| metrics | optional: `two_bot_voice_names_total{outcome="created_with_template"}` grows (SKIP without a metrics token) |

A failed `create` names the refusal reason: the run snapshots
`two_bot_voice_joins_received_total` and per-reason
`two_bot_voice_join_refused_total` before and after the join, and the create
detail reports the deltas (for example `joins received=2,
unknown_creator=2`), so the next restart-cured stall names its reason instead
of just `room=None`. Without a metrics token there is no suffix.

The script refuses (exit 2, no request sent) unless the guild is TWO Staging
and the creator is the staging Create a Lobby channel.

## Response size cap

The final HTTP response body the script reads is bounded at `MAX_BODY_BYTES`
(64 KiB, the same value as `scripts/staging_probe.py`). That covers the Discord
REST reads (`rest`) and the staging `/ops/metrics` read (`metrics_value`), on
success and on HTTP error status alike. Intermediate redirect bodies are drained
by urllib's redirect handler before the final response and are not capped,
unlike `scripts/staging_probe.py`, which never follows redirects. The gateway
WebSocket is capped at the same value on three levels: the handshake response
head, every frame payload and every reassembled (continuation-joined)
message. The declared frame length is validated before the payload is read,
so a huge declared length refuses without an unbounded read, and a
continuation refuses before its bytes are read once the running total would
pass the cap.

- Each read takes at most `MAX_BODY_BYTES + 1` bytes, so one extra byte exposes
  a hidden suffix without buffering an unbounded body.
- A body, frame or message of exactly 64 KiB passes. Anything longer is refused
  before it is decoded or parsed, so the extra byte never reaches a successful
  JSON path.
- Control frames (ping, pong, close) carry at most 125 payload bytes per
  RFC 6455; a longer control payload refuses as a malformed frame before the
  payload is read.
- The refusal is a `BodyTooLarge` error with a fixed message: `Discord REST read
  answered over the body cap`, `metrics read answered over the body cap`,
  `gateway handshake answered over the byte cap`, `gateway frame answered over
  the byte cap` or `gateway message answered over the byte cap`. It never
  echoes the body, headers, URL or a parse exception.
- A refused read fails the run: the `run` check reports `FAIL` with that fixed
  message, the evidence JSON still uploads, and the script exits 1. The
  workflow's retries apply as for any other failure.
- A metrics read that answers an in-cap error status stays optional (`metrics`
  reports SKIP). Only an over-cap body is a failure.

A malformed frame (a control payload over 125 bytes), a malformed hello
(missing or non-object `d`, missing or invalid `heartbeat_interval` —
non-numeric, non-finite, non-positive or above the 600 s timing bound —
close-before-hello) or a malformed dispatch (missing `t`/`d`, READY without
a string `user.id`, `GUILD_CREATE`/`CHANNEL_DELETE` without a string `d.id`)
raises `GatewayShapeError` with the fixed message `gateway frame was
malformed`, `gateway hello was malformed` or `gateway dispatch was
malformed`, and fails the run the same way: a `run` FAIL with that message,
evidence uploaded, exit 1. No traceback reaches the output and no wire bytes
are echoed. Valid frames, hellos and dispatches behave byte-identically.

The offline suites (`scripts/test_staging_voice_synthetic.py`,
`scripts/test_staging_voice_synthetic_gateway_intake_acceptance.py` and
`scripts/test_staging_voice_synthetic_owned_room_acceptance.py`) pin the
caps, the exact-cap passes, the cap-plus-one refusals on every path and the
fixed messages.

## When it runs

- After every `deploy-staging` run (job `voice-synthetic`). A failure fails the
  staging deploy, and `deploy-production` only promotes a commit whose latest
  `deploy-staging` run succeeded, so a broken voice lifecycle cannot ship.
- Every 6 hours (`voice-synthetic.yml` schedule) to catch drift. A failed run on
  main is re-run once by the host flake ledger and opens a `[main-red]` card if
  it fails again.
- On pull requests only the offline suites (`scripts/test_staging_voice_synthetic.py`,
  `scripts/test_staging_voice_synthetic_gateway_intake_acceptance.py` and
  `scripts/test_staging_voice_synthetic_owned_room_acceptance.py`)
  run, and only when voice inputs change (`scripts/job-inputs.py` `voice`). A
  PR's code is not deployed to staging before merge, so a live run would test
  main, not the PR.

Each live run retries twice after 60 s; three failures fail it. The longer
window covers the documented 95-139 s per-deploy gateway drop as well as a
transient gateway/REST hiccup. Evidence JSON is
uploaded as the `voice-synthetic-<run id>` artifact for 14 days.

## Identity

The test bot is a separate Discord application, **TWO Voice Synthetic
(staging)**, invited only to the TWO Staging guild with View Channels, Connect
and View Audit Log. It is never the bot under test.

Discord marks it as a bot, and bots never count as room occupants. The staging
Worker therefore sets `TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS` to its user ID, so
the voice runtime treats it as a human: it can own a room and empty it by
leaving. The setting is environment-only (never dashboard-stored), takes at most
four IDs, refuses malformed values, and is ignored for the live guild by code.
Production never sets it.

Bindings:

| Where | Name | Value |
|---|---|---|
| GitHub environment `voice-synthetic` (main only, no reviewer) | secret `TWO_VOICE_SYNTHETIC_BOT_TOKEN` | the test bot token |
| GitHub repository variable (optional) | `VOICE_SYNTHETIC_REQUIRED` | `false` only while the test bot is being provisioned; required by default |
| GitHub repository variable (optional) | `STAGING_EMPTY_GRACE_SECONDS` | staging empty grace, default 60 (every timing input — `--create-within`, `--settle`, `--grace`, `--delete-slack`, flag or env — must be finite, above 0 and at most 600 s, else the run refuses with exit 2 before any live call) |
| Staging Worker setting | `TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS` | the test bot's user ID |

A missing token fails the run: the voice gate is required by default, so an
unconfigured gate never reads as green. Set `VOICE_SYNTHETIC_REQUIRED=false`
to restore the old "not configured" notice-and-pass while the test bot is
being provisioned.

## Rename case (one-time staging role grant)

The `rename` check renames the staging creator to `<name> (synthetic)`,
joins it, and restores the original name in a `finally` block, so a renamed
creator that stops moving joins fails the gate. Renaming needs Manage
Channels: grant the test bot's staging role Manage Channels once (staging
guild only; the test bot lives nowhere else). Until then the check reports
SKIP with that detail and every other check still runs. A rename PATCH
answered with HTTP 429 (Discord allows 2 channel renames per 10 minutes)
also reports SKIP: the workflow retries after 60 seconds and dispatches can
collide within 10 minutes, and neither must fail the run. A name already
carrying ` (synthetic)` means a previous run did not restore: the rename is
skipped and the check FAILs naming the residue, so it is restored manually
instead of being renamed over. Restore runs whenever a rename was attempted
and the live name differs from the original, so a rename Discord applied but
whose response was lost is still restored.

## Owner-command checks (limit, privacy, name panel)

While the test bot owns its room, the synthetic exercises the same Discord
fields `/limit`, `/unlimit` and `/name` manage through the REST paths the bot
writes: `user_limit` PATCHes and a final read of the template-rendered room
name. Each write is verified by a read-back and restored, so a green run leaves
the room as the bot made it. Privacy is different: it performs only GETs and
checks the room's public @everyone Connect baseline, the staging product bot's
effective View Channel and Manage Roles permission (the `/private` overwrite
precondition), and that no member overwrite grants Manage
Roles. It does not edit or restore an overwrite.

What this proves — and what it does not: a PASS means the room the staging
bot created is owner-manageable for the tested limit/name operations, and its
privacy permissions match the read-only assertions. It does not invoke the
slash commands themselves: Discord creates interactions only for real users,
so a bot token cannot. The slash path (registration, gating, replies) stays
covered by the Rust test suite and the registry smoke.

Grants and SKIP conditions, mirroring the rename case:

- `limit` needs the owner Manage Channels grant the room already carries. A
  403 FAILs: the test bot owns its room, so the grant was lost or ineffective
  after creation (the `owner` check reads creation-time grants and cannot see
  this loss). A 429 SKIPs.
- `privacy` is read-only and requires no Manage Roles grant for the test bot.
  A mismatch or REST read error FAILs; privacy never SKIPs on 403 or 429. If
  the staging product bot lacks effective View Channel and Manage Roles, the
  failure names its staging role.
- `name_panel` is read-only and needs no grant: it FAILs when the
  post-rename room name is missing or the `<display>'s room` fallback.

## Owned-room binding and uncertain writes

Every room probe targets exactly the test bot's observed destination in the
staging guild, and only when a same-window `CHANNEL_CREATE` with that same
id, a voice type and the creator's category corroborates it. A concurrent
unrelated create, a foreign-guild event, a move into a pre-existing room or a
missing move refuses before any `PATCH`: `room_id` is `None`, `create` FAILs
and no room write is sent. The rejoin verdict needs the same corroboration
for its own id (neither the creator, the first room nor a pre-existing
channel).

An attempted `user_limit` write without a verified read-back (lost response,
failed read, mismatch, or a non-2xx status other than 403/429) enters one
bounded cleanup — a single restore `PATCH` plus one verification read — and
the `limit` check still FAILs with `room user_limit write was not verified`,
the room id, the original value and a next action (`re-run` when verified,
`restore ... manually` otherwise). A 403 still FAILs and a 429 still SKIPs,
both without a restore.

`scripts/test_staging_voice_synthetic_owned_room_acceptance.py` pins the
binding, the rejoin correlation, the cleanup bound and the fixed vocabulary
with offline fake-gateway/REST fixtures.

## Run it by hand

Actions, then **voice-synthetic**, then **Run workflow** on `main`. Locally (staging
only):

```sh
TWO_VOICE_SYNTHETIC_BOT_TOKEN=... python3 scripts/staging_voice_synthetic.py --evidence out.json
```
