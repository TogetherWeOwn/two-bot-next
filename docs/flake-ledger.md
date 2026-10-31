# Flake ledger

Fixed test flakes: what was nondeterministic, why, and what changed.
Each entry names the failing tests, the shared state, and the evidence.
No retries, no `#[ignore]`.

## Nextest pilot verdict (2026-10-15): PROMOTE to required

Five advisory ledgers (15,112 test executions, ~3,020 tests each): 0
fail-then-pass flakes, 0 skips, 3 terminal failures — every red ran both
attempts and failed both (deterministic, `flaky=false`). The three reds
were real breakage the required gate agreed with: a compile break on an
in-progress branch (no JUnit, lane failed closed, report job still green)
and two genuine test failures that went green after the branch fixed them.
The lane ran the lib/bins scope in ~7 min against ~12 min for the
`rust-tests` lane on the same SHAs. Verdict: required via the check.yml
`nextest` job (rust-selector gated, aggregated by `ci-ok`); red-group
ejection is the reusable lane's `quarantine` input (filters to `--skip`),
drilled on a throwaway draft PR before merge.

| Date | Tests | Shared state | Fix |
|---|---|---|---|
| 2026-10-20 | `audit_global_admission_precedes_the_late_authorization` (`rust tests`, run 39360867142) | The global cooldown is 400 ms plus a 250 ms safety pad, but the elapsed-time clock started after installation. A scheduler delay over 50 ms consumed the assertion's margin before measurement began. | Start measurement before cooldown installation, so a scheduler gap can only make the wait longer; keep the 600 ms floor and zero-remaining-pause assertion unchanged. The merged test does not add a post-installation yield. |
| 2026-10-17 | `voice_rooms::tests::create_429_waits_exactly_and_does_not_consume_the_failure_budget` (1 fail in the `rust tests` step, pass in the same job's coverage rerun) plus five exact-boundary siblings | Not shared state: a real clock inside a synthetic one. `mark_rate_limited` starts the wait at `elapsed_ms(now_ms, started)` — the actor time plus the real elapsed time to the 429 response — so the deadline is `now + elapsed + retry_after`. Tests dispatching at exactly `now + retry_after` are due only when the in-process fake HTTP round trip rounds to 0 ms; a loaded runner pushes it past 1 ms and `dispatch_one` correctly returns false. | The six bot-level tests now dispatch 1 s past the deadline (the not-yet-due assertions are unchanged: the deadline is never early). Exact deadline arithmetic stays pinned deterministically by the core queue tests (`queue_honours_429_retry_after`), which drive `mark_rate_limited` with a pure synthetic clock. |
| 2026-10-13 | `internal_action_http::tests::member_preclaim_malformed_emits_no_terminal_diagnostic`, `internal_action_http::tests::settings::unwired_membership_verb_stays_refused_with_settings_flag_on` (2 fails on the PR #910 `rust tests` run, which did not touch them) | Two layers. (1) The tracing capture is process-global and each reader slices from its own start offset, so a parallel test emitting the same `internal_action_terminal_persist_failed` / `internal_action_unknown_state_persist_failed` lines pollutes the slice (`left: 1` vs `0`). (2) Cascade: the first panic skipped its end-of-test flag restore, leaving `TWO_INTERNAL_ALLOW_ADD_MEMBER` on, which flipped the second test's `guild.add_member` verdict from 403 to 200. | (1) New `TRACING_DIAGNOSTIC_LOCK`, held for the whole test (before any flag lock) in all four tests that read or write those diagnostics; the sync helper became `#[tokio::test]` to share it. (2) The settings test now pins its required off-state explicitly (`set_moderation_flags(false)`, `set_add_member_flag(false)`) while holding the locks instead of assuming it. |

Evidence: pre-fix, 10 parallel runs of `cargo test -p two-bot --locked --bins internal_action_http` failed 5 times with exactly those 2 tests; serial runs passed. Post-fix, 50/50 parallel and 10/10 `--test-threads=1` runs pass.

## Coverage fixture storage (2026-10-20)

Run 39360867142's patch-coverage replay exhausted the disposable Postgres
service's 1 GiB tmpfs while creating a WAL segment (`pg_wal/xlogtemp.12570`),
then failed the post-suite connection check during recovery. This was a second
failure, not a coverage-percentage shortfall. The `rust-tests` fixture now
checkpoints at a 128 MB soft WAL target with a 32 MB recycling floor, down from
512/64 MB. The other disposable lanes keep their 512/64 MB budget. The 1 GiB
memory ceiling, durability defaults, suite selection and fail-closed coverage
gate are unchanged. The offline workflow test pins the
new settings, reload ordering and exact settings readback. Hosted execution
must still prove the complete coverage replay fits; `max_wal_size` is a soft
target, not a quota.
