# Quality gates

Deterministic CI gates (no reviewer needed). WARN reports without failing;
HARD fails the PR. All Rust gates run as steps of the `check` lane in
`check.yml` on the existing routed runner, so no new runner minutes beyond
the step itself.

- Patch coverage (WARN): `cargo llvm-cov --workspace --lib` plus
  `scripts/ci/check-patch-coverage.py` requires 80% line coverage over the
  lines the PR adds. Shortfalls annotate the PR and pass.
- Dependency advisories (HARD): the `cargo-deny` step (`check all`, which
  includes advisories) runs on every PR, not only Rust-selected ones.
- Request-path unwrap (HARD): the local `sgconfig.yml` runs the request-path
  `.unwrap()` rule, and `scripts/ci/check-unwrap-request-paths.py` fails the
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

## Semgrep backlog (41 `panic!` hits, grandfathered until 2027-01-15)

Grandfathered on the HARD promotion: these pre-existing hits stay silent
(the gate only fails on lines the diff adds) until 2027-01-15. Work them
down or renew the date with a reason; never silently delete the list to get
green. Counts below are the pinned snapshot; re-pin with real semgrep
output if they drift.

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
