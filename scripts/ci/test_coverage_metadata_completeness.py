"""Offline acceptance tests for fail-closed coverage dependency metadata.

Removing the nonempty/completeness check must make the negative cases fail:
missing nodes may not look like packages with no reverse dependents. Exercise
both the original names-only package fixture and cargo-style package IDs.
Complete controls rule out rejecting all metadata instead of validating it.
"""

import copy
import importlib.util
from pathlib import Path
import unittest


PLANNER_PATH = Path(__file__).with_name("plan-coverage-shards.py")
SPEC = importlib.util.spec_from_file_location("metadata_planner", PLANNER_PATH)
PLANNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PLANNER)

STORE_CHANGE = ["crates/store/src/lib.rs"]
PACKAGES = (
    "two-bot-core",
    "two-bot-discord",
    "two-bot-store",
    "two-bot-cutover",
    "two-bot",
    "two-bot-testsupport",
    "two-bot-next",
)
EXPECTED_REVERSE = {
    "two-bot-core": {"two-bot-discord", "two-bot-store", "two-bot-cutover", "two-bot"},
    "two-bot-discord": {"two-bot-cutover", "two-bot"},
    "two-bot-store": {"two-bot-cutover", "two-bot"},
    "two-bot-cutover": {"two-bot"},
    "two-bot": set(),
    "two-bot-testsupport": set(),
    "two-bot-next": set(),
}


def package_id(name):
    return f"{name} 0.4.0 (path+file:///repo)"


def complete_metadata(with_ids):
    # Copied from the original protected acceptance fixture, not from planner
    # internals. The names-only form deliberately has no workspace_members.
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
    metadata = {
        "packages": [{"name": name} for name in edges],
        "resolve": {"nodes": [
            {"id": package_id(name), "deps": [
                {"name": dep, "pkg": package_id(dep),
                 "dep_kinds": [{"kind": None, "target": None}]}
                for dep in deps
            ]}
            for name, deps in edges.items()
        ]},
    }
    if with_ids:
        for package in metadata["packages"]:
            package["id"] = package_id(package["name"])
        metadata["workspace_members"] = [package_id(name) for name in PACKAGES]
    return metadata


def store_plan(metadata):
    return PLANNER.plan(
        STORE_CHANGE, 4, event_name="pull_request", ref="refs/pull/1/merge",
        metadata=metadata,
    )


class CoverageMetadataCompletenessTests(unittest.TestCase):
    def assert_full_plan(self, metadata):
        result = store_plan(metadata)
        self.assertIs(result["widened"], True, result)
        full_ids = [leg["id"] for leg in PLANNER.FULL_LEGS]
        self.assertEqual(len(full_ids), len(set(full_ids)))
        self.assertCountEqual(result["legs"], full_ids)
        self.assertEqual(len(result["shards"]), 4)
        self.assertCountEqual(
            [leg for shard in result["shards"] for leg in shard], full_ids,
        )

    def assert_scoped_store_plan(self, metadata, affected):
        result = store_plan(metadata)
        self.assertIs(result["widened"], False, result)
        expected = set(PLANNER.ALWAYS_ON_LEGS) | {
            leg["id"] for leg in PLANNER.FULL_LEGS
            if affected.intersection(leg["crates"])
        }
        self.assertCountEqual(result["legs"], expected)
        self.assertLess(expected, {leg["id"] for leg in PLANNER.FULL_LEGS})
        self.assertEqual(len(result["shards"]), 4)
        self.assertCountEqual(
            [leg for shard in result["shards"] for leg in shard], expected,
        )

    def test_empty_nodes_api_raises_value_error(self):
        for with_ids in (False, True):
            with self.subTest(with_ids=with_ids):
                metadata = complete_metadata(with_ids)
                metadata["resolve"]["nodes"] = []
                with self.assertRaises(ValueError):
                    PLANNER.reverse_deps_from_metadata(metadata)

    def test_empty_nodes_plan_widens_to_every_leg_once(self):
        for with_ids in (False, True):
            with self.subTest(with_ids=with_ids):
                metadata = complete_metadata(with_ids)
                metadata["resolve"]["nodes"] = []
                self.assert_full_plan(metadata)

    def test_missing_workspace_node_api_raises_value_error(self):
        # Cutover/bot are store dependents; core/store are dependencies.
        # Isolated packages also need nodes even without dangling edges.
        for with_ids in (False, True):
            for missing in PACKAGES:
                with self.subTest(with_ids=with_ids, missing=missing):
                    metadata = complete_metadata(with_ids)
                    metadata["resolve"]["nodes"] = [
                        node for node in metadata["resolve"]["nodes"]
                        if node["id"] != package_id(missing)
                    ]
                    with self.assertRaises(ValueError):
                        PLANNER.reverse_deps_from_metadata(metadata)

    def test_missing_workspace_node_plan_widens_to_every_leg_once(self):
        for with_ids in (False, True):
            for missing in PACKAGES:
                with self.subTest(with_ids=with_ids, missing=missing):
                    metadata = complete_metadata(with_ids)
                    metadata["resolve"]["nodes"] = [
                        node for node in metadata["resolve"]["nodes"]
                        if node["id"] != package_id(missing)
                    ]
                    self.assert_full_plan(metadata)

    def test_complete_original_graph_api_preserves_reverse_dependents(self):
        for with_ids in (False, True):
            with self.subTest(with_ids=with_ids):
                self.assertEqual(
                    PLANNER.reverse_deps_from_metadata(complete_metadata(with_ids)),
                    EXPECTED_REVERSE,
                )

    def test_complete_original_graph_plan_stays_scoped(self):
        for with_ids in (False, True):
            with self.subTest(with_ids=with_ids):
                self.assert_scoped_store_plan(
                    complete_metadata(with_ids),
                    {"two-bot-store", "two-bot-cutover", "two-bot"},
                )

    def transitive_only_metadata(self, with_ids):
        metadata = complete_metadata(with_ids)
        # Route store <- discord <- cutover <- bot. Dedicated cutover legs
        # now require transitive traversal; there is no direct cutover/store
        # edge that could let a one-hop-only implementation pass.
        for node in metadata["resolve"]["nodes"]:
            node["deps"] = [dep for dep in node["deps"]
                            if dep["pkg"] != package_id("two-bot-store")]
            if node["id"] == package_id("two-bot-discord"):
                node["deps"].append({
                    "name": "two-bot-store", "pkg": package_id("two-bot-store"),
                    "dep_kinds": [{"kind": None, "target": None}],
                })
        return metadata

    def test_complete_transitive_graph_api_keeps_direct_edges(self):
        expected = copy.deepcopy(EXPECTED_REVERSE)
        expected["two-bot-store"] = {"two-bot-discord"}
        for with_ids in (False, True):
            with self.subTest(with_ids=with_ids):
                self.assertEqual(
                    PLANNER.reverse_deps_from_metadata(
                        self.transitive_only_metadata(with_ids)),
                    expected,
                )

    def test_complete_transitive_graph_plan_keeps_indirect_store_dependents(self):
        for with_ids in (False, True):
            with self.subTest(with_ids=with_ids):
                self.assert_scoped_store_plan(
                    self.transitive_only_metadata(with_ids),
                    {"two-bot-store", "two-bot-discord", "two-bot-cutover", "two-bot"},
                )


if __name__ == "__main__":
    unittest.main()
