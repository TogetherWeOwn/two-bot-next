"""Offline contract for the disabled staging measurement wrapper."""

from copy import deepcopy
import unittest

from disabled_voice_measurement import GUARDS, WORKFLOW
import test_staging_voice_mutual_exclusion_acceptance as lifecycle
import test_workflows as policy


class VoiceMeasurementSkeletonTests(unittest.TestCase):
    def test_both_policy_consumers_fail_closed_on_activation(self):
        workflows = lifecycle.load_all()
        canonical = workflows[lifecycle.DEPLOY]["concurrency"]["group"].casefold()
        self.assertEqual(policy.workflow_policy_errors(workflows), [])
        self.assertEqual(lifecycle.repository_errors(workflows, canonical), [])
        self.assertEqual(workflows[WORKFLOW]["jobs"]["voice-first"]["uses"],
                         "./.github/workflows/voice-synthetic.yml")
        for name in GUARDS:
            with self.subTest(job=name):
                changed = deepcopy(workflows)
                job = changed[WORKFLOW]["jobs"][name]
                job["if"] = job["if"].replace("false && ", "", 1)
                self.assertTrue(policy.workflow_policy_errors(changed))
                self.assertTrue(lifecycle.repository_errors(changed, canonical))
        for field in ("on", "permissions", "concurrency"):
            with self.subTest(field=field):
                changed = deepcopy(workflows)
                changed[WORKFLOW][field] = {"contents": "read"}
                self.assertTrue(policy.workflow_policy_errors(changed))
                self.assertTrue(lifecycle.repository_errors(changed, canonical))
        changed = deepcopy(workflows)
        changed[WORKFLOW]["jobs"]["voice-first"]["uses"] = "./.github/workflows/other.yml"
        self.assertTrue(policy.workflow_policy_errors(changed))
        self.assertTrue(lifecycle.repository_errors(changed, canonical))

    def test_every_job_is_hard_disabled_and_the_graph_keeps_ownership(self):
        workflow = policy.load_workflows()["voice-rest-measurement.yml"]
        self.assertEqual(set(workflow["on"]), {"workflow_dispatch"})
        self.assertEqual(workflow["permissions"], {})
        self.assertEqual(workflow["concurrency"], {
            "group": "voice-rest-measurement-disabled", "cancel-in-progress": "false"})
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
        self.assertEqual(begin["outputs"], {
            name: "${{ steps.begin.outputs." + name + " }}"
            for name in ("expires_at", "sha", "deployment_id")
        })
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
        for job, step, phase in ((begin, step, "start"), (release, finish, "stop")):
            receipt = "${{ runner.temp }}/staging-use-measurement-" + phase + ".jsonl"
            self.assertEqual(step["env"]["STAGING_USE_RECEIPT"], receipt)
            upload = next(s for s in job["steps"] if s.get("with", {}).get("path") == receipt)
            self.assertEqual(upload["if"], "always()")
            self.assertEqual(upload["uses"], "actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9")
            self.assertEqual(upload["with"]["name"],
                             "staging-use-measurement-" + phase + "-${{ github.run_id }}-${{ github.run_attempt }}")
            self.assertEqual(upload["with"]["if-no-files-found"], "error")
            self.assertEqual(upload["with"]["retention-days"], "14")


if __name__ == "__main__":
    unittest.main()
