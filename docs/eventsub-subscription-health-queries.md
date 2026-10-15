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
dataset and filters. The `cloudflare-workers` queries use the dashboard or
the same telemetry endpoint with `datasets: ["cloudflare-workers"]`; the
Worker logs only scalar `event`/`status`/`error_class`, so its lines are safe
to paste.

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
"stream.online"`, `version: "1"`, `transport.method: "webhook"`,
`transport.callback` equal to the bot's callback, and `status` either
`enabled` or `webhook_callback_verification_pending`. Any other status, or a
missing entry, is the unhealthy set. This command was not executed while
writing this pack (no Twitch credentials on the writing run); its URL shape
is the tested one above.

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

Sample (staging container, 30 min, 2026-10-15):

```bash
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 30 --contains "eventsub_reconcile"
```

```json
{"event_count": 0, "window": {"start": "2026-10-15T19:43:53.000Z", "end": "2026-10-15T20:13:53.000Z"}}
```

Expected while the reconcile slice is undeployed. After it deploys, expect
one `healthy` line per hour per container boot plus one per startup; a gap
with `TWO_EVENTSUB=1` set means the task is not running and is itself the
finding.

## Q3 — revocation count (total; by reason from the `reason` scalar)

Each verified revocation logs one `revocation acknowledged` info line
(`crates/bot/src/eventsub_http.rs`, `event = "eventsub_receiver"`) with a
bounded `reason` scalar: `user_removed`, `authorization_revoked`,
`notification_failures_exceeded`, `version_removed`, or `other` (unmapped
Twitch statuses collapse to `other`; no subscription id, header, body or
secret is logged). Count the lines for the total; group by `reason` for
the breakdown:

```bash
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 30 --contains "revocation acknowledged"
```

```bash
# Same output, grouped by reason (needs this slice deployed; re-check the
# container id with `containers list` first — ids change on re-provision).
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 30 --contains "revocation acknowledged" \
  | jq '[.events[].reason] | group_by(.) | map({reason: .[0], count: length})'
```

Sample (staging container, 30 min, 2026-10-15 — pre-dates the `reason`
scalar, so only the total is shown):

```json
{"event_count": 0, "window": {"start": "2026-10-15T19:43:51.000Z", "end": "2026-10-15T20:13:52.000Z"}}
```

The Q1 Helix listing (`status` per entry) remains the cross-check for
current-state reasons; it is a snapshot, not a count.

## Q4 — challenge-failure count (exact, from the `message_type` scalar)

A good challenge logs `challenge answered` (echoed as `200 text/plain`;
Twitch then enables the subscription). A bad one is refused before any
announcement path with `error_class = "malformed"` and
`message_type = "webhook_callback_verification"` — the bounded
`message_type` scalar (`webhook_callback_verification` / `notification` /
`revocation` / `unknown`, `unknown` before headers are established) separates
failed challenges from malformed notifications and edge probes in logs alone:

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
stalls) plus the `message_type` scalar on every line. The Worker edge
adds its own `eventsub_ingress` refusals with the same vocabulary plus
`rate_limited` (429) and `unavailable` (503: DO/binding/relay failures)
(`wrangler/src/eventsub.ts`):

Sample (staging Worker, 30 min, 2026-10-15): `eventsub_ingress` count 0.

Exact challenge-failure query (needs this slice deployed):

```bash
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 30 --contains "delivery refused" \
  | jq '[.events[] | select(.message_type == "webhook_callback_verification")] | length'
```

Sample (staging container, 30 min, 2026-10-15 — pre-dates the
`message_type` scalar): `eventsub_ingress` count 0, `delivery refused`
count 0. A stuck `webhook_callback_verification_pending` (or
`webhook_callback_verification_failed` after a reconcile) in the Q1 Helix
listing remains the cross-check for a subscription whose challenges never
succeed.

Remaining receiver lines for completeness: `notification acknowledged`
(verified go-live acked; same `--contains "acknowledged"` query covers it —
0 in the sampled window), `delivery ignored` (unknown type or deterministic
planning error, acked 2xx so Twitch does not retry — 0 in the sampled
window), and `eventsub startup refused` (fatal config/bind failure, exits
for supervisor restart — 0 in the sampled window).
