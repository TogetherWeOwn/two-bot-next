# Protected sticky uncertainty acceptance contract

## Ownership and combined change

This corpus protects the combined sticky uncertain-delivery change. The
implementation owner preserves the protected files and owns the combined PR,
passing CI, review and merge. Corrections to this corpus remain with its
acceptance author; do not weaken assertions to match implementation behavior.
A correction commit is an immutable test-only input, not a competing PR.

Protected files:

- `crates/bot/src/sticky_uncertain_acceptance_tests.rs` — behavioral corpus,
  stateful local HTTP/admission doubles, and disposable-DB helpers.
- `crates/bot/src/sticky_uncertain_acceptance_manifest.md` — this manifest.

`crates/bot/src/command_runtime_tests.rs` registers the module. Preserve that
registration and unrelated tests when consuming a correction; do not replace
the large parent file wholesale.

The combined change includes migration
`crates/cutover/migrations/0454_sticky_delivery_uncertainty.sql` and its
`crates/cutover/tests/sticky_delivery_uncertainty_migration.rs` coverage. The
migration suite checks the file contract and exercises rollback/re-application
in an owned disposable database. It demonstrates row preservation, fence-column
removal, idempotent re-application and restoration of the certain default; it
does not demonstrate safe live rollback while delivery is uncertain. The
combined change also reconciles two obsolete non-protected expectations that
released claims after unusable success responses or failed record completion.
Neither the migration suite nor those reconciled tests replaces this corpus.

## Contract and mutation targets

The real `CommandRuntime::on_message`, administrator command handler, Discord
HTTP executor, sticky store and migration chain are exercised. A local Discord
double records attempted nonces/POSTs, owns an independent live-message map,
accepts a message before losing/holding its response, and removes an orphan
only after a configured confirmed-absence response. A narrowly scoped admission
double returns typed durable admission refusals before the real executor can
send, then admits a later activity. No sticky/runtime outcome is injected.
All HTTP requests go to loopback. No live Discord or application database is
used.

Twenty-four tests protect the runtime contract and its CI execution guard:

| Group | Tests | Wrong behavior caught |
| --- | ---: | --- |
| Accepted POST, response lost / timed out | 2 | Fresh runtime sends another nonce after possible acceptance. |
| HTTP 200 with missing, malformed JSON, null, empty, zero, non-snowflake, overflowing ID | 7 | Unusable success ID releases durable delivery uncertainty. |
| Record refusal + forbidden / transport-lost orphan cleanup | 2 | Cleanup attempt is treated as confirmed absence and permits a duplicate. |
| Record refusal + confirmed deletion / already-absent orphan | 2 | Overly conservative fencing prevents a proven-safe retry. |
| Worker cancellation after acceptance + expired ordinary lease | 1 | Crash/lease expiry implicitly grants another worker permission to send. |
| Administrator update / remove during in-flight POST | 2 | Obsolete completion overwrites new configuration or resurrects removed state. |
| Explicit no-send HTTP rejection | 1 | Definitive 403 unnecessarily wedges future delivery. |
| Pre-wire admission Storage / Configuration / StaleClaim refusals | 3 | A proven no-send refusal retains uncertainty or a claim, preventing one later admitted POST. |
| Confirmed replacement and debounce | 1 | New fence prevents ordinary success, loses retirement, or bypasses debounce. |
| Disabled automation, bot/foreign-guild/DM activity | 1 | Delivery machinery bypasses existing admission controls. |
| Structured diagnosis and no false success | 1 | Uncertainty is silent, leaks payload/credentials, or is audited as success. |
| Missing database URL in CI / local subprocesses | 1 | CI silently skips acceptance coverage, or local opt-out loses its explicit SKIP. |

Uncertainty assertions include outbound POST/nonce count, live Discord state,
persisted previous-message identity, a newly constructed runtime with an
independent DB pool, and a direct store admission attempt at `now + 120s`.
The crash test also ages `claimed_at` by two minutes without modifying delivery
uncertainty. This is deterministic lease advancement, not a real-time sleep or
a production clock change. Cancellation simulates worker death, not an
OS-process restart; new runtime and independent pool witness persistence. The
double does not implement Discord's nonce TTL or reconciliation.

The three pre-wire controls pass `AdmissionError::Storage`, `Configuration`
and `StaleClaim` through the real HTTP adapter's error conversion. Each requires
zero first-attempt POSTs/deletes, unchanged previous-message identity, cleared
`delivery_uncertain`, null `claim_token`/`claimed_at`, and exactly one later
admitted POST that retires the previous message and debounces follow-up.
These are exact authored no-send refusals, not permission to treat arbitrary
`Unavailable` strings or ordinary `Timeout` as safe. The accepted-response-loss
and held-response controls remain conservative. Ambiguous admission-timeout
versus header-timeout representation is outside this bounded correction.

The diagnostic vocabulary is:

- `code = "sticky_delivery_uncertain"`
- `retryable = false`
- `next_action = "inspect_channel_before_sticky_reset"`

The recovery action means inspect the channel and resolve/delete any possible
orphan before explicitly resetting/removing/reconfiguring the sticky. It must
not recommend waiting out the lease or blind resending. This does not authorize
a reset command, live cleanup, migration, or new platform. Operator guidance
must explain the supported safe recovery path. Metrics label auditing is not
covered by this corpus.

## Run and interpret

On the persistent controller, select the documented disposable bootstrap using
`TWO_TEST_DATABASE_URL`, then run:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot sticky_uncertain_acceptance_tests -- --test-threads=1
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test sticky_delivery_uncertainty_migration -- --test-threads=1
cargo fmt --all -- --check
python3 scripts/ci/check-floor-guard.py --base-ref origin/main
python3 scripts/ci/check-test-inventory.py
```

Use only a URL accepted by `two_bot_testsupport::TestDatabase`. The helper
creates, migrates and explicitly drops a unique database per case, including
assertion failures. It does not reset the bootstrap. A missing variable fails
when `GITHUB_ACTIONS=true`; outside CI it prints an explicit SKIP, which is not
behavioral evidence. A configured connection/setup failure fails the test. The
CI-guard oracle runs an existing fixture test in isolated subprocesses with the
URL absent, checking CI failure and local SKIP without changing process-wide
environment in concurrent tests. CI's existing unit/binary step supplies the URL
and executes these unignored tests without a new workflow.

Interpret only actual command output:

- Assertion failures after successful compilation/setup are behavioral RED.
- A compile error is not RED evidence; correct the test harness first.
- A cache refusal (exit 75) or database setup error is infrastructure refusal,
  not observed behavioral RED and not GREEN.
- Formatting/static checks do not establish compilation or runtime behavior.

At correction authoring, the focused runtime command was attempted against the
documented disposable bootstrap, but the bounded cache refused it with exit 75:
`no idle, below-budget slot; no per-worktree fallback`. No behavioral RED,
compilation success, database execution, or migration round-trip result is
claimed for that attempt. The consuming change must record its actual results
on the final combined head.

Scope excluded: implementation changes by the acceptance correction, live
Discord, production/staging DBs, deployment, production migration, credentials
or permissions, a reconciliation platform, full-workspace CI/review, and a real
OS-process restart proof.
