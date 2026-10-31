# Rollback-delta reverse import — protected acceptance manifest

These files are the acceptance contract for the `rollback-delta import`
subcommand. The builder implements production code against them
**unchanged**: do not edit, rename, delete, reformat, or regenerate any
file listed below. If a hash differs, the acceptance contract changed
hands — stop and surface it to the test author instead of adapting the
tests.

| File | SHA-256 |
| --- | --- |
| `crates/cutover/tests/rollback_delta_import_acceptance.rs` | `23984797119c1f882fdca0b5fe367b14f353da7a382e36e3e84906f994bb7d1d` |
| `crates/cutover/tests/fixtures/rollback_import_valid.ndjson` | `a3b055d32d4d5ab952de9ec812ede2954cfb55235493b0162d6599cd618d73b0` |
| `crates/cutover/tests/fixtures/rollback_import_malformed.ndjson` | `f49813e642931df1959423adc9fe67c1057256aaa5f4ba86ee270e9b5e77c47c` |
| `crates/cutover/tests/fixtures/rollback_import_unknown_table.ndjson` | `7ece3ab430bc81a7a1051c0c35f524f4ec47493a9aed6e757b1ffc6e1789340e` |

Verify before and after implementing:

```sh
sha256sum crates/cutover/tests/rollback_delta_import_acceptance.rs \
  crates/cutover/tests/fixtures/rollback_import_valid.ndjson \
  crates/cutover/tests/fixtures/rollback_import_malformed.ndjson \
  crates/cutover/tests/fixtures/rollback_import_unknown_table.ndjson
```

## Pinned contract

Invocation: `rollback-delta import --file <export.ndjson> [--apply]
[--allow-live-target]` on the existing `rollback-delta` binary, target
from `TWO_DATABASE_URL` (as the report mode already does). Full contract
text lives in the header of `rollback_delta_import_acceptance.rs`.
Named tests, one per acceptance criterion:

- `export_then_import_round_trip_reproduces_measured_rows` — real
  `export_delta` output replays into an empty disposable target; the
  target holds exactly the exported rows and its own delta report
  recounts them; a preceding dry run wrote nothing; no migrations run.
- `import_malformed_file_refuses_with_named_error`,
  `import_unknown_table_refuses_with_named_error`,
  `import_row_cap_overrun_refuses_with_named_error` — exit 2, empty
  stdout, stderr naming `malformed import file` + `line <n>`,
  `unknown table` + table, `row cap exceeded` + table (cap is the
  existing `MAX_EXPORT_ROWS_PER_TABLE` = 50,000 rows per table;
  the over-cap fixture is generated in-test at cap + 1). All file
  validation is offline: it precedes any database open.
- `import_malformed_after_valid_lines_writes_nothing` — the same refusal
  on `--apply` leaves every target table at zero rows.
- `import_rerun_is_idempotent` — a second identical `--apply` exits 0
  and changes no row.
- `import_dry_run_plan_counts_rows_per_table_offline` — default dry run
  prints `{"version": 1, "mode": "dry-run", "tables": [{"table", "rows"}],
  "total_rows"}` from the file alone; `--apply` prints the same plan
  shape with `"mode": "apply"` before mutating.
- `import_apply_without_live_target_opt_in_refuses_before_connecting` —
  `--apply` to a live/unknown target without `--allow-live-target`
  refuses before connecting, naming the flag and never echoing the URL
  (the `legacy_copy::options::guarded_target` precedent). Disposable
  `agent-testdb` targets in the tests always pass both flags, which must
  be accepted.
- `import_missing_file_argument_and_unreadable_file_refuse` — missing
  `--file` names `--file`; an unreadable path names
  `cannot read import file`.

## Explicitly unpinned (builder's choice)

- Table names that are classified but never exported (unmeasurable):
  only names outside the exporter's vocabulary are pinned to refuse.
- Conflict policy when a target row already exists with different
  values (identical re-import is pinned idempotent; divergent values
  are not pinned).
- Serial primary-key (`BIGSERIAL id`) handling; the round-trip uses
  natural-PK tables only (`member_levels`, `members`, `tickets`).
- Any stdout fields beyond the pinned plan shape, internal module/API
  layout, and insert mechanics.

## Running

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test \
  python3 scripts/cargo_cache.py run -- test -p two-bot-cutover \
  --test rollback_delta_import_acceptance
```

Without `TWO_TEST_DATABASE_URL` the three database tests skip (the
house pattern); CI sets it, so all nine tests execute there. Disposable
databases only — never staging or production.
