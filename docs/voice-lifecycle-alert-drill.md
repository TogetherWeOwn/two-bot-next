# Voice lifecycle metrics staging alert drill (read-only)

Staging-only, read-only drill over the voice room lifecycle outcome
signals emitted by the shared-executor worker. It proves lifecycle
failures surface in the container-log query, and records one coverage
gap in the metrics alert rules. No mutations, no room create or delete,
no production contact. Line numbers below are at main head `9b73b33a`.

## 1. Signal inventory (what the drill queries)

The worker emits fixed-cardinality counters plus one token-free log line
per terminal outcome (`crates/bot/src/voice_rooms.rs`, `crates/core/src/metrics.rs`):

| Exposition series | Log event and fields | Emit site |
| --- | --- | --- |
| `two_bot_voice_operations_total{op,outcome}` (`op`: create/move/delete; `outcome`: success/category_full/discord/persistence/cancelled) | `voice_event="voice_operation"` with `op`, `outcome` (`info` on success, `warn` otherwise) | `voice_rooms.rs:371` (`observe_voice_operation`) |
| `two_bot_voice_reconcile_actions_total{action}` (delete_enqueued/suspended/resumed/succession_enqueued) | `voice_event="voice_reconcile"` with plan counts (`info`, only when nonzero) | `voice_rooms.rs:1463` |
| `two_bot_voice_dead_letters_total{action}` (create/move/delete/companion/ownership/kick/rename/other, after 10 attempts) | `voice_event="voice_dead_letter"` with `action`, `attempts` (`warn`) | `voice_rooms.rs:1995` (`mark_failed_observed`) |
| `two_bot_voice_tracked_rooms`, `two_bot_voice_compensation_pending` (gauges) | state refreshed per reconcile pass via `voice_state` | `voice_rooms.rs:1468` |
| `two_bot_voice_orphans_total` (counter) | `voice_event="voice_creator_orphan"` with `outcome="manual_needed"` (`warn`) | `voice_rooms.rs:4902` |

No channel, member, token, body or ID leaves the process in any label or
field. Series contract: `docs/metrics.md` ("Internal metrics" table and
"Label allowlists"). Logs use the default `tracing_subscriber::fmt()`
text format (`crates/bot/src/main.rs`), so each field renders as
`key="value"` on one line and stays greppable.

## 2. Evidence table (queries run + results)

| # | Query (read-only) | Where run | Result |
| --- | --- | --- | --- |
| Q1 | Staging deploy state: latest `deploy-staging` runs and conclusions | `gh run list --workflow=deploy-staging.yml` (public CI metadata) | Run `37154963396` (head `87d98060`) concluded `failure` at the rollout gate; two newer pushes (`cc349ed5`, `9b73b33a`) were still `in_progress` at drill time. Staging was not healthy, so no live voice exercise was possible or attempted. |
| Q2 | Series-existence check: every drill series renders from the checked-in registry | `docs/metrics.md` table plus `crates/core/src/metrics.rs` render sites (`voice_operations_total`, `voice_reconcile_actions_total`, `voice_dead_letters_total`, `voice_tracked_rooms`, `voice_compensation_pending`, `voice_orphans_total`) and the `voice_signals_stay_bounded_and_saturate` unit test | All six series exist with fixed cardinality; hostile labels collapse without new series. |
| Q3 | Log-field check: failure outcomes carry a greppable `voice_event` line | `grep voice_event crates/bot/src/voice_rooms.rs` at `9b73b33a` | Five emit sites (see §1): every non-success outcome logs at `warn` with bounded fields. The failure query is `voice_event="voice_operation"` with any non-`success` outcome, plus `voice_event="voice_dead_letter"` and `voice_event="voice_creator_orphan"`. |
| Q4 | Alert-query drill: synthetic exposition (42 successful creates, then injected `create/discord x7`, `move/persistence x4`, dead-letter `create x3`, `compensation_pending 2`, `orphans 1`) fed through the real `evaluateMetrics` from `wrangler/src/alert-rules.ts` (node v24, type-stripping, zero edits to the rule file) | Offline node script importing the checked-in evaluator | `RULES: job_stale,job_consecutive_failures,rest_429_rate,db_pool_saturated`; `FIRING-HEALTHY: []`; `FIRING-VOICE-FAILURES: []`. No rule reads any `two_bot_voice_*` series, so injected lifecycle failures fire nothing. |
| Q5 | Duplicate search: open PRs and issues matching this drill | `gh pr list --search "lifecycle alert drill"`, `gh issue list --search "voice metrics staging"` | No related open PR or issue; only the scheduled release PR matched. |

## 3. Verdict: PASS on the log query, NEEDS WORK on rule coverage

**PASS:** lifecycle failures surface in the log query. Every failure path
(Q3) emits a `warn` line with a bounded `voice_event`, so the §2/Q3
filter catches discord, persistence, category-full, cancelled, dead-letter
and orphan outcomes without any new instrumentation.

**NEEDS WORK (follow-up, not this doc):** no metrics alert rule covers
the voice series (Q4). The four checked-in rules (`job_stale`,
`job_consecutive_failures`, `rest_429_rate`, `db_pool_saturated` in
`wrangler/src/alert-rules.ts:43`) never read `two_bot_voice_*`, so a
lifecycle failure storm pages nobody until someone greps the logs. Adding
a voice-failure rule (with its runbook anchor and packet spelling) is a
separate Worker slice.

## 4. Limits (what this drill did not do)

- No live `/ops/metrics` scrape: the route needs the operator-held
  bearer token, and staging was unhealthy (Q1), so there was nothing
  meaningful to scrape.
- No container-log access from this seat; the log half rests on emit
  sites plus the documented exposition contract, not on a live tail.
- No database probe of any kind; counters reset on process restart, so
  a zero delta means restart-or-quiet, never proven quiet.
- No room create/move/delete, no config change, no production contact.
