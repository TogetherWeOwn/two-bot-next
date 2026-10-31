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
and Cloudflare retention varies by dataset and plan; see the link below.

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
`containers list` before trusting the id: ids change on re-provision. Check the
retention guidance below before relying on older telemetry.

The `containers` queries below use
`scripts/container_stdout_read.py` (read-only; secret-redacting; up to 100
lines per call; `possibly_truncated: true` means the result may be incomplete,
so exact counts require narrower windows or smaller time slices. `--minutes` caps
at 30; page older windows with `--end 2026-10-15T16:15:00Z`). The script keeps the message plus a fixed field
allowlist (`error_class`, `missing`, `job`, …) and drops `status`, reconcile
counts (`broadcasters`, `deleted`, `created`, `already_exists`), and reconcile
details (`failed`, `prior_failed`, `action`, `error`). Use the dashboard Query
Builder over the same dataset and filters for those fields. The
`cloudflare-workers` signal is dashboard-only in
this pack: the container reader is fixed to `containers` and cannot query the
Worker dataset. In Workers Observability Query Builder, select dataset
`cloudflare-workers`, service `two-bot-next-staging`, and filter the scalar
`event` field to include `eventsub_ingress`; inspect `status` and
`error_class`. The Worker logs only scalar `event`/`status`/`error_class`, so
its lines are safe to paste. Retention varies by dataset and plan; check
[Cloudflare's dataset retention](https://developers.cloudflare.com/observability/logs/datasets/)
before querying older windows.

## Q1 — are subscriptions healthy?

No log line proves healthy. The authoritative check is the Helix listing
(Operator-run, GET-only, never a mutation). In an approved Operator shell, set
`TWO_TWITCH_CLIENT_ID`, `APP_TOKEN` (the app access token), `EXPECTED_CALLBACK`,
and `EXPECTED_BROADCASTERS_JSON` (a JSON array of expected broadcaster ids)
from their approved sources; do not print their values. The following Bash loop
requests pages until the cursor is empty (with a 100-page safety cap), compares
the result with the expected set, and prints aggregate status counts plus
pending creation times only. It does not print broadcaster ids/names or the
callback URL:

```bash
(
set -euo pipefail
after=""
pages=0
subscriptions='[]'
while :; do
  if (( pages >= 100 )) && [[ -n "$after" ]]; then
    printf '%s\n' 'Helix pagination exceeded 100 pages; no summary emitted' >&2
    exit 1
  fi
  pages=$((pages + 1))
  curl_args=(--silent --show-error --fail --max-time 15 --get -K - \
    --data-urlencode 'type=stream.online' --data-urlencode 'first=100')
  if [[ -n "$after" ]]; then
    curl_args+=(--data-urlencode "after=$after")
  fi
  page="$(printf 'header = "Client-Id: %s"\nheader = "Authorization: Bearer %s"\n' \
    "$TWO_TWITCH_CLIENT_ID" "$APP_TOKEN" |
    curl "${curl_args[@]}" \
      'https://api.twitch.tv/helix/eventsub/subscriptions')"
  subscriptions="$(printf '%s\n%s\n' "$subscriptions" "$page" |
    jq -s '.[0] + (.[1].data // [])')"
  after="$(printf '%s\n' "$page" | jq -r '.pagination.cursor // empty')"
  [[ -n "$after" ]] || break
done
printf '%s\n' "$subscriptions" |
  EXPECTED_CALLBACK="$EXPECTED_CALLBACK" \
  EXPECTED_BROADCASTERS_JSON="$EXPECTED_BROADCASTERS_JSON" jq '
    . as $subscriptions |
    ($ENV.EXPECTED_BROADCASTERS_JSON | fromjson) as $expected |
    ($ENV.EXPECTED_CALLBACK) as $callback |
    def matches($id):
      .condition.broadcaster_user_id == $id and
      .type == "stream.online" and .version == "1" and
      .transport.method == "webhook" and .transport.callback == $callback;
    {
      total_subscriptions: ($subscriptions | length),
      expected_broadcasters: ($expected | length),
      enabled_broadcasters: ([$subscriptions[] |
        select(.type == "stream.online" and .version == "1" and
               .transport.method == "webhook" and
               .transport.callback == $callback and .status == "enabled") |
        .condition.broadcaster_user_id] | unique | length),
      pending_broadcasters: ([$subscriptions[] |
        select(.type == "stream.online" and .version == "1" and
               .transport.method == "webhook" and
               .transport.callback == $callback and
               .status == "webhook_callback_verification_pending") |
        .condition.broadcaster_user_id] | unique | length),
      pending_created_at: [$subscriptions[] |
        select(.type == "stream.online" and .version == "1" and
               .transport.method == "webhook" and
               .transport.callback == $callback and
               .status == "webhook_callback_verification_pending") |
        .created_at],
      broadcasters_without_healthy_subscription: ([$expected[] as $id |
        select([$subscriptions[] | select(matches($id) and .status == "enabled")] | length == 0)] | length),
      status_counts: ([$subscriptions[].status] | sort | group_by(.) |
        map({status: .[0], count: length}))
    }'
)
```

The headers reach curl on stdin, so neither the token nor the client id is in
the process argument list. The expected ids and callback stay in the Operator
shell environment and are read by jq without appearing in its argument list.
The callback comparison is exact, including a trailing slash; set
`EXPECTED_CALLBACK` to the exact configured URL. `total_subscriptions` and
`status_counts` include all returned subscriptions, including other callbacks;
the `*_broadcasters` counts are scoped to the expected callback. Do not replace
the headers with inline `-H` flags.

Shape contract (pinned by the `listing_url_carries_exactly_one_filter` unit
test in `crates/core/src/twitch_helix.rs`): the implementation comment says
Twitch returns 400 for multiple listing filters; the unit test pins the
client-side URL builder's single-filter shape. This loop adds `first=100` and
the pagination cursor, but no second filter. It follows `pagination.cursor`
until empty, or fails without printing a summary after 100 pages. Every
expected broadcaster must have a `stream.online` version
`1` webhook subscription on the expected callback. In steady state, the
summary's `broadcasters_without_healthy_subscription` must be zero. A nonzero
count is acceptable only for broadcasters with a
`webhook_callback_verification_pending` status while that challenge is actively
in flight; use `pending_created_at` to check its age. A pending entry at or
beyond five minutes, or one without an active challenge, is unhealthy and needs
the Q4 challenge-failure investigation. The five-minute cutoff is an
operational triage threshold, not a Twitch guarantee. If the count is nonzero,
use the approved Operator workflow to identify affected broadcasters without
copying ids or names into logs or tickets. Any other status, or a missing
subscription, is unhealthy. This query was not executed while writing this
pack (no Twitch credentials on the writing run).

The log proxy (executed below) covers the negative direction: in a healthy
window there are no `revocation acknowledged` lines (Q3), no
`reconcile_prior_failed`, `reconcile_action_failed`,
`reconcile_unhealthy`, `reconcile_failed`, or `reconcile_pending` outcomes (Q2,
when reconciliation is enabled and the current implementation is deployed),
and no `delivery refused` bursts (Q4).
A `parked` line with `reason="flag_dark"` means reconcile is disabled; it is
not evidence of healthy subscriptions. Absence is not proof — a silently
dropped subscription (a deleted callback, an expired Operator secret) emits
nothing until Twitch revokes or the next reconcile lists it.

Sample (staging container, `delivery refused` sweep, 30 min):

```bash
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 30 --end 2026-10-15T20:13:48Z --contains "delivery refused"
```

Excerpted output (other envelope fields omitted):

```json
{"event_count":0,"possibly_truncated":false,"window":{"start":"2026-10-15T19:43:48.000Z","end":"2026-10-15T20:13:48.000Z"}}
```

Calibration: the same service emitted 32 unrelated lines
(`two_bot::website_jobs`) in the surrounding 30 minutes, so the container
window was live. The staging Worker likewise logged zero `eventsub_ingress`
lines in 30 minutes; the Worker success path is silent, so this does not prove
that the route served routine probe traffic.

## Q2 — last reconcile outcome

The current implementation runs once at startup and on a bounded hourly
interval when `TWO_EVENTSUB=1` ([stream funnel](stream-funnel.md#transport-webhook)).
A revocation is acknowledged by the receiver; the next reconcile pass heals it,
not an immediate revocation-triggered pass. Confirm that the staging image
contains the current implementation before using Q2; an older image will not
emit these receipts. Reconcile lists subscriptions, diffs deletes-before-creates
on the bot's own callback, executes the plan, then confirms with a follow-up
listing. The `--contains` option filters message text, not the structured
`event` field. The `eventsub reconcile` filter finds diagnostic/action-failure
messages; healthy and pending terminal messages require their own filters, as
shown below. The message plus retained `error_class` (and `missing` on some
lines) classify these lines. The reader drops `status`, counts and
reconcile detail fields (`failed`, `prior_failed`, `action`, `error`); use the
dashboard for them.

| Message | Meaning |
| --- | --- |
| `eventsub subscriptions reconciled`, `status="healthy"` (+ `deleted` / `created` / `already_exists` counts on the dashboard; the read script drops count fields) | last pass fully healthy |
| `eventsub reconcile did not converge: an action failed or a healthy subscription is missing`, `error_class="reconcile_unhealthy"` (+ `missing`) | an action failed or a broadcaster still has no healthy subscription — inspect dashboard fields for `failed` / `missing` / `prior_failed` |
| `eventsub reconcile action failed; continuing with the rest of the plan`, `error_class="reconcile_action_failed"` | a delete or create failed; `action` and `error` identify which one and why in the dashboard; later actions and follow-up listing still run |
| `eventsub reconcile found failed subscriptions on our callback; deleting and re-creating`, `error_class="reconcile_prior_failed"` | pre-pass failed entries were found and scheduled for replacement; `prior_failed` count is available in the dashboard |
| `eventsub reconcile failed`, `error_class="reconcile_failed"` | the pass itself errored (list, plan or execute) |
| `eventsub reconcile pass interrupted by shutdown; remaining actions skipped and subscription state unverified until the next startup pass`, `error_class="reconcile_shutdown_interrupted"` (warn) | shutdown landed mid-pass; `completed`/`remaining` dashboard counts say how far the plan got and the result was not verified — expected during deploys, the next startup pass re-checks |
| `eventsub reconcile task did not finish within the shutdown drain; …`, `error_class="reconcile_shutdown_drain_exceeded"` | the worker outlived its bounded shutdown drain and was aborted, so one in-flight mutation outcome is uncertain until the next startup pass re-checks — investigate if it repeats outside deploys |
| `eventsub reconcile task ended abnormally`, `error_class="reconcile_task_failed"` | the reconcile worker panicked or was cancelled (`panicked` dashboard field says which) — investigate |
| `eventsub reconcile parked: broadcaster allowlist is empty, no subscriptions mutated`, `error_class="reconcile_no_broadcasters"` (warn) | allowlist empty, nothing mutated |
| `eventsub reconcile parked: TWO_EVENTSUB is not exactly 1`, `reason="flag_dark"` (`status="parked"` in the dashboard) | feature gate is off; reconcile is not running |
| `eventsub reconcile refused …`, `error_class="reconcile_config_invalid"` (error) | callback, credentials or secret invalid; needs the Operator |
| `eventsub subscriptions re-created but not all verified yet`, `error_class="reconcile_pending"`, `status="pending"` | at least one desired subscription is not enabled yet; callback verification has not completed for every desired broadcaster |

One pass can emit multiple lines. Read the final line together with the
earlier diagnostic lines: an unhealthy convergence emits `reconcile_unhealthy`
then the outer `reconcile_failed` wrapper; that pair is one failed pass, not two.
`prior_failed` and `action_failed` lines add detail, not separate pass outcomes.
The terminal result is `healthy` (reconciled), `pending` (verification
incomplete), `reconcile_no_broadcasters` (parked), `reconcile_config_invalid`
(refused), `reconcile_failed` (errored, sometimes preceded by
`reconcile_unhealthy`), or `reconcile_shutdown_interrupted` (stopped by
shutdown before verification; the next startup pass is the verdict). A
`flag_dark` parked line means no pass ran. The
dashboard exposes fields this script drops.
The broad filter catches diagnostic, error, refused, and parked messages whose
text contains `eventsub reconcile`; it does not match the healthy or pending
messages, which use different wording. Run all three filters below for each
page: the broad diagnostic filter, the healthy terminal message, and the pending
terminal message. Read a rolling 90-minute look-back as three adjacent 30-minute
pages (the reader caps each request at 30 minutes). This spans one hourly
interval plus 30 minutes for pass duration, if a pass completes in that time.
The `flag_dark` line is logged only once at process start; it is not a per-pass
outcome and can fall outside a rolling look-back. If a live container has no
terminal outcome in 90 minutes, distinguish a gated-off task from a dead
container or a still-running pass: check the deployed `TWO_EVENTSUB` value and
in Workers Observability inspect `event = "eventsub_reconcile"` over a timeframe
that includes the container's last boot (use its rollout time). A `flag_dark`
line in that boot-inclusive view confirms the gate was off; no line plus
`TWO_EVENTSUB=1` leaves dead vs. mid-pass unresolved. Do not treat paging past
retention or silence alone as a discriminator. For each page, note timestamps
and interpret all matching lines together using the table above. Confirm the
staging image contains the reconcile implementation before using readback to
assess cadence; query results describe only the deployed image.

Run this in Bash on a host with GNU `date`; it issues all three message filters
for each of three adjacent pages, newest first. The subshell keeps
`set -euo pipefail` from changing the calling shell, and `pipefail` makes a
reader failure or refusal fail the command instead of looking like an empty
match. Expect nine JSON blocks; fewer means a page read failed. The `jq` keeps
only safe summary fields and matching message text in the sample output:

```bash
(
set -euo pipefail
for end in "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
           "$(date -u -d '30 minutes ago' +%Y-%m-%dT%H:%M:%SZ)" \
           "$(date -u -d '60 minutes ago' +%Y-%m-%dT%H:%M:%SZ)"; do
  for message in "eventsub reconcile" \
                 "eventsub subscriptions reconciled" \
                 "subscriptions re-created but not all verified yet"; do
    python3 scripts/container_stdout_read.py \
      --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
      --minutes 30 --end "$end" --contains "$message" \
      | jq '{window,event_count,possibly_truncated,events:[.events[].message]}'
  done
done
)
```

Staging sample (2026-10-16; each of the nine filter/page queries was executed;
for each page the broad diagnostic filter is followed by healthy then pending):

```json
{"window":{"start":"2026-10-16T23:07:28.000Z","end":"2026-10-16T23:37:28.000Z"},"event_count":0,"possibly_truncated":false,"events":[]}
{"window":{"start":"2026-10-16T23:07:28.000Z","end":"2026-10-16T23:37:28.000Z"},"event_count":0,"possibly_truncated":false,"events":[]}
{"window":{"start":"2026-10-16T23:07:28.000Z","end":"2026-10-16T23:37:28.000Z"},"event_count":0,"possibly_truncated":false,"events":[]}
{"window":{"start":"2026-10-16T22:37:28.000Z","end":"2026-10-16T23:07:28.000Z"},"event_count":0,"possibly_truncated":false,"events":[]}
{"window":{"start":"2026-10-16T22:37:28.000Z","end":"2026-10-16T23:07:28.000Z"},"event_count":0,"possibly_truncated":false,"events":[]}
{"window":{"start":"2026-10-16T22:37:28.000Z","end":"2026-10-16T23:07:28.000Z"},"event_count":0,"possibly_truncated":false,"events":[]}
{"window":{"start":"2026-10-16T22:07:28.000Z","end":"2026-10-16T22:37:28.000Z"},"event_count":0,"possibly_truncated":false,"events":[]}
{"window":{"start":"2026-10-16T22:07:28.000Z","end":"2026-10-16T22:37:28.000Z"},"event_count":0,"possibly_truncated":false,"events":[]}
{"window":{"start":"2026-10-16T22:07:28.000Z","end":"2026-10-16T22:37:28.000Z"},"event_count":0,"possibly_truncated":false,"events":[]}
```

No matching reconcile outcome appeared in the 90-minute look-back. Before
attributing silence to a stopped task, check that the container is emitting any
logs; the reader's unfiltered command is:

```bash
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 --minutes 5 \
  | jq '{window,event_count,possibly_truncated,events:[.events[].message]}'
```

Staging liveness sample (2026-10-16; six routine logs, no EventSub message):

```json
{"window":{"start":"2026-10-16T23:30:27.000Z","end":"2026-10-16T23:35:27.000Z"},"event_count":6,"possibly_truncated":false,"events":["raid history ungrounded; publication skipped","raid history ungrounded; publication skipped","raid history ungrounded; publication skipped","raid history ungrounded; publication skipped","raid history ungrounded; publication skipped","raid history ungrounded; publication skipped"]}
```

This confirms the container emitted logs at the time; it does not establish that
the reconcile task is enabled. A zero Q2 result means no matching message was
observed, not that the task stopped. Determine whether it is stopped, gated off,
or still in a long-running pass. Do not restart on silence alone. Missing
`healthy` lines alone are not a stopped-task signal: a pass may instead be
pending, unhealthy, refused, parked, or failed. Use terminal outcomes across
successive passes to assess cadence, not the spacing between lines within a pass.

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
  --minutes 5 --contains "revocation acknowledged"
```

```bash
# Same output, grouped by reason (needs this slice deployed; re-check the
# container id with `containers list` first — ids change on re-provision).
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 5 --contains "revocation acknowledged" \
  | jq '[.events[] | select(.reason != null) | .reason] | sort | group_by(.) | map({reason: .[0], count: length})'
```

Samples (staging container, 5 min, 2026-10-16; both queries executed):

```text
Total query excerpt: {"event_count":0,"possibly_truncated":false,"window":{"start":"2026-10-16T16:57:13.000Z","end":"2026-10-16T17:02:13.000Z"}}
By-reason query: []
```

No revocation was observed in either query window. The grouped query returns
an empty array when the total is zero; missing `reason` fields on nonempty
results may indicate that the image emitting those lines predates the reason
field, not a zero-by-reason result.
For either query, verify `possibly_truncated` is false before treating the
result as an exact count; otherwise narrow or page the window.

The Q1 Helix listing (`status` per entry) remains the cross-check for
current-state reasons; it is a snapshot, not a count.

## Q4 — container-attributed challenge failures (lower bound)

A good challenge logs `challenge answered` (echoed as `200 text/plain`;
Twitch then enables the subscription). A bad one is refused before any
announcement path with `error_class = "malformed"` and
`message_type = "webhook_callback_verification"` — the bounded
`message_type` scalar (`webhook_callback_verification` / `notification` /
`revocation` / `unknown`) separates failed challenges from malformed
notifications and edge probes in logs alone. Refusals before valid delivery
headers are extracted have `unknown`; after extraction, unrecognized values
also map to `unknown`:

Sample (staging container, 5 min, 2026-10-16; query executed):

```bash
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 5 --contains "challenge answered"
```

Excerpted output (other envelope fields omitted):

```json
{"event_count":0,"possibly_truncated":false,"window":{"start":"2026-10-16T16:57:14.000Z","end":"2026-10-16T17:02:14.000Z"}}
```

Failure side: `eventsub delivery refused` warn lines carry `error_class`
(`unauthorized` = missing/mismatched signature or headers; `malformed` =
bad query, empty body, or an invalid challenge; `unsupported_media_type`,
`payload_too_large`, `request_timeout`, `method_not_allowed`, `not_found`,
`busy`, and `upstream_timeout`) plus `message_type`. Only refusals after valid
delivery headers are extracted have the actual bounded message type; earlier
refusals use `unknown`. The Worker edge has its own, narrower
`eventsub_ingress` vocabulary: `unsupported_media_type`, `unauthorized`,
`malformed`, `payload_too_large`, `request_timeout`, `rate_limited`,
`upstream_timeout`, and `unavailable` (`wrangler/src/eventsub.ts`). A wrong
route/shape can return a bare 404 without an ingress log, so Worker log counts
do not include every rejected request.

Sample (staging Worker, 30 min, 2026-10-15): `eventsub_ingress` count 0.

Container-attributed challenge-failure query (needs this slice deployed):

```bash
python3 scripts/container_stdout_read.py \
  --service a038b0ad-a1f9-4966-bfb7-f992c602d938 \
  --minutes 5 --contains "delivery refused" \
  | jq '{container_attributed_challenge_failures: ([.events[] | select(.message_type == "webhook_callback_verification")] | length), possibly_truncated}'
```

Sample interpretation (staging container, 5 min, 2026-10-16; query result):

```json
{"container_attributed_challenge_failures":0,"possibly_truncated":false}
```

This is a lower bound, even when `possibly_truncated` is false: it counts
only challenge refusals attributed after valid delivery headers reached the
container. The Worker can refuse challenges with `rate_limited` (429),
`unavailable` (503), or `upstream_timeout` (504) without a `message_type`; the
container can also log `busy` or `upstream_timeout` refusals as
`message_type="unknown"`. Those failures are not included in this query. A zero
therefore means no container-attributed challenge failure was observed, not
that no challenge failed. During subscription setup, cross-check Worker
`eventsub_ingress` refusals in Workers Observability and inspect `unknown`-type
container refusals alongside the Q1 Helix listing. A stuck
`webhook_callback_verification_pending` (or
`webhook_callback_verification_failed` after a reconcile) remains evidence of a
subscription whose challenge has not succeeded.

Remaining receiver lines for completeness: `notification acknowledged`
(verified go-live acked; a `--contains "acknowledged"` read also matches
`revocation acknowledged`), `delivery ignored` (unknown type, acknowledged
with 2xx so Twitch does not retry), and the sampled `eventsub delivery
classification failed` warn (deterministic planning error, acknowledged with
2xx, `error_variant` naming the variant). The separate
`eventsub startup refused` line is emitted from `crates/bot/src/main.rs` during
receiver setup; it records a fatal config/bind failure before the process exits
nonzero, not a receiver-request log.

## Q5 — intake and reconcile counters (metrics scrape)

Q1–Q4 read logs. Three counters in the same `/ops/metrics` scrape answer the
aggregate direction — how many deliveries arrived, how many reconcile
mutations landed, and where reconcile failed — with no log window to page.
Series contract: [metrics](metrics.md). This section installs no monitor or
rule; the only live reconcile rule is the failed-pass ticket
(`eventsub_reconcile_failed`), and where this pack and a live rule disagree
the live rule wins.

Fetch one scrape (operator supplies the Worker URL and the
already-provisioned scrape token; the token never goes in a PR or log):

```bash
curl --silent --show-error --max-time 10 \
  -H "Authorization: Bearer ${METRICS_SCRAPE_TOKEN}" \
  "${WORKER_URL}/ops/metrics" > /tmp/eventsub-start.txt
```

Repeat at the end of the window into a second file and diff the counters.
A counter that reset to zero means the process restarted, not that the
window was quiet — a fresh deploy runs a startup reconcile pass before the
first scrape, so re-baseline both files after a reset.

Copy-paste reads over the two scrapes (exact rendered names; label-free
`received_total` takes a trailing space, the labelled families take `{`):

```bash
grep "^two_bot_eventsub_received_total " /tmp/eventsub-start.txt /tmp/eventsub-end.txt
grep "^two_bot_eventsub_reconcile_actions_total{" /tmp/eventsub-start.txt /tmp/eventsub-end.txt
grep "^two_bot_eventsub_reconcile_errors_total{" /tmp/eventsub-start.txt /tmp/eventsub-end.txt
```

PromQL equivalents for a Prometheus-compatible reader over the scrapes:

```promql
sum(increase(two_bot_eventsub_received_total[1h]))
sum by (action) (increase(two_bot_eventsub_reconcile_actions_total[1h]))
sum by (error_class) (increase(two_bot_eventsub_reconcile_errors_total[1h]))
```

Reading the result:

- `two_bot_eventsub_received_total` (label-free scalar) counts deliveries
  accepted for signature verification, including tampered signatures.
  Requests refused before body intake are not deliveries, so a zero delta
  beside expected traffic points at the Worker edge (dashboard
  `eventsub_ingress` refusals) or the Q4 container refusals, not at a quiet
  Twitch.
- `two_bot_eventsub_reconcile_actions_total{action}` counts successful
  mutations only: `action` is `create` (2xx creates) or `delete` (2xx
  deletes, including already-gone deletes). 409 already-exists
  confirmations fold into the pass outcome, not into actions. Deltas move
  only when a pass mutates — startup plus the bounded hourly interval —
  so a flat window with a healthy Q2 outcome is steady state, not a gap.
- `two_bot_eventsub_reconcile_errors_total{error_class}` counts failure
  sites by the same fixed strings as the Q2 log `error_class` field:
  `reconcile_action_failed` (once per failed plan action),
  `reconcile_prior_failed` (once per pass that found failed
  subscriptions), `reconcile_unhealthy` (once per non-converged pass),
  `reconcile_failed` (listing or plan failures), plus `other` for unknown
  classes. Join a nonzero delta to the Q2 log lines by `error_class`;
  `reconcile_pending`, config refusals and parked passes stay log-only
  and never appear here. These counters add detail only and never
  multiply the `eventsub_reconcile_failed` ticket (one ticket per failed
  pass however many sites moved; see
  [runbook](runbook.md#alert-eventsub-reconcile-failed)).
