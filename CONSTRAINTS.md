# Constraints

Last reviewed: 2026-10-11 (quality-gates phase 1).

This repo's quality bar, with numbers. Read this before writing code. Do not
weaken it to make a change pass: every row names the command that produces
the verdict, and the floor guard fails the PR when the bar itself is lowered.

## Floor (always enforced, no setup required)

Checked by `scripts/ci/check-floor-guard.py` (HARD) on every PR, in milliseconds:

- No new checker waivers: Rust `#[allow(...)]` / `#![allow(...)]`,
  `#[expect(...)]`, `cfg_attr(..., allow/expect/ignore)`, `#[rustfmt::skip]`,
  JS `eslint-disable`, Python `# noqa` / `# type: ignore`,
  `nosemgrep`, `gitleaks:allow`, `Stryker disable`, and the like. A waiver the
  Exceptions table names is allowed until that row expires.
- No test made easier to get green: a newly added test skip (a bare
  `#[ignore]`, an `#[ignore = "..."]` that names neither the agent test DB
  nor a disposable test database, focused-test calls), a deleted test file,
  or assertions dropped from test code with nothing added back in the same diff.
- No lowered number in this file. Raising a threshold is silent; lowering one
  fails the PR, and so does removing a row unless the metric is promoted at
  least as strictly. The Rule cell carries the numbers, so a command or flag
  edit elsewhere on the row never reads as a lowered threshold. Graduating a
  WARN-first gate to HARD (dropping the marker) and rewording its Checked-by
  cell pass; adding the marker back fails as a demotion, and replacing the
  Checked-by or Runs-at cell wholesale fails as a narrowed scope. The guard
  also requires every Checked-by command token to remain: swapping a
  subcommand, dropping a flag, or dropping the `check` lane label fails,
  while adding flags passes. It rejects a Runs-at change from every PR to
  every Rust PR, nightly, or manual, from every Rust PR to nightly or
  manual, or any qualifier on the same or a broader rank (`except
  docs-only`, `on main`, a trailing `never` — including every Rust PR to
  every PR with such a qualifier). Plain broadening without a qualifier
  passes.
- No secrets in source (enforced by the `secret-scan` required check and
  gitleaks in the supply-chain lane, not the floor guard, so a leaked value
  never lands in a diff report).

Markdown prose may mention any of the above; mentioning a waiver in docs is
not adding one.

## Enforced with numbers

| Dimension | Rule | Checked by | Runs at |
|-----------|------|-----------|---------|
| Format | `cargo fmt --all -- --check` clean | `check` lane | every PR |
| Lint | Zero clippy warnings | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`, `check` lane | every Rust PR |
| Floor | No waivers, skips, deleted tests, stripped assertions, lowered thresholds; `#[expect(...)]` and `cfg_attr(..., allow/expect/ignore)` count as waivers | `python3 scripts/ci/check-floor-guard.py`, `check` lane | every PR |
| Patch coverage | Changed lines >= 80% covered | `cargo llvm-cov` sweeps + per-suite guarded re-runs, merged with `llvm-cov report --workspace`, + `scripts/ci/check-patch-coverage.py`, `patch-coverage` lane with disposable Postgres (see docs/quality-gates.md) | every Rust PR |
| Dependencies | Nothing at high or above; advisories fail | `cargo-deny check all`, `check` lane | every PR |
| Worker dependencies | Nothing at high or above; audit errors fail | `npm audit --audit-level=high --include=dev --json`, `worker check` lane | every worker PR, full main and scheduled runs |
| Request-path unwrap | No new `.unwrap()` in route/handler/interaction/command/webhook files | ast-grep rule + `scripts/ci/check-unwrap-request-paths.py`, `check` lane | every Rust PR |
| Rust pitfalls | No new `todo!`/`unimplemented!`, empty `.expect("")`, `panic!` in non-test code, `process::exit` in library code | vendored `.semgrep/rust-warn.yml` + `scripts/ci/check-semgrep-warn.py`, `check` lane | every Rust PR |
| Test inventory | Every test/fuzz target is built or run by some CI job | `scripts/ci/check-test-inventory.py`, `check` lane | every PR |
| Docs build | Zero rustdoc warnings | `cargo doc --workspace --all-features --no-deps --locked` with `RUSTDOCFLAGS: -D warnings`, `check` lane | every Rust PR |

Why 80% patch coverage: high enough to force a test for new logic, low
enough to let a config line or a rename through. Patch coverage is HARD;
unwrap and Semgrep are HARD from the WARN-first soak; unwrap has no
 grandfathered production-only set, and pre-existing Semgrep findings are
 grandfathered through 2027-01-15 (docs/quality-gates.md). Existing request-path
 unwrap findings are catalogued in
docs/unwrap-backlog.md. Below high-or-above for dependencies is mostly noise,
so the advisory scan stays HARD from day one.

## Measured, not yet enforced

| Metric | Today | Direction |
|--------|-------|-----------|
| Project-wide line coverage | baseline: first green `main` run of the coverage step | must not fall |

Promote a metric by keeping its name in the first column. A renamed promotion
is accepted only when the old row has a number and the new enforced row is at
least as strict.

## Exceptions

A waiver the floor does not allow can land only with a row here. The guard
reads this table from the merge base, so a PR cannot exempt its own waiver:
the row must already be on `main`. Each row needs a path glob, the waiver
text, an owner, and an expiry date. The row stops covering on the day after
its date, and the waiver fails again until someone renews the row.

| Path glob | Waiver | Owner | Expires | Why |
|-----------|--------|-------|---------|-----|
| crates/*/tests/support/*.rs | `allow(dead_code)` | CI maintainers | 2027-10-11 | shared test-support module compiled into several test binaries, so each binary uses only part of it |
| crates/discord/tests/*.rs | `allow(dead_code)` | CI maintainers | 2027-10-11 | each integration-test binary compiles the shared `common` helper module but uses only part of it |
