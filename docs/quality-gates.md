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
  Shortfalls and missing or empty reports fail the PR. The step runs after the
  ordinary rust-tests steps and uses `always()` so their failures do not suppress
  coverage output or get hidden by a coverage shortfall. Measurement also uses
  `--ignore-run-fail` to preserve LCOV output when tests invoked by llvm-cov fail.
  A build or report-generation failure fails this gate.
- Dependency advisories (HARD): the `cargo-deny` step (`check all`, which
  includes advisories) runs on every PR, not only Rust-selected ones.
- Rust ast-grep advisories (WARN): the local `sgconfig.yml` runs the request-path
  `.unwrap()` rule plus the six Rust detectors and positive/negative fixtures
  adapted from TogetherWeOwn/paperclip-ops-tooling `quality/meta-only/` at
  source commit `a10103b5f8c9846b8d12cc2d19c7bc2246d12349`. The additional rules
  flag byte-based UTF-8 splitting, dropped `wait_for` futures, erased `map_err`
  causes, wildcard errors collapsed to `AccessDenied`, and process-global env or
  metrics access in tests. Only findings on added lines warn; the unwrap backlog
  remains in `docs/unwrap-backlog.md`. Thanks to the shared tooling contributors.
  The corresponding Rust review checklist is available at the same source commit
  ([`rust-bot.yaml`](https://github.com/TogetherWeOwn/paperclip-ops-tooling/blob/a10103b5f8c9846b8d12cc2d19c7bc2246d12349/quality/meta-only/checklists/rust-bot.yaml));
  it is review guidance with fail-unknown evidence rules, not an automated CI gate.
- Semgrep Rust rules (WARN): vendored `.semgrep/rust-warn.yml` (local rules
  only, no registry) flags new `todo!`/`unimplemented!`, empty `.expect("")`,
  `panic!` in non-test code and `process::exit` in library code; current
  backlog is 41 `panic!` hits below.
- Squawk migration lint (WARN): `scripts/ci/check-squawk-migrations.py`
  flags risky statements (`adding-required-field`, `creating-index`,
  `adding-check-constraint`, `changing-column-type`, `dropping-object`) in
  changed migration files only. Findings annotate the PR and pass; broken
  tooling fails so blindness cannot hide. Promotion to HARD flips the step
  to `--strict` after the advisory soak; see `docs/migrations.md`.
- Voice synthetic required (HARD): `voice-synthetic.yml` fails when the
  staging test-bot token is missing unless explicitly opted out; see
  `docs/voice-synthetic.md`.
- Test-target inventory (HARD): `scripts/ci/check-test-inventory.py` fails
  when a test or fuzz target is built by no CI job; exemptions carry a reason
  in the script.
- Floor guard (HARD): `scripts/ci/check-floor-guard.py` fails when a diff
  adds a checker waiver, adds a test skip, deletes a test file, drops
  assertions from test code, or lowers a number in `CONSTRAINTS.md`. Two
  waivers are allowed: a database ignore that names the agent test DB (or a
  disposable test database) in an integration target or a crate `src` tree,
  and a row in the `CONSTRAINTS.md` Exceptions table. `#[expect(...)]` and
  `cfg_attr(..., allow/expect/ignore)` count as waivers. The
  guard reads that table from the base branch, so a PR cannot exempt itself.

## Semgrep backlog (41 `panic!` hits, WARN)

- `crates/bot/src/gateway.rs`: 12
- `crates/core/src/onboarding.rs`: 7
- `crates/bot/src/backup_cli.rs`: 3
- `crates/core/src/rsvp_store.rs`: 2
- `crates/cutover/src/parse.rs`: 2
- `crates/cutover/src/voice_config_apply.rs`: 2
- `crates/testsupport/src/lib.rs`: 2
- `crates/bot/src/dispatch.rs`, `gateway_failure.rs`, `logging.rs`,
  `crates/core/src/channel_moderation_store.rs`, `community_store.rs`,
  `disable_preflight.rs`, `inactivity_store.rs`, `internal_actions.rs`,
  `presence_store.rs`, `settings.rs`, `voice_rooms.rs`: 1 each
