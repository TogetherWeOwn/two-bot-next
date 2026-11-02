# Guild, window and pool inputs — protected acceptance manifest

The implementation consumes these tests unchanged. Do not edit, rename, delete,
reformat or regenerate the protected file; return corrections to its author.
The immutable commit and observed verification are recorded in the handoff.
Do not merge a deliberately red test-only branch.

| Protected file | SHA-256 |
| --- | --- |
| `crates/cutover/tests/guild_window_pool_cli_acceptance.rs` | `8d6401ed22d9dbe45ef9a48b7644d1ccc37e49100fdbbc1523b36d859457ee1f` |

Read-only fixture dependency, already protected by the rollback import contract:
`crates/cutover/tests/fixtures/rollback_import_valid.ndjson`, SHA-256
`a3b055d32d4d5ab952de9ec812ede2954cfb55235493b0162d6599cd618d73b0`.
This handoff does not change that fixture or claim ownership of it.

## Behavioral contract

The test target runs the real Cargo-built `capture`, `backfill-messages`,
`backfill` and `rollback-delta` binaries. It clears every child's environment,
uses only synthetic token/guild values, and supplies an invalid database URL.
That URL fails before any socket opens. Dry run is **not** the safety boundary:
the tools normally open databases in dry run too.

- `capture` and `backfill-messages` reject malformed/noncanonical env guilds,
  including signed or zero-padded live-guild aliases, before admission or
  database bootstrap. Bare `--allow-live-guild` does not legalize malformed IDs.
- The canonical live guild retains its existing exact refusal and requires the
  existing bare opt-in. Valid guilds, with the opt-in where required, still
  reach the redacted admission-authority failure for the poison URL.
- Malformed, empty or missing `--since` refuses rather than removing the scan
  bound. Absent `--since`, UTC/fractional timestamps and a timezone-offset
  timestamp remain accepted by `backfill-messages`.
- `--max-pages` refuses empty, missing, noninteger, zero, negative and enormous
  values in both scan tools, in both separate/equal forms. Existing defaults
  remain 25 (`backfill`) and 60 (`backfill-messages`).
- `TWO_DB_POOL_MAX` refuses unusable values before consumer bootstrap in all
  four tools, including the separate `rollback-delta import --apply` path.
  Absent values and explicit 1/5 preserve the old bootstrap/offline-plan path.
- Refusals exit 2 with empty stdout and start `input error: `. They name the
  affected field and contain a nonempty machine token `code=<stable-code>`
  plus `next_action=<corrective action>`. The code/action spelling is the
  builder's choice. Diagnostics for different bad values of the same family
  are identical, and synthetic credential/input sentinels never appear.
- `--help` exits 0, writes stdout only and needs no environment. Each applicable
  help lists the field and its inclusive integer range in `1..=N` notation.
  The builder chooses finite upper bounds below `u32::MAX`; page bounds must
  admit 60, pool bounds must admit 5. Tests read those declared bounds, accept
  both endpoints and reject N+1. No arbitrary new upper limit is frozen here.
  Page help also discloses the unchanged default; message help lists `--since`.

The enormous-page and declared-boundary tests protect the public input path
against reaching overflow-prone hint arithmetic. They do not directly exercise
an internal hint-rendering branch: reaching scan completion would require a
real database/admission authority. Preserve the implementation requirement to
use saturating hint math in **both** scan tools, and add a focused arithmetic
unit test in the implementation if extracting a helper. This suite neither
claims a saturated hint was observed nor introduces an otherwise-unused API
solely to test an unreachable value after range validation.

## Running

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover \
  --test guild_window_pool_cli_acceptance --test safety_flags_cli --no-fail-fast
cargo fmt --all -- --check
python3 scripts/ci/check-floor-guard.py --base-ref origin/main
python3 scripts/ci/check-test-inventory.py
sha256sum crates/cutover/tests/guild_window_pool_cli_acceptance.rs
```

No database variable, network fixture, ignored test or registration shim is
needed. Cargo discovers this integration target automatically; the existing
workspace integration/coverage lane runs it. A refused Cargo lease is infra
refusal, never behavioral RED. The builder owns the combined implementation PR,
all green checks, independent review and the normal merge path.
