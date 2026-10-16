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

## Current scope inventory (2026-10-15, `f6d1ad2f7`; re-verified on the HARD promotion)

There is no grandfathered production-only set: the HARD gate applies to all
new matches in scope, including inline test modules. The existing inventory
below records the observed locations; it is not an exemption from enforcement.

Every `.unwrap()` found in the rule-scope files at this snapshot sits inside
an inline `#[cfg(test)]` module: 50 hit lines across 9 files, none before its
file's test-module start. ast-grep is not installed on the controller, so this
was verified by replicating the rule globs with grep, applying the rule
ignores (`*_tests.rs`, `tests/`), and checking each hit against its file's
`#[cfg(test)]` boundary:

- `crates/bot/src/commands_cli.rs`: test mod `:278`, hits `:295+`
- `crates/bot/src/gateway_commands.rs`: test mod `:289`, hits `:383+`
- `crates/bot/src/interaction_admission.rs`: test mod `:112`, hits `:180+`
- `crates/core/src/commands.rs`: test mod `:329`, hits `:338+`
- `crates/core/src/custom_commands.rs`: test mod `:619`, hits `:868+`
- `crates/core/src/router.rs`: test mod `:742`, hits `:1665+`
- `crates/discord/src/command_registry.rs`: test mod `:231`, hits `:238+`
- `crates/discord/src/interactions.rs`: test mods `:838`/`:862`, hits `:848+`
- `crates/discord/src/lfg_interactions.rs`: test mod `:471`, hits `:497+`

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
