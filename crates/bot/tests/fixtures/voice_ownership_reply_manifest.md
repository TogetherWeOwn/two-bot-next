# Protected voice ownership and worker-reply acceptance handoff

## Consume this handoff

Base: `fffae20366719cbfdb79a4ed934d88416847c8df`.
Public branch: `test/voice-ownership-reply-acceptance`.
The immutable acceptance commit is recorded with the branch work product and
handoff evidence. Build on that commit in the builder's own workspace, or
cherry-pick its test/registration changes onto current main. Do not merge a
red test-only branch. The builder owns the combined implementation PR.

Protected files (SHA-256):

| File | SHA-256 |
| --- | --- |
| `crates/bot/src/voice_ownership_receipt_acceptance_tests.rs` | `c1ce2668ed26d520d598f838725d071efdcf70fc4861d8023e664840228fc5bc` |
| `crates/bot/src/voice_worker_reply_acceptance_tests.rs` | `f64e6f7b6ca4719cb2c6a4775acc2fbcec1afca1ac54d0375e16544cdfeda998` |

Test-author correction: closed-worker delivery calls the handler directly so snapshot publication cannot respawn the deliberately closed inbox.

### Backpressure constructor compatibility correction

Based on implementation head `f9f478f205477ba945569318f84ed71c098c08ad`,
public branch `test/voice-worker-backpressure-fixture` adds only
`backpressure: ActorInboxBackpressure::default()` to `stalled_runtime`'s
`GuildActor` constructor. The existing `live` snapshot, `tx` inbox, all ten
worker-reply tests and every assertion remain unchanged. The worker-reply hash
above replaces `66282c127eaf2ba1356c49d8684845ebeb63e7a10a5563f1bf27cf069e93d372`;
the ownership-receipt fixture is unchanged. Import the corrected fixture and
this manifest verbatim into the implementation branch; no separate PR is opened.

Before correction, the focused worker-reply library command below exited 101
with exactly one E0063 error: the constructor lacked `backpressure`. After the
interface-only correction, the same command passed **10/10**, and the focused
ownership-receipt library command passed **14/14** (zero ignored). Both used the
bounded Cargo wrapper, which admitted both runs without a pool refusal.
Formatting and floor guard passed. These results supersede the historical pool
refusal below for this combined head, not the original RED baseline. No full
workspace suite, deployment, live Discord or database test was run.

Do not edit, skip, or weaken these files. Return a mistaken assertion or a
needed trait-adapter correction to the test author. Preserve the hashes before
and after the implementation. The four registration lines in
`crates/bot/src/voice_rooms_tests.rs` are mergeable wiring, not a protected copy
of that large shared test file. This manifest is the only non-Rust addition.
No production implementation, CI workflow, or deployment is included.

## Contract and boundaries

The suites contain 24 behavioral tests (14 receipt/ownership tests and 10
worker-reply tests). They use the real library's command parsing, guild-role
authority, actor inboxes, room decisions, responder and permit admission.
`ReceiptStore` forwards the existing stateful persistence fixture, including
recipient intent/revision handling, and controls only `update_ownership`'s
external completion: `Ok(true)`, `Ok(false)`, error, or a held future.
`ReceiptReplies` records durable ownership at completion, not after a later
timer tick. Discord writes use the existing offline `Http` fixture. The wait
suite installs an authoritative live snapshot and an inbox that retains or
drops the actual command's oneshot sender. No network, database, live Discord,
staging, or production is used.

The acknowledgement ceiling is **2,000 ms**, matching `/limit`; a 2,100 ms
outer test guard detects an unbounded production future, and elapsed-time
assertions still enforce the 2,000 ms ceiling with paused Tokio time. Document
the production bound alongside the implementation. A warm worker timeout or
lost reply must name a transient failure and a next action, not claim the
worker is warming up/syncing or that a failed lookup proves no tracked room.

### Coverage map

- `transfer_success_observes_accepted_commit_at_reply_boundary` and
  `reclaim_success_observes_accepted_commit_at_reply_boundary`: success copy
  observes the accepted durable owner at the completion boundary.
- Error, refused-revision and stalled-commit tests cover transfer and reclaim:
  no committed-success copy; say recorded and finalizing, with a next action.
- `a_commit_released_inside_the_budget_can_report_success`: an actual held
  commit released after 100 ms can produce success; an early reply fails even
  if the commit succeeds later.
- `admin_transfer_outside_voice_identifies_the_selected_room`: guild-owner
  authority works without channel-granted permissions, and copy mentions
  `<#500>` rather than an ambiguous "this room".
- `admin_in_another_tracked_room_keeps_own_room_precedence`: **chosen permitted
  resolution is documentation correction**, not expanded command authority.
  Preserve existing tracked-own-room precedence; an admin in room 600 cannot
  transfer room 500 by naming its occupant. The builder must narrow the V2
  wording in `docs/voice-rooms.md` to describe that precedence and the fallback
  to the recipient's tracked room only when the admin has no tracked room.
  This test pins runtime behavior; it does not pretend to validate prose.
- `immediate_followup_transfer_does_not_add_an_ownership_cooldown` and
  `ownership_refusals_still_have_no_durable_or_grant_side_effects`: no new
  transfer cooldown; preserve original-creator, owner, bot-recipient,
  recipient-presence and channel-grant-versus-guild-admin rules.
- `succession_logs_previous_and_next_owner_once_at_info_with_bounded_counter`:
  choose the earliest-present human, retain original creator, emit one INFO
  transition with `previous_owner_id` and `next_owner_id`, and increment the
  existing fixed-vocabulary succession counter without identity labels.
  Repeated reconciliation must not emit another transition. Metric growth is
  a lower bound because the process-global recorder is shared with other tests.
- Five separately named `*_wait_is_bounded_with_a_live_unanswered_inbox` tests
  cover ownership, privacy, vote start, ballot and room lookup. The unrelated
  setup/status wait is deliberately not counted as one of these five.
- Lost, stalled and closed worker-copy tests cover ownership, privacy, ballot,
  vote room lookup and vote start. The fixture retains the oneshot sender for
  a true stall, rather than accidentally testing sender cancellation.
- `delivered_worker_results_are_not_replaced_with_timeout_copy`: a delivered
  worker result reaches the caller unchanged.
- `worker_timeouts_return_responder_permits_and_ignore_late_success`: fill the
  responder lane using distinct interaction IDs and authors, expire its waits,
  recover every permit, reject late old success and admit a new ping.

## Focused commands

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot --lib ownership_receipt_acceptance -- --test-threads=1
python3 scripts/cargo_cache.py run -- test -p two-bot --lib worker_reply_acceptance -- --test-threads=1
cargo fmt --all -- --check
python3 scripts/ci/check-test-inventory.py
python3 scripts/ci/check-floor-guard.py --base-ref origin/main
python3 scripts/check-secret-debug.py --test
python3 scripts/check-secret-debug.py
```

## Observed evidence and limits

On unmodified production source at the base above, the library-target receipt
suite compiled and ran: **3 passed, 11 failed, 0 ignored** (exit 101). These are
behavioral failures, not compiler errors:

- Transfer/reclaim success replies observed the old durable owner (300 versus
  expected 301 for transfer; 301 versus expected 300 for reclaim).
- Error, refused-revision and held-commit cases emitted optimistic success
  instead of recorded/finalizing copy.
- Releasing a held commit inside the budget still exposed the pre-commit owner
  at the reply boundary.
- Remote admin reply said "this room" without identifying room 500.
- Succession emitted only the aggregate reconcile event, without previous/next
  owner fields.

The three green controls were own-room admin precedence, immediate follow-up
transfer, and unchanged ownership refusals. Full raw output is attached to the
handoff record, not embedded here with machine-local build paths.

The worker-reply suite's execution attempts were **refused by the bounded
Cargo pool, exit 75**, with `no idle, below-budget slot; no per-worktree
fallback`. No behavioral RED is claimed for its ten tests. Its initial version
was included in the successful library compilation; the subsequent fixture
change to use distinct authors in the saturation case has not been recompiled.
A binary-target attempt ran **zero tests** and is explicitly not acceptance
evidence. No alternate target/cache, direct Cargo compilation, hosted CI run,
full-workspace suite, or live-service test was used to evade the refusal.
The builder must execute both focused commands at the combined head.

Formatting, floor, inventory and secret-Debug guards passed. The pre-push
context pack may list unrelated paths from base/main drift; this handoff changes
only the two new protected suites, their four registration lines and this
manifest. Production callers are intentionally untouched by the test author.

Skills used: test-driven-development, verification-before-completion, paperclip-superpowers-adapter, paperclip.
