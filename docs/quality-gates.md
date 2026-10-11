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
- Unwrap in request paths (WARN): ast-grep rule
  `ast-grep-rules/no-unwrap-request-paths.yml` forbids new `.unwrap()` in
  non-test route/handler/interaction/command/webhook files; current hits are
  listed in `docs/unwrap-backlog.md`.
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
