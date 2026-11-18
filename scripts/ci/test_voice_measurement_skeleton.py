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

    def test_capture_artifacts_and_fail_closed_steps(self):
        jobs = policy.load_workflows()[WORKFLOW]["jobs"]
        before = jobs["measurement-before"]["steps"]
        after = jobs["measurement-after"]["steps"]
        for job_name, steps in (("measurement-before", before), ("measurement-after", after)):
            job = jobs[job_name]
            self.assertEqual(job["permissions"], {"contents": "read"})
            self.assertEqual(job["environment"], "staging")
            self.assertEqual(job["runs-on"], "ubuntu-latest")
            self.assertEqual(job["timeout-minutes"], "5")
            self.assertEqual(len(steps), 4 if steps is before else 5)
            self.assertEqual(steps[0], {
                "uses": "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
                "with": {"persist-credentials": "false", "fetch-depth": "1"},
            })
            self.assertEqual(steps[1], {
                "uses": "actions/setup-node@949feb2413d6458794dcd2491c4babbbce0c15c1",
                "with": {"node-version": "22", "package-manager-cache": "false"},
            })
        for steps, name in ((before, "Capture the initial sanitized receipt"),
                            (after, "Capture and compare sanitized receipts")):
            capture = next(step for step in steps if step.get("name") == name)
            self.assertEqual(steps.index(capture), 2 if steps is before else 3)
            self.assertNotIn("if", capture)
            self.assertNotIn("continue-on-error", capture)
            self.assertEqual(capture["env"], {
                "STAGING_USE_SHA": "${{ needs.staging-window.outputs.sha }}",
                "STAGING_USE_DEPLOYMENT": "${{ needs.staging-window.outputs.deployment_id }}",
                "STAGING_USE_EXPIRES_AT": "${{ needs.staging-window.outputs.expires_at }}",
                "STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}",
                "PRODUCTION_WORKER_URL": "${{ vars.PRODUCTION_WORKER_URL }}",
                "METRICS_SCRAPE_TOKEN": "${{ secrets.STAGING_METRICS_SCRAPE_TOKEN }}",
            })
            commands = capture["run"].splitlines()
            self.assertEqual(commands[0], 'node wrangler/scripts/voice-measurement-metadata.mjs "$RUNNER_TEMP/voice-metadata.json"')
            self.assertEqual(commands[1], 'node wrangler/scripts/voice-rest-measurement.mjs capture --metadata "$RUNNER_TEMP/voice-metadata.json" --metrics-url "$STAGING_WORKER_URL" --out "$RUNNER_TEMP/voice-' + ("before" if steps is before else "after") + '.json"')
            self.assertEqual(len(commands), 2 if steps is before else 3)
            if steps is after:
                self.assertEqual(commands[2], 'node wrangler/scripts/voice-rest-measurement.mjs compare --before "$RUNNER_TEMP/voice-before/voice-before.json" --after "$RUNNER_TEMP/voice-after.json" --out "$RUNNER_TEMP/voice-comparison.json"')
            upload = steps[-1]
            self.assertNotIn("if", upload)
            self.assertNotIn("continue-on-error", upload)
            self.assertEqual(upload["uses"], "actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9")
            suffix = "before" if steps is before else "result"
            path = "voice-before.json" if steps is before else "voice-comparison.json"
            self.assertEqual(upload["with"], {
                "name": "voice-measurement-" + suffix + "-${{ github.run_id }}-${{ github.run_attempt }}",
                "path": "${{ runner.temp }}/" + path,
                "if-no-files-found": "error", "retention-days": "14",
            })
        download = next(step for step in after if step.get("uses", "").startswith("actions/download-artifact@"))
        self.assertEqual(download["uses"], "actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333")
        self.assertNotIn("if", download)
        self.assertNotIn("continue-on-error", download)
        self.assertEqual(download["with"], {
            "name": "voice-measurement-before-${{ github.run_id }}-${{ github.run_attempt }}",
            "path": "${{ runner.temp }}/voice-before",
        })
        self.assertLess(after.index(download), next(i for i, step in enumerate(after) if step.get("name") == "Capture and compare sanitized receipts"))

    def test_every_job_is_hard_disabled_and_the_graph_keeps_ownership(self):
        workflow = policy.load_workflows()["voice-rest-measurement.yml"]
        self.assertEqual(set(workflow["on"]), {"workflow_dispatch"})
        self.assertEqual(workflow["permissions"], {})
        self.assertEqual(workflow["concurrency"], {
            "group": "voice-rest-measurement-disabled", "cancel-in-progress": "false"})
        jobs = workflow["jobs"]
        self.assertEqual(set(jobs), {"staging-window", "measurement-before", "voice-first",
                                     "measurement-after", "staging-use-release"})
        self.assertNotIn("defaults", workflow)
        for name, job in jobs.items():
            with self.subTest(job=name):
                self.assertNotIn("continue-on-error", job)
                self.assertNotIn("defaults", job)
                for step in job.get("steps", []):
                    self.assertNotIn("continue-on-error", step)
                    self.assertNotIn("shell", step)
        guards = {
            "staging-window": "${{ false && github.ref == 'refs/heads/main' }}",
            "measurement-before": "${{ false && github.ref == 'refs/heads/main' && needs.staging-window.result == 'success' }}",
            "voice-first": "${{ false && github.ref == 'refs/heads/main' && needs.staging-window.result == 'success' && needs.measurement-before.result == 'success' }}",
            "measurement-after": "${{ false && github.ref == 'refs/heads/main' && needs.voice-first.result == 'success' }}",
            "staging-use-release": "${{ false && always() && github.ref == 'refs/heads/main' && needs.staging-window.outputs.expires_at != '' }}",
        }
        self.assertEqual(GUARDS, guards)
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
        self.assertEqual(set(jobs["measurement-before"]["needs"] if isinstance(jobs["measurement-before"]["needs"], list) else [jobs["measurement-before"]["needs"]]), {"staging-window"})
        self.assertEqual(set(voice["needs"]), {"staging-window", "measurement-before"})
        self.assertEqual(set(jobs["measurement-after"]["needs"]),
                         {"staging-window", "measurement-before", "voice-first"})
        self.assertEqual(voice["uses"], "./.github/workflows/voice-synthetic.yml")
        self.assertEqual(voice["with"]["artifact_invocation"], "first")
        for name, output in (("staging_use_expires_at", "expires_at"),
                             ("staging_use_sha", "sha"),
                             ("staging_use_deployment", "deployment_id")):
            self.assertEqual(voice["with"][name], "${{ needs.staging-window.outputs." + output + " }}")
        release = jobs["staging-use-release"]
        self.assertEqual(set(release["needs"]), {"staging-window", "measurement-before", "voice-first", "measurement-after"})
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
