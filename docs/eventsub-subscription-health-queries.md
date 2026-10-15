# EventSub subscription-health query pack (on-call)

Standalone on-call pack: copy-paste queries that answer the four subscription
questions from telemetry that already ships. It installs no monitor, alert
rule, scrape job or dashboard, provisions no secret, and mutates nothing:
every query is read-only. When this file and a live rule disagree, the live
rule wins and this file is the one to fix.

Scope: Twitch `stream.online` webhook subscriptions only (transport decision:
[stream funnel](stream-funnel.md#transport-webhook)). Message vocabulary is
pinned by [the observability catalog](observability-event-catalog.md) and the
`eventsub_receiver_emissions_match_the_catalog` conformance test. Remediation
(change the callback, re-provision secrets, restart the container) is Operator
work and lives outside this pack.

Rules for every query: read-only; never print secret values, tokens, header
values, challenge text, body bytes, callback URLs with secrets, or member
data. A zero count means "not observed in this window", never "proven quiet":
the Worker ingress logs only refusals, the receiver logs only what it saw,
and Workers Observability retains 7 days.

## Step 0 — point the queries at staging

Two datasets, two service names:

| Signal | Dataset | Staging service |
| --- | --- | --- |
| Worker edge refusals (`eventsub_ingress`) | `cloudflare-workers` | `two-bot-next-staging` |
| Rust receiver + reconcile (`eventsub_receiver`, `eventsub_reconcile`) | `containers` | the staging container application id (UUID) |

Resolve the container id with the runbook's read-only command (needs a token
with containers scope; the least-privilege agent token does not have it):

```bash
npm --prefix wrangler run containers -- list --env staging
```

At pack writing (2026-10-15) the staging id was
`a038b0ad-a1f9-4966-bfb7-f992c602d938`, confirmed two ways: it booted
(`listening`, `two_bot::server`) at 14:16:16Z inside the day's green staging
deploy window, and it logged zero `durable gateway failed` lines in the
15:45–16:15Z window where the production container logged 10. Re-check with
`containers list` before trusting the id: ids change on re-provision, and
telemetry retention is 7 days.

The `containers` queries below use
`scripts/container_stdout_read.py` (read-only; secret-redacting; newest 100
lines per call; `--minutes` caps at 30, page older windows with `--end
2026-10-15T16:15:00Z`). That script keeps the message plus a fixed field
allowlist (`error_class`, `missing`, `job`, …) and drops `status` and the
reconcile count fields (`broadcasters`, `deleted`, `created`,
`already_exists`): for those, use the dashboard Query Builder over the same
dataset and filters. The `cloudflare-workers` signal is dashboard-only in
this pack: the container reader is fixed to `containers` and cannot query the
Worker dataset. In Workers Observability Query Builder, select dataset
`cloudflare-workers`, service `two-bot-next-staging`, and filter the scalar
`event` field to include `eventsub_ingress`; inspect `status` and
`error_class`. The Worker logs only scalar `event`/`status`/`error_class`, so
its lines are safe to paste.

## Q1 — are subscriptions healthy?

No log line proves healthy. The authoritative check is the Helix listing
(Operator-run, GET-only, never a mutation):

```bash
curl --silent --show-error --max-time 15 \
  -H "Client-Id: ${TWO_TWITCH_CLIENT_ID}" \
  -H "Authorization: Bearer ${APP_TOKEN}" \
  "https://api.twitch.tv/helix/eventsub/subscriptions?type=stream.online"
```

Shape contract (pinned by the `listing_url_carries_exactly_one_filter` unit
test in `crates/core/src/twitch_helix.rs`): exactly one filter — Twitch
answers 400 to more — and follow `pagination.cursor` until it is empty. For
every desired broadcaster there must be an entry with `type:
"stream.online"`, `version: "1"`, `transport.method: "webhook"`, and
`transport.callback` equal to the bot's callback. In steady state, only
`status: "enabled"` is healthy. A newly created subscription may be
`webhook_callback_verification_pending` for up to five minutes only while its
challenge is actively in flight; use `created_at` to measure that age. A
pending entry at or beyond five minutes is unhealthy and needs the Q4
challenge-failure investigation. The five-minute cutoff is an operational
triage threshold, not a Twitch guarantee. Any other status, or a missing
entry, is unhealthy. This command was not executed while writing this pack
(no Twitch credentials on the writing run); its URL shape is the tested one
above.

The log proxy (executed below) covers the negative direction: in a healthy
window there are no `revocation acknowledged` lines (Q3), no
`reconcile_unhealthy` / `reconcile_failed` lines (Q2), and no `delivery
refused` bursts (Q4). Absence is not proof — a silently dropped subscription
(a deleted callback, an expired Operator secret) emits nothing until Twitch
revokes or the next reconcile lists it.

Sample (staging container, `delivery refused` sweep, 30 min):

```bash
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 30 --contains "delivery refused"
```

```json
{"event_count": 0, "window": {"start": "2026-10-15T19:43:48.000Z", "end": "2026-10-15T20:13:48.000Z"}}
```

Calibration: the same service emitted 32 unrelated lines
(`two_bot::website_jobs`) in the surrounding 30 minutes, so the window was
live and the zero is a true negative, not an outage. The staging Worker
likewise logged zero `eventsub_ingress` lines in 30 minutes while serving
routine probe traffic.

## Q2 — last reconcile outcome

Reconcile runs at startup and hourly behind `TWO_EVENTSUB=1` (PR #989,
unmerged at pack writing): list, diff deletes-before-creates on the bot's
own callback, execute, then confirm with a follow-up listing. Every pass
logs one scalar line with `event = "eventsub_reconcile"`:

| Line | Meaning |
| --- | --- |
| `eventsub subscriptions reconciled`, `status="healthy"` (+ `deleted` / `created` / `already_exists` counts on the dashboard; the read script drops count fields) | last pass fully healthy |
| `eventsub reconcile follow-up listing is missing a healthy subscription`, `error_class="reconcile_unhealthy"` (+ `missing`) | deletes/creates ran but a broadcaster still has no healthy subscription — the unhealthy set |
| `eventsub reconcile failed`, `error_class="reconcile_failed"` | the pass itself errored (list, plan or execute) |
| `eventsub reconcile parked …`, `error_class="reconcile_no_broadcasters"` (warn) | allowlist empty, nothing mutated |
| `eventsub reconcile refused …`, `error_class="reconcile_config_invalid"` (error) | callback, credentials or secret invalid; needs the Operator |

The newest `eventsub_reconcile` line in the window is the last outcome; a
`healthy` line after failures is the recovery. Until PR #989 deploys to
staging there is no scheduled reconcile at all, so this query is the canary
for the rollout itself.

The container reader's `--contains` option checks message text; it does not
filter the flattened `event` field, and the script omits that field from its
output. For the complete event-level view, use Workers Observability Query
Builder on dataset `containers`, select this service id, filter
`event = "eventsub_reconcile"`, and display `message`, `status`, and
`error_class`. The CLI's message filters are two separate read-only queries:

```bash
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 30 --contains "eventsub reconcile"
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 30 --contains "eventsub subscriptions reconciled"
```

The first matches failure and refusal messages; the second matches the
successful `eventsub subscriptions reconciled` message. For example, the
literal `eventsub subscriptions reconciled` matches the second filter; this
is an illustrative match from the reconcile log contract, not an observed
staging event. The live staging read for 2026-10-15 21:41–22:11Z returned
zero lines for both filters, so it cannot provide a truthful positive sample.
Expected while the reconcile slice is undeployed. After it deploys, expect
one `healthy` line per hour per container boot plus one per startup; a gap
with `TWO_EVENTSUB=1` set means the task is not running and is itself the
finding.

## Q3 — revocation count (total today; by reason needs a logging slice)

Each verified revocation logs one `revocation acknowledged` info line
(`crates/bot/src/eventsub_http.rs`, `event = "eventsub_receiver"`). Count
them for the total:

Sample (staging container, 30 min, 2026-10-15):

```bash
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 30 --contains "revocation acknowledged"
```

```json
{"event_count": 0, "window": {"start": "2026-10-15T19:43:51.000Z", "end": "2026-10-15T20:13:52.000Z"}}
```

By reason is not answerable from logs today: the ack line drops
Twitch's `status` (`user_removed`, `authorization_revoked`,
`notification_failures_exceeded`, `version_removed`, other), and the
reconcile only runs at boot/hourly, so a reason breakdown needs a
receiver-logging slice that records the reason as one bounded scalar (no
header, body or secret ever leaves the process). Until that lands, get the
current-state reasons from the Q1 Helix listing (`status` per entry), which
is a snapshot, not a count.

## Q4 — challenge-failure count (approximate today)

A good challenge logs `challenge answered` (echoed as `200 text/plain`;
Twitch then enables the subscription). A bad one is refused before any
announcement path:

Sample (staging container, 30 min, 2026-10-15):

```bash
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 30 --contains "challenge answered"
```

```json
{"event_count": 0, "window": {"start": "2026-10-15T19:43:50.000Z", "end": "2026-10-15T20:13:50.000Z"}}
```

Failure side: `eventsub delivery refused` warn lines carry `error_class`
(`unauthorized` = missing/mismatched signature or headers; `malformed` =
bad query, empty body, or an invalid challenge; `unsupported_media_type`,
`payload_too_large`, `request_timeout`, `method_not_allowed`, `not_found`,
`busy` / `request_timeout` / `upstream_timeout` for the three retryable
stalls). The Worker edge
adds its own `eventsub_ingress` refusals with the same vocabulary plus
`rate_limited` (429) and `unavailable` (503: DO/binding/relay failures)
(`wrangler/src/eventsub.ts`):

Dashboard-only sample (not a copy-paste command): in Workers Observability,
select dataset `cloudflare-workers`, service `two-bot-next-staging`, and the
`event` filter `includes "eventsub_ingress"`; the recorded 30-minute sample
on 2026-10-15 had count 0. The `containers` reader cannot reproduce this
Worker-dataset query.

The count is approximate: refusal lines carry no message-type field, so a
failed challenge (`InvalidChallenge` → `malformed`) cannot be separated from
a malformed notification or a garbage probe. Treat `malformed` + `401
unauthorized` refusals during a subscription setup as suspected
challenge failures, and confirm against the subscription's Helix status
(`webhook_callback_verification_pending` stuck, or
`webhook_callback_verification_failed` after a reconcile). Exact
per-type attribution needs the same logging slice as Q3 (one bounded
message-type scalar on the refuse paths, or a challenge-specific
error class).

Remaining receiver lines for completeness: `notification acknowledged`
(verified go-live acked; same `--contains "acknowledged"` query covers it —
0 in the sampled window), `delivery ignored` (unknown type or deterministic
planning error, acked 2xx so Twitch does not retry — 0 in the sampled
window), and `eventsub startup refused` (fatal config/bind failure, exits
for supervisor restart — 0 in the sampled window).
