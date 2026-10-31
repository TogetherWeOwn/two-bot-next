# Sticky debounce and activity-gate acceptance

`crates/core/tests/sticky_debounce_acceptance.rs` pins the public
`two_bot_core::sticky` decision API (`crates/core/src/sticky.rs`). It is pure
and offline: no database, no Discord client, no feature flags. This page
covers that pure suite first, then records where the wired runtime behaviour
is implemented and tested, so the two are not confused.

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test sticky_debounce_acceptance
```

CI runs it with the other integration targets (`cargo test --workspace --test '*'`).

Parity: [parity](parity.md) §1 #20–21 (`/sticky`, `/sticky-remove`) and the §4
`automationMessageAccepted` sticky re-post. Legacy `onChannelActivity` order is
claim → post replacement → record → delete previous (best-effort).

## Preview unsaved input privately

Use `/sticky body:Read the rules debounce:5 preview:true` before replacing a
working configuration. Under the same guild, feature and Manage Server gates
as saving, it returns **Preview — not saved**, proposed content and the
normalized debounce (1–300 seconds; omitted means 5). It validates the same raw
body as saving: 1–2000 UTF-16 units, not empty or invisible-only. The private
rendering uses delivery's mention sanitizer, additionally breaks user/role
mention pills and marks content truncated to fit the response framing.

Preview does **not** read or write saved stickies, audits, delivery claims,
uncertainty fences, counters or jobs, and sends/edits/deletes no channel message.
It does not verify actual delivery permissions or state, promise identical live
rendering, or recover uncertain delivery. Save separately with the same body and
debounce and `preview:false` (or omit preview); saving revalidates and retains
its existing audit/recovery semantics. Wrong-type, duplicate and unknown options
refuse privately before any storage access, never falling through to save.

`crates/bot/src/sticky_unsaved_preview_acceptance_tests.rs` drives real dispatch
with an unavailable pool, a live SQL-acquisition spy, whole-row state snapshots
and mock REST. It covers validation, authority, strict flags, mention/Unicode
bounds, registry/help discovery, zero preview effects, and real save/remove/
uncertainty-reset controls. It runs in the normal bot unit suite; the disposable
DB cases require the authorized test bootstrap. No live-guild execution is
claimed by these fixtures.

## Acceptance matrix

| # | Contract | Tests |
| --- | --- | --- |
| 1 | `normalize_debounce`: omitted → 5; 0 and 301 refused with the legacy message; 1 and 300 accepted | `debounce_omitted_defaults_to_five_seconds`, `debounce_refuses_zero_and_three_hundred_one`, `debounce_accepts_inclusive_bounds_and_refuses_outside` |
| 2 | `repost_due`: never posted → due; held until `debounce × 1000` ms elapse; the exact boundary posts; clock skew holds | `repost_due_when_never_posted`, `repost_due_only_after_debounce_seconds_elapse`, `repost_holds_under_clock_skew` |
| 3 | `claim_blocks`: a claim younger than 60 s refuses a second claimant; stale at exactly 60 s; a future-dated claim blocks | `no_claim_never_blocks`, `second_claim_inside_window_is_refused`, `claim_goes_stale_at_sixty_seconds`, `future_dated_claim_conservatively_blocks` |
| 4 | `activity_eligible`: bot authors (including our own re-post), guild mismatch and absent channel never reach the check, even when a re-post is due | `activity_gate_truth_table`, `ineligible_activity_is_ignored_even_when_repost_is_due`, `eligible_activity_without_enabled_sticky_is_no_sticky` |
| 5 | `decide_activity` end state: the `Repost` plan names the previous sticky; driven post → record → delete, the old copy is gone and the fresh copy is last; a failed post leaves the old sticky in place and retries next activity; bursts hold inside the window, then chain | `repost_plan_targets_the_previous_sticky_for_deletion`, `successful_repost_leaves_fresh_sticky_at_the_bottom_and_old_gone`, `failed_post_never_drops_the_existing_sticky`, `burst_coalesces_until_window_then_chains_old_then_fresh` |

## Runtime wiring (covered elsewhere, not by this file)

The executor is wired to the sticky path; this pure suite just does not
exercise it. In `crates/bot/src/command_runtime.rs`:

- `dispatch_with_verdict` sends an accepted `MessageCreate` to `on_message`
  when automations are enabled.
- `on_message` runs lookup (`store::get_sticky`) → the `decide_activity`
  precheck → the atomic `store::claim_sticky_post` → `executor.post_message`
  → `store::record_sticky_post` → best-effort delete of the previous sticky →
  a `sticky.run` audit row. A replacement is only recorded when Discord
  returns a usable non-zero numeric message id. Only a proven no-send
  (explicit rejection, rate limit, local guard, pre-wire admission
  refusal) releases the claim so the next eligible activity can retry.
  Anything ambiguous — a timeout, transport loss, unusable success id, or
  a record failure with unconfirmed orphan cleanup — keeps the
  `delivery_uncertain` fence the atomic claim armed: the next activity
  holds instead of sending a possible duplicate, across restarts and past
  the 60 s claim lease, until an administrator reconfigures. Every
  uncertain outcome audits `post_failed` (never `ok`) and logs the bounded
  `sticky_delivery_uncertain` diagnostic with its inspection-first
  recovery action. A record failure with proven orphan absence, or a
  moved-on claim with confirmed cleanup, reports held without fencing;
  an unconfirmed cleanup on the moved-on branch re-arms the fence
  instead.
- `/sticky` (`sticky_set`) validates the body and debounce, upserts the row,
  audits (`sticky.create`/`sticky.update`, `ok` or `rejected`) and replies;
  `/sticky-remove` (`sticky_remove`) deletes the row,
  best-effort deletes the posted sticky, audits (`sticky.delete`, `ok` or
  `absent`) and replies. Both defer ephemerally before I/O and complete by
  editing the original response; a failed defer mutates nothing. The
  interaction defer/edit calls use the executor's blocked-retry variants,
  which is separate from the activity-driven re-post retry above.
- All state goes through the `two_bot_core::sticky::store` functions over
  `sticky_messages` and `automation_audit_log`: `get_sticky`, `put_sticky`
  (conflict update preserves the post columns and clears any claim and
  uncertainty fence — the supported recovery path), `claim_sticky_post`,
  `record_sticky_post`, `release_sticky_post`,
  `mark_sticky_delivery_uncertain`, `delete_sticky` and `audit_sticky`.

That behavior is pinned by the database-backed tests in
`crates/bot/src/command_runtime_tests.rs` — for example
`burst_coalesces_to_one_repost_and_retires_previous`,
`held_claim_blocks_the_second_attempt`, `debounce_window_holds_the_repost`,
`post_transport_failure_retains_uncertainty_fence`,
`unusable_success_receipt_retains_uncertainty_fence`,
`sticky_set_writes_row_audits_and_confirms`,
`sticky_set_rejection_audits_rejected_and_still_replies`,
`sticky_remove_clears_state_and_deletes_the_message`,
`sticky_remove_absent_reports_absent_without_delete` and
`failed_defer_leaves_set_and_remove_state_untouched`. They are `#[ignore]`d
locally, run in CI against the test-database service
(`cargo test -p two-bot --locked command_runtime_tests -- --ignored`), and
drive a mock Discord REST server — not a live guild.

## Uncertain delivery recovery

If a re-post attempt cannot prove its outcome (lost response, unreadable
receipt, or a record failure with unconfirmed orphan cleanup), the channel's
sticky is fenced: further activity holds instead of sending a possible
duplicate, across restarts and lease expiry, and the bot logs
`code="sticky_delivery_uncertain"` with
`next_action="inspect_channel_before_sticky_reset"`. Recovery: inspect the
channel, delete any duplicate or orphan sticky post, then run `/sticky` again
to reset the fence (or `/sticky-remove` plus `/sticky` to start over). Never
wait out the fence or resend blindly.

## Not covered here

- Row 5 drives the plan through a test-local channel model in the documented
  wire order. It proves the plan supports post-before-delete; the runtime
  executor, storage and REST calls are covered by the tests above, not by
  this file.
- `decide_activity` is a precheck only. The atomic `claim_sticky_post` race
  (one winner per window) is verified by the ignored live store suite
  `crates/core/tests/sticky_store_live.rs` against agent-testdb, and at the
  runtime level by `burst_coalesces_to_one_repost_and_retires_previous`.
- Nothing cited here proves live-guild behavior: real Discord permissions,
  delivery and ordering in a staging or production guild remain outside both
  suites.
- Body-validation bounds, reply text and audit action/outcome strings are
  pinned by the unit tests in `crates/core/src/sticky.rs`, not by this pure
  suite.
- Previous-copy retirement after a confirmed record stays best-effort: a
  failed delete is logged and the flow continues, so removal of the old
  copy is not guaranteed. Orphan cleanup on the record-failure and
  claim-moved-on branches is confirming instead: only proven absence
  reopens the window, and an unconfirmed cleanup keeps or re-arms the
  uncertainty fence. Those branches are pinned by
  `completion_db_failure_audits_post_failed_and_preserves_newer_claim`,
  the obsolete-completion tests, and the
  `sticky_uncertain_acceptance_tests` corpus.
