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
- Voice synthetic required (HARD): `voice-synthetic.yml` fails when the
  staging test-bot token is missing unless explicitly opted out; see
  `docs/voice-synthetic.md`.
- Test-target inventory (HARD): `scripts/ci/check-test-inventory.py` fails
  when a test or fuzz target is built by no CI job; exemptions carry a reason
  in the script.
- Floor guard (HARD): `scripts/ci/check-floor-guard.py` fails when a diff
  adds a checker waiver, adds a test skip, deletes a test file, strips an
  assertion from a test that stays, or lowers a number in `CONSTRAINTS.md`.

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
