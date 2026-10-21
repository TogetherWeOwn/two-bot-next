# Protected sticky uncertainty acceptance handoff

## Ownership and consumption

This is a deliberately RED, test-only handoff, not a standalone merge candidate.
The implementation owner consumes the immutable handoff commit, preserves the
protected corpus, and owns the combined passing PR. Do not merge these tests
alone or weaken them to match the baseline's release-on-error behavior. Test
corrections stay with the acceptance author.

Public handoff ref: `test/sticky-uncertain-delivery`.
The issue's branch/commit work products record the immutable SHA and observed
execution evidence; this file does not contain a self-referential commit hash.

Protected files:

- `crates/bot/src/sticky_uncertain_acceptance_tests.rs` — behavioral corpus,
  stateful local HTTP double, and disposable-DB helpers.
- `crates/bot/src/sticky_uncertain_acceptance_manifest.md` — this manifest.

Minimal registration (not a production change): two lines in
`crates/bot/src/command_runtime_tests.rs` include the new test module. When
transplanting onto another head, retain its existing tests and add only this
module registration; do not replace that large parent file wholesale.

## Contract and mutation targets

The real `CommandRuntime::on_message`, administrator command handler, Discord
HTTP executor, sticky store and migration chain are exercised. Only Discord is
a double: it records attempted nonces/POSTs, owns an independent live-message
map, accepts a message before losing/holding its response, and removes an
orphan only after a configured confirmed-absence response. All requests go to a
loopback listener. No live Discord or application database is used.

Twenty tests catch these breaks:

| Group | Tests | Wrong behavior caught |
| --- | ---: | --- |
| Accepted POST, response lost / timed out | 2 | Fresh runtime sends another nonce after possible acceptance. |
| HTTP 200 with missing, malformed JSON, null, empty, zero, non-snowflake, overflowing ID | 7 | Unusable success ID releases durable delivery uncertainty. |
| Record refusal + forbidden / transport-lost orphan cleanup | 2 | Cleanup attempt is treated as confirmed absence and permits a duplicate. |
| Record refusal + confirmed deletion / already-absent orphan | 2 | Overly conservative fencing prevents a proven-safe retry. |
| Worker cancellation after acceptance + expired ordinary lease | 1 | Crash/lease expiry implicitly grants another worker permission to send. |
| Administrator update / remove during in-flight POST | 2 | Obsolete completion overwrites new configuration or resurrects removed state. |
| Explicit no-send rejection | 1 | Definitive 403 unnecessarily wedges future delivery. |
| Confirmed replacement and debounce | 1 | New fence prevents ordinary success, loses retirement, or bypasses debounce. |
| Disabled automation, bot/foreign-guild/DM activity | 1 | Delivery machinery bypasses existing admission controls. |
| Structured diagnosis and no false success | 1 | Uncertainty is silent, leaks payload/credentials, or is audited as success. |

Uncertainty assertions include outbound POST/nonce count, live Discord state,
persisted previous-message identity, a newly constructed runtime with an
independent DB pool, and a direct store admission attempt at `now + 120s`.
The crash test also ages `claimed_at` by two minutes without modifying any new
uncertainty state. This is deterministic lease advancement, not a real-time
sleep or a production clock change. Cancellation is a worker-death simulation,
not an OS-process restart; new runtime and independent pool are the persistence
witnesses. The double does not implement Discord's nonce TTL or reconciliation.

The diagnostic test fixes a small structured vocabulary for this handoff:

- `code = "sticky_delivery_uncertain"`
- `retryable = false`
- `next_action = "inspect_channel_before_sticky_reset"`

The recovery action means inspect the channel and resolve/delete any possible
orphan before explicitly resetting/removing/reconfiguring the sticky. It must
not recommend waiting out the lease or blind resending. This does not authorize
a reset command, live cleanup, migration, or new platform. Operator guidance
must explain the supported safe recovery path. No high-cardinality metric labels
are introduced by this test-only branch; any implementation metrics need review.

## Run and interpret

On the persistent controller, select the documented disposable bootstrap using
`TWO_TEST_DATABASE_URL`, then run:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot sticky_uncertain_acceptance_tests -- --test-threads=1
cargo fmt --all -- --check
python3 scripts/ci/check-floor-guard.py --base-ref origin/main
python3 scripts/ci/check-test-inventory.py
```

Use only a URL accepted by `two_bot_testsupport::TestDatabase`. The helper creates,
migrates and explicitly drops a unique database per case, including assertion
failures. It does not reset the bootstrap. If the variable is absent, tests print
an explicit SKIP; that is not behavioral evidence. If configured but setup fails,
the run fails instead of silently skipping. CI's existing unit/binary step sets
this variable, so these unignored tests execute there without a new workflow.

Interpret only actual command output:

- Assertion failures after successful compilation/setup are behavioral RED.
- A compile error is not RED evidence; correct the test harness first.
- A cache refusal (exit 75) or database setup error is infrastructure refusal,
  not observed behavioral RED and not GREEN.

The baseline has older tests that explicitly require claim release after unusable
200 responses or completion failure. Those expectations conflict with this new
contract. The implementation owner must reconcile obsolete, non-protected tests;
this test-only handoff does not edit them. No migration/schema is proposed here.
If implementation adds schema support, its combined PR must add migration and
rollback-compatibility tests; applying fresh migrations here alone does not prove
rollback compatibility.

Scope excluded: implementation, live Discord, production/staging DBs, deployment,
production migration, credentials/permissions, reconciliation platform, metrics
label audit, full-workspace CI/review and a real OS-process restart proof.

Skills used: paperclip, paperclip-superpowers-adapter, test-driven-development, verification-before-completion.
