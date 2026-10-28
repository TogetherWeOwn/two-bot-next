"""Feature-closure controls below the real integration-partition planner.

Only Cargo metadata is doubled. Package IDs, dependency aliases, enabled and
required features are real metadata shapes; expected selections are hand-derived.
These synthetic controls complement, not replace, real Cargo/LLVM parity evidence.
"""

import importlib.util
import json
import shlex
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from test_critical_path_support import (
    ROOT, Sandbox, assertions_message, dependency, feature_state, ledger, metadata, selections,
)


CORE = "two-bot-core"
CONSUMER = "two-bot-discord"
BRIDGE = "two-bot-store"


def graph_fixture(*, alias=None, transitive=False, optional=False):
    rows = [dict(package=package, kind="test", target=target, name=name, ignored=False)
            for package, target in ((CORE, "core_contract"), (CONSUMER, "consumer_contract"))
            for name in ("ordinary", "property_smoke")]
    rows.append(dict(package=CONSUMER, kind="test", target="disabled_contract",
                     name="must_not_be_selected", ignored=False, required_features=["unselected"]))
    ids = {name: f"path+file:///fixture/{name}#{name}@0.4.0"
           for name in (CORE, CONSUMER, BRIDGE)}
    direct = dependency(CORE, rename=alias, optional=optional)
    declarations = {
        CORE: {"default": [], "db": []},
        CONSUMER: {"default": [], "unselected": []},
        BRIDGE: {"default": [], "db": [CORE + "/db"]},
    }
    if optional:
        declarations[CONSUMER][alias or CORE] = ["dep:" + (alias or CORE)]
        declarations[CONSUMER]["db"] = [(alias or CORE) + "/db"]
    edges = {CORE: [], CONSUMER: [direct], BRIDGE: [dependency(CORE)]}
    if transitive:
        edges[CONSUMER] = [dependency(BRIDGE)]
        edges[BRIDGE] = [dependency(CORE)]
    packages = []
    for name in (CORE, CONSUMER, BRIDGE):
        packages.append(dict(
            id=ids[name], name=name, version="0.4.0",
            manifest_path=f"/fixture/{name}/Cargo.toml", dependencies=edges[name],
            features=declarations[name], targets=[dict(
                name=row["target"], kind=["test"], crate_types=["bin"], test=True,
                doctest=False, src_path=f"/fixture/{name}/tests/{row['target']}.rs",
                edition="2021", **{"required-features": row.get("required_features", [])})
                for row in rows if row["package"] == name and row["name"] != "property_smoke"]))
    # Literal all-workspace resolution for --features two-bot-core/db. The
    # separate oracle must not read these resolved features as its expectation.
    resolved = {CORE: ["db", "default"], CONSUMER: ["default"], BRIDGE: ["default"]}
    nodes = [dict(id=ids[name], features=resolved[name],
                  dependencies=[ids[dep["name"]] for dep in edges[name]],
                  deps=[dict(name=(dep.get("rename") or dep["name"]).replace("-", "_"),
                             pkg=ids[dep["name"]], dep_kinds=[dict(kind=dep["kind"], target=None)])
                        for dep in edges[name]]) for name in ids]
    return rows, dict(packages=packages, workspace_members=list(ids.values()),
                      workspace_default_members=list(ids.values()), version=1,
                      workspace_root="/fixture", target_directory="/fixture/target",
                      resolve=dict(nodes=nodes))


def runner():
    spec = importlib.util.spec_from_file_location("feature_graph_partition_runner",
                                                 ROOT / "scripts/ci/run-test-partition.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def integration_records(graph):
    module = runner()
    records = []
    with patch.object(module.subprocess, "run", return_value=SimpleNamespace(stdout=json.dumps(graph))):
        for owner in range(3):
            records.extend(dict(argv=command[0], env={}) for command in module.integration_commands(owner))
    return records


class DependencyGraphControls(unittest.TestCase):
    def test_direct_and_renamed_dependencies_preserve_effective_features_and_multiplicity(self):
        for alias in (None, "domain"):
            with self.subTest(alias=alias):
                rows, graph = graph_fixture(alias=alias)
                records = integration_records(graph)
                wanted = [dict(argv=["test", "--workspace", "--tests", "--features", CORE + "/db"], env={}),
                          dict(argv=["test", "--workspace", "--tests", "--features", CORE + "/db", "property_"], env={})]
                self.assertEqual(ledger(records, rows, graph=graph), ledger(wanted, rows, graph=graph))
                consumer = [r for r in records if CONSUMER in r["argv"]]
                self.assertTrue(consumer)
                self.assertTrue(all((alias or CORE) + "/db" in r["argv"] for r in consumer))
                self.assertFalse(any("disabled_contract" in r["argv"] for r in records))

    def test_renamed_dependency_accepts_only_its_manifest_alias_for_a_narrow_selection(self):
        rows, graph = graph_fixture(alias="domain")
        selected = selections(["test", "-p", CONSUMER, "--features", "domain/db"], rows, graph)
        self.assertEqual(selected[0][4], (CORE + "/db", CORE + "/default", CONSUMER + "/default"))
        with self.assertRaises(ValueError):
            selections(["test", "-p", CONSUMER, "--features", CORE + "/db"], rows, graph)

    def test_transitive_feature_forwarding_is_meaningful_but_not_a_direct_core_flag(self):
        rows, graph = graph_fixture(transitive=True)
        units = selections(["test", "-p", CONSUMER, "--features", BRIDGE + "/db"], rows, graph)
        self.assertEqual(units[0][4], (CORE + "/db", CORE + "/default", CONSUMER + "/default",
                                      BRIDGE + "/db", BRIDGE + "/default"))
        with self.assertRaises(ValueError):
            selections(["test", "-p", CONSUMER, "--features", CORE + "/db"], rows, graph)

    def test_unsupported_transitive_partition_path_fails_closed_instead_of_omitting_db(self):
        _, graph = graph_fixture(transitive=True)
        with self.assertRaisesRegex(ValueError, "transitiv"):
            integration_records(graph)

    def test_optional_renamed_dependency_is_visible_only_after_its_feature_activates_it(self):
        rows, graph = graph_fixture(alias="domain", optional=True)
        plain = selections(["test", "-p", CONSUMER], rows, graph)
        self.assertEqual(plain[0][4], (CONSUMER + "/default",))
        enabled = selections(["test", "-p", CONSUMER, "--features", "db"], rows, graph)
        self.assertEqual(enabled[0][4], (CORE + "/db", CORE + "/default",
                                       CONSUMER + "/db", CONSUMER + "/default", CONSUMER + "/domain"))

    def test_required_feature_disabled_target_is_neither_discovered_nor_explicitly_run(self):
        rows, graph = graph_fixture()
        records = integration_records(graph)
        self.assertFalse(any("disabled_contract" in r["argv"] for r in records))
        with self.assertRaisesRegex(ValueError, "disabled features"):
            selections(["test", "-p", CONSUMER, "--test", "disabled_contract"], rows, graph)
        enabled = selections(["test", "-p", CONSUMER, "--test", "disabled_contract",
                              "--features", "unselected"], rows, graph)
        self.assertEqual([unit[3] for unit in enabled], ["must_not_be_selected"])

    def test_metadata_and_declarations_do_not_share_the_feature_oracle(self):
        _, graph = graph_fixture()
        for node in graph["resolve"]["nodes"]:
            node["features"] = ["fabricated-feature"]
        _, projected = feature_state(graph, [CONSUMER], [CORE + "/db"])
        self.assertEqual(projected[CONSUMER], (CORE + "/db", CORE + "/default", CONSUMER + "/default"))


class WorkspaceUnificationAcceptance(unittest.TestCase):
    def test_discord_integration_keeps_the_features_enabled_by_the_workspace(self):
        rows = [dict(package=CONSUMER, kind="test", target="feature_sensitive_contract",
                     name="ordinary", ignored=False)]
        graph = metadata(rows)
        baseline = selections(["test", "--workspace", "--tests", "--features", CORE + "/db"], rows, graph)
        self.assertIn(CONSUMER + "/db", baseline[0][4])
        actual = ledger(integration_records(graph), rows, graph=graph)
        self.assertTrue(actual)
        self.assertTrue(all(CONSUMER + "/db" in unit[4] for unit in actual),
                        "package partition lost the consumer db feature unified by the bot dependency")

    def test_workspace_enabled_required_target_does_not_fail_in_its_integration_partition(self):
        row = dict(package=CONSUMER, kind="test", target="requires_consumer_db",
                   name="ordinary", ignored=False, required_features=["db"])
        graph = metadata([row])
        sandbox = Sandbox([row], graph)
        self.addCleanup(sandbox.close)
        records = [r for r in integration_records(graph) if "property_" not in r["argv"]]
        self.assertEqual(len(records), 1, "workspace-enabled integration target was silently discarded")
        proc = sandbox.run(shlex.join(["cargo", *records[0]["argv"]]))
        self.assertEqual(proc.returncode, 0, assertions_message(proc))


if __name__ == "__main__":
    unittest.main()
