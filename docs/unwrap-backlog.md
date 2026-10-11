# `.unwrap()` backlog in non-test request paths

Rule: `ast-grep-rules/no-unwrap-request-paths.yml` (WARN: new hits warn, never
fail). Snapshot of the 28 enforced-scope hits below, taken from `ast-grep scan`
output. Drive this list down; the gate warns on any line the diff adds.

Scope: production source (`crates/*/src`, `src`) whose path contains
`route`, `handler`, `interaction`, `command` or `webhook`, excluding test
files and test-only directories. Another ~400 `.unwrap()` calls live in test
files and `#[cfg(test)]` modules alongside these paths; `unwrap` in tests is
idiomatic and stays out of scope.

## Hits

- `crates/bot/src/commands_cli.rs:393`: `crate::website_jobs::admission_pool_with_tls(url, TlsPolicy::LocalOnly).unwrap()`
- `crates/bot/src/commands_cli.rs:447`: `desired_definitions(2222, &env).unwrap()`
- `crates/bot/src/gateway_commands.rs:382`: `GatewayCommandConfig::from_map(1, &vars).unwrap()`
- `crates/bot/src/gateway_commands.rs:389`: `GatewayCommandConfig::from_map(1, &vars).unwrap()`
- `crates/bot/src/gateway_commands.rs:580`: `trigger.as_deref().unwrap()`
- `crates/bot/src/gateway_commands.rs:620`: `serde_json::from_slice(&posts[0].body).unwrap()`
- `crates/core/src/commands.rs:334`: `crate::voice_rooms::voice_commands()` (`.find(...).unwrap()` follows)
- `crates/core/src/commands.rs:338`: `serde_json::to_value(&definition).unwrap()`
- `crates/core/src/commands.rs:339`: `wire["options"]` (two findings: `.as_array().unwrap()`, `.find(...).unwrap()`)
- `crates/core/src/commands.rs:339`: `wire["options"]` (second finding on the same line)
- `crates/core/src/commands.rs:347`: `serde_json::from_value(wire).unwrap()`
- `crates/core/src/commands.rs:350`: `serde_json::to_value(&unrestricted).unwrap()`
- `crates/core/src/custom_commands.rs:867`: `router.publish_set(&[]).unwrap()`
- `crates/core/src/router.rs:1664`: `router.publish_set(&[row]).unwrap()`
- `crates/core/src/router.rs:1665`: `definitions` (`.find(...).unwrap()` follows)
- `crates/discord/src/command_registry.rs:236`: `base.as_object_mut()` (`.unwrap()` follows)
- `crates/discord/src/command_registry.rs:238`: `extra.as_object().unwrap()`
- `crates/discord/src/command_registry.rs:239`: `decode_guild_commands(&serde_json::to_vec(&vec![base]).unwrap())` (three findings on this line)
- `crates/discord/src/command_registry.rs:239`: same line, second finding
- `crates/discord/src/command_registry.rs:239`: `serde_json::to_vec(&vec![base]).unwrap()`
- `crates/discord/src/command_registry.rs:313`: `diff_commands(&[a], &[b]).unwrap()`
- `crates/discord/src/command_registry.rs:337`: `diff_commands(&[old, gone], &[new, added]).unwrap()`
- `crates/discord/src/interactions.rs:844`: `commands` (`.find(...).unwrap()` follows)
- `crates/discord/src/interactions.rs:848`: `import` (`.find(...).unwrap()` follows)
- `crates/discord/src/interactions.rs:854`: `serde_json::to_value(import).unwrap()`
- `crates/discord/src/lfg_interactions.rs:492`: `ActionExecutor::with_proxy(` (`.unwrap()` follows)
- `crates/discord/src/lfg_interactions.rs:503`: `service.admit_signup(&request(), "1", "2", 3).await.unwrap()`
- `crates/discord/src/lfg_interactions.rs:504`: `service.admit().await.unwrap()`
