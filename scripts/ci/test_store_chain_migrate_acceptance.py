"""Failing-first acceptance for Land #981 store-chain migrate (TOG-25226).

Covers the red lanes on PR #981 (`feat/store-chain-migrate`, head `b1f6aad3c`):
`check` (actionlint: 11/12 workflow_dispatch inputs > GH limit 10),
`rust tests` (E0425 missing `embedded_versions`, E0063 missing store Request
fields), `ignored db (runtime)` + nightly (E0061 missing store helper args).

Each test is red on origin/main (`46a30b19e`, store chain unhandled) and
green after the Land-card fix binds both chains under their own ledgers.
No database, no dispatch, no secrets; offline structural + behavioral checks
that run in the `check` lane via `unittest discover -s scripts/ci`.
"""

import inspect
from pathlib import Path
import unittest

import yaml

import staging_migrate_claim as staging_claim
import production_migrate_claim as production_claim
import staging_schema_gate as gate

ROOT = Path(__file__).resolve().parents[2]


def _workflow_inputs(name):
    text = (ROOT / ".github/workflows" / name).read_text()
    workflow = yaml.safe_load(text)
    # YAML 1.1 parses the `on:` key as boolean True; test_workflows.py uses a
    # custom loader, we accept either spelling here.
    on = workflow.get("on", workflow.get(True, {})) or {}
    return ((on.get("workflow_dispatch") or {}).get("inputs") or {}), text


class SchemaGateStoreDriftTests(unittest.TestCase):
    """The gate must refuse when EITHER chain is behind (PR description)."""

    def test_pending_versions_accepts_a_store_key(self):
        sig = inspect.signature(gate.pending_versions)
        self.assertIn(
            "key",
            sig.parameters,
            "gate ignores the store chain: pending_versions has no key param",
        )
        manifest = {"pending_before": [], "pending_store_before": [400]}
        self.assertEqual(gate.pending_versions(manifest), [])
        self.assertEqual(
            gate.pending_versions(manifest, "pending_store_before"),
            [400],
        )

    def test_store_only_drift_refuses_with_store_versions(self):
        # Positive control: a fully-applied manifest stays clean.
        clean = gate.drift_message([], "a" * 40)
        self.assertNotIn("Store chain", clean)
        # Store-only drift must refuse and name the store list.
        try:
            message = gate.drift_message([], "a" * 40, [400])
        except TypeError:
            self.fail(
                "gate ignores the store chain: "
                "drift_message takes no store_pending"
            )
        self.assertIn("400", message)
        self.assertIn("expected_store_pending", message)


class ClaimStoreBindingTests(unittest.TestCase):
    """Claim publisher verifies + transports the store binding (PR body)."""

    def test_staging_claim_exposes_store_projection(self):
        self.assertTrue(
            hasattr(staging_claim, "store_projection_hash"),
            "staging claim ignores the store chain: "
            "no store_projection_hash",
        )
        manifest = {
            "source_sha": "a" * 40,
            "pending_store_before": [400],
            "source_store_migrations": [
                {
                    "version": 400,
                    "description": "store first",
                    "sha384": "0" * 96,
                }
            ],
        }
        digest = staging_claim.store_projection_hash(manifest)
        self.assertEqual(len(digest), 64)
        bad = dict(manifest, pending_store_before=[401])
        with self.assertRaises(staging_claim.Refused):
            staging_claim.store_projection_hash(bad)

    def test_production_claim_exposes_store_projection(self):
        self.assertTrue(
            hasattr(production_claim, "store_projection_hash"),
            "production claim ignores the store chain: "
            "no store_projection_hash",
        )
        manifest = {
            "source_sha": "a" * 40,
            "pending_store_before": [],
            "source_store_migrations": [
                {
                    "version": 400,
                    "description": "store first",
                    "sha384": "0" * 96,
                }
            ],
        }
        digest = production_claim.store_projection_hash(manifest)
        self.assertEqual(len(digest), 64)

    def test_claim_module_binds_store_env(self):
        for module in (staging_claim, production_claim):
            source = Path(module.__file__).read_text()
            self.assertIn(
                "STORE_PLAN_MANIFEST_SHA256",
                source,
                f"{module.__name__} does not transport the store binding",
            )
            self.assertIn("EXPECTED_STORE_PENDING", source)


class WorkflowStoreShapeTests(unittest.TestCase):
    """Migrate workflows carry both chains but stay within the GH input cap."""

    def test_migrate_workflows_carry_store_inputs(self):
        for name in ("staging-migrate.yml", "production-migrate.yml"):
            with self.subTest(workflow=name):
                inputs, _ = _workflow_inputs(name)
                self.assertIn(
                    "expected_store_pending",
                    inputs,
                    f"{name} plans only the cutover chain",
                )
                self.assertIn(
                    "store_plan_manifest_sha256",
                    inputs,
                    f"{name} binds only the cutover hash",
                )

    def test_migrate_workflows_stay_within_dispatch_cap(self):
        # `check` lane repro: PR #981 has 12/11 inputs, GH allows at most 10.
        for name in ("staging-migrate.yml", "production-migrate.yml"):
            with self.subTest(workflow=name):
                inputs, _ = _workflow_inputs(name)
                self.assertLessEqual(
                    len(inputs),
                    10,
                    f"{name} exceeds the workflow_dispatch input cap",
                )

    def test_apply_threads_store_binding_to_runner(self):
        for name in ("staging-migrate.yml", "production-migrate.yml"):
            with self.subTest(workflow=name):
                _, text = _workflow_inputs(name)
                self.assertIn("--expected-store-pending", text)
                self.assertIn("--store-plan-manifest-sha256", text)


class RunnerStoreWiringTests(unittest.TestCase):
    """Rust runner plans/applies/verifies both chains under their own ledger."""

    SOURCE = ROOT / "crates/cutover/src/staging_migrate.rs"

    def test_request_binds_both_hashes(self):
        source = self.SOURCE.read_text()
        self.assertIn("expected_store_pending", source)
        self.assertIn("store_plan_manifest_sha256", source)

    def test_store_chain_resolves_without_top_level_lookup(self):
        # `rust tests` E0425 repro: `two_bot_store::embedded_versions()`
        # does not exist; the chain must resolve via its module or a
        # re-export. Either correct wiring passes; the bare lookup fails.
        store_lib = (ROOT / "crates/store/src/lib.rs").read_text()
        runner = self.SOURCE.read_text()
        reexported = "embedded_versions" in store_lib
        via_module = "migrations::embedded_versions" in runner
        self.assertTrue(
            reexported or via_module,
            "store chain has no resolvable version source "
            "(E0425: embedded_versions not found)",
        )
        cutover_cargo = (ROOT / "crates/cutover/Cargo.toml").read_text()
        self.assertIn("two-bot-store", cutover_cargo)


class DbHelperStoreBindingTests(unittest.TestCase):
    """`ignored db (runtime)` helpers thread the store binding (E0061 repro)."""

    SOURCE = ROOT / "crates/cutover/tests/staging_migrate_db.rs"

    def test_helpers_take_store_pending_and_hash(self):
        source = self.SOURCE.read_text()
        self.assertIn("expected_store_pending", source)
        self.assertIn("--expected-store-pending", source)
        self.assertIn("--store-plan-manifest-sha256", source)


class RoleMatrixStoreLedgerTests(unittest.TestCase):
    """Role matrix grants the RO plan login SELECT on the store ledger."""

    def test_store_ledger_is_a_planned_ledger(self):
        matrix = (ROOT / "sql/database_role_matrix.sql").read_text()
        self.assertIn("_two_bot_migrations", matrix)


if __name__ == "__main__":
    unittest.main()
