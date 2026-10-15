# `.unwrap()` backlog in non-test request paths

Rule: `ast-grep-rules/no-unwrap-request-paths.yml` (HARD: newly added hits
fail the PR). The gate fails on any line the diff adds.

Scope: production source (`crates/*/src`, `src`) whose path contains
`route`, `handler`, `interaction`, `command` or `webhook`, excluding test
files, test-only directories, and lines inside inline `#[cfg(test)]` modules.
The ast-grep rule itself only ignores test files/dirs, so its raw output
still flags `.unwrap()` calls inside inline test modules; those flags are out
of production scope. `unwrap` in tests is idiomatic.

## Status: zero production hits (2026-10-15, `f6d1ad2f7`; re-verified on the HARD promotion)

Grandfathered production set is empty until 2027-01-15: there is nothing to
work down, and the gate fails on any newly added hit.

Every `.unwrap()` in a rule-scope file sits inside an inline `#[cfg(test)]`
module: 50 hit lines across 9 files, none before its file's test-module
start. ast-grep is not installed on the controller, so this was verified by
replicating the rule globs with grep, applying the rule ignores
(`*_tests.rs`, `tests/`), and checking each hit against its file's
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

Rule-scope files with no production hit and no hit before their test module
include `command_runtime.rs`, `self_role_handlers.rs`,
`command_permissions.rs`, `command_restoration.rs`, `custom_command_service.rs`,
`custom_command_store.rs`, `feature_commands.rs`, `handlers.rs`,
`command_identity.rs`, and `discord/custom_commands.rs`.

## Retired snapshot

The previous 28-hit file:line list was retired on 2026-10-12 instead of
re-pinned: every pin had drifted into an inline test module, and re-pinning
test-module lines would misdirect hardening work at non-production code. The
old pins stay in git history (`git log -p docs/unwrap-backlog.md`).
