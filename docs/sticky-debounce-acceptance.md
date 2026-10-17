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

## Acceptance matrix

| # | Contract | Tests |
| --- | --- | --- |
| 1 | `normalize_debounce`: omitted → 5; 0 and 301 refused with the legacy message; 1 and 300 accepted | `debounce_omitted_defaults_to_five_seconds`, `debounce_refuses_zero_and_three_hundred_one`, `debounce_accepts_inclusive_bounds_and_refuses_outside` |
| 2 | `repost_due`: never posted → due; held until `debounce × 1000` ms elapse; the exact boundary posts; clock skew holds | `repost_due_when_never_posted`, `repost_due_only_after_debounce_seconds_elapse`, `repost_holds_under_clock_skew` |
| 3 | `claim_blocks`: a claim younger than 60 s refuses a second claimant; stale at exactly 60 s; a future-dated claim blocks | `no_claim_never_blocks`, `second_claim_inside_window_is_refused`, `claim_goes_stale_at_sixty_seconds`, `future_dated_claim_conservatively_blocks` |
| 4 | `activity_eligible`: bot authors (including our own re-post), guild mismatch and absent channel never reach the check, even when a re-post is due | `activity_gate_truth_table`, `ineligible_activity_is_ignored_even_when_repost_is_due`, `eligible_activity_without_enabled_sticky_is_no_sticky` |
| 5 | `decide_activity` end state: the `Repost` plan names the previous sticky; driven post → record → delete, the old copy is gone and the fresh copy is last; a failed post leaves the old sticky in place and retries next activity; bursts hold inside the window, then chain | `repost_plan_targets_the_previous_sticky_for_deletion`, `successful_repost_leaves_fresh_sticky_at_the_bottom_and_old_gone`, `failed_post_never_drops_the_existing_sticky`, `burst_coalesces_until_window_then_chains_old_then_fresh` |

## Wired runtime

The executor is wired. `crates/bot/src/command_runtime.rs` is the composition
owner: the shared interaction router routes `/sticky` and `/sticky-remove` to
the automation-admin handler (automations gate, Manage Server required), and
accepted `MessageCreate` events reach `CommandRuntime::on_message` when the
automations gate (`TWO_AUTOMATIONS=1`) is on. What that code does, in order:

- **Commands.** `/sticky` validates the body (1–2000 UTF-16 units with
  sendable text) and the debounce (omitted → 5, otherwise 1–300), upserts the
  row enabled, audits `sticky.create`/`sticky.update` (`ok`, or `rejected`
  for invalid input) and confirms ephemerally. `/sticky-remove` deletes the
  row, best-effort deletes the previous Discord message, audits
  `sticky.delete` (`ok` or `absent`) and replies. Both defer ephemerally
  before I/O and complete by editing the original response; a failed defer
  mutates nothing.
- **Storage.** All state goes through the `two_bot_core::sticky::store`
  functions over `sticky_messages` and `automation_audit_log`: `get_sticky`,
  `put_sticky` (conflict update preserves the post columns and clears any
  claim), `claim_sticky_post`, `record_sticky_post`, `release_sticky_post`,
  `delete_sticky` and `audit_sticky`.
- **Re-post.** `on_message` re-reads the row, runs the pure `decide_activity`
  precheck, then takes the store's atomic `claim_sticky_post` — only the
  claim authorizes a post. The winner posts the stored body through the
  shared REST executor (mention-suppressed, numeric nonce with
  `enforce_nonce`), requires a usable non-zero message id, records it, then
  best-effort deletes the previous copy and audits `sticky.run`/`ok`.
- **Retry.** The re-post itself is a single executor attempt per claim; there
  is no in-place retry loop. On a post error or an unusable message id the
  runtime releases its claim, keeps the previous sticky, audits
  `sticky.run`/`post_failed` and reports held, so the next accepted message
  can retry (a claim that cannot be released expires after 60 s). If the
  record step finds the claim moved on, or fails, the just-posted replacement
  is deleted as an orphan where its id is known. The interaction defer/edit
  calls use the executor's blocked-retry variants, which is separate from
  this activity-driven re-post retry.

That runtime behaviour is covered by database-backed tests in
`crates/bot/src/command_runtime_tests.rs` (isolated Postgres plus a mock REST
server; ignored by default, run in CI's ignored-db-runtime lane):

| Behaviour | Tests |
| --- | --- |
| Burst of three activities coalesces to one re-post; previous copy retired, claim released, `sticky.run`/`ok` audited | `burst_coalesces_to_one_repost_and_retires_previous` |
| A foreign claim or an open debounce window holds the re-post with no wire call and no audit row | `held_claim_blocks_the_second_attempt`, `debounce_window_holds_the_repost` |
| Post failure releases the claim, audits `post_failed`, and the next activity re-posts | `post_failure_releases_claim_and_audits_post_failed` |
| A replacement without a usable message id preserves the previous sticky and releases the claim | `unconfirmed_replacement_preserves_previous_and_releases_claim` |
| `/sticky` writes the row, audits and confirms; invalid input audits `rejected` and writes no row | `sticky_set_writes_row_audits_and_confirms`, `sticky_set_rejection_audits_rejected_and_still_replies` |
| `/sticky-remove` clears state and deletes the message; absent reports `absent` with no delete | `sticky_remove_clears_state_and_deletes_the_message`, `sticky_remove_absent_reports_absent_without_delete` |
| A failed defer leaves set/remove state and audits untouched | `failed_defer_leaves_set_and_remove_state_untouched` |

## Not covered here

- Row 5 of the pure matrix drives the plan through a test-local channel model
  in the documented wire order. It proves the plan supports
  post-before-delete; by itself it does not test the executor, the store or
  Discord. Those are covered by the runtime tests above, not by this suite.
- `decide_activity` is a precheck only. The atomic `claim_sticky_post` race
  (one winner per window) is verified by the ignored live store suite
  `crates/core/tests/sticky_store_live.rs` against agent-testdb, and at the
  runtime level by `burst_coalesces_to_one_repost_and_retires_previous`.
- Body-validation bounds, reply text and audit action/outcome strings are
  pinned by the unit tests in `crates/core/src/sticky.rs`, not by this pure
  suite.
- The runtime tests use a mock REST server and an isolated test database.
  They do not verify behaviour in a live Discord guild (real permissions,
  rate limits or gateway delivery), so this page makes no live-guild claim.
- Previous-copy and orphan deletes are best-effort: a failed delete is logged
  and the flow continues, so removal of the old copy is not guaranteed. The
  record-failure and claim-moved-on orphan-cleanup branches in `on_message`
  have code but no dedicated test yet.
