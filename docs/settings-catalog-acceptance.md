# Settings catalog validate-write and refresh-report acceptance

`crates/core/tests/settings_catalog_acceptance.rs` pins the public
`two_bot_core::settings` decision API (`crates/core/src/settings.rs`). It is
pure and offline: no database, no network, no process-environment reads.

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test settings_catalog_acceptance
```

CI runs it with the other integration targets (`cargo test --workspace --test '*'`).

The suite never names a key: every key under test comes from
`SETTING_CLASSES`, `HOT_WIRED`, or `ENV_ONLY_KEY_PREFIXES`, plus two
unlisted probes built without key literals. The file carries no `TWO_*`
string literals, so the container-env drift scanner
(`wrangler/test/container-env.test.ts`) keeps passing without
reclassification. Loader defaults come from empty-map calls only.

## Acceptance matrix

| # | Contract | Tests |
| --- | --- | --- |
| 1 | `classify_key` / `is_env_only_key` / `is_storable_key` partition every catalog entry per its declared class; unknown names fail closed; the `TWO_INTERNAL_` prefix refuses future gates; every catalog key under the prefix is env-only | `catalog_partition_matches_declared_classes` |
| 2 | `assert_storable_key` passes storable classes and refuses env-only and unknown keys by name | `assert_storable_key_refuses_env_only_and_unknown` |
| 3 | `validate_write` decides per key class (`ValidatedWrite` with `WriteAction::Upsert` / `Delete` for hot/cold, `WriteRefusal::EnvOnly` / `Unknown` before any SQL); the key guard runs before attribution (`MissingActor`); decoded NUL in values, arrays, and object entries is refused (`NullCharacter`) while a literal backslash escape is valid text | `validate_write_decides_per_key_class`, `validate_write_outcome_matrix` |
| 4 | `HOT_WIRED` is a writable hot subset; `SettingsCache::refresh` reports exactly the wired key as hot with env-string from/to, hot-but-unwired alongside cold, and ignored rows carrying `IgnoreReason::EnvOnly` (catalog or prefix) vs `Unknown`; ignored rows never read back through `get` or `env_snapshot`; deleting the wired row reports a hot change back to unset | `hot_wired_is_a_writable_hot_subset`, `refresh_partitions_hot_cold_and_ignored` |
| 5 | `to_env_string` renders typed values the way the environment carried them; empty-map loader defaults (`FeatureGates`, `AutomodConfig`, `ScorecardGates`, `ClassifierConfig`, `ModerationGates`, `OnboardingGates`) render without live IDs | `env_rendering_round_trips_typed_values`, `empty_map_loader_defaults_render_without_live_ids` |

## Not covered here

- The suite pins decisions, not the census: it drives whatever the catalog
  declares rather than duplicating the key list. The in-module tripwire
  (`catalogue_pins_every_key_class_in_both_directions` in `settings.rs`)
  remains the guard against silent reclassification.
- Per-key numeric ranges (feed poll 60–86400, automod repeat count 2–20)
  live in the typed loaders and would need key literals to address; they are
  covered by the loaders' own unit tests, not here.
- The sqlx seam (`crates/cutover/src/settings.rs`: transactions, audit rows,
  CAS tokens) is covered by the ignored live store suite against
  agent-testdb, not here.
