"""Offline tests for the floor-guard HARD gate (CONSTRAINTS.md floor).

TDD: these fixtures were written before each rule. They feed synthetic
added/removed line sets, full CONSTRAINTS.md texts and miniature git repos
through the gate and assert it fails added suppressions, added skips, deleted
test files, stripped assertions and lowered thresholds. They also assert it
passes clean diffs, routed database ignores, moved assertions, recorded
exceptions, tightened thresholds and markdown prose.

Only the inline-suppression fixtures are string-concatenated: any contiguous
suppression literal in this file's own committed diff would trip the guard it
tests (the fixture strings are data, not suppressions). Rust attributes need
no such treatment because the gate only honours them at line start.
"""

import importlib.util
import os
import subprocess
import tempfile
import unittest
from datetime import date
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]

SPEC = importlib.util.spec_from_file_location(
    "floor_guard", Path(__file__).with_name("check-floor-guard.py")
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)

# Inline suppressions a real checker would honour mid-line. Concatenated so
# this file never contains the contiguous literal the guard scans for.
ESLINT_OFF = "eslint-" + "disable-next-line no-console"
NOQA = "# " + "noqa: F401"
GITLEAKS_ALLOW = "gitleaks:" + "allow"
NOSEMGREP = "nose" + "mgrep"

TODAY = date(2026, 10, 11)
SUPPORT_EXCEPTION = ("crates/*/tests/support/*.rs", "allow(dead_code)", date(2027, 10, 11))
SUPPORT_PATH = "crates/core/tests/support/tracing_capture.rs"

PATCH_ROW = "| Patch coverage | Changed lines >= 80% covered | llvm-cov | every Rust PR |\n"
REAL_PATCH_ROW = "| Patch coverage | Changed lines >= 80% covered | `cargo llvm-cov` + `scripts/ci/check-patch-coverage.py`, `check` lane | every Rust PR |\n"
MOVED_PATCH_ROW = "| Patch coverage | Changed lines >= 80% covered | `cargo llvm-cov --workspace --lib --bins --tests --features two-bot-core/db -- --include-ignored` + `scripts/ci/check-patch-coverage.py`, `rust-tests` lane with disposable Postgres | every Rust PR |\n"
SPLIT_PATCH_ROW = "| Patch coverage | Changed lines >= 80% covered | `cargo llvm-cov --workspace --lib --bins --tests --features two-bot-core/db -- --include-ignored` + `scripts/ci/check-patch-coverage.py`, `patch-coverage` lane with disposable Postgres | every Rust PR |\n"
LINT_ROW = "| Lint | Zero clippy warnings | `cargo clippy --workspace --all-targets -- -D warnings` | every Rust PR |\n"
REAL_LINT_ROW = "| Lint | Zero clippy warnings | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`, `check` lane | every Rust PR |\n"
REAL_DEPENDENCIES_ROW = "| Dependencies | Nothing at high or above; advisories fail | `cargo-deny check all`, `check` lane | every PR |\n"
REAL_DOCS_ROW = "| Docs build | Zero rustdoc warnings | `cargo doc --workspace --all-features --no-deps --locked` with `RUSTDOCFLAGS: -D warnings`, `check` lane | every Rust PR |\n"
REAL_FORMAT_ROW = "| Format | `cargo fmt --all -- --check` clean | `check` lane | every PR |\n"
REAL_FLOOR_ROW = "| Floor | No waivers, skips, deleted tests, stripped assertions, lowered thresholds; `#[expect(...)]` and `cfg_attr(..., allow/expect/ignore)` count as waivers | `python3 scripts/ci/check-floor-guard.py`, `check` lane | every PR |\n"
FLOOR_RULE_ROW = "| Floor | No waivers, skips, deleted tests, stripped assertions, lowered thresholds | `python3 scripts/ci/check-floor-guard.py` | every PR |\n"
ENFORCED_BASE = (
    "# Constraints\n\n## Enforced with numbers\n\n"
    "| Dimension | Rule | Checked by | Runs at |\n"
    "|-----------|------|-----------|---------|\n"
    + PATCH_ROW
    + LINT_ROW
    + FLOOR_RULE_ROW
)
MEASURED_ROW = "| Project-wide line coverage | 71.3% | must not fall |\n"
EXCEPTION_ROW = (
    "| crates/*/tests/support/*.rs | `allow(dead_code)` | CI maintainers "
    "| 2027-10-11 | shared support |\n"
)
BASE = (
    "# Constraints\n\n## Enforced with numbers\n\n"
    "| Dimension | Rule | Checked by | Runs at |\n"
    "|-----------|------|-----------|---------|\n"
    + PATCH_ROW
    + "\n## Measured, not yet enforced\n\n"
    "| Metric | Today | Direction |\n"
    "|--------|-------|-----------|\n"
    + MEASURED_ROW
    + "\n## Exceptions\n\n"
    "| Path glob | Waiver | Owner | Expires | Why |\n"
    "|-----------|--------|-------|---------|-----|\n"
    + EXCEPTION_ROW
)


def added(path, *lines):
    return [(path, line) for line in lines]


class ScanTests(unittest.TestCase):
    def test_clean_diff_passes(self):
        errors = gate.scan(
            added("src/handler.rs", "fn handle() {", "    Ok(())", "}"),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_rust_allow_attribute_fails(self):
        errors = gate.scan(
            added("src/handler.rs", "#[allow(dead_code)]", "fn stale() {}"),
            [],
            [],
        )
        self.assertEqual(len(errors), 1)
        self.assertIn("silenced-checker", errors[0])
        self.assertIn("src/handler.rs", errors[0])

    def test_crate_level_allow_attribute_fails(self):
        errors = gate.scan(
            added("src/lib.rs", "#![allow(clippy::all)]"),
            [],
            [],
        )
        self.assertTrue(any("silenced-checker" in e for e in errors), errors)

    def test_expect_attribute_fails(self):
        errors = gate.scan(
            added("src/handler.rs", "#[expect(clippy::too_many_arguments)]"),
            [],
            [],
        )
        self.assertTrue(any("silenced-checker" in e for e in errors), errors)

    def test_crate_level_expect_attribute_fails(self):
        errors = gate.scan(
            added("src/lib.rs", "#![expect(dead_code)]"),
            [],
            [],
        )
        self.assertTrue(any("silenced-checker" in e for e in errors), errors)

    def test_cfg_attr_allow_fails(self):
        errors = gate.scan(
            added("src/handler.rs", "#[cfg_attr(test, allow(dead_code))]"),
            [],
            [],
        )
        self.assertTrue(any("silenced-checker" in e for e in errors), errors)

    def test_cfg_attr_ignore_fails(self):
        errors = gate.scan(
            added("src/handler.rs", '#[cfg_attr(not(feature = "db"), ignore)]'),
            [],
            [],
        )
        self.assertTrue(any("silenced-checker" in e for e in errors), errors)

    def test_prose_mention_of_cfg_attr_is_not_a_suppression(self):
        errors = gate.scan(
            added("src/handler.rs", "// cfg_attr allowlist goes through review"),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_prose_mention_of_allow_is_not_a_suppression(self):
        errors = gate.scan(
            added("src/handler.rs", "// prefer expect() over allow attributes"),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_eslint_disable_fails(self):
        errors = gate.scan(
            added("wrangler/ui.mjs", "// " + ESLINT_OFF),
            [],
            [],
        )
        self.assertTrue(any("silenced-checker" in e for e in errors), errors)

    def test_noqa_and_gitleaks_allow_fail(self):
        for marker in (NOQA, GITLEAKS_ALLOW, NOSEMGREP):
            with self.subTest(marker=marker):
                errors = gate.scan(
                    added("scripts/ci/helper.py", "token = x  " + marker),
                    [],
                    [],
                )
                self.assertTrue(
                    any("silenced-checker" in e for e in errors), errors
                )

    def test_added_ignore_attribute_fails(self):
        errors = gate.scan(
            added("crates/core/tests/store.rs", "#[ignore]", "#[test]", "fn it() {}"),
            [],
            [],
        )
        self.assertTrue(any("test-made-easier" in e for e in errors), errors)

    def test_routed_db_ignore_in_integration_target_passes(self):
        errors = gate.scan(
            added(
                "crates/core/tests/store.rs",
                '#[ignore = "requires agent-testdb or CI service"]',
                "fn it() {}",
            ),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_routed_db_ignore_in_src_test_module_passes(self):
        errors = gate.scan(
            added(
                "crates/bot/src/gateway_tests/rsvp.rs",
                '#[ignore = "requires agent-testdb or CI service"]',
            ),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_routed_db_ignore_in_tests_support_passes(self):
        errors = gate.scan(
            added(
                "crates/core/tests/support/member_moderation_backup.rs",
                '#[ignore = "requires approved agent-testdb or CI Postgres service"]',
            ),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_routed_db_ignore_outside_any_crate_target_fails(self):
        errors = gate.scan(
            added(
                "tools/harness.rs",
                '#[ignore = "requires agent-testdb or CI service"]',
            ),
            [],
            [],
        )
        self.assertTrue(any("test-made-easier" in e for e in errors), errors)

    def test_disposable_database_wording_passes_in_src(self):
        for path, line in (
            (
                "crates/core/src/community_store.rs",
                '#[ignore = "needs a disposable test database; routed to a check step"]',
            ),
            (
                "crates/bot/src/gateway_tests/voice.rs",
                '#[ignore = "requires an authorized disposable gateway test database"]',
            ),
        ):
            with self.subTest(path=path):
                errors = gate.scan(added(path, line), [], [])
                self.assertEqual(errors, [])

    def test_reasoned_ignore_without_a_routing_marker_fails(self):
        errors = gate.scan(
            added("crates/core/tests/store.rs", '#[ignore = "flaky on CI"]'),
            [],
            [],
        )
        self.assertTrue(any("test-made-easier" in e for e in errors), errors)

    def test_ignore_in_suffix_or_directory_test_module_fails(self):
        for path in (
            "crates/bot/src/onboarding_tests.rs",
            "crates/bot/src/gateway_tests/rsvp.rs",
        ):
            with self.subTest(path=path):
                errors = gate.scan(added(path, "#[ignore]"), [], [])
                self.assertTrue(any("test-made-easier" in e for e in errors), errors)

    def test_ignore_in_inline_test_module_fails(self):
        errors = gate.scan(added("crates/core/src/store.rs", "#[ignore]"), [], [])
        self.assertTrue(any("test-made-easier" in e for e in errors), errors)

    def test_added_skip_call_fails(self):
        # Concatenated like the module-level markers: the contiguous call
        # would trip the guard on this file's own committed diff.
        errors = gate.scan(
            added("wrangler/ui.test.mjs", "x" + "it('renders', () => {});"),
            [],
            [],
        )
        self.assertTrue(any("test-made-easier" in e for e in errors), errors)

    def test_rust_iterator_skip_is_not_a_test_skip(self):
        errors = gate.scan(
            added("crates/core/src/store.rs", "items.iter().skip(1).collect()"),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_exit_call_is_not_a_skip(self):
        errors = gate.scan(
            added("scripts/ci/check-x.py", "    raise SystemExit(main())"),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_semgrep_exit_pattern_is_not_a_skip(self):
        errors = gate.scan(
            added(".semgrep/rust-warn.yml", "      - pattern: process::exit(...)"),
            [],
            [],
        )
        self.assertEqual(errors, [])

    def test_deleted_test_file_fails(self):
        errors = gate.scan([], [], ["crates/core/tests/store.rs"])
        self.assertEqual(len(errors), 1)
        self.assertIn("deleted-test", errors[0])

    def test_deleted_suffix_test_file_fails(self):
        errors = gate.scan([], [], ["crates/bot/src/onboarding_tests.rs"])
        self.assertTrue(any("deleted-test" in e for e in errors), errors)

    def test_deleted_non_test_file_passes(self):
        self.assertEqual(gate.scan([], [], ["docs/old-note.md"]), [])

    def test_deleted_constraints_file_fails(self):
        errors = gate.scan([], [], ["CONSTRAINTS.md"])
        self.assertTrue(any("threshold-lowered" in error for error in errors), errors)

    def test_removed_assertion_fails(self):
        errors = gate.scan(
            [],
            [("crates/core/tests/store.rs", "    assert_eq!(got, want);")],
            [],
        )
        self.assertEqual(len(errors), 1)
        self.assertIn("assertion-removed", errors[0])

    def test_replacing_an_assertion_does_not_count_as_stripping_one(self):
        errors = gate.scan(
            added("crates/core/tests/store.rs", "    assert_eq!(got, expected);"),
            [("crates/core/tests/store.rs", "    assert_eq!(got, want);")],
            [],
        )
        self.assertEqual(errors, [])

    def test_assertion_moved_to_another_test_file_passes(self):
        errors = gate.scan(
            added("crates/core/tests/b.rs", "    assert_eq!(got, want);"),
            [("crates/core/tests/a.rs", "    assert_eq!(got, want);")],
            [],
        )
        self.assertEqual(errors, [])

    def test_assertion_stripped_from_suffix_test_file_fails(self):
        errors = gate.scan(
            [],
            [("crates/bot/src/onboarding_tests.rs", "    assert_eq!(got, want);")],
            [],
        )
        self.assertTrue(any("assertion-removed" in e for e in errors), errors)

    def test_removed_ordinary_line_passes(self):
        errors = gate.scan(
            [],
            [("crates/core/tests/store.rs", "    let got = load();")],
            [],
        )
        self.assertEqual(errors, [])

    def test_removed_assertion_outside_tests_passes(self):
        errors = gate.scan(
            [],
            [("src/handler.rs", "    assert_eq!(got, want);")],
            [],
        )
        self.assertEqual(errors, [])

    def test_active_exception_covers_its_own_waiver(self):
        errors = gate.scan(
            added(SUPPORT_PATH, "#![allow(dead_code)]"),
            [],
            [],
            exceptions=[SUPPORT_EXCEPTION],
            today=TODAY,
        )
        self.assertEqual(errors, [])

    def test_expired_exception_no_longer_covers_its_waiver(self):
        errors = gate.scan(
            added(SUPPORT_PATH, "#![allow(dead_code)]"),
            [],
            [],
            exceptions=[SUPPORT_EXCEPTION],
            today=date(2027, 10, 12),
        )
        self.assertTrue(any("silenced-checker" in e for e in errors), errors)

    def test_exception_covers_only_its_waiver_text_and_path(self):
        for path, line in (
            (SUPPORT_PATH, "#[allow(deprecated)]"),
            ("crates/core/src/tracing_capture.rs", "#![allow(dead_code)]"),
        ):
            with self.subTest(path=path, line=line):
                errors = gate.scan(
                    added(path, line),
                    [],
                    [],
                    exceptions=[SUPPORT_EXCEPTION],
                    today=TODAY,
                )
                self.assertTrue(any("silenced-checker" in e for e in errors), errors)

    def test_numbers_outside_constraints_are_not_thresholds(self):
        errors = gate.scan(
            added("docs/plan.md", "coverage fell from 80% to 70% this week"),
            [("docs/plan.md", "coverage is 80% this week")],
            [],
        )
        self.assertEqual(errors, [])

    def test_markdown_prose_may_mention_suppressions(self):
        errors = gate.scan(
            added(
                "docs/guide.md",
                "Never add #[allow(dead_code)] or #[ignore] to get green.",
            ),
            [],
            [],
        )
        self.assertEqual(errors, [])


class ThresholdTests(unittest.TestCase):
    def test_lowered_threshold_fails(self):
        errors = gate.threshold_violations(BASE, BASE.replace(">= 80%", ">= 70%"))
        self.assertEqual(len(errors), 1)
        self.assertIn("threshold-lowered", errors[0])

    def test_raised_threshold_passes(self):
        errors = gate.threshold_violations(BASE, BASE.replace(">= 80%", ">= 90%"))
        self.assertEqual(errors, [])

    def test_review_date_change_is_not_a_lowered_threshold(self):
        base = "Last reviewed: 2026-10-11.\n" + BASE
        head = "Last reviewed: 2026-11-02.\n" + BASE
        self.assertEqual(gate.threshold_violations(base, head), [])

    def test_removed_enforced_row_fails(self):
        errors = gate.threshold_violations(BASE, BASE.replace(PATCH_ROW, ""))
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_enforced_row_deleted_with_unrelated_numbered_row_fails(self):
        head = BASE.replace(
            PATCH_ROW,
            "| Docs links | Link-check timeout 90 seconds | `lychee`, `check` lane | every PR |\n",
        )
        errors = gate.threshold_violations(BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_lowered_threshold_with_history_parenthetical_fails(self):
        head = BASE.replace(">= 80% covered", ">= 50% covered (was 80%)")
        errors = gate.threshold_violations(BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_lowered_threshold_with_extra_count_fails(self):
        head = BASE.replace(">= 80% covered", ">= 50% of 100 files covered")
        errors = gate.threshold_violations(BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_lowered_threshold_with_trailing_reference_fails(self):
        head = BASE.replace(">= 80% covered", ">= 70% covered (see section 100)")
        errors = gate.threshold_violations(BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_raised_threshold_with_history_note_passes(self):
        head = BASE.replace(">= 80% covered", ">= 90% covered (was 80%)")
        self.assertEqual(gate.threshold_violations(BASE, head), [])

    def test_warn_first_graduation_passes(self):
        base = BASE.replace(
            "Changed lines >= 80% covered",
            "Changed lines >= 80% covered (WARN first)",
        )
        self.assertEqual(gate.threshold_violations(base, base.replace(" (WARN first)", "")), [])

    def test_checked_by_cell_edit_passes(self):
        head = BASE.replace(PATCH_ROW, PATCH_ROW.replace("llvm-cov", "llvm-cov, `check` job"))
        self.assertEqual(gate.threshold_violations(BASE, head), [])

    def test_checked_by_command_change_is_not_a_lowered_threshold(self):
        head = ENFORCED_BASE.replace(
            "`python3 scripts/ci/check-floor-guard.py`",
            "`uv run scripts/ci/check-floor-guard.py`",
        )
        self.assertEqual(gate.threshold_violations(ENFORCED_BASE, head), [])

    def test_checked_by_flag_addition_is_not_a_lowered_threshold(self):
        head = BASE.replace(PATCH_ROW, PATCH_ROW.replace("llvm-cov", "llvm-cov --jobs=2"))
        self.assertEqual(gate.threshold_violations(BASE, head), [])

    def test_lowered_rule_with_stray_checked_by_operator_still_fails(self):
        head = BASE.replace(
            "Changed lines >= 80% covered", "Changed lines at least 50% covered"
        ).replace("llvm-cov", "llvm-cov --fail-under-lines=90")
        errors = gate.threshold_violations(BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_lowered_percent_masked_by_reference_count_still_fails(self):
        head = BASE.replace(
            "Changed lines >= 80% covered",
            "Changed lines at least 50% covered (of >= 100 files)",
        )
        errors = gate.threshold_violations(BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_adding_warn_first_to_an_enforced_row_fails(self):
        head = ENFORCED_BASE.replace(
            "Zero clippy warnings", "Zero clippy warnings (WARN first)"
        )
        errors = gate.threshold_violations(ENFORCED_BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_adding_warn_first_to_the_floor_row_fails(self):
        head = ENFORCED_BASE.replace("lowered thresholds", "lowered thresholds (WARN first)")
        errors = gate.threshold_violations(ENFORCED_BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_runs_at_replacement_fails(self):
        head = ENFORCED_BASE.replace(LINT_ROW, LINT_ROW.replace("every Rust PR", "manual"))
        errors = gate.threshold_violations(ENFORCED_BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_checked_by_replacement_fails(self):
        head = ENFORCED_BASE.replace(
            "`cargo clippy --workspace --all-targets -- -D warnings`", "`true`"
        )
        errors = gate.threshold_violations(ENFORCED_BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_checked_by_real_row_rejects_true_even_with_check_lane(self):
        base_row = REAL_LINT_ROW.replace("every Rust PR", "every PR")
        base = ENFORCED_BASE.replace(LINT_ROW, base_row)
        head = base.replace(
            base_row,
            base_row.replace(
                "`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`",
                "`true`",
            ),
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Checked-by scope narrowed" in e for e in errors), errors)

    def test_checked_by_real_row_rejects_dropped_clippy_severity_flag(self):
        base = ENFORCED_BASE.replace(LINT_ROW, REAL_LINT_ROW)
        head = base.replace(
            "-- -D warnings`, `check` lane",
            "`, `check` lane",
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Checked-by scope narrowed" in e for e in errors), errors)

    def test_checked_by_real_row_allows_added_flags(self):
        base = ENFORCED_BASE.replace(LINT_ROW, REAL_LINT_ROW)
        head = base.replace(
            "-- -D warnings`, `check` lane",
            "-- -D warnings --message-format short`, `check` lane",
        )
        self.assertEqual(gate.threshold_violations(base, head), [])

    def test_runs_at_real_every_pr_row_cannot_narrow_to_rust_prs(self):
        base = ENFORCED_BASE + REAL_DEPENDENCIES_ROW
        head = base.replace(
            REAL_DEPENDENCIES_ROW,
            REAL_DEPENDENCIES_ROW.replace("every PR", "every Rust PR"),
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Runs-at scope narrowed" in e for e in errors), errors)

    def test_runs_at_real_every_rust_pr_row_cannot_narrow_to_nightly(self):
        base = ENFORCED_BASE.replace(LINT_ROW, REAL_LINT_ROW)
        head = base.replace(
            REAL_LINT_ROW,
            REAL_LINT_ROW.replace("every Rust PR", "nightly"),
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Runs-at scope narrowed" in e for e in errors), errors)

    def test_checked_by_real_row_rejects_subcommand_swap(self):
        base = ENFORCED_BASE.replace(LINT_ROW, REAL_LINT_ROW)
        head = base.replace(
            "`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`",
            "`cargo check --workspace --all-targets --all-features --locked -- -D warnings`",
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Checked-by scope narrowed" in e for e in errors), errors)

    def test_checked_by_real_row_rejects_dropped_deny_subcommand(self):
        base = ENFORCED_BASE + REAL_DEPENDENCIES_ROW
        for narrowed in (
            REAL_DEPENDENCIES_ROW.replace("`cargo-deny check all`", "`cargo-deny check advisories`"),
            REAL_DEPENDENCIES_ROW.replace("`cargo-deny check all`", "`cargo-deny help`"),
        ):
            with self.subTest(narrowed=narrowed):
                errors = gate.threshold_violations(base, base.replace(REAL_DEPENDENCIES_ROW, narrowed))
                self.assertTrue(any("Checked-by scope narrowed" in e for e in errors), errors)

    def test_checked_by_real_row_rejects_doc_to_check_swap(self):
        base = ENFORCED_BASE.replace(LINT_ROW, REAL_LINT_ROW) + REAL_DOCS_ROW
        head = base.replace(
            "`cargo doc --workspace --all-features --no-deps --locked`",
            "`cargo check --workspace --all-features --no-deps --locked`",
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Checked-by scope narrowed" in e for e in errors), errors)

    def test_checked_by_patch_coverage_allows_moving_to_another_lane(self):
        base = ENFORCED_BASE.replace(PATCH_ROW, REAL_PATCH_ROW)
        head = base.replace(REAL_PATCH_ROW, MOVED_PATCH_ROW)
        self.assertEqual(gate.threshold_violations(base, head), [])

    def test_checked_by_patch_coverage_allows_moving_to_patch_coverage_lane(self):
        base = ENFORCED_BASE.replace(PATCH_ROW, MOVED_PATCH_ROW)
        head = base.replace(MOVED_PATCH_ROW, SPLIT_PATCH_ROW)
        self.assertEqual(gate.threshold_violations(base, head), [])

    def test_checked_by_patch_coverage_rejects_skipping_to_patch_coverage_lane(self):
        base = ENFORCED_BASE.replace(PATCH_ROW, REAL_PATCH_ROW)
        head = base.replace(
            REAL_PATCH_ROW,
            REAL_PATCH_ROW.replace("`check` lane", "`patch-coverage` lane"),
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Checked-by scope narrowed" in e for e in errors), errors)

    def test_checked_by_patch_coverage_rejects_move_from_patch_coverage_lane(self):
        base = ENFORCED_BASE.replace(PATCH_ROW, SPLIT_PATCH_ROW)
        head = base.replace(
            SPLIT_PATCH_ROW,
            SPLIT_PATCH_ROW.replace("`patch-coverage` lane", "`worker` lane"),
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Checked-by scope narrowed" in e for e in errors), errors)

    def test_checked_by_patch_coverage_rejects_move_to_unapproved_lane(self):
        base = ENFORCED_BASE.replace(PATCH_ROW, REAL_PATCH_ROW)
        head = base.replace(
            REAL_PATCH_ROW,
            REAL_PATCH_ROW.replace("`check` lane", "`worker` lane"),
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Checked-by scope narrowed" in e for e in errors), errors)

    def test_checked_by_patch_coverage_rejects_move_from_unapproved_lane(self):
        worker_row = REAL_PATCH_ROW.replace("`check` lane", "`worker` lane")
        base = ENFORCED_BASE.replace(PATCH_ROW, worker_row)
        head = base.replace(
            worker_row,
            worker_row.replace("`worker` lane", "`rust-tests` lane"),
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Checked-by scope narrowed" in e for e in errors), errors)

    def test_checked_by_real_row_rejects_unapproved_lane_migration(self):
        base = ENFORCED_BASE.replace(LINT_ROW, REAL_LINT_ROW) + REAL_DEPENDENCIES_ROW
        head = base.replace("`cargo-deny check all`, `check` lane",
                            "`cargo-deny check all`, `rust-tests` lane")
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Checked-by scope narrowed" in e for e in errors), errors)

    def test_checked_by_real_row_rejects_dropped_lane(self):
        base = ENFORCED_BASE.replace(LINT_ROW, REAL_LINT_ROW) + REAL_DEPENDENCIES_ROW
        head = base.replace(
            "`cargo-deny check all`, `check` lane",
            "`cargo-deny check all`",
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Checked-by scope narrowed" in e for e in errors), errors)

    def test_runs_at_real_row_rejects_docs_only_qualifier(self):
        base = ENFORCED_BASE + REAL_DEPENDENCIES_ROW
        head = base.replace(
            REAL_DEPENDENCIES_ROW,
            REAL_DEPENDENCIES_ROW.replace("every PR", "every PR except docs-only"),
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Runs-at scope narrowed" in e for e in errors), errors)

    def test_runs_at_real_row_rejects_protected_branches_rewording(self):
        base = ENFORCED_BASE + REAL_FORMAT_ROW
        head = base.replace(
            REAL_FORMAT_ROW,
            REAL_FORMAT_ROW.replace("every PR", "every push to protected branches"),
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Runs-at scope narrowed" in e for e in errors), errors)

    def test_runs_at_real_row_rejects_trailing_never_qualifier(self):
        base = ENFORCED_BASE.replace(LINT_ROW, REAL_LINT_ROW).replace(FLOOR_RULE_ROW, REAL_FLOOR_ROW)
        head = base.replace(
            REAL_FLOOR_ROW,
            REAL_FLOOR_ROW.replace("| every PR |", "| every PR never |"),
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Runs-at scope narrowed" in e for e in errors), errors)

    def test_runs_at_real_row_rejects_on_main_qualifier(self):
        base = ENFORCED_BASE.replace(LINT_ROW, REAL_LINT_ROW)
        head = base.replace(
            REAL_LINT_ROW,
            REAL_LINT_ROW.replace("every Rust PR", "every Rust PR on main"),
        )
        errors = gate.threshold_violations(base, head)
        self.assertTrue(any("Runs-at scope narrowed" in e for e in errors), errors)

    def test_runs_at_rust_pr_row_rejects_qualifier_on_broader_rank(self):
        base = ENFORCED_BASE.replace(LINT_ROW, REAL_LINT_ROW)
        for narrowed in (
            "every PR never",
            "every PR on main",
            "every PR except docs-only",
            "every PR on release branches only",
        ):
            with self.subTest(narrowed=narrowed):
                head = base.replace(
                    REAL_LINT_ROW,
                    REAL_LINT_ROW.replace("every Rust PR", narrowed),
                )
                errors = gate.threshold_violations(base, head)
                self.assertTrue(any("Runs-at scope narrowed" in e for e in errors), errors)

    def test_runs_at_rust_pr_row_allows_plain_broadening_to_every_pr(self):
        base = ENFORCED_BASE.replace(LINT_ROW, REAL_LINT_ROW)
        head = base.replace(
            REAL_LINT_ROW,
            REAL_LINT_ROW.replace("every Rust PR", "every PR"),
        )
        self.assertEqual(gate.threshold_violations(base, head), [])

    def test_measured_promotion_with_flagged_checked_by_passes(self):
        measured = "| Project-wide line coverage | >= 70% on main | must not fall |\n"
        base = BASE.replace(MEASURED_ROW, measured)
        head = base.replace(measured, "").replace(
            PATCH_ROW,
            PATCH_ROW + "| Line coverage | Project-wide line coverage >= 70% | llvm-cov --jobs=2 | every PR |\n",
        )
        self.assertEqual(gate.threshold_violations(base, head), [])

    def test_enforced_row_moved_to_measured_fails(self):
        head = ENFORCED_BASE.replace(LINT_ROW, "")
        head += (
            "\n## Measured, not yet enforced\n\n"
            "| Metric | Today | Direction |\n"
            "|--------|-------|-----------|\n"
            "| Lint | Zero clippy warnings | - | watch |\n"
        )
        errors = gate.threshold_violations(ENFORCED_BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_enforced_row_reworded_fails(self):
        head = ENFORCED_BASE.replace(
            "Zero clippy warnings", "Clippy warnings are advisory"
        )
        errors = gate.threshold_violations(ENFORCED_BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_floor_row_rule_text_changed_fails(self):
        head = ENFORCED_BASE.replace(
            "No waivers, skips, deleted tests, stripped assertions, lowered thresholds",
            "Anything goes",
        )
        errors = gate.threshold_violations(ENFORCED_BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_reference_number_before_threshold_passes(self):
        head = BASE.replace(
            "Changed lines >= 80% covered",
            "Per RFC 7, changed lines >= 80% covered",
        )
        self.assertEqual(gate.threshold_violations(BASE, head), [])

    def test_measured_row_promoted_at_least_as_strict_passes(self):
        head = BASE.replace(MEASURED_ROW, "").replace(
            PATCH_ROW,
            PATCH_ROW + "| Line coverage | Project-wide line coverage >= 72% | llvm-cov | every PR |\n",
        )
        self.assertEqual(gate.threshold_violations(BASE, head), [])

    def test_measured_row_promoted_below_its_baseline_fails(self):
        head = BASE.replace(MEASURED_ROW, "").replace(
            PATCH_ROW,
            PATCH_ROW + "| Line coverage | Project-wide line coverage >= 70% | llvm-cov | every PR |\n",
        )
        errors = gate.threshold_violations(BASE, head)
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_measured_row_removed_without_promotion_fails(self):
        errors = gate.threshold_violations(BASE, BASE.replace(MEASURED_ROW, ""))
        self.assertTrue(any("threshold-lowered" in e for e in errors), errors)

    def test_removed_exception_row_passes(self):
        self.assertEqual(
            gate.threshold_violations(BASE, BASE.replace(EXCEPTION_ROW, "")),
            [],
        )

    def test_missing_head_constraints_is_left_to_the_deletion_check(self):
        self.assertEqual(gate.threshold_violations(BASE, None), [])


def git(*args, cwd, env=None):
    subprocess.run(
        ["git", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )


def head_sha(root):
    return subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=root, text=True
    ).strip()


def make_repo():
    tmp = tempfile.TemporaryDirectory()
    root = Path(tmp.name)
    environ = {
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@t",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@t",
    }
    git("init", "-b", "main", cwd=root)
    git("config", "commit.gpgsign", "false", cwd=root)
    (root / "src").mkdir()
    (root / "src" / "main.rs").write_text("fn main() {}\n")
    git("add", ".", cwd=root)
    git("commit", "-m", "base", cwd=root, env={**os.environ, **environ})
    return tmp, root, {**os.environ, **environ}


class GitPlumbingTests(unittest.TestCase):
    def rename_errors(self, old_path, new_path):
        tmp, root, env = make_repo()
        self.addCleanup(tmp.cleanup)
        source, target = root / old_path, root / new_path
        source.parent.mkdir(parents=True, exist_ok=True)
        source.write_text("#[test]\nfn it_works() { assert_eq!(1, 1); }\n")
        git("add", ".", cwd=root)
        git("commit", "-m", "test fixture", cwd=root, env=env)
        base = head_sha(root)
        target.parent.mkdir(parents=True, exist_ok=True)
        git("mv", old_path, new_path, cwd=root)
        git("commit", "-m", "rename fixture", cwd=root, env=env)
        return gate.check(root, base, head_sha(root))

    def test_rename_to_non_runnable_suffix_deletes_test(self):
        old_path = "crates/core/tests/store.rs"
        for name in ("store.md", "store.txt", "store.rs.disabled", "store.RS"):
            with self.subTest(name=name):
                errors = self.rename_errors(old_path, "crates/core/tests/" + name)
                self.assertTrue(
                    any("deleted-test" in e and old_path in e for e in errors), errors
                )

    def test_non_crate_rename_to_non_runnable_suffix_deletes_test(self):
        # Outside crates/, the integration-target clause cannot mask a
        # regression in the suffix check for the broader test-path model.
        old_path = "tools/tests/store.rs"
        errors = self.rename_errors(old_path, "tools/tests/store.txt")
        self.assertTrue(
            any("deleted-test" in e and old_path in e for e in errors), errors
        )

    def test_markdown_destination_never_preserves_test_scope(self):
        # Pin the markdown clause independently of a suffix change: the
        # guard's broad test-path heuristic also covers files under tests/.
        old_path = "crates/core/tests/store.md"
        errors = self.rename_errors(old_path, "crates/core/tests/renamed.md")
        self.assertTrue(
            any("deleted-test" in e and old_path in e for e in errors), errors
        )

    def test_integration_rename_to_test_named_source_deletes_test(self):
        old_path = "crates/core/tests/test_store.rs"
        for new_path in (
            "crates/core/src/test_store.rs",
            "crates/core/src/store_tests.rs",
            "crates/core/src/gateway_tests/store.rs",
        ):
            with self.subTest(new_path=new_path):
                errors = self.rename_errors(old_path, new_path)
                self.assertTrue(
                    any("deleted-test" in e and old_path in e for e in errors), errors
                )

    def test_rename_between_integration_tests_preserves_test(self):
        self.assertEqual(
            self.rename_errors(
                "crates/core/tests/test_store.rs", "crates/core/tests/renamed.rs"
            ),
            [],
        )

    def test_copy_status_consumes_both_paths_without_deleting_source(self):
        stream = (
            "C100\0crates/core/tests/test_store.rs\0src/test_store.rs\0"
            "M\0src/main.rs\0D\0crates/core/tests/deleted.rs\0"
            "R100\0crates/core/tests/old.rs\0crates/core/tests/new.rs\0"
        )
        with mock.patch.object(
            gate, "_git", side_effect=[
                subprocess.CompletedProcess([], 0, "", ""),
                subprocess.CompletedProcess([], 0, stream, ""),
                subprocess.CompletedProcess([], 0, "", ""),
            ]
        ):
            added_lines, removed_lines, deleted = gate.collect(ROOT, "base")
        self.assertEqual(added_lines, [])
        self.assertEqual(removed_lines, [])
        self.assertEqual(deleted, ["crates/core/tests/deleted.rs"])

    def test_git_collection_failures_raise_and_exit_2(self):
        tmp, root, _ = make_repo()
        self.addCleanup(tmp.cleanup)
        real_git = gate._git
        for command, option in (
            ("diff", "--unified=0"),
            ("diff", "--name-status"),
            ("ls-files", "--others"),
        ):
            with self.subTest(command=command, option=option):
                def failing_git(cwd, *args):
                    if args[0] == command and option in args:
                        return subprocess.CompletedProcess([], 128, "", "failed")
                    return real_git(cwd, *args)

                with mock.patch.object(gate, "_git", side_effect=failing_git):
                    with self.assertRaisesRegex(ValueError, "git .* failed.*128"):
                        gate.collect(root, "HEAD")
                    with self.assertRaises(SystemExit) as ctx:
                        gate.main(["--root", str(root), "--base-ref", "HEAD"])
                    self.assertEqual(ctx.exception.code, 2)

    def test_missing_base_raises_no_merge_base(self):
        tmp, root, _ = make_repo()
        try:
            with self.assertRaises(gate.NoMergeBase):
                gate.check(root, "origin/main")
        finally:
            tmp.cleanup()

    def test_main_exits_2_without_a_merge_base(self):
        tmp, root, _ = make_repo()
        try:
            with self.assertRaises(SystemExit) as ctx:
                gate.main(["--root", str(root), "--base-ref", "origin/main"])
            self.assertEqual(ctx.exception.code, 2)
        finally:
            tmp.cleanup()

    def test_check_flags_a_suppression_through_git(self):
        tmp, root, env = make_repo()
        try:
            (root / "src" / "main.rs").write_text(
                "#[allow(dead_code)]\nfn stale() {}\nfn main() {}\n"
            )
            git("add", ".", cwd=root)
            git("commit", "-m", "suppression", cwd=root, env=env)
            errors = gate.check(root, "HEAD~1")
        finally:
            tmp.cleanup()
        self.assertEqual(len(errors), 1)
        self.assertIn("silenced-checker", errors[0])

    def test_check_passes_a_clean_commit(self):
        tmp, root, env = make_repo()
        try:
            (root / "src" / "main.rs").write_text("fn main() {\n    println!(\"hi\");\n}\n")
            git("add", ".", cwd=root)
            git("commit", "-m", "clean", cwd=root, env=env)
            errors = gate.check(root, "HEAD~1")
        finally:
            tmp.cleanup()
        self.assertEqual(errors, [])

    def test_pull_request_head_excludes_unrelated_base_branch_changes(self):
        tmp, root, env = make_repo()
        try:
            base = head_sha(root)
            git("checkout", "-b", "pr", cwd=root)
            (root / "src" / "pr.rs").write_text("fn pr() {}\n")
            git("add", ".", cwd=root)
            git("commit", "-m", "PR change", cwd=root, env=env)
            pr_head = head_sha(root)

            git("checkout", "main", cwd=root)
            (root / "scripts").mkdir()
            waiver = "import example  # " + "noqa: E402\n"
            (root / "scripts" / "test_upstream.py").write_text(waiver)
            git("add", ".", cwd=root)
            git("commit", "-m", "unrelated base change", cwd=root, env=env)
            git("merge", "--no-ff", "pr", "-m", "PR merge", cwd=root, env=env)

            merge_sha = head_sha(root)
            merge_parent = subprocess.check_output(
                ["git", "rev-parse", "HEAD^1"], cwd=root, text=True
            ).strip()
            merge_errors = gate.check(root, base, merge_sha)
            first_parent_errors = gate.check(root, merge_parent, merge_sha)
            pr_errors = gate.check(root, base, pr_head)
        finally:
            tmp.cleanup()
        self.assertTrue(any("silenced-checker" in error for error in merge_errors))
        self.assertEqual(first_parent_errors, [])
        self.assertEqual(pr_errors, [])

    def test_exception_recorded_on_base_covers_a_pr_waiver(self):
        tmp, root, env = make_repo()
        try:
            (root / "CONSTRAINTS.md").write_text(BASE)
            git("add", ".", cwd=root)
            git("commit", "-m", "constraints", cwd=root, env=env)
            base = head_sha(root)

            git("checkout", "-b", "pr", cwd=root)
            (root / "crates" / "core" / "tests" / "support").mkdir(parents=True)
            (root / SUPPORT_PATH).write_text("#![allow(dead_code)]\npub fn helper() {}\n")
            git("add", ".", cwd=root)
            git("commit", "-m", "PR waiver", cwd=root, env=env)
            errors = gate.check(root, base, head_sha(root), today=TODAY)
        finally:
            tmp.cleanup()
        self.assertEqual(errors, [])

    def test_exception_added_in_the_same_pr_does_not_cover_itself(self):
        tmp, root, env = make_repo()
        try:
            base = head_sha(root)
            git("checkout", "-b", "pr", cwd=root)
            (root / "CONSTRAINTS.md").write_text(BASE)
            (root / "crates" / "core" / "tests" / "support").mkdir(parents=True)
            (root / SUPPORT_PATH).write_text("#![allow(dead_code)]\npub fn helper() {}\n")
            git("add", ".", cwd=root)
            git("commit", "-m", "self-exempting PR", cwd=root, env=env)
            errors = gate.check(root, base, head_sha(root), today=TODAY)
        finally:
            tmp.cleanup()
        self.assertTrue(any("silenced-checker" in e for e in errors), errors)


if __name__ == "__main__":
    unittest.main()
