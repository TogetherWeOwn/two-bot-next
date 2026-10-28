"""Acceptance tests for the two-bot-next CI coverage/test shard planner.

The paired build card adds a shard planner to CI: a script that emits the
coverage/test job matrix from the changed files, runs parallel shards, and a
merge step that applies the unchanged HARD 80%-of-added-lines bar. These
tests pin the planner contract. They are RED while the planner is absent and
GREEN once the build card lands. This file touches only tests; the Builder
owns the implementation.

Contract the Builder must implement (repo-relative paths):

- ``scripts/ci/plan-coverage-shards.py`` with:
  - ``FULL_LEGS``: list of leg dicts, each with ``id`` (unique str),
    ``command`` (str or argv list holding the llvm-cov invocation), and
    ``crates`` (list of workspace package names the leg exercises).
  - ``ALWAYS_ON_LEGS``: list of leg ids, a non-empty proper subset of the
    full set, including at least one workspace sweep.
  - ``classify_path(path)`` -> ``"crate"`` | ``"docs"`` | ``"widen"`` |
    ``"unknown"`` for a repo-relative path.
  - ``crate_for_path(path)`` -> workspace package name (``two-bot-core``,
    ``two-bot-discord``, ``two-bot``, ``two-bot-cutover``, ``two-bot-store``,
    ``two-bot-testsupport``, ``two-bot-next``) or None for non-crate paths.
  - ``reverse_deps_from_metadata(metadata)`` -> dict of package name to the
    set of workspace packages that directly depend on it, parsed from
    ``cargo metadata`` JSON (``resolve.nodes[].deps[]``). Raises ValueError
    on malformed metadata.
  - ``plan(changed_files, shard_count, *, event_name, ref, metadata)`` ->
    ``{"legs": [...], "shards": [[...], ...], "widened": bool,
    "reason": str}``. Never raises on planning inputs: any error fails
    closed to the full set with ``widened`` True.
  - ``main(argv=None)`` -> int. Writes the plan as JSON to ``--output``.
    Always exits 0: an unreadable diff fails closed to the full set.
- ``scripts/ci/merge-coverage-shards.py`` with:
  - ``merge_lcov(paths)`` -> ``{(file, line): count}`` union of the shard
    LCOV reports. Raises on a missing, empty or malformed report.
  - ``main(argv=None)`` -> int. Takes repeatable ``--report`` paths,
    ``--expected-shards N`` and ``--out PATH``. Exits 1 with a ``::error::``
    line unless exactly N valid reports merge; exit 0 writes the merged
    LCOV. The HARD verdict itself stays in check-patch-coverage.py,
    unchanged, reading the merged report.

Explicitly unspecified (the Builder chooses and notes in the PR): dev-only
dependency edges, scoping of the workspace sweeps beyond the always-on set,
shard counts above the leg count, non-check workflow files, and merge_group
events.
"""

import importlib.util
import io
import json
import re
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PLANNER_PATH = Path(__file__).with_name("plan-coverage-shards.py")
MERGE_PATH = Path(__file__).with_name("merge-coverage-shards.py")

SPEC = importlib.util.spec_from_file_location(
    "coverage_gate", Path(__file__).with_name("check-patch-coverage.py")
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)

ABSENT = (
    "RED: the shard planner is absent; the paired build card has not landed "
    "yet. This failure is the expected pre-build state, not a regression."
)

# The full leg list in .github/workflows/check.yml: package partitions of
# the two workspace sweeps (six packages per family: core, discord, bot,
# cutover, store, other) plus every dedicated cov leg, as
# (package, target, selected-via---test-flag). The serial
# invite-saturation store leg is included in the dedicated set.
EXPECTED_DEDICATED_LEGS = [
    ("two-bot-core", "stream_live_store", True),
    ("two-bot-discord", "mutation_429_proves_no_effect_channel", True),
    ("two-bot-core", "channel_moderation", False),
    ("two-bot-discord", "channel_moderation_runtime", True),
    ("two-bot-discord", "internal_channel_moderation", True),
    ("two-bot-core", "feeds_store", True),
    ("two-bot-cutover", "legacy_verify_db", True),
    ("two-bot-cutover", "legacy_tls_fence", True),
    ("two-bot-cutover", "staging_migrate_db", True),
    ("two-bot-discord", "internal_events", True),
    ("two-bot-cutover", "raid_list_db", True),
    ("two-bot-cutover", "reengagement_list_db", True),
    ("two-bot-cutover", "game_activity_report_db", True),
    ("two-bot-core", "member_moderation_db", True),
    ("two-bot-store", "invite_uses_saturation_acceptance", True),
]
# Six sweep packages per family (nonignored + ignored): the remainder
# ("other") owns root/testsupport and any future workspace member.
EXPECTED_SWEEP_PACKAGES = ("core", "discord", "bot", "cutover", "store", "other")
EXPECTED_SWEEP_FAMILIES = ("sweep-nonignored", "sweep-ignored")
EXPECTED_SWEEP_LEGS = len(EXPECTED_SWEEP_PACKAGES) * len(EXPECTED_SWEEP_FAMILIES)
EXPECTED_TOTAL_LEGS = len(EXPECTED_DEDICATED_LEGS) + EXPECTED_SWEEP_LEGS

KNOWN_PACKAGES = {
    "two-bot-core",
    "two-bot-discord",
    "two-bot",
    "two-bot-cutover",
    "two-bot-store",
    "two-bot-testsupport",
    "two-bot-next",
}

WIDEN_PATHS = [
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    "crates/store/build.rs",
    "crates/cutover/build.rs",
    ".cargo/config.toml",
    ".github/workflows/check.yml",
    "scripts/ci/plan-coverage-shards.py",
    "scripts/ci/merge-coverage-shards.py",
    "crates/cutover/migrations/0001_funnel.sql",
    "crates/store/migrations/0001_init.sql",
]

UNKNOWN_PATHS = [
    "vendor/blob.bin",
    "newdir/file.txt",
    "fuzz/fuzz_targets/new_parse.rs",
]

DOCS_PATHS = [
    "docs/foo.md",
    "docs/a/b.md",
    "README.md",
    "CONTRIBUTING.md",
]

_module_cache = {}


def load_module(test, name, path):
    """Import a planner module, failing (not skipping) while it is absent."""
    if name not in _module_cache:
        test.assertTrue(Path(path).exists(), f"{ABSENT} Missing: {path.name}")
        spec = importlib.util.spec_from_file_location(name, path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        _module_cache[name] = module
    return _module_cache[name]


def load_planner(test):
    return load_module(test, "shard_planner", PLANNER_PATH)


def load_merge(test):
    return load_module(test, "shard_merge", MERGE_PATH)


def command_text(leg):
    cmd = leg["command"]
    return " ".join(cmd) if isinstance(cmd, list) else cmd


def matches_dedicated(cmd, pkg, target, via_test_flag):
    if f"-p {pkg}" not in cmd and f"--package {pkg}" not in cmd:
        return False
    if via_test_flag:
        return f"--test {target}" in cmd or f"--test={target}" in cmd
    return re.search(r"(?:^|\s)--\s+channel_moderation(?:\s|$)", cmd) is not None


def is_sweep_leg(leg):
    leg_id = leg.get("id", "")
    return any(leg_id == f"{family}-{package}"
               for family in EXPECTED_SWEEP_FAMILIES
               for package in EXPECTED_SWEEP_PACKAGES)


def is_workspace_sweep(leg):
    return "--workspace" in command_text(leg)


def stub_metadata():
    """cargo-metadata-shaped graph mirroring the workspace normal deps."""
    edges = {
        "two-bot-core": [],
        "two-bot-discord": ["two-bot-core"],
        "two-bot-store": ["two-bot-core"],
        "two-bot-cutover": ["two-bot-core", "two-bot-discord", "two-bot-store"],
        "two-bot": ["two-bot-core", "two-bot-discord", "two-bot-store",
                    "two-bot-cutover"],
        "two-bot-testsupport": [],
        "two-bot-next": [],
    }

    def pid(name):
        return f"{name} 0.4.0 (path+file:///repo)"

    nodes = [
        {
            "id": pid(name),
            "deps": [
                {"name": dep, "pkg": pid(dep),
                 "dep_kinds": [{"kind": None, "target": None}]}
                for dep in deps
            ],
        }
        for name, deps in edges.items()
    ]
    return {"packages": [{"name": n} for n in edges],
            "resolve": {"nodes": nodes}}


def make_plan(planner, changed, shards=4, event="pull_request",
              ref="refs/pull/1/merge", metadata="stub"):
    if metadata == "stub":
        metadata = stub_metadata()
    return planner.plan(list(changed), shards, event_name=event, ref=ref,
                        metadata=metadata)


def full_leg_ids(planner):
    return [leg["id"] for leg in planner.FULL_LEGS]


def dedicated_commands(planner):
    return {leg["id"]: command_text(leg) for leg in planner.FULL_LEGS
            if not is_sweep_leg(leg)}


def lcov_text(entries):
    parts = []
    for path in sorted(entries):
        parts.append("TN:")
        parts.append(f"SF:{path}")
        for line in sorted(entries[path]):
            parts.append(f"DA:{line},{entries[path][line]}")
        parts.append("end_of_record")
    return "\n".join(parts) + "\n"


FIXTURE_FILE = "crates/core/src/shard_fixture.rs"

# Three fixture diffs: (added lines, shard cover maps, single cover map,
# expected covered, expected total). Shard maps are disjoint except one
# shared line with deliberately different positive counts, so the tests pin
# key union and verdict parity without pinning count arithmetic.
PARITY_FIXTURES = [
    {
        "name": "passing diff at 90 percent",
        "added": [(FIXTURE_FILE, n) for n in range(1, 11)],
        "shards": [
            {FIXTURE_FILE: {1: 1, 2: 1, 3: 1}},
            {FIXTURE_FILE: {3: 2, 4: 1, 5: 1, 6: 1}},
            {FIXTURE_FILE: {7: 1, 8: 1, 9: 1, 10: 0}},
        ],
        "single": {FIXTURE_FILE: {1: 1, 2: 1, 3: 1, 4: 1, 5: 1, 6: 1,
                                  7: 1, 8: 1, 9: 1, 10: 0}},
        "covered": 9,
        "total": 10,
    },
    {
        "name": "failing diff at 50 percent",
        "added": [(FIXTURE_FILE, n) for n in range(1, 11)],
        "shards": [
            {FIXTURE_FILE: {1: 1, 2: 1}},
            {FIXTURE_FILE: {1: 3, 3: 1, 4: 0, 5: 0}},
            {FIXTURE_FILE: {6: 1, 7: 0, 8: 0, 9: 0, 10: 1}},
        ],
        "single": {FIXTURE_FILE: {1: 1, 2: 1, 3: 1, 4: 0, 5: 0, 6: 1,
                                  7: 0, 8: 0, 9: 0, 10: 1}},
        "covered": 5,
        "total": 10,
    },
    {
        "name": "boundary diff at exactly 80 percent",
        "added": [(FIXTURE_FILE, n) for n in range(1, 6)],
        "shards": [
            {FIXTURE_FILE: {1: 1, 2: 1}},
            {FIXTURE_FILE: {2: 5, 3: 1}},
            {FIXTURE_FILE: {4: 1, 5: 0}},
        ],
        "single": {FIXTURE_FILE: {1: 1, 2: 1, 3: 1, 4: 1, 5: 0}},
        "covered": 4,
        "total": 5,
    },
]


class PlannerPresenceTests(unittest.TestCase):
    def test_planner_script_exists(self):
        self.assertTrue(PLANNER_PATH.exists(), ABSENT)

    def test_merge_script_exists(self):
        self.assertTrue(MERGE_PATH.exists(), ABSENT)


class FullLegListTests(unittest.TestCase):
    def test_full_legs_match_todays_check_yml_leg_count(self):
        planner = load_planner(self)
        self.assertEqual(len(planner.FULL_LEGS), EXPECTED_TOTAL_LEGS)
        ids = full_leg_ids(planner)
        self.assertEqual(len(set(ids)), len(ids), "leg ids must be unique")
        sweeps = [leg for leg in planner.FULL_LEGS if is_sweep_leg(leg)]
        dedicated = [leg for leg in planner.FULL_LEGS if not is_sweep_leg(leg)]
        self.assertEqual(len(sweeps), EXPECTED_SWEEP_LEGS)
        self.assertEqual(len(dedicated), len(EXPECTED_DEDICATED_LEGS))

    def test_every_leg_has_an_id_a_command_and_known_crates(self):
        planner = load_planner(self)
        for leg in planner.FULL_LEGS:
            with self.subTest(leg=leg.get("id")):
                self.assertIsInstance(leg["id"], str)
                self.assertTrue(leg["id"])
                self.assertTrue(command_text(leg))
                self.assertIn("llvm-cov", command_text(leg))
                self.assertIsInstance(leg["crates"], list)
                self.assertTrue(leg["crates"])
                for crate in leg["crates"]:
                    self.assertIn(crate, KNOWN_PACKAGES)

    def test_workspace_sweeps_are_present(self):
        planner = load_planner(self)
        sweeps = [leg for leg in planner.FULL_LEGS if is_sweep_leg(leg)]
        self.assertEqual(len(sweeps), EXPECTED_SWEEP_LEGS)
        by_id = {leg["id"]: leg for leg in sweeps}
        for family in EXPECTED_SWEEP_FAMILIES:
            for package in EXPECTED_SWEEP_PACKAGES:
                leg_id = f"{family}-{package}"
                with self.subTest(leg=leg_id):
                    self.assertIn(leg_id, by_id)
                    cmd = command_text(by_id[leg_id])
                    # Automatic target discovery: sweeps never name an
                    # integration target, so a new target is covered without
                    # a planner change.
                    self.assertNotIn("--test ", cmd)
                    self.assertIn("--lib --bins --tests", cmd)
                    if package == "other":
                        self.assertIn("--workspace", cmd)
                        for excluded in ("two-bot-core", "two-bot-discord",
                                         "two-bot", "two-bot-cutover",
                                         "two-bot-store"):
                            self.assertIn(f"--exclude {excluded}", cmd)
                    else:
                        self.assertNotIn("--workspace", cmd)
        # Only the two "other" remainders still carry --workspace.
        workspace = [leg for leg in planner.FULL_LEGS if is_workspace_sweep(leg)]
        self.assertEqual(
            sorted(leg["id"] for leg in workspace),
            ["sweep-ignored-other", "sweep-nonignored-other"],
        )

    def test_every_dedicated_leg_matches_exactly_one_expected_leg(self):
        planner = load_planner(self)
        cmds = dedicated_commands(planner)
        self.assertEqual(len(cmds), len(EXPECTED_DEDICATED_LEGS))
        for pkg, target, via_flag in EXPECTED_DEDICATED_LEGS:
            with self.subTest(pkg=pkg, target=target):
                hits = [lid for lid, cmd in cmds.items()
                        if matches_dedicated(cmd, pkg, target, via_flag)]
                self.assertEqual(len(hits), 1)

    def test_always_on_is_a_proper_nonempty_subset_with_a_sweep(self):
        planner = load_planner(self)
        full = set(full_leg_ids(planner))
        always = list(planner.ALWAYS_ON_LEGS)
        self.assertTrue(always)
        self.assertLess(set(always), full)
        by_id = {leg["id"]: leg for leg in planner.FULL_LEGS}
        self.assertTrue(any(is_sweep_leg(by_id[lid]) for lid in always))
        # The always-on set is the nonignored remainder only: docs-only
        # changes measure the root/testsupport packages without running
        # every dedicated replay.
        self.assertEqual(always, ["sweep-nonignored-other"])


class PartitionTests(unittest.TestCase):
    def test_shards_cover_every_leg_exactly_once(self):
        planner = load_planner(self)
        full = set(full_leg_ids(planner))
        # A workspace-wide change widens to the full 27-leg set; a core
        # change is scoped to its affected packages (see
        # test_core_change_selects_affected_packages_without_widening).
        for count in (1, 2, 4, 6):
            with self.subTest(shards=count):
                result = make_plan(planner, ["Cargo.lock"],
                                   shards=count)
                self.assertTrue(result["widened"])
                self.assertEqual(set(result["legs"]), full)
                union = [lid for shard in result["shards"] for lid in shard]
                self.assertEqual(len(result["shards"]), count)
                self.assertEqual(len(union), len(set(union)))
                self.assertEqual(set(union), full)

    def test_scoped_selection_still_partitions_exactly_once(self):
        planner = load_planner(self)
        result = make_plan(planner, ["crates/store/src/lib.rs"], shards=4)
        union = [lid for shard in result["shards"] for lid in shard]
        self.assertEqual(set(union), set(result["legs"]))
        self.assertEqual(len(union), len(set(union)))

    def test_plan_is_deterministic_for_the_same_input(self):
        planner = load_planner(self)
        changed = ["crates/store/src/lib.rs", "docs/note.md"]
        first = make_plan(planner, changed, shards=4)
        second = make_plan(planner, changed, shards=4)
        self.assertEqual(first, second)

    def test_plan_ignores_changed_file_order(self):
        planner = load_planner(self)
        first = make_plan(planner, ["crates/store/src/a.rs", "docs/n.md"],
                          shards=4)
        second = make_plan(planner, ["docs/n.md", "crates/store/src/a.rs"],
                           shards=4)
        self.assertEqual(first, second)


class CrateMappingTests(unittest.TestCase):
    def test_crate_paths_map_to_workspace_packages(self):
        planner = load_planner(self)
        cases = {
            "crates/core/src/lib.rs": "two-bot-core",
            "crates/discord/src/x.rs": "two-bot-discord",
            "crates/bot/src/main.rs": "two-bot",
            "crates/cutover/src/y.rs": "two-bot-cutover",
            "crates/store/src/z.rs": "two-bot-store",
            "crates/store/tests/postgres.rs": "two-bot-store",
            "crates/testsupport/src/w.rs": "two-bot-testsupport",
            "src/lib.rs": "two-bot-next",
        }
        for path, expected in cases.items():
            with self.subTest(path=path):
                self.assertEqual(planner.crate_for_path(path), expected)
                self.assertEqual(planner.classify_path(path), "crate")

    def test_docs_paths_map_to_docs(self):
        planner = load_planner(self)
        for path in DOCS_PATHS:
            with self.subTest(path=path):
                self.assertEqual(planner.classify_path(path), "docs")
                self.assertIsNone(planner.crate_for_path(path))

    def test_widen_and_unknown_paths_have_no_crate(self):
        planner = load_planner(self)
        for path in WIDEN_PATHS + UNKNOWN_PATHS:
            with self.subTest(path=path):
                self.assertIsNone(planner.crate_for_path(path))

    def test_reverse_deps_come_from_cargo_metadata(self):
        planner = load_planner(self)
        reverse = planner.reverse_deps_from_metadata(stub_metadata())
        self.assertEqual(reverse["two-bot-core"],
                         {"two-bot-discord", "two-bot-store", "two-bot-cutover",
                          "two-bot"})
        self.assertEqual(reverse["two-bot-discord"],
                         {"two-bot-cutover", "two-bot"})
        self.assertEqual(reverse["two-bot-store"], {"two-bot-cutover", "two-bot"})
        self.assertEqual(reverse["two-bot-cutover"], {"two-bot"})
        self.assertEqual(reverse["two-bot"], set())
        self.assertEqual(reverse["two-bot-next"], set())
        self.assertEqual(reverse["two-bot-testsupport"], set())

    def test_malformed_metadata_raises(self):
        planner = load_planner(self)
        for bad in ({}, {"bogus": True}, {"resolve": {}}):
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):
                    planner.reverse_deps_from_metadata(bad)


class ChangedCrateSelectionTests(unittest.TestCase):
    def selected(self, planner, changed):
        return set(make_plan(planner, changed, shards=4)["legs"])

    def assert_pkg_legs(self, planner, selected, pkg, expect_selected):
        for epkg, target, flag in EXPECTED_DEDICATED_LEGS:
            if epkg != pkg:
                continue
            with self.subTest(pkg=pkg, target=target):
                cmds = dedicated_commands(planner)
                hits = [lid for lid, cmd in cmds.items()
                        if matches_dedicated(cmd, epkg, target, flag)]
                self.assertEqual(len(hits), 1)
                if expect_selected:
                    self.assertIn(hits[0], selected)
                else:
                    self.assertNotIn(hits[0], selected)

    def test_store_change_selects_dependents_not_dependencies(self):
        planner = load_planner(self)
        selected = self.selected(planner, ["crates/store/src/lib.rs"])
        self.assert_pkg_legs(planner, selected, "two-bot-store", True)
        self.assert_pkg_legs(planner, selected, "two-bot-cutover", True)
        self.assert_pkg_legs(planner, selected, "two-bot-core", False)
        self.assert_pkg_legs(planner, selected, "two-bot-discord", False)
        self.assertLess(set(planner.ALWAYS_ON_LEGS), selected)

    def test_discord_change_selects_discord_and_cutover_legs(self):
        planner = load_planner(self)
        selected = self.selected(planner, ["crates/discord/src/a.rs"])
        self.assert_pkg_legs(planner, selected, "two-bot-discord", True)
        self.assert_pkg_legs(planner, selected, "two-bot-cutover", True)
        self.assert_pkg_legs(planner, selected, "two-bot-core", False)
        self.assert_pkg_legs(planner, selected, "two-bot-store", False)

    def test_core_change_selects_affected_packages_without_widening(self):
        # Core sits below every package with dedicated legs, so a core
        # change selects all 15 dedicated replays plus the affected
        # package sweeps, without widening. The only leg left out is the
        # ignored remainder: root/testsupport do not depend on core, and
        # the always-on set covers only the nonignored remainder.
        planner = load_planner(self)
        result = make_plan(planner, ["crates/core/src/a.rs"], shards=4)
        self.assertFalse(result["widened"])
        selected = set(result["legs"])
        self.assertEqual(selected, set(full_leg_ids(planner)) - {"sweep-ignored-other"})
        for pkg in ("two-bot-core", "two-bot-discord", "two-bot-cutover",
                    "two-bot-store"):
            self.assert_pkg_legs(planner, selected, pkg, True)
        self.assertLess(set(planner.ALWAYS_ON_LEGS), selected)

    def test_leaf_crate_change_selects_no_dedicated_legs(self):
        planner = load_planner(self)
        selected = self.selected(planner, ["crates/bot/src/main.rs"])
        for pkg in ("two-bot-core", "two-bot-discord", "two-bot-cutover",
                    "two-bot-store"):
            self.assert_pkg_legs(planner, selected, pkg, False)
        self.assertTrue(set(planner.ALWAYS_ON_LEGS) <= selected)

    def test_docs_only_change_selects_exactly_the_always_on_set(self):
        planner = load_planner(self)
        result = make_plan(planner, DOCS_PATHS, shards=4)
        self.assertEqual(set(result["legs"]), set(planner.ALWAYS_ON_LEGS))
        self.assertFalse(result["widened"])


class WideningTests(unittest.TestCase):
    def test_workspace_wide_paths_select_the_full_set(self):
        planner = load_planner(self)
        full = set(full_leg_ids(planner))
        for path in WIDEN_PATHS:
            with self.subTest(path=path):
                self.assertEqual(planner.classify_path(path), "widen")
                result = make_plan(planner, [path], shards=4)
                self.assertEqual(set(result["legs"]), full)
                self.assertTrue(result["widened"])

    def test_push_to_main_always_uses_the_full_set(self):
        planner = load_planner(self)
        result = make_plan(planner, DOCS_PATHS, shards=4, event="push",
                           ref="refs/heads/main")
        self.assertEqual(set(result["legs"]), set(full_leg_ids(planner)))
        self.assertTrue(result["widened"])

    def test_scheduled_runs_always_use_the_full_set(self):
        planner = load_planner(self)
        result = make_plan(planner, DOCS_PATHS, shards=4, event="schedule",
                           ref="refs/heads/main")
        self.assertEqual(set(result["legs"]), set(full_leg_ids(planner)))
        self.assertTrue(result["widened"])

    def test_pull_request_event_does_not_widen_by_itself(self):
        planner = load_planner(self)
        result = make_plan(planner, DOCS_PATHS, shards=4,
                           event="pull_request", ref="refs/pull/1/merge")
        self.assertEqual(set(result["legs"]), set(planner.ALWAYS_ON_LEGS))
        self.assertFalse(result["widened"])


class FailClosedTests(unittest.TestCase):
    def test_unknown_paths_select_the_full_set(self):
        planner = load_planner(self)
        full = set(full_leg_ids(planner))
        for path in UNKNOWN_PATHS:
            with self.subTest(path=path):
                self.assertEqual(planner.classify_path(path), "unknown")
                result = make_plan(planner, [path], shards=4)
                self.assertEqual(set(result["legs"]), full)
                self.assertTrue(result["widened"])

    def test_missing_or_corrupt_metadata_selects_the_full_set(self):
        planner = load_planner(self)
        full = set(full_leg_ids(planner))
        for metadata in (None, {}, {"bogus": True}):
            with self.subTest(metadata=metadata):
                result = make_plan(planner, ["crates/store/src/lib.rs"],
                                   shards=4, metadata=metadata)
                self.assertEqual(set(result["legs"]), full)
                self.assertTrue(result["widened"])

    def test_plan_never_raises_on_planning_inputs(self):
        planner = load_planner(self)
        inputs = [
            (["crates/store/src/lib.rs"], None),
            (UNKNOWN_PATHS, stub_metadata()),
            ([], stub_metadata()),
        ]
        for changed, metadata in inputs:
            with self.subTest(changed=changed):
                result = make_plan(planner, changed, shards=4,
                                   metadata=metadata)
                self.assertEqual(set(result),
                                 {"legs", "shards", "widened", "reason"})
                self.assertTrue(result["reason"])

    def test_planner_main_fails_closed_on_an_unreadable_diff(self):
        planner = load_planner(self)
        with tempfile.TemporaryDirectory() as tmp:
            out = str(Path(tmp) / "matrix.json")
            code = planner.main([
                "--base-ref", "definitely-not-a-ref-aaaaaaaa",
                "--head-ref", "definitely-not-a-ref-bbbbbbbb",
                "--shards", "4",
                "--event", "pull_request",
                "--ref", "refs/pull/1/merge",
                "--output", out,
            ])
            self.assertEqual(code, 0)
            result = json.loads(Path(out).read_text())
            self.assertEqual(set(result["legs"]),
                             set(full_leg_ids(planner)))
            self.assertTrue(result["widened"])


class MergeParityTests(unittest.TestCase):
    def test_merged_shards_equal_the_single_job_result(self):
        merge = load_merge(self)
        self.assertEqual(len(PARITY_FIXTURES), 3)
        for fixture in PARITY_FIXTURES:
            with self.subTest(fixture=fixture["name"]):
                with tempfile.TemporaryDirectory() as tmp:
                    tmpdir = Path(tmp)
                    shard_paths = []
                    for index, entries in enumerate(fixture["shards"]):
                        path = tmpdir / f"shard-{index}.info"
                        path.write_text(lcov_text(entries))
                        shard_paths.append(str(path))
                    single_path = tmpdir / "single.info"
                    single_path.write_text(lcov_text(fixture["single"]))
                    merged = merge.merge_lcov(shard_paths)
                    single_hits, error = gate.read_lcov(str(single_path))
                    self.assertEqual(error, "")
                    self.assertEqual(set(merged), set(single_hits))
                    added = set(fixture["added"])
                    merged_result = gate.evaluate(merged, added)
                    single_result = gate.evaluate(single_hits, added)
                    self.assertEqual(merged_result[:2], single_result[:2])
                    self.assertEqual(merged_result[:2],
                                     (fixture["covered"], fixture["total"]))
                    merged_verdict = merged_result[0] / merged_result[1] >= 0.8
                    single_verdict = single_result[0] / single_result[1] >= 0.8
                    self.assertEqual(merged_verdict, single_verdict)


class MergeFailClosedTests(unittest.TestCase):
    def write_reports(self, tmpdir, count):
        paths = []
        for index in range(count):
            path = tmpdir / f"shard-{index}.info"
            path.write_text(lcov_text(
                {FIXTURE_FILE: {index + 1: 1}}))
            paths.append(str(path))
        return paths

    def run_merge_main(self, merge, args):
        out = io.StringIO()
        with redirect_stdout(out):
            code = merge.main(args)
        return code, out.getvalue()

    def test_complete_reports_merge_successfully(self):
        merge = load_merge(self)
        with tempfile.TemporaryDirectory() as tmp:
            tmpdir = Path(tmp)
            paths = self.write_reports(tmpdir, 3)
            out = str(tmpdir / "merged.info")
            args = ["--expected-shards", "3", "--out", out]
            for path in paths:
                args += ["--report", path]
            code, output = self.run_merge_main(merge, args)
            self.assertEqual(code, 0, output)
            hits, error = gate.read_lcov(out)
            self.assertEqual(error, "")
            self.assertEqual(len(hits), 3)

    def test_a_missing_shard_report_fails_the_merge(self):
        merge = load_merge(self)
        with tempfile.TemporaryDirectory() as tmp:
            tmpdir = Path(tmp)
            paths = self.write_reports(tmpdir, 2)
            out = str(tmpdir / "merged.info")
            args = ["--expected-shards", "3", "--out", out]
            for path in paths:
                args += ["--report", path]
            code, output = self.run_merge_main(merge, args)
            self.assertEqual(code, 1)
            self.assertIn("::error::", output)

    def test_a_nonexistent_shard_report_fails_the_merge(self):
        merge = load_merge(self)
        with tempfile.TemporaryDirectory() as tmp:
            tmpdir = Path(tmp)
            paths = self.write_reports(tmpdir, 2)
            out = str(tmpdir / "merged.info")
            code, output = self.run_merge_main(
                merge, ["--report", paths[0], "--report",
                        str(tmpdir / "absent.info"),
                        "--expected-shards", "2", "--out", out])
            self.assertEqual(code, 1)
            self.assertIn("::error::", output)

    def test_an_empty_shard_report_fails_the_merge(self):
        merge = load_merge(self)
        with tempfile.TemporaryDirectory() as tmp:
            tmpdir = Path(tmp)
            paths = self.write_reports(tmpdir, 1)
            empty = tmpdir / "empty.info"
            empty.write_text("")
            out = str(tmpdir / "merged.info")
            code, output = self.run_merge_main(
                merge, ["--report", paths[0], "--report", str(empty),
                        "--expected-shards", "2", "--out", out])
            self.assertEqual(code, 1)
            self.assertIn("::error::", output)

    def test_a_malformed_shard_report_fails_the_merge(self):
        merge = load_merge(self)
        with tempfile.TemporaryDirectory() as tmp:
            tmpdir = Path(tmp)
            paths = self.write_reports(tmpdir, 1)
            bad = tmpdir / "bad.info"
            bad.write_text("not an lcov report\n")
            out = str(tmpdir / "merged.info")
            code, output = self.run_merge_main(
                merge, ["--report", paths[0], "--report", str(bad),
                        "--expected-shards", "2", "--out", out])
            self.assertEqual(code, 1)
            self.assertIn("::error::", output)

    def test_merge_never_passes_on_partial_data(self):
        merge = load_merge(self)
        with tempfile.TemporaryDirectory() as tmp:
            tmpdir = Path(tmp)
            paths = self.write_reports(tmpdir, 3)
            out = str(tmpdir / "merged.info")
            # Fewer reports than shards (a failed or cancelled shard uploads
            # nothing) and more reports than shards both fail.
            for count in ("2", "4"):
                with self.subTest(expected=count):
                    args = ["--expected-shards", count, "--out", out]
                    for path in paths:
                        args += ["--report", path]
                    code, output = self.run_merge_main(merge, args)
                    self.assertEqual(code, 1)
                    self.assertIn("::error::", output)


if __name__ == "__main__":
    unittest.main()
