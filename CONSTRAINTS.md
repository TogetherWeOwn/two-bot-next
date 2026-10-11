# Constraints

Last reviewed: 2026-10-11 (quality-gates phase 1).

This repo's quality bar, with numbers. Read this before writing code. Do not
weaken it to make a change pass: every row names the command that produces
the verdict, and the floor guard fails the PR when the bar itself is lowered.

## Floor (always enforced, no setup required)

Checked by `scripts/ci/check-floor-guard.py` (HARD) on every PR, in milliseconds:

- No new checker waivers: Rust `#[allow(...)]` / `#![allow(...)]`,
  `#[rustfmt::skip]`, JS `eslint-disable`, Python `# noqa` / `# type: ignore`,
  `nosemgrep`, `gitleaks:allow`, `Stryker disable`, and the like.
- No test made easier to get green: a newly added test skip (`#[ignore]`,
  focused-test calls), a deleted test file, or an assertion stripped from a
  test that stays.
- No lowered number in this file. Raising a threshold is silent; lowering one
  fails the PR.
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
| Floor | No waivers, skips, deleted tests, stripped assertions, lowered thresholds | `python3 scripts/ci/check-floor-guard.py`, `check` lane | every PR |
| Patch coverage | Changed lines >= 80% covered (WARN first) | `cargo llvm-cov` + `scripts/ci/check-patch-coverage.py`, `check` lane | every Rust PR |
| Dependencies | Nothing at high or above; advisories fail | `cargo-deny check all`, `check` lane | every PR |
| Request-path unwrap | No new `.unwrap()` in route/handler/interaction/command/webhook files (WARN first) | ast-grep rule + `scripts/ci/check-unwrap-request-paths.py`, `check` lane | every Rust PR |
| Rust pitfalls | No new `todo!`/`unimplemented!`, empty `.expect("")`, `panic!` in non-test code, `process::exit` in library code (WARN first) | vendored `.semgrep/rust-warn.yml` + `scripts/ci/check-semgrep-warn.py`, `check` lane | every Rust PR |
| Test inventory | Every test/fuzz target is built or run by some CI job | `scripts/ci/check-test-inventory.py`, `check` lane | every PR |
| Docs build | Zero rustdoc warnings | `cargo doc --workspace --all-features --no-deps --locked` with `RUSTDOCFLAGS: -D warnings`, `check` lane | every Rust PR |

Why 80% patch coverage: high enough to force a test for new logic, low
enough to let a config line or a rename through. WARN-first (like unwrap and
semgrep) because the codebase predates the gate; HARD once the backlog is
worked down. Below high-or-above for dependencies is mostly noise, so the
advisory scan stays HARD from day one.

## Measured, not yet enforced

| Metric | Today | Direction |
|--------|-------|-----------|
| Project-wide line coverage | baseline: first green `main` run of the coverage step | must not fall |

## Exceptions

None. An exception needs an owner and an expiry date in this table, and a
reason the floor guard cannot check away; until then the floor holds.
