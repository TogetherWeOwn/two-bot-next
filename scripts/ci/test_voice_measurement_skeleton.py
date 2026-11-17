"""Offline contract for the disabled staging measurement wrapper."""

import unittest

import test_workflows as policy


class VoiceMeasurementSkeletonTests(unittest.TestCase):
    def test_every_job_is_hard_disabled_and_the_graph_keeps_ownership(self):
        workflow = policy.load_workflows()["voice-rest-measurement.yml"]
        self.assertEqual(set(workflow["on"]), {"workflow_dispatch"})
        self.assertEqual(workflow["permissions"], {})
        self.assertEqual(workflow["concurrency"], {
            "group": "staging-lifecycle", "cancel-in-progress": "false"})
        jobs = workflow["jobs"]
        self.assertEqual(set(jobs), {"staging-window", "voice-first", "staging-use-release"})
        guards = {
            "staging-window": "${{ false && github.ref == 'refs/heads/main' }}",
            "voice-first": "${{ false && github.ref == 'refs/heads/main' && needs.staging-window.result == 'success' }}",
            "staging-use-release": "${{ false && always() && github.ref == 'refs/heads/main' && needs.staging-window.outputs.expires_at != '' }}",
        }
        for name, guard in guards.items():
            self.assertEqual(jobs[name]["if"], guard, name)
        begin = jobs["staging-window"]
        self.assertEqual(begin["environment"], "staging")
        step = next(s for s in begin["steps"] if s.get("id") == "begin")
        self.assertEqual(step["run"], "node wrangler/scripts/staging-use-pipeline.mjs begin")
        self.assertEqual(step["env"]["STAGING_USE_EXPECTED_SHA"], "${{ github.sha }}")
        self.assertEqual(step["env"]["STAGING_USE_MINUTES"], "45")
        voice = jobs["voice-first"]
        self.assertEqual(voice["needs"], "staging-window")
        self.assertEqual(voice["uses"], "./.github/workflows/voice-synthetic.yml")
        self.assertEqual(voice["with"]["artifact_invocation"], "first")
        for name, output in (("staging_use_expires_at", "expires_at"),
                             ("staging_use_sha", "sha"),
                             ("staging_use_deployment", "deployment_id")):
            self.assertEqual(voice["with"][name], "${{ needs.staging-window.outputs." + output + " }}")
        release = jobs["staging-use-release"]
        self.assertEqual(set(release["needs"]), {"staging-window", "voice-first"})
        finish = next(s for s in release["steps"] if s.get("name") == "Release only this run's window")
        self.assertEqual(finish["run"], "node wrangler/scripts/staging-use-pipeline.mjs finish")
        self.assertEqual(finish["env"]["STAGING_USE_EXPIRES_AT"],
                         "${{ needs.staging-window.outputs.expires_at }}")


if __name__ == "__main__":
    unittest.main()
