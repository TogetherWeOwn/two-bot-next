# Sticky debounce and activity-gate acceptance

`crates/core/tests/sticky_debounce_acceptance.rs` pins the public
`two_bot_core::sticky` decision API (`crates/core/src/sticky.rs`). It is pure
and offline: no database, no Discord client, no feature flags.

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

## Not covered here

- Row 5 drives the plan through a test-local channel model in the documented
  wire order. It proves the plan supports post-before-delete; it does not test
  an executor, which is not wired to the sticky path yet.
- `decide_activity` is a precheck only. The atomic `claim_sticky_post` race
  (one winner per window) is verified by the ignored live store suite
  `crates/core/tests/sticky_store_live.rs` against agent-testdb.
- Body validation, reply text and audit rows are outside this card.
