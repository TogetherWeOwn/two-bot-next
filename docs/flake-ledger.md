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
| 2026-10-13 | `internal_action_http::tests::member_preclaim_malformed_emits_no_terminal_diagnostic`, `internal_action_http::tests::settings::unwired_membership_verb_stays_refused_with_settings_flag_on` (2 fails on the PR #910 `rust tests` run, which did not touch them) | Two layers. (1) The tracing capture is process-global and each reader slices from its own start offset, so a parallel test emitting the same `internal_action_terminal_persist_failed` / `internal_action_unknown_state_persist_failed` lines pollutes the slice (`left: 1` vs `0`). (2) Cascade: the first panic skipped its end-of-test flag restore, leaving `TWO_INTERNAL_ALLOW_ADD_MEMBER` on, which flipped the second test's `guild.add_member` verdict from 403 to 200. | (1) New `TRACING_DIAGNOSTIC_LOCK`, held for the whole test (before any flag lock) in all four tests that read or write those diagnostics; the sync helper became `#[tokio::test]` to share it. (2) The settings test now pins its required off-state explicitly (`set_moderation_flags(false)`, `set_add_member_flag(false)`) while holding the locks instead of assuming it. |

Evidence: pre-fix, 10 parallel runs of `cargo test -p two-bot --locked --bins internal_action_http` failed 5 times with exactly those 2 tests; serial runs passed. Post-fix, 50/50 parallel and 10/10 `--test-threads=1` runs pass.
