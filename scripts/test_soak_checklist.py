"""Coverage guard regressions: local fixtures only, no Discord or databases."""

from collections import Counter
import copy
import json
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

from check_soak_checklist import (check_verification, delta_rows, parity_rows, render,
                                  libtest_names, validate)

ROOT = Path(__file__).resolve().parents[1]


class SoakChecklistTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.parity = (ROOT / "docs/parity.md").read_text()
        cls.checklist = json.loads((ROOT / "docs/soak-checklist.json").read_text())

    def test_repository_coverage_and_render(self):
        counts = validate(self.parity, self.checklist)
        self.assertEqual(set(counts), set(range(1, 9)) | {12, 13})
        self.assertEqual((ROOT / "docs/soak-checklist.md").read_text(), render(self.checklist))

    def test_unnumbered_section_ends_parity_table_scope(self):
        heading = "## 2. Non-command interactions"
        changed = self.parity.replace(heading,
            "## Registry golden exceptions\n\n"
            "| Intentional difference | Matrix reference | Exact allowance |\n"
            "|---|---|---|\n"
            "| rsvp-attendance | §1 #12 / #25 | Rename only |\n\n" + heading)
        self.assertNotEqual(changed, self.parity)
        self.assertEqual(validate(changed, self.checklist), validate(self.parity, self.checklist))
        with self.assertRaisesRegex(ValueError, "must end in Map"):
            parity_rows(changed.replace("## Registry golden exceptions", "### Registry golden exceptions"))

    def test_missing_row_fails(self):
        data = copy.deepcopy(self.checklist)
        data["entries"].pop()
        with self.assertRaisesRegex(ValueError, "missing="):
            validate(self.parity, data)

    def test_new_mapped_row_fails(self):
        changed = self.parity.replace("## 9. Drops", "| Behaviour | Detail | Map |\n|---|---|---|\n|new observable | new effect | **S5** |\n\n## 9. Drops")
        with self.assertRaisesRegex(ValueError, "missing="):
            validate(changed, self.checklist)

    def test_drop_ignored_but_mixed_drop_retained(self):
        rows = parity_rows(self.parity)
        self.assertFalse(any("/rota-acknowledge" in str(row) for row in rows))
        self.assertTrue(any("community_facts" in str(row) for row in rows))
        self.assertTrue(any("Operator scripts" in str(row) for row in rows))
        changed = self.parity.replace("## 9. Drops", "| Behaviour | Detail | Map |\n|---|---|---|\n|discarded | reason | **DROP** — intentionally absent |\n\n## 9. Drops")
        self.assertEqual(set(rows), set(parity_rows(changed)))

    def test_mixed_drop_requires_coverage_in_both_orders(self):
        for mapping in (
            "**DROP** (runtime); **S6** (shape-check)",
            "**S6** (shape-check); **DROP** (runtime)",
            "**DROP** (runtime); **B4** (acceptance)",
            "**DROP** (runtime); [TOG-9881](/TOG/issues/TOG-9881)",
            "**DROP** — replaced by session persistence (**S5**)",
            "**DROP** (runtime); **NEW-42** (shape-check)",
        ):
            with self.subTest(mapping=mapping):
                changed = self.parity.replace("## 9. Drops", "| Behaviour | Detail | Map |\n|---|---|---|\n|mixed observable | mapped remainder | " + mapping + " |\n\n## 9. Drops")
                self.assertIn((8, ("mixed observable", "mapped remainder")), parity_rows(changed))
                with self.assertRaisesRegex(ValueError, "missing="):
                    validate(changed, self.checklist)

    def test_drop_only_clauses_are_excluded(self):
        changed = self.parity.replace("## 9. Drops", "| Behaviour | Detail | Map |\n|---|---|---|\n|discarded | reason | **DROP** (runtime); **DROP** (shape-check) |\n\n## 9. Drops")
        self.assertEqual(set(parity_rows(self.parity)), set(parity_rows(changed)))

    def test_duplicate_rows_and_ids_fail(self):
        data = copy.deepcopy(self.checklist)
        data["entries"].append(copy.deepcopy(data["entries"][0]))
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            validate(self.parity, data)
        data["entries"][-1]["parity"]["row"] = ["bogus"]
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            validate(self.parity, data)

    def test_changed_options_and_stale_entry_fail(self):
        changed = self.parity.replace("`member` User opt", "`member` User req", 1)
        with self.assertRaisesRegex(ValueError, "stale="):
            validate(changed, self.checklist)

    def test_repeated_surfaces_are_distinct(self):
        rows = parity_rows(self.parity)
        ready = [row for row in rows if row[0] == 3 and row[1][0] == "`ClientReady`"]
        self.assertEqual(len(ready), 2)
        data = copy.deepcopy(self.checklist)
        data["entries"] = [e for e in data["entries"] if (e["parity"]["section"], tuple(e["parity"]["row"])) != ready[0]]
        with self.assertRaisesRegex(ValueError, "Coverage mismatch"):
            validate(self.parity, data)

    def test_invalid_status_and_blank_fields_fail(self):
        for field, value in (("status", "passed"), ("action", " "), ("evidence", ""), ("expected", "")):
            with self.subTest(field=field):
                data = copy.deepcopy(self.checklist)
                data["entries"][0][field] = value
                with self.assertRaises(ValueError):
                    validate(self.parity, data)

    def test_waiver_requires_reason_and_approver_and_automation_requires_command(self):
        for status, required in (("waived", "reason"), ("waived", "approver"),
                                 ("automated", "verification")):
            data = copy.deepcopy(self.checklist)
            data["entries"][0]["status"] = status
            data["entries"][0].pop(required, None)
            with self.assertRaisesRegex(ValueError, "requires"):
                validate(self.parity, data)

    def test_every_filed_waiver_carries_reason_and_approver(self):
        waived = [e for e in self.checklist["entries"] if e["status"] == "waived"]
        self.assertGreater(len(waived), 0, "expected the first filed waivers to exist")
        for entry in waived:
            with self.subTest(entry=entry["id"]):
                self.assertTrue(entry.get("reason", "").strip())
                self.assertTrue(entry.get("approver", "").strip())

    def test_voice_requires_external_reference(self):
        data = copy.deepcopy(self.checklist)
        voice = next(e for e in data["entries"] if "VoiceStateUpdate" in str(e["parity"]))
        voice.pop("reference")
        with self.assertRaisesRegex(ValueError, "TOG-10119"):
            validate(self.parity, data)

    def test_rest_verification_propagates_either_failure(self):
        entry = next(e for e in self.checklist["entries"] if e["id"] == "s6-01")
        prefix = "python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test "
        for first, second in ((0, 0), (17, 0), (0, 23)):
            with self.subTest(first=first, second=second):
                command = entry["verification"].replace(prefix + "executor_acceptance", f"(exit {first})")
                command = command.replace(prefix + "executor_regressions", f"(exit {second})")
                self.assertNotIn("python3", command)
                # Only exit-status doubles execute: no Cargo, DB or network.
                result = subprocess.run(["sh", "-c", command], check=False)
                self.assertEqual(result.returncode, first or second)

    def test_config_prose_is_covered(self):
        data = copy.deepcopy(self.checklist)
        data["entries"] = [e for e in data["entries"] if e["parity"]["section"] != 7]
        with self.assertRaisesRegex(ValueError, "Config / env catalogue"):
            validate(self.parity, data)
        with self.assertRaisesRegex(ValueError, "config classes"):
            parity_rows(self.parity.replace("env_only", "unknown_class"))

    def test_malformed_table_and_missing_section_fail_closed(self):
        with self.assertRaisesRegex(ValueError, "malformed"):
            parity_rows(self.parity.replace("| 1 | `/rank` |", "| `/rank` |", 1))
        with self.assertRaisesRegex(ValueError, "sections 1–8"):
            parity_rows(self.parity.replace("## 8. Observable behaviours", "## 80. Observable behaviours"))
        with self.assertRaisesRegex(ValueError, "must end in Map"):
            parity_rows(self.parity.replace("| Behaviour | Detail | Map |", "| Behaviour | Detail | Mapping |"))

    def test_borderless_mapped_row_fails_closed(self):
        # A GFM body row without its leading border, appended inside the
        # §8 table scope, must fail rather than silently skip coverage.
        changed = self.parity.replace(
            "\n\n## 9. Drops", "\nnew observable | new effect | **S5** |\n\n## 9. Drops")
        with self.assertRaisesRegex(ValueError, "without leading border"):
            parity_rows(changed)
        with self.assertRaisesRegex(ValueError, "without leading border"):
            validate(changed, self.checklist)

    def test_blank_line_borderless_mapped_row_fails_closed(self):
        # Same fail-closed guarantee when a blank line separates the
        # borderless mapped row from its table.
        tail = "| Rate limits: staging-verifier 3 retries"
        changed = self.parity.replace(
            tail, tail.split("|")[0].rstrip() + "\n\nnew observable | new effect | **S5** |",
            1)
        with self.assertRaisesRegex(ValueError, "borderless table row"):
            parity_rows(changed)
        with self.assertRaisesRegex(ValueError, "borderless table row"):
            validate(changed, self.checklist)

    def test_borderless_prose_without_mapping_stays_prose(self):
        # Ordinary pipe-carrying prose outside tables must not trip the
        # fail-closed paths above.
        changed = self.parity.replace(
            "## 9. Drops", "A pipe | in ordinary prose carries no mapping.\n\n## 9. Drops")
        self.assertEqual(set(parity_rows(self.parity)), set(parity_rows(changed)))
        self.assertEqual(validate(changed, self.checklist), validate(self.parity, self.checklist))

    def test_escaped_pipe_stays_in_cell(self):
        changed = self.parity.replace("## 9. Drops", "| Behaviour | Detail | Map |\n|---|---|---|\n|new observable | one \\| two | **S5** |\n\n## 9. Drops")
        self.assertIn((8, ("new observable", "one | two")), parity_rows(changed))


WRAPPED = "python3 scripts/cargo_cache.py run -- test "
FIXTURE = {
    "Cargo.toml": """
        [workspace]
        members = ["crates/demo"]
    """,
    "crates/demo/Cargo.toml": """
        [package]
        name = "demo"
    """,
    "crates/demo/src/lib.rs": """
        //! #[test] fn doc_ghost() {}
        /* outer /* nested #[test] fn block_ghost() {} */ still a comment } */
        pub mod outer;
        #[path = "elsewhere/renamed.rs"]
        mod moved;
        const RAW: &str = r##"{ "#[test] fn raw_ghost() {}" "##;
        const OPEN: char = '{';
        fn helper<'a>(x: &'a str) -> &'a str { let _ = "} #[test] fn string_ghost"; x }
        #[cfg(test)]
        mod tests {
            #[test]
            fn unit() {}
            fn not_a_test() {
                #[test]
                fn nested_hidden() {}
            }
            proptest::proptest! {
                #[test]
                fn property(x in 0..1u8) { let _ = x; }
            }
        }
    """,
    "crates/demo/src/outer.rs": """
        mod inner;
        #[tokio::test(flavor = "current_thread")]
        async fn outer_async() {}
    """,
    "crates/demo/src/outer/inner.rs": "#[test] fn deep() {}",
    "crates/demo/src/elsewhere/renamed.rs": """
        mod sibling;
        #[test] fn moved_test() {}
    """,
    "crates/demo/src/elsewhere/sibling.rs": "#[test] fn sib() {}",
    "crates/demo/tests/smoke.rs": """
        mod support;
        #[test] fn smoke_case() {}
    """,
    "crates/demo/tests/support/mod.rs": """
        pub mod nested;
        #[test] fn support_case() {}
    """,
    "crates/demo/tests/support/nested.rs": "#[test] fn nested_case() {}",
    "crates/demo/tests/multi/main.rs": "#[test] fn multi_case() {}",
}


class AutomatedVerificationTests(unittest.TestCase):
    """Automated rows must name a real package, test target and test (offline)."""

    @classmethod
    def setUpClass(cls):
        cls.parity = (ROOT / "docs/parity.md").read_text()
        cls.checklist = json.loads((ROOT / "docs/soak-checklist.json").read_text())
        cls.fixture = tempfile.TemporaryDirectory()
        cls.root = Path(cls.fixture.name)
        for name, source in FIXTURE.items():
            (cls.root / name).parent.mkdir(parents=True, exist_ok=True)
            (cls.root / name).write_text(textwrap.dedent(source))

    @classmethod
    def tearDownClass(cls):
        cls.fixture.cleanup()

    def rewritten(self, entry_id, old, new):
        data = copy.deepcopy(self.checklist)
        entry = next(e for e in data["entries"] if e["id"] == entry_id)
        self.assertIn(old, entry["verification"])
        entry["verification"] = entry["verification"].replace(old, new)
        return data

    def test_every_automated_row_resolves(self):
        automated = [e for e in self.checklist["entries"] if e["status"] == "automated"]
        self.assertGreater(len(automated), 0)
        for entry in automated:
            with self.subTest(entry=entry["id"]):
                check_verification(entry["id"], entry["verification"])

    def test_unknown_package_fails_with_row_id(self):
        data = self.rewritten("s6-07", "-p two-bot-core", "-p two-bot-kore")
        with self.assertRaisesRegex(ValueError, r"^s6-07: unknown package -p two-bot-kore"):
            validate(self.parity, data)

    def test_missing_test_target_fails_with_row_id(self):
        data = self.rewritten("s6-07", "--test backup_transport", "--test backup_transports")
        with self.assertRaisesRegex(
                ValueError, r"^s6-07: missing test target --test backup_transports "
                            r"\(crates/core/tests/backup_transports\.rs\)"):
            validate(self.parity, data)

    def test_renamed_test_fn_fails_with_row_id(self):
        name = "guild_config_capture_plan_apply_round_trip"
        data = self.rewritten("s6-03", name, name + "_v2")
        with self.assertRaisesRegex(ValueError, rf"^s6-03: no test named '{name}_v2'"):
            validate(self.parity, data)
        # Without --exact a prefix still selects the test; with it, only the full name.
        data = self.rewritten("s6-03", name, "guild_config_capture")
        validate(self.parity, self.rewritten("s6-03", name + " -- --exact", "guild_config_capture"))
        with self.assertRaisesRegex(ValueError, r"^s6-03: no test named 'guild_config_capture'"):
            validate(self.parity, data)

    def test_renamed_lib_module_filter_fails_with_row_id(self):
        data = self.rewritten("s13-90ab4b7", "backup::dump_file", "backup::dump_files")
        with self.assertRaisesRegex(ValueError, r"^s13-90ab4b7: no test matching 'backup::dump_files'"):
            validate(self.parity, data)

    def test_every_chained_invocation_is_checked(self):
        data = self.rewritten("s6-01", "--test executor_regressions", "--test executor_regression")
        with self.assertRaisesRegex(ValueError, r"^s6-01: missing test target --test executor_regression "):
            validate(self.parity, data)

    def test_unsupported_command_shapes_fail_closed(self):
        for command, error in (
            ("cargo test -p two-bot-core --test backup_transport", "run Cargo as"),
            ("python3 scripts/cargo_cache.py run -- check -p two-bot-core", "automated verification must be `cargo test`"),
            (WRAPPED + "--test backup_transport", "name exactly one -p"),
            (WRAPPED + "-p two-bot-core -p two-bot-discord --lib", "name exactly one -p"),
            (WRAPPED + "-p two-bot-core --doc", "unsupported target selector --doc"),
            (WRAPPED + "-p two-bot-next --lib", "no tests in the selected targets"),
            ("python3 scripts/check_soak_checklist.py", "no cargo test invocation"),
        ):
            with self.subTest(command=command):
                with self.assertRaisesRegex(ValueError, f"^row: {error}"):
                    check_verification("row", command)

    def test_test_names_follow_modules_and_ignore_comments_and_literals(self):
        demo = self.root / "crates/demo"
        self.assertEqual(sorted(libtest_names(demo / "src/lib.rs")), [
            "moved::moved_test", "moved::sibling::sib", "outer::inner::deep",
            "outer::outer_async", "tests::property", "tests::unit"])
        self.assertEqual(sorted(libtest_names(demo / "tests/smoke.rs")), [
            "smoke_case", "support::nested::nested_case", "support::support_case"])

    def test_fixture_commands_resolve_by_target_and_filter(self):
        for command in (
            WRAPPED + "-p demo --lib outer::inner",
            WRAPPED + "-p demo --test smoke support::nested::nested_case -- --exact",
            WRAPPED + "-p demo --test multi && " + WRAPPED + "-p demo --tests sib",
            WRAPPED + "-p demo -- --skip unit --test-threads 1 moved_test",
        ):
            with self.subTest(command=command):
                check_verification("row", command, self.root)
        for ghost in ("doc_ghost", "block_ghost", "raw_ghost", "string_ghost", "nested_hidden"):
            with self.subTest(ghost=ghost):
                with self.assertRaisesRegex(ValueError, f"^row: no test matching '{ghost}'"):
                    check_verification("row", WRAPPED + "-p demo " + ghost, self.root)


class DeltaChecklistTests(unittest.TestCase):
    """Parity §12 additions and §13 non-dropped ledger rows (B4 gate)."""

    @classmethod
    def setUpClass(cls):
        cls.parity = (ROOT / "docs/parity.md").read_text()
        cls.checklist = json.loads((ROOT / "docs/soak-checklist.json").read_text())

    def line(self, needle):
        return next(line for line in self.parity.splitlines() if needle in line)

    def entry(self, data, entry_id):
        return next(e for e in data["entries"] if e["id"] == entry_id)

    def without_entry(self, entry_id):
        data = copy.deepcopy(self.checklist)
        data["entries"] = [e for e in data["entries"] if e["id"] != entry_id]
        return data

    def test_ledger_coverage_is_every_non_dropped_row(self):
        # Independent of the table scanner: split the delimited ledger by hand.
        ledger = self.parity.split("<!-- post-freeze-ledger:start -->")[1].split("<!-- post-freeze-ledger:end -->")[0]
        statuses = {}
        for line in ledger.splitlines():
            if line.startswith("| ["):
                cells = [c.strip() for c in line.strip("|").split("|")]
                statuses[cells[0][1:cells[0].index("]")]] = cells[3]
        covered = {row[0][1:row[0].index("]")] for section, row in delta_rows(self.parity) if section == 13}
        self.assertEqual(covered, {sha for sha, status in statuses.items() if status != "dropped"})
        replays = [line for line in ledger.splitlines() if "| dropped | drop: history-rewrite replay" in line]
        self.assertGreater(len(replays), 100)
        for line in replays:
            self.assertFalse(line[3:line.index("]")] in covered)
        counts = Counter(section for section, _ in delta_rows(self.parity))
        self.assertEqual(counts[12], len([e for e in self.checklist["entries"] if e["parity"]["section"] == 12]))

    def test_removing_delta_entry_fails(self):
        for entry_id in ("s12-01", "s12-14", "s13-f114c44", "s13-90ab4b7"):
            with self.subTest(entry=entry_id):
                with self.assertRaisesRegex(ValueError, "missing=\\[\\(1[23],"):
                    validate(self.parity, self.without_entry(entry_id))

    def test_removing_delta_source_row_leaves_stale_entry(self):
        for needle in ("| Temporary voice creator channels", "| [Lookup failure", "| [f114c44]", "| [bffccf3]"):
            with self.subTest(row=needle):
                changed = self.parity.replace(self.line(needle) + "\n", "", 1)
                with self.assertRaisesRegex(ValueError, "missing=\\[\\]; stale=\\[\\(1[23],"):
                    validate(changed, self.checklist)

    def test_new_delta_rows_need_entries(self):
        for needle, row in (
            ("| [Lookup failure", "| New surface | evidence | [TOG-1](/TOG/issues/TOG-1) |"),
            ("| [bffccf3]", "| [abc1234](https://example.invalid) | `src/x/` | fix(x): new | gap | [TOG-1](/TOG/issues/TOG-1) — obligation |"),
        ):
            with self.subTest(row=row):
                line = self.line(needle)
                changed = self.parity.replace(line, line + "\n" + row, 1)
                with self.assertRaisesRegex(ValueError, "missing="):
                    validate(changed, self.checklist)

    def test_ledger_status_changes_are_stale(self):
        line = self.line("| [bffccf3]")
        dropped = line.replace("| carded | ", "| dropped | drop: superseded; ", 1)
        with self.assertRaisesRegex(ValueError, "missing=\\[\\]; stale="):
            validate(self.parity.replace(line, dropped, 1), self.checklist)
        with self.assertRaisesRegex(ValueError, "missing=\\[\\(13,.*stale=\\[\\(13,"):
            validate(self.parity.replace(line, line.replace("| carded |", "| gap |", 1), 1), self.checklist)

    def test_drop_and_replay_rows_are_excluded(self):
        rows = delta_rows(self.parity)
        for needle, row in (
            ("| [Lookup failure", "| Retired surface | evidence | **DROP** — intentionally absent |"),
            ("| [bffccf3]", "| [abc1234](https://example.invalid) | `src/x/` | replayed | dropped | drop: history-rewrite replay |"),
        ):
            with self.subTest(row=row):
                line = self.line(needle)
                changed = self.parity.replace(line, line + "\n" + row, 1)
                self.assertEqual(delta_rows(changed), rows)
                self.assertEqual(validate(changed, self.checklist), validate(self.parity, self.checklist))
        # A DROP prefix naming an owner card keeps the §12 row in scope.
        line = self.line("| [Lookup failure")
        mixed = line + "\n| Mixed surface | evidence | **DROP** the Node hop; [TOG-1](/TOG/issues/TOG-1) keeps the contract |"
        self.assertIn((12, ("Mixed surface", "evidence")), delta_rows(self.parity.replace(line, mixed, 1)))

    def test_retained_delta_rows_need_owner_cards(self):
        line = self.line("| [Lookup failure")
        with self.assertRaisesRegex(ValueError, "§12: row needs an owner card"):
            delta_rows(self.parity.replace(line, line + "\n| Orphan surface | evidence | to be decided |", 1))
        line = self.line("| [bffccf3]")
        for status in ("carded", "gap"):
            with self.subTest(status=status):
                row = f"| [abc1234](https://example.invalid) | `src/x/` | fix(x): new | {status} | no card yet |"
                with self.assertRaisesRegex(ValueError, f"§13: {status} row needs an owner card"):
                    delta_rows(self.parity.replace(line, line + "\n" + row, 1))
        # Ported rows cite source evidence instead, yet still need an entry.
        row = "| [abc1234](https://example.invalid) | `src/x/` | fix(x): new | ported | implemented in `x.rs:1` |"
        with self.assertRaisesRegex(ValueError, "missing="):
            validate(self.parity.replace(line, line + "\n" + row, 1), self.checklist)

    def test_owner_cards_must_match_disposition(self):
        data = copy.deepcopy(self.checklist)
        self.entry(data, "s13-1d64196")["owner"] = ["TOG-11146"]
        with self.assertRaisesRegex(ValueError, "s13-1d64196: stale owner"):
            validate(self.parity, data)
        self.entry(data, "s13-1d64196")["owner"] = ["TOG-11145"]
        line = self.line("| [1d64196]")
        moved = self.parity.replace(line, line.replace("TOG-11145", "TOG-99999"), 1)
        with self.assertRaisesRegex(ValueError, "s13-1d64196: stale owner"):
            validate(moved, data)
        for owner in (None, [], ["owner"], "TOG-11145"):
            with self.subTest(owner=owner):
                self.entry(data, "s13-1d64196")["owner"] = owner
                with self.assertRaisesRegex(ValueError, "requires owner cards"):
                    validate(self.parity, data)
        # A ported row with no cited card still names its owning slice.
        data = copy.deepcopy(self.checklist)
        self.entry(data, "s13-3dc9720").pop("owner")
        with self.assertRaisesRegex(ValueError, "s13-3dc9720: §13 entry requires owner cards"):
            validate(self.parity, data)

    def test_delta_tables_fail_closed(self):
        header = "| Legacy commit | Area | Change | Status | Disposition |"
        with self.assertRaisesRegex(ValueError, "ledger columns"):
            delta_rows(self.parity.replace(header, "| Legacy commit | Area | Change | State | Disposition |", 1))
        with self.assertRaisesRegex(ValueError, "must end in Disposition"):
            delta_rows(self.parity.replace(header, "| Legacy commit | Area | Change | Status | Outcome |", 1))
        line = self.line("| [bffccf3]")
        with self.assertRaisesRegex(ValueError, "unknown ledger status 'pending'"):
            delta_rows(self.parity.replace(line, line.replace("| carded |", "| pending |", 1), 1))
        with self.assertRaisesRegex(ValueError, "malformed table row"):
            delta_rows(self.parity.replace(line, line.replace("| carded |", "|", 1), 1))
        with self.assertRaisesRegex(ValueError, "sections 12 and 13"):
            delta_rows(self.parity.replace("## 13. Exact-range", "## 31. Exact-range", 1))
        line = self.line("| [Lookup failure")
        with self.assertRaisesRegex(ValueError, "without leading border"):
            delta_rows(self.parity.replace(line, line + "\nNew surface | evidence | [TOG-1](/TOG/issues/TOG-1) |", 1))

    def test_delta_waivers_and_voice_rows_keep_their_rules(self):
        waived = [e for e in self.checklist["entries"] if e["parity"]["section"] in (12, 13) and e["status"] == "waived"]
        self.assertGreater(len(waived), 0)
        for field in ("reason", "approver"):
            with self.subTest(field=field):
                data = copy.deepcopy(self.checklist)
                self.entry(data, waived[0]["id"]).pop(field)
                with self.assertRaisesRegex(ValueError, f"waiver requires {field}"):
                    validate(self.parity, data)
        for entry_id in ("s12-01", "s13-59965d0", "s13-f114c44"):
            with self.subTest(entry=entry_id):
                data = copy.deepcopy(self.checklist)
                self.entry(data, entry_id).pop("reference")
                with self.assertRaisesRegex(ValueError, "TOG-10119"):
                    validate(self.parity, data)

    def test_ported_rows_verify_through_the_cache_wrapper(self):
        automated = [e for e in self.checklist["entries"] if e["parity"]["section"] == 13 and e["status"] == "automated"]
        self.assertEqual({e["parity"]["row"][3] for e in automated}, {"ported"})
        for entry in automated:
            self.assertRegex(entry["verification"], r"^python3 scripts/cargo_cache\.py run -- test -p two-bot(-core)? ")

    def test_render_lists_delta_sections_and_owners(self):
        output = render(self.checklist)
        self.assertIn("\n## 12. ", output)
        self.assertIn("\n## 13. ", output)
        self.assertIn("\n### s13-f114c44: f114c44 — TOG-3052: temp-voice generator", output)
        self.assertIn("- **Owner:** [TOG-11145](/TOG/issues/TOG-11145)\n", output)


if __name__ == "__main__":
    unittest.main()
