"""MCP workflow finite-use and credential-isolation policy, offline only."""

from copy import deepcopy
from pathlib import Path
import unittest

from scripts.ci import test_workflows as policy


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/staging-mcp-proveout.yml"


def workflow():
    return policy.yaml.load(WORKFLOW.read_text(), Loader=policy.UniqueKeyLoader)


class McpWindowTests(unittest.TestCase):
    def test_repository_contract_and_deployment_lock_agree(self):
        source = workflow()
        self.assertEqual(policy.staging_mcp_proveout_errors(source), [])
        deploy = policy.yaml.load(
            (ROOT / ".github/workflows/deploy-staging.yml").read_text(),
            Loader=policy.UniqueKeyLoader)
        self.assertEqual(source["concurrency"], deploy["concurrency"])
        steps = source["jobs"]["proveout"]["steps"]
        self.assertNotIn("if", steps[4])  # default success(): no proof after failed begin/ready
        self.assertEqual(steps[5]["if"], "always() && steps.use.outputs.expires_at != ''")
        self.assertEqual(steps[6]["if"], steps[5]["if"])
        self.assertNotIn("OWNERSHIP_CONTROL_TOKEN", steps[4]["env"])
        self.assertNotIn("STAGING_MCP_TOKEN", steps[2]["env"])
        self.assertNotIn("STAGING_MCP_TOKEN", steps[5]["env"])

    def test_mutations_cannot_drop_lifecycle_or_mix_credentials(self):
        source = workflow()
        job = lambda w: w["jobs"]["proveout"]
        step = lambda w, index: job(w)["steps"][index]
        mutations = {
            "old split lock": lambda w: w["concurrency"].update({"group": "staging-mcp-proveout"}),
            "per-run lock": lambda w: w["concurrency"].update({"group": "mcp-${{ github.run_id }}"}),
            "cancel holder": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "nested lock": lambda w: job(w).update({"concurrency": deepcopy(w["concurrency"])}),
            "non-main": lambda w: job(w).update({"if": "always()"}),
            "scheduled": lambda w: w["on"].update({"schedule": [{"cron": "*/5 * * * *"}]}),
            "caller expiry": lambda w: w["on"].update({"workflow_dispatch": {"inputs": {"expiry": {}}}}),
            "wrong environment": lambda w: job(w).update({"environment": "production"}),
            "job token": lambda w: job(w).update({"env": {"TOKEN": "${{ secrets.STAGING_MCP_TOKEN }}"}}),
            "workflow token": lambda w: w.update({"env": {"TOKEN": "${{ secrets.STAGING_MCP_TOKEN }}"}}),
            "job tolerated": lambda w: job(w).update({"continue-on-error": "true"}),
            "extra permission": lambda w: job(w)["permissions"].update({"actions": "write"}),
            "wrong runner": lambda w: job(w).update({"runs-on": "ubuntu-latest-16-cores"}),
            "extended timeout": lambda w: job(w).update({"timeout-minutes": "60"}),
            "missing begin": lambda w: job(w)["steps"].pop(2),
            "missing finish": lambda w: job(w)["steps"].pop(5),
            "extra step": lambda w: job(w)["steps"].append({"run": "true"}),
            "duplicate begin": lambda w: job(w)["steps"].append(deepcopy(step(w, 2))),
            "late begin": lambda w: job(w)["steps"].insert(5, job(w)["steps"].pop(2)),
            "early finish": lambda w: job(w)["steps"].insert(4, job(w)["steps"].pop(5)),
            "checkout credential": lambda w: step(w, 0)["with"].update({"persist-credentials": "true"}),
            "unpinned node": lambda w: step(w, 1).update({"uses": "actions/setup-node@main"}),
            "node loader": lambda w: step(w, 1).update({"env": {"NODE_OPTIONS": "--require=other.js"}}),
            "begin skipped": lambda w: step(w, 2).update({"if": "false"}),
            "begin tolerated": lambda w: step(w, 2).update({"continue-on-error": "true"}),
            "begin without ready": lambda w: step(w, 2)["env"].update({"STAGING_USE_ACQUIRE_ONLY": "1"}),
            "begin long window": lambda w: step(w, 2)["env"].update({"STAGING_USE_MINUTES": "360"}),
            "begin wrong origin": lambda w: step(w, 2)["env"].update({"STAGING_WORKER_URL": "${{ vars.OTHER }}"}),
            "begin MCP bearer": lambda w: step(w, 2)["env"].update({"STAGING_MCP_TOKEN": "${{ secrets.STAGING_MCP_TOKEN }}"}),
            "begin new expiry": lambda w: step(w, 2)["env"].update({"STAGING_USE_EXPIRES_AT": "1"}),
            "no start receipt": lambda w: step(w, 2)["env"].pop("STAGING_USE_RECEIPT"),
            "optional start artifact": lambda w: step(w, 3)["with"].update({"if-no-files-found": "ignore"}),
            "start only on success": lambda w: step(w, 3).pop("if"),
            "start artifact glob": lambda w: step(w, 3)["with"].update({"path": "${{ runner.temp }}/*"}),
            "proof after failed readiness": lambda w: step(w, 4).update({"if": "always()"}),
            "proof skipped": lambda w: step(w, 4).update({"if": "false"}),
            "proof tolerated": lambda w: step(w, 4).update({"continue-on-error": "true"}),
            "proof holds control": lambda w: step(w, 4)["env"].update({"TOKEN": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}"}),
            "proof other origin": lambda w: step(w, 4)["env"].update({"STAGING_MCP_URL": "${{ vars.OTHER }}"}),
            "proof echo bearer": lambda w: step(w, 4).update({"run": 'printf "%s" "$STAGING_MCP_TOKEN"'}),
            "proof accepts fence": lambda w: step(w, 4)["env"].update({"STAGING_MCP_EXPECT_FENCE": "1"}),
            "finish only on success": lambda w: step(w, 5).pop("if"),
            "finish begin success only": lambda w: step(w, 5).update({"if": "steps.use.outcome == 'success'"}),
            "finish tolerated": lambda w: step(w, 5).update({"continue-on-error": "true"}),
            "finish begins": lambda w: step(w, 5).update({"run": step(w, 2)["run"]}),
            "finish wrong expiry": lambda w: step(w, 5)["env"].update({"STAGING_USE_EXPIRES_AT": "1"}),
            "finish wrong origin": lambda w: step(w, 5)["env"].update({"STAGING_WORKER_URL": "${{ vars.OTHER }}"}),
            "finish other attempt": lambda w: step(w, 5)["env"].update({"GITHUB_RUN_ATTEMPT": "1"}),
            "finish MCP bearer": lambda w: step(w, 5)["env"].update({"STAGING_MCP_TOKEN": "${{ secrets.STAGING_MCP_TOKEN }}"}),
            "finish shares receipt": lambda w: step(w, 5)["env"].update({"STAGING_USE_RECEIPT": step(w, 2)["env"]["STAGING_USE_RECEIPT"]}),
            "stop artifact only success": lambda w: step(w, 6).pop("if"),
            "optional stop artifact": lambda w: step(w, 6)["with"].update({"if-no-files-found": "ignore"}),
            "short stop retention": lambda w: step(w, 6)["with"].update({"retention-days": "1"}),
            "proof artifact only success": lambda w: step(w, 7).pop("if"),
            "optional proof artifact": lambda w: step(w, 7)["with"].update({"if-no-files-found": "ignore"}),
            "proof artifact glob": lambda w: step(w, 7)["with"].update({"path": "**/*"}),
        }
        for label, mutate in mutations.items():
            changed = deepcopy(source)
            mutate(changed)
            with self.subTest(mutation=label):
                self.assertTrue(policy.staging_mcp_proveout_errors(changed))


if __name__ == "__main__":
    unittest.main()
