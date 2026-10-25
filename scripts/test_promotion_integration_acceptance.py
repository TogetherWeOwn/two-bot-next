"""Offline intake regressions independent of the original protected contract.

Breaks caught: selecting inventory[0] instead of the current certificate,
assuming repository ID 1, accepting a fork, widening the closed receipt,
allowing local modules to drift, or an environment override rebuilding bytes.
Only GitHub/image-copy I/O is replaced by the original strict offline fixture.
"""

import contextlib
import copy
import hashlib
import io
import json
from pathlib import Path
import sys
import unittest
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
import production_deploy
import test_immutable_bot_promotion as contract


class PromotionInventoryIntegrationTests(contract.PromotionFixture):
    def inventory_with_diagnostics(self):
        matching = copy.deepcopy(self.github.artifact)
        others = [dict(matching, id=7697, name="staging-rollout-evidence"),
                  dict(matching, id=7698, name="voice-synthetic-report"),
                  dict(matching, id=7699, name="bot-promotion-501-1")]
        self.github.listing = {"total_count": 4, "artifacts": [others[0], matching, *others[1:]]}

    def bind_repository(self, repository_id):
        record = contract.receipt()
        record["repository"]["id"] = repository_id
        self.replace_package(record)
        self.github.run["repository"]["id"] = repository_id
        self.github.run["head_repository"]["id"] = repository_id
        self.github.artifact["workflow_run"]["repository_id"] = repository_id
        self.github.artifact["workflow_run"]["head_repository_id"] = repository_id
        self.github.listing["artifacts"] = [copy.deepcopy(self.github.artifact)]

    def test_complete_mixed_inventory_admits_only_exact_current_certificate(self):
        self.inventory_with_diagnostics()
        self.assertEqual(self.admit(), 0)
        self.assertEqual(self.mutations, [(contract.STAGE_IMAGE, contract.PROD_REPO)])
        self.assertEqual(self.github.calls, [contract.RUN_ROUTE, contract.ARTIFACTS_ROUTE,
                                            contract.ARTIFACT_ROUTE, contract.ZIP_ROUTE,
                                            contract.RUN_ROUTE])
        for name, data in contract.MODULES.items():
            self.assertEqual((self.out / "modules" / name).read_bytes(), data)

    def test_mixed_inventory_with_truncated_count_refuses_before_copy(self):
        self.inventory_with_diagnostics()
        self.github.listing["total_count"] = 5
        self.refused()
        self.assertNotIn(contract.ZIP_ROUTE, self.github.calls)

    def test_mixed_inventory_with_duplicate_matching_name_refuses_before_copy(self):
        self.inventory_with_diagnostics()
        self.github.listing["artifacts"].append(dict(self.github.artifact, id=7701))
        self.github.listing["total_count"] = 5
        self.refused()
        self.assertNotIn(contract.ZIP_ROUTE, self.github.calls)

    def test_mixed_inventory_without_current_attempt_certificate_refuses(self):
        self.inventory_with_diagnostics()
        self.github.listing["artifacts"].pop(1)
        self.github.listing["total_count"] = 3
        self.refused()

    def test_consistent_nonfixture_repository_id_passes_end_to_end(self):
        self.bind_repository(1098765432)
        self.inventory_with_diagnostics()
        self.assertEqual(self.admit(), 0)
        admitted = json.loads((self.out / "admitted.json").read_text())
        self.assertEqual(admitted["receipt"]["repository"],
                         {"id": 1098765432, "full_name": contract.REPO})
        self.assertEqual(self.mutations, [(contract.STAGE_IMAGE, contract.PROD_REPO)])

    def test_fork_and_cross_record_repository_ids_refuse_before_mutation(self):
        for location in ("run-head", "artifact-repository", "artifact-head", "receipt", "reread"):
            with self.subTest(location=location):
                self.bind_repository(1098765432)
                if location == "run-head":
                    self.github.run["head_repository"]["id"] = 1098765433
                elif location.startswith("artifact-"):
                    key = "repository_id" if location == "artifact-repository" else "head_repository_id"
                    self.github.artifact["workflow_run"][key] = 1098765433
                    self.github.listing["artifacts"] = [copy.deepcopy(self.github.artifact)]
                elif location == "receipt":
                    self.github.run["repository"]["id"] = 1098765433
                    self.github.run["head_repository"]["id"] = 1098765433
                    self.github.artifact["workflow_run"]["repository_id"] = 1098765433
                    self.github.artifact["workflow_run"]["head_repository_id"] = 1098765433
                    self.github.listing["artifacts"] = [copy.deepcopy(self.github.artifact)]
                else:
                    self.github.refreshed_run = copy.deepcopy(self.github.run)
                    self.github.refreshed_run["repository"]["id"] = 1098765433
                    self.github.refreshed_run["head_repository"]["id"] = 1098765433
                self.refused()

    def test_closed_staging_receipt_rejects_even_known_diagnostic_extras(self):
        for key, value in (("config", {"vars": {"FIXTURE": "not-runtime-config"}}),
                           ("unsafe_config", {"main": "other.js"}),
                           ("application_listing_lag", True), ("active_lag", True)):
            with self.subTest(key=key):
                record = contract.receipt()
                record["staging"][key] = value
                self.replace_package(record)
                self.refused()

    def test_known_lag_diagnostics_are_projected_out_before_certification(self):
        evidence = dict(contract.receipt()["staging"], application_listing_lag=True, active_lag=True)
        self.assertEqual(self.certify(stage=evidence), 0)
        with zipfile.ZipFile(self.output_zip) as archive:
            record = json.loads(archive.read("promotion-receipt.json"))
            self.assertEqual(record, contract.receipt())
            self.assertNotIn("application_listing_lag", record["staging"])
            self.assertNotIn("active_lag", record["staging"])
        self.github = contract.GitHubFixture(self.output_zip.read_bytes())
        self.assertEqual(self.admit(), 0)

    def test_hash_consistent_but_unsafe_module_suffixes_refuse_before_copy(self):
        for path in ("extra.ts", "extra.json", "extra.map", "extra.js.exe", "nested/.hidden.js"):
            with self.subTest(path=path):
                members = dict(contract.MODULES, **{path: b"fixture-only"})
                record = contract.receipt()
                record["worker"]["modules"].append({"path": path, "size": 12,
                                                      "sha256": hashlib.sha256(b"fixture-only").hexdigest()})
                self.replace_package(record, members)
                self.refused()


class SavedBundleIntegrationTests(contract.PromotionFixture):
    def write_admitted(self, image=contract.PROD_IMAGE):
        self.out.mkdir()
        self.artifact = self.out / "admitted.json"
        self.artifact.write_text(json.dumps({"schema_version": 1, "receipt": contract.receipt(),
                                             "production_image": image}))
        (self.out / "modules").mkdir()
        for name, data in contract.MODULES.items():
            (self.out / "modules" / name).write_bytes(data)

    def render(self):
        target = self.root / "rendered.json"
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = production_deploy.main([
                "render", "--config", str(self.config), "--out", str(target),
                "--sha", contract.SHA, "--run-id", "900", "--attempt", "1",
                "--deploy-id", contract.EVENT, "--artifact", str(self.artifact)])
        return code, json.loads(target.read_text()) if target.exists() else None

    def test_added_served_module_after_admission_refuses_render_without_output(self):
        self.write_admitted()
        (self.out / "modules" / "injected.js").write_bytes(b"export default 'not certified';")
        code, config = self.render()
        self.assertEqual(code, 1)
        self.assertIsNone(config)

    def test_saved_module_symlink_refuses_even_when_target_bytes_match(self):
        self.write_admitted()
        target = self.root / "external.js"
        target.write_bytes(contract.MODULES["index.js"])
        entrypoint = self.out / "modules" / "index.js"
        entrypoint.unlink()
        entrypoint.symlink_to(target)
        code, config = self.render()
        self.assertEqual(code, 1)
        self.assertIsNone(config)

    def test_same_digest_in_foreign_production_account_refuses_render(self):
        self.write_admitted(image=contract.PROD_IMAGE.replace("/acct/", "/foreign/"))
        code, config = self.render()
        self.assertEqual(code, 1)
        self.assertIsNone(config)

    def test_environment_main_and_build_overrides_cannot_replace_saved_bytes(self):
        self.write_admitted()
        self.config.write_text(contract.CONFIG.replace(
            "[env.production.vars]", '[env.production]\nmain = "evil.ts"\n'
            'base_dir = "/untrusted"\nno_bundle = false\nminify = true\n'
            '[env.production.build]\ncommand = "fixture-must-never-execute"\n'
            '[env.production.vars]'))
        code, config = self.render()
        self.assertEqual(code, 0)
        self.assertTrue(config["no_bundle"])
        self.assertFalse(config["minify"])
        self.assertEqual(Path(config["main"]).read_bytes(), contract.MODULES["index.js"])
        self.assertEqual(Path(config["base_dir"]), self.out / "modules")
        production = config["env"]["production"]
        for key in ("main", "build", "base_dir", "no_bundle", "minify"):
            self.assertNotIn(key, production)
        self.assertEqual(production["vars"]["ENV_SENTINEL"], "production-only")
        self.assertEqual(production["containers"][0]["image"], contract.PROD_IMAGE)
        self.assertNotIn("image_vars", production["containers"][0])


if __name__ == "__main__":
    unittest.main()
