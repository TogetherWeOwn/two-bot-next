# Quality gates

Deterministic CI gates (no reviewer needed). WARN reports without failing;
HARD fails the PR. Gates run in their designated lanes in `check.yml` on the
existing routed runner. Patch coverage runs in `rust-tests`, alongside the
existing disposable Postgres service, rather than the lint-only `check` lane.

- Patch coverage (HARD): `cargo llvm-cov --workspace --lib --bins --tests
  --features two-bot-core/db -- --include-ignored` plus
  `scripts/ci/check-patch-coverage.py` requires 80% line coverage over the
  lines the PR adds. Measurement uses the `rust-tests` lane's disposable
  Postgres service and test database so ignored database-backed tests can run.
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
  Shortfalls and missing or empty reports fail the PR. The step runs after the
  ordinary rust-tests steps and uses `always()` so their failures do not suppress
  coverage output or get hidden by a coverage shortfall. Measurement also uses
  `--ignore-run-fail` to preserve LCOV output when tests invoked by llvm-cov fail.
  A build or report-generation failure fails this gate.
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
  Known gap: an empty `.expect("")` inside a macro body (for example
  `vec![x.expect("")]`) is not matched by the `$X.expect("")` pattern, for
  the same macro-token-tree reason as the unwrap gate; there are no such
  hits in the tree today, so the gap is documented rather than gated.
- Squawk migration lint (HARD; `dropping-object` WARN):
  `scripts/ci/check-squawk-migrations.py` flags risky statements in changed
  migration files only. `adding-required-field`, `creating-index`,
  `adding-check-constraint`, and `changing-column-type` findings fail the PR
  under `--strict`; `dropping-object` remains advisory because coordinated
  removal still requires a final `DROP TABLE` or `DROP COLUMN`. Broken tooling
  fails so blindness cannot hide; see `docs/migrations.md`.
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

## Semgrep backlog (50 `panic!` hits, grandfathered until 2027-01-15)

Grandfathered on the HARD promotion: these pre-existing hits stay silent
(the gate only fails on lines the diff adds) until 2027-01-15. Work them
down or renew the date with a reason; never silently delete the list to get
green. Counts below are the pinned snapshot; re-pin with real semgrep
output if they drift.

- `crates/bot/src/gateway.rs`: 12
- `crates/core/src/onboarding.rs`: 7
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
  `voice_rooms.rs`, `crates/cutover/src/privacy.rs`, `staging_migrate.rs`,
  `crates/store/src/backfill_history.rs`: 1 each
