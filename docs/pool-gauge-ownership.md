# Pool gauge ownership for named pool gauges

Status: proposed — pending reviewer approval. Docs-only; no code change.

## Current state

- The bot runs one `PgPool` (the Store pool). `main.rs:235` clones it into the
  metrics slot once at startup via `metrics_http::register_pool`.
- `Metrics::render` takes `pool: Option<(u32, usize, u32)>`
  (`crates/core/src/metrics.rs:709-718`); the scrape samples SQLx bookkeeping
  without SQL or acquiring a connection (`crates/bot/src/metrics_http.rs:88-110`).
- Four unlabeled gauges are emitted
  (`crates/core/src/metrics.rs:1107-1131`): `two_bot_db_pool_configured`,
  `two_bot_db_pool_connections`, `two_bot_db_pool_idle_connections`,
  `two_bot_db_pool_max_connections`.
- The single slot is keep-first since #802: a second `register_pool` drops its
  handle (never closes it — clones share the pool), counts the rejection, and
  logs at most once. `replace_pool` is the explicit replacement path; the sole
  production caller stays on `register_pool`.
- The Worker rule fires `db_pool_saturated` when `configured == 1`, `max > 0`,
  `size == max`, `idle == 0` for 3 consecutive samples
  (`wrangler/src/alert-rules.ts:376-382`).

## Decision: gauge-per-pool with a `pool` label

When a second `PgPoolOptions` instance exists, each pool gets its own gauge
set distinguished by a `pool="<name>"` label, e.g.
`two_bot_db_pool_connections{pool="store"}`. The four metric names do not
change; only the label set grows.

Rejected alternative — gauge-per-role: "role" is ambiguous between the
database login (one role can serve many pools via `SET ROLE`; see
`docs/database-roles.md`) and the subsystem consumer (gateway, jobs, and
commands all share clones of the one pool today). Labeling by consumer would
emit N identical series for one pool and page the wrong owner. The thing under
pressure is the pool instance, so the pool owns the identity.

## Owner of pool identity

- The registration site owns names: `register_pool(name, pool)` in
  `crates/bot/src/metrics_http.rs` (the slot owner), called from the single
  wiring site in `main.rs`. The name is a `&'static str` from a closed set
  (today: exactly one, `"store"`).
- Names are never derived from connection strings, URLs, or runtime config:
  those are secret-adjacent and unbounded. A new pool name is a code-reviewed
  addition to the closed set plus a row in `docs/metrics.md` and an alert-rule
  fixture — same checklist as any new label value.
- No pool counts are guessed here: a second name appears only when a real
  second `PgPoolOptions` exists. Pool sizing is out of scope.

## Exposition migration (a future code change, not this doc)

The Worker validator rejects labeled pool series today. `ALERT_INPUT_LABELS`
pins the four families to zero labels (`wrangler/src/metrics-scrape.ts:11-14`),
and `parseAlertScrape` returns `null` for any sample whose label count
differs (`metrics-scrape.ts:93-98`). A `null` scrape only logs `two-bot
metrics scrape failed` and keeps the previous state
(`wrangler/src/index.ts:794-809`), so a container that emits `{pool="store"}`
copies before the validator accepts them freezes every metrics alert: no new
pages and no recoveries, not just `db_pool_saturated`. The opposite mistake
also fails: adding a `pool` axis to `ALERT_INPUT_LABELS` makes the label
mandatory on every sample and adds the labeled rows to `REQUIRED_INPUTS`, so
scrapes from older containers fail once the Worker deploys. Ship in this
order, one PR each, so every step is safe on its own:

1. **Worker accepts both shapes.** The four unlabeled series stay required.
   Add a pool-specific shape check, not an `ALERT_OPTIONAL_INPUT_LABELS`
   entry for the same names: that map is spread over `ALERT_INPUT_LABELS` in
   `VALIDATED_INPUT_LABELS` (`metrics-scrape.ts:43-46`), so reusing a family
   name would demand a label on the unlabeled series too. The check accepts a
   sample with zero labels, or exactly one `pool` label whose value is in the
   closed name set, and never makes the labeled rows required. Keep the
   inherited-property guard (`in Object.prototype`) for `pool` values, since
   per-pool state will use plain objects. Make the evaluator read the
   unlabeled series explicitly: `gauge(name)[0]` at `alert-rules.ts:376-379`
   takes the first sample of the name whatever its labels, so labeled copies
   would otherwise be read by exposition order. Add a `metrics-scrape.test.ts`
   fixture where the unlabeled series plus labeled copies must still parse,
   and one where a labeled copy with an unknown `pool` value returns `null`.
2. **Container emits the labeled copies.** Keep the four unlabeled series for
   the `store` pool verbatim, and add `{pool="<name>"}` copies for every
   registered pool. Keep `render(pool)` working for single-pool callers; add
   a named-pools entry point (e.g. `render_named`) that takes one sample per
   registered name. Cardinality stays bounded because the name set is closed
   and small.
3. **Rule switches to per-pool grouping**, as described under the alert-rules
   join below. Retire the unlabeled series only after every consumer (alert
   rules, dashboards, validator) reads the labeled form, in a separate change
   that makes the labeled rows required and removes the zero-label shape.

Rolling the container back after step 2 is safe: older containers emit only
the unlabeled series, which the step 1 validator still accepts.

## Alert-rules join

- Group pool samples by the `pool` label; unlabeled legacy samples join as
  `pool="store"`.
- Evaluate saturation per pool with the exact current predicate
  (`configured == 1 && max > 0 && size == max && idle == 0`).
- Track the 3-sample streak per pool (`poolStreaks: Record<pool, number>`,
  migrating the scalar `poolStreak`; missing state restarts a streak, never
  fires).
- A saturated non-default pool fires with a subject suffix following the
  existing convention (`db_pool_saturated:<pool>`, like `job_stale:<job>`); the
  bare `db_pool_saturated` keeps meaning the `store` pool, and the rule id
  spelling in `crates/core/src/evidence.rs` is unchanged.
- With one pool registered, behavior is bit-identical to today: the same
  predicate, the same 3-sample streak, the same firing id.

## Guard preservation (#802)

- Every named slot keeps the keep-first guard independently: first registration
  wins per name, duplicates drop without closing, rejection counts stay
  per-name, one warning per process. Silent re-pointing stays impossible for
  every pool, and `replace_pool(name, pool)` stays the only deliberate-swap
  path.
- Cross-pool misregistration (registering the store handle under another name)
  is a wiring bug, not a guard bug: names are assigned only at the `main.rs`
  wiring site, and the per-name `max_connections` pin test pattern from #802
  extends to each name.

## Out of scope

Implementing the gauges or the rule change, changing pool sizing, adding a
second pool, touching `two_bot_db_errors_total{op}` (errors, not pressure),
and any production/staging database work.
