# Settings command parser acceptance

`crates/core/tests/internal_settings_acceptance.rs` pins the public
`two_bot_core::internal_settings` parser API
(`crates/core/src/internal_settings.rs`). It is pure and offline: no store, no
environment, no network.

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test internal_settings_acceptance
```

CI runs it with the other integration targets (`cargo test --workspace --test '*'`).

Spec: [internal-action-settings](internal-action-settings.md) — `SettingsCommand::parse`
validated settings.get/set shapes; values redacted from Debug; CAS
`expected_version`; wired to the `require_settings_key` / `is_storable_key`
guards.

## Acceptance matrix

| # | Contract | Tests |
| --- | --- | --- |
| 1 | `SettingsCommand::parse` accepts `settings.get` with a storable key and `settings.set` with a value plus a snowflake `updated_by` actor; `expected_version` is an optional opaque signed CAS token, omitted for legacy unconditional save | `get_accepts_a_storable_key_and_parses_as_a_read`, `set_accepts_a_value_a_snowflake_actor_and_an_optional_cas_token` |
| 2 | Unknown action, unknown/malformed/env-only keys, missing value, non-snowflake actor, NUL-containing value and non-integer `expected_version` all refuse with the documented codes (`settings_action_unknown`, `settings_key_unknown`, `settings_key_malformed`, `settings_key_env_only`, `missing_value`, `bad_updated_by`, `settings_value_nul`, `bad_expected_version`) without echoing the value; literal `\u0000` text stays accepted | `unknown_action_refuses_with_action_not_allowed`, `unknown_malformed_and_env_only_keys_refuse_with_documented_codes`, `parser_agrees_with_the_settings_key_guards`, `set_requires_a_value_and_a_snowflake_audit_actor`, `nul_values_and_non_integer_versions_refuse_without_echoing_input` |
| 3 | JSON null parses as a delete (write present, value `None`), distinct from a read (write `None`); a zero token is a valid CAS token | `null_value_parses_as_delete_distinct_from_a_read` |
| 4 | `Debug` on commands and outcomes redacts values but still names the key; the log outcome carries no value | `debug_output_redacts_values_but_keeps_the_key` |
| 5 | `SettingsOutcome` carries `result` (legacy wire shape), `outcome` (log-safe summary) and `observed_version` (0 after deletion / unset reads) | `outcomes_carry_legacy_results_and_log_safe_summaries` |

## Not covered here

- Parse-time validation only. Execution, CAS conflict semantics (409
  `version_conflict`), the revision lock, audit rows and the poll revision live
  in `two_bot_cutover::internal_settings` and are verified by the ignored live
  suites (`settings_db`, `legacy_copy_db`) against agent-testdb.
- Every `TWO_*` literal in the test is already classified in
  `wrangler/src/container-env.ts`; the drift test fails on new names.
