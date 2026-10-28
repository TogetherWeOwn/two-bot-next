# Quality gates

Deterministic CI gates (no reviewer needed). WARN reports without failing;
HARD fails the PR. Gates run in their designated lanes in `check.yml` on the
existing routed runner. Patch coverage is sharded: `coverage-plan` selects the
legs, the `coverage-shard` matrix runs them alongside per-shard disposable
Postgres services, and `patch-coverage` merges and gates — rather than the
lint-only `check` lane.

- Patch coverage (HARD): `cargo llvm-cov --workspace --lib --bins --tests
  --features two-bot-core/db -- --include-ignored` plus
  `scripts/ci/check-patch-coverage.py` requires 80% line coverage over the
  lines the PR adds. Measurement uses the coverage shards' disposable
  Postgres services and test databases so ignored database-backed tests can run.
  Non-ignored tests run at full parallelism; ignored tests run serially
  (`--test-threads=1`): the ignored stores share tables and locks, so
  parallel runs race. The reference-docs regen is skipped: it rewrites
  committed docs and must never run in CI. Suites whose
  guards accept only the `agent_test` database are re-measured with their
  accepted URL (`cargo llvm-cov --no-report` per suite, merged with a final
  `cargo llvm-cov report --workspace --lcov` — without `--workspace` the
  report scopes to the root package only and exports an empty report): the
  channel-moderation, feeds-store,
  legacy-verify and staging-migrate suites under the exact `agent_test` URL,
  the internal-events, raid-list and reengagement-list suites under the
  empty-password form, and member-moderation behind `MEMBER_TESTDB`.
  Shortfalls and missing or empty reports fail the PR. Diff paths Git
  C-quotes (non-ASCII or escaped file names) are decoded to the same
  repository-relative names the LCOV report uses, so they count like any
  other changed file; a diff header the parser cannot read fails closed
  instead of dropping the file from the denominator. The legs run in their own
  sharded jobs so the unit-test signal and the coverage verdict fail
  independently, each legs step uses `always()` so a failure never suppresses
  coverage output, and the merge refuses anything but exactly N-of-N valid
  shard reports. The merge converts absolute container `SF:` paths to
  repository-relative paths with `--source-root /__w/two-bot-next/two-bot-next`;
  its own checkout root and already-relative paths are also accepted. Unknown
  absolute roots and parent traversal fail closed, rather than silently
  producing an uncovered verdict on the host runner's different checkout root.
  Shard uploads overwrite the same shard's earlier artifact on a failed-job
  re-run. Measurement also uses
  `--ignore-run-fail` to preserve LCOV output when tests invoked by llvm-cov fail.
  A build or report-generation failure fails this gate. Each shard probes the
  compiler cache before measurement with a 30-second limit. If its backend cannot
  start, the shard warns and clears `RUSTC_WRAPPER` so the same coverage commands
  compile directly; no test or report is skipped. Cache installation and checksum
  verification remain mandatory. Cache statistics are advisory and unavailable
  on this fallback path, not a zero-percent hit-rate measurement.
- Dependency advisories (HARD): the `cargo-deny` step (`check all`, which
  includes advisories) runs on every PR, not only Rust-selected ones.
- Request-path unwrap (HARD): the local `sgconfig.yml` runs the request-path
  `.unwrap()` rules (`no-unwrap-request-paths` for direct calls plus
  `no-unwrap-request-paths-in-macros` for `unwrap` inside macro token trees,
  which the base pattern cannot see), and
  `scripts/ci/check-unwrap-request-paths.py` fails the
  PR when the diff adds a hit. The six shared Rust detectors from
  TogetherWeOwn/paperclip-ops-tooling `quality/meta-only/` at source commit
  `a10103b5f8c9846b8d12cc2d19c7bc2246d12349` stay advisory (WARN): they flag
  byte-based UTF-8 splitting, dropped `wait_for` futures, erased `map_err`
  causes, wildcard errors collapsed to `AccessDenied`, and process-global env or
  metrics access in tests. Only findings on added lines fail or warn; the
  unwrap backlog remains in `docs/unwrap-backlog.md`. Thanks to the shared
  tooling contributors.
  The corresponding Rust review checklist is available at the same source commit
  ([`rust-bot.yaml`](https://github.com/TogetherWeOwn/paperclip-ops-tooling/blob/a10103b5f8c9846b8d12cc2d19c7bc2246d12349/quality/meta-only/checklists/rust-bot.yaml));
  it is review guidance with fail-unknown evidence rules, not an automated CI gate.
- Semgrep Rust rules (HARD): vendored `.semgrep/rust-warn.yml` (local rules
  only, no registry) fails the PR on newly added `todo!`/`unimplemented!`,
  empty `.expect("")`, `panic!` in non-test code or `process::exit` in
  library code. Pre-existing hits are grandfathered below, not deleted.
  Empty `.expect("")` is covered twice: `rust-empty-expect` for direct calls
  plus `rust-empty-expect-in-macros` (text regex, for `.expect("")` inside
  macro token trees such as `vec![x.expect("")]`, which the base pattern
  cannot see); the gate reports the macro hit on the call end line and drops
  it when the base rule already covers the line, so there are no double
  reports. Because the macro rule scans source text, it ignores comment-only
  `//` and `///` lines via an anchored `pattern-not-regex`; a trailing `//`
  on a code line can still match, as can `/* */` block comments and raw
  strings, which is the accepted tradeoff of covering macro token trees.
  There are no such hits in the tree today.
- Squawk migration lint (HARD; `dropping-object` WARN):
  `scripts/ci/check-squawk-migrations.py` flags risky statements in changed
  migration files only. `adding-required-field`, `creating-index`,
  `adding-check-constraint`, and `changing-column-type` findings fail the PR
  under `--strict`; `dropping-object` remains advisory because coordinated
  removal still requires a final `DROP TABLE` or `DROP COLUMN`. Broken tooling
  fails so blindness cannot hide. Two exact transactional event-store indexes
  have documented statement-specific waivers; see `docs/migrations.md`.
- Voice synthetic required (HARD): `voice-synthetic.yml` fails when the
  staging test-bot token is missing unless explicitly opted out; see
  `docs/voice-synthetic.md`.
- Test-target inventory (HARD): `scripts/ci/check-test-inventory.py` fails
  when a test or fuzz target is built by no CI job; exemptions carry a reason
  in the script. Ignored integration targets need executable routing
  evidence in a `run:` command (an explicit `--test <name>` argument, or an
  explicit target-list loop); a name mentioned only in display text, an
  `env:` value, a comment, an `echo` or a `--skip` argument is no route.
- Floor guard (HARD): `scripts/ci/check-floor-guard.py` fails when a diff
  adds a checker waiver, adds a test skip, deletes a test file, drops
  assertions from test code, or lowers a number in `CONSTRAINTS.md`. Two
  waivers are allowed: a database ignore that names the agent test DB (or a
  disposable test database) in an integration target or a crate `src` tree,
  and a row in the `CONSTRAINTS.md` Exceptions table. `#[expect(...)]` and
  `cfg_attr(..., allow/expect/ignore)` count as waivers. The
  guard reads that table from the base branch, so a PR cannot exempt itself.

## Scanner dependency triage (review by 2026-10-29)

The hash-pinned `scripts/ci/semgrep-requirements.txt` targets CI's CPython 3.11
on Linux x86_64 in the Bookworm job container, not the bot or Worker runtime.
Certifi 2026.10.19 fixes [GHSA-6q48-6c7w-9gw2](https://github.com/advisories/GHSA-6q48-6c7w-9gw2).
After the hash-checked install, the HARD Semgrep step runs
`python3 scripts/ci/check-semgrep-trust-store.py` against that installed
`certifi.where()` bundle before scanning. It rejects the compromised emSign ECC
Root CA - G3 by its DER SHA-256, not its name, version or PEM formatting. Missing,
invalid or empty bundles fail closed. The command is offline/read-only and emits
JSON (`semgrep-trust-store.v1`): exit 0 is clean, 1 is a compromised anchor, and 2
is an unavailable bundle, with `code`, `retryable` and `next_action` fields.

Rerun the offline negative-case and CI-wiring regressions with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_semgrep_trust_store.py' -v
```

The public-certificate fixtures `scripts/ci/fixtures/emsign-g3.pem` and
`isrg-x1.pem` are copied verbatim from the certifi 2026.7.22 wheel (SHA-256
`62f22742b58a1a33014a2b6b706588a8d7e2a88ae7bd1a6ebe8c992928483775`).
They contain no private keys. The old G3 anchor is the negative case; ISRG Root X1
is the unaffected positive control. Tests pin both certificate identities and
run the actual checker against both, separately and as a mixed bundle. Use
`python3 scripts/ci/check-semgrep-trust-store.py --bundle PATH` to replay a bundle;
CI always uses the default installed bundle so reverting the dependency pin
restores G3 and fails the required lane. This is a targeted trust-anchor regression,
not a general Python dependency advisory audit.

As of 2026-10-23, Semgrep 1.180.0 and the latest release, 1.182.1, both require
`mcp==1.29.0` and `opentelemetry-api~=1.37.0`. Those constraints exclude the
fixes for the two medium advisories:
[MCP GHSA-84m7-p3x7-pcfv](https://github.com/advisories/GHSA-84m7-p3x7-pcfv)
(fixed in 1.30.0) and
[OpenTelemetry GHSA-xcq7-p3pf-p88h](https://github.com/advisories/GHSA-xcq7-p3pf-p88h)
(fixed in 1.45.1). Keep both alerts visible; do not override dependency
constraints or remove hashes to force an incompatible graph.

Bounded mitigation: use this environment only for the existing one-shot
`semgrep scan` with vendored rules. Do not start an MCP/Streamable HTTP server
or an inbound instrumented HTTP service with it. Those are the inbound paths
these advisories require; this is a reachability assessment, not a claim that
the packages are patched. CI maintainers must re-check upstream constraints by
**2026-10-29** and upgrade the compatible graph when available. Reassess sooner
if scanner invocation or advisory scope changes. No scan gate or permission
is relaxed by this mitigation.

## Semgrep backlog (30 `panic!` hits, grandfathered until 2027-01-15)

Grandfathered on the HARD promotion: these pre-existing hits stay silent
(the gate only fails on lines the diff adds) until 2027-01-15. Work them
down or renew the date with a reason; never silently delete the list to get
green. Counts below are the pinned snapshot; re-pin with real semgrep
output if they drift.

- `crates/bot/src/gateway.rs`: 0 (converted: test assertion fallbacks now `assert!(matches!(…))` with message guards, plus three `assert!(false, …)` single-line fallbacks; identical messages, no `panic!` macro left)
- `crates/core/src/onboarding.rs`: 0 (converted: test assertion fallbacks now `assert!(matches!(…))` plus content `if let` checks; identical messages, no `panic!` macro left)
- `crates/bot/src/backup_cli.rs`: 3
- `crates/discord/src/voice_rooms.rs`: 3
- `crates/core/src/rsvp_store.rs`: 2
- `crates/cutover/src/parse.rs`: 2
- `crates/cutover/src/voice_config_apply.rs`: 2
- `crates/testsupport/src/lib.rs`: 2
- `crates/bot/src/dispatch.rs`, `gateway_failure.rs`, `logging.rs`,
  `ops_http.rs`, `crates/core/src/backup/s3.rs`,
  `channel_moderation_store.rs`, `community_store.rs`,
  `disable_preflight.rs`, `game_activity_store.rs`, `inactivity_store.rs`,
  `internal_actions.rs`, `presence_store.rs`, `settings.rs`,
  `voice_rooms.rs`, `crates/cutover/src/privacy.rs`, `staging_migrate.rs`: 1 each
