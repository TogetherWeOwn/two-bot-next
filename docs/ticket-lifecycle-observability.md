# Ticket lifecycle observability

Each instrumented ticket lifecycle event emits one structured log line and one
counter sample, so agents and operators can answer "why did this ticket
vanish / why is this member stuck on Creating" from logs. Instrumented events
are retire, abandon, purge, and successful staff-button claim/close executed
through the button path (table below). Recovery completions
(`recover_saved_close`, `retry_cleanup`, `delete_interrupted_create`, the
single-match `find_creating_channel` rollback, and `abandon_unsaved_close`)
and the open-path controls-post rollback reach their terminal states without a
`ticket_lifecycle` line or counter sample. Emission lives in
`crates/bot/src/ticket_runtime.rs` (`emit_lifecycle` and `purge`); the
counter lives in `crates/core/src/metrics.rs`. See [tickets.md](tickets.md)
for the lifecycle itself.

## Counter

`two_bot_ticket_lifecycle_total{event,outcome}` — 22 rendered series from
process start: each event renders its success outcomes plus the `query`
collapse trapdoor, and `purge` additionally renders the full `db_cause` set
(the full 5 x 18 cross product would not fit the Worker scrape body cap).
No member text, channel names, ticket ids or error strings ever become
labels; the vocabulary is closed (no `other` row).

| event | outcome | cause (log) | Meaning |
| --- | --- | --- | --- |
| `retire` | `retired` | `missing_channel` | Recovery retired an Open ticket whose channel is typed-absent (code 10003) |
| `abandon` | `abandoned` | `create_rejected` | A safe pre-mutation channel-create rejection released the reservation |
| `abandon` | `abandoned` | `orphan_not_found` | A successful guild search found no orphan for a stale Creating reservation |
| `abandon` | `already_gone` | `already_gone` | `abandon_creating` deleted zero rows; the reservation was already gone |
| `purge` | `deleted` | `retention_expired` | The purge pass deleted expired transcript bodies; counts one per deleted body, not one per pass |
| `purge` | `<db_cause>` | `<db_cause>` | The purge pass storage write failed; the failure class survives (see below) |
| `claim` | `claimed` | `staff_claim` | A staffer claimed the ticket |
| `close` | `closed` | `staff_close` | A staff close completed through the button path |

Storage-failure outcomes reuse the shared `db_cause` vocabulary
(`crates/core/src/error_chain.rs`): `unique_violation`,
`foreign_key_violation`, `not_null_violation`, `check_violation`,
`exclusion_violation`, `database`, `pool_timeout`, `pool_closed`,
`worker_crashed`, `connection`, `row_not_found`, `query`.

## Log lines

One `ticket_lifecycle` line per event above, with `event`, `ticket_id`,
`prior_status` and `cause`. Pass-level purge lines carry the deleted body
count as `deleted` instead of a `ticket_id` (`deleted=0` when the pass
failed before deleting). Member display names, channel names and transcript
text never appear in these lines.

## Warns

- `ticket recovery deferred` keeps `error_class` and carries `action`, the
  failed recovery action (`reattach_open_controls`,
  `find_creating_channel`, `delete_interrupted_create`, `retry_cleanup`,
  `recover_saved_close` or `restore_opener_then_reopen`), plus `ticket_id`
  and `status`. Rows that fail before any action is selected (the
  transcript probe, or an item timeout) carry `ticket_id` and `status`
  without an `action`.
- `ticket action incomplete` keeps `error_class` and carries `action`, the
  button verb (`open`, `claim` or `close`). Claim/close failures cite the
  channel-bound `ticket_id` and `status` when the ticket still resolves;
  failed opens, failures in channels with no tracked ticket, and SQL
  storage failures (the lookup is skipped) carry the `action` alone.

## Volume

Lifecycle lines are steady-state (one per ticket event, one per hourly
purge pass), not hot-path: they never fire per dispatch. The counter adds
22 fixed series (per-event subsets, not the full cross product) to the
[log-volume guard](log-volume-guard.md) budget.
