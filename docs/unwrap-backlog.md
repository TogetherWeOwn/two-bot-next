# `.unwrap()` request-path HARD gate scope

Rules: `ast-grep-rules/no-unwrap-request-paths.yml` (direct `$X.unwrap()`
calls) and `ast-grep-rules/no-unwrap-request-paths-in-macros.yml` (`unwrap`
inside macro token trees, which the base pattern cannot see because ast-grep
parses macro arguments as opaque `token_tree`). The gate fails on any added
hit reported by either rule (HARD).

Scope: Rust source (`crates/*/src`, `src`) whose path contains `route`,
`handler`, `interaction`, `command` or `webhook`, excluding test files and
test-only directories. The rules do not parse Rust module attributes, so
inline `#[cfg(test)]` modules are included: adding or touching a matching
`.unwrap()` in one of those modules fails the gate too. This is intentionally
stricter than a production-only rule; `unwrap` may be idiomatic in tests, but
those hits are not filtered from enforcement. Existing hits remain silent
unless the PR adds the hit line.

## Current scope inventory (2026-10-16, re-verified with ast-grep 0.50.0)

There is no grandfathered production-only set: the HARD gate applies to all
new matches in scope, including inline test modules. The existing inventory
below records the observed locations; it is not an exemption from enforcement.

The pinned ast-grep 0.50.0 scan reported 50 findings across 9 rule-scope
files (36 direct plus 14 inside macros, no overlapping lines); these are
scanner matches, not a count of unique source lines. Every match is inside an
inline `#[cfg(test)]` module. The per-file findings and module boundaries
were:

- `crates/bot/src/commands_cli.rs`: 2 direct + 4 in macros; test module starts at `:278`
- `crates/bot/src/gateway_commands.rs`: 4 direct; test module starts at `:289`
- `crates/bot/src/interaction_admission.rs`: 8 direct; test module starts at `:112`
- `crates/core/src/commands.rs`: 6 direct + 7 in macros; test module starts at `:329`
- `crates/core/src/custom_commands.rs`: 1 direct + 1 in macros; test module starts at `:619`
- `crates/core/src/router.rs`: 2 direct; test module starts at `:742`
- `crates/discord/src/command_registry.rs`: 7 direct + 1 in macros; test module starts at `:231`
- `crates/discord/src/interactions.rs`: 3 direct + 1 in macros; test modules start at `:838` and `:862`
- `crates/discord/src/lfg_interactions.rs`: 3 direct; test module starts at `:471`

In-macro hits by file (all inside the same inline test modules above, one
per line, start line equals end line): `crates/core/src/commands.rs`: 7
(`:354`, `:382`, `:383`, `:406`, `:407`, `:421`, `:468`),
`crates/bot/src/commands_cli.rs`: 4 (`:295`, `:299`, `:492`, `:498`),
`crates/discord/src/command_registry.rs`: 1 (`:327`),
`crates/core/src/custom_commands.rs`: 1 (`:885`),
`crates/discord/src/interactions.rs`: 1 (`:921`).

This inventory reflects the pinned ast-grep 0.50.0 scan with both
request-path rules. The test-module boundaries are descriptive only and do
not filter hits from enforcement.

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
