# `.unwrap()` request-path HARD gate scope

Rule: `ast-grep-rules/no-unwrap-request-paths.yml` (HARD: newly added hits
fail the PR). The gate fails on any added hit reported by the rule.

Scope: Rust source (`crates/*/src`, `src`) whose path contains `route`,
`handler`, `interaction`, `command` or `webhook`, excluding test files and
test-only directories. The rule does not parse Rust module attributes, so
inline `#[cfg(test)]` modules are included: adding or touching a matching
`.unwrap()` in one of those modules fails the gate too. This is intentionally
stricter than a production-only rule; `unwrap` may be idiomatic in tests, but
those hits are not filtered from enforcement. Existing hits remain silent
unless the PR adds the hit line.

## Current scope inventory (2026-10-16, re-verified at `4280f388b`)

There is no grandfathered production-only set: the HARD gate applies to all
new matches in scope, including inline test modules. The existing inventory
below records the observed locations; it is not an exemption from enforcement.

The pinned ast-grep 0.50.0 scan reported 36 findings across 9 rule-scope
files; these are scanner matches, not a count of unique source lines. Every
match is inside an inline `#[cfg(test)]` module. The per-file findings and
module boundaries were:

- `crates/bot/src/commands_cli.rs`: 2; test module starts at `:278`
- `crates/bot/src/gateway_commands.rs`: 4; test module starts at `:289`
- `crates/bot/src/interaction_admission.rs`: 8; test module starts at `:112`
- `crates/core/src/commands.rs`: 6; test module starts at `:329`
- `crates/core/src/custom_commands.rs`: 1; test module starts at `:619`
- `crates/core/src/router.rs`: 2; test module starts at `:742`
- `crates/discord/src/command_registry.rs`: 7; test module starts at `:231`
- `crates/discord/src/interactions.rs`: 3; test modules start at `:838` and `:862`
- `crates/discord/src/lfg_interactions.rs`: 3; test module starts at `:471`

This inventory reflects the pinned ast-grep 0.50.0 scan at commit
`4280f388b`. The test-module boundaries are descriptive only and do not filter
hits from enforcement.

Other rule-scope files with no hit before their inline test module
include `command_runtime.rs`, `self_role_handlers.rs`,
`command_permissions.rs`, `command_restoration.rs`, `custom_command_service.rs`,
`custom_command_store.rs`, `feature_commands.rs`, `handlers.rs`,
`command_identity.rs`, and `discord/custom_commands.rs`.

## Retired snapshot

The previous 28-hit file:line list was retired on 2026-10-12 instead of
re-pinned: every pin had drifted into an inline test module, and re-pinning
test-module lines would misdirect hardening work at non-production code. The
old pins stay in git history (`git log -p docs/unwrap-backlog.md`).
