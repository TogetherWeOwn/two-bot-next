"""Offline contract for the staging measurement workflow."""

from copy import deepcopy
import unittest

from voice_measurement_policy import GUARDS, WORKFLOW
import test_staging_voice_mutual_exclusion_acceptance as lifecycle
import test_workflows as policy


class VoiceMeasurementSkeletonTests(unittest.TestCase):
    def test_terminal_follows_cleanup_and_uploads_refusal(self):
        job = policy.load_workflows()[WORKFLOW]["jobs"]["measurement-terminal"]
        self.assertEqual(set(job["needs"]), {
            "staging-window", "measurement-before", "voice-first", "measurement-after",
            "voice-second", "measurement-final", "staging-use-release",
        })
        steps = job["steps"]
        self.assertEqual(len(steps), 9)
        for step, name, path in zip(steps[2:6],
                ("staging-use-measurement-stop-${{ github.run_id }}-${{ github.run_attempt }}",
                 "voice-measurement-candidate-${{ github.run_id }}-${{ github.run_attempt }}",
                 "voice-synthetic-${{ github.run_id }}-first",
                 "voice-synthetic-${{ github.run_id }}-second"),
                ("cleanup", "candidate", "first", "second")):
            expected = {
                "id": path,
                "uses": "actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333",
                "continue-on-error": "true",
                "with": {"name": name, "path": "${{ runner.temp }}/voice-evidence/" + path},
            }
            if path == "second":
                expected["if"] = "${{ needs.measurement-after.outputs.second_required == 'true' }}"
            self.assertEqual(step, expected)
        metadata, evaluate, upload = steps[6:]
        grant_env = {f"STAGING_USE_{name}": "${{ needs.staging-window.outputs." + output + " }}"
                     for name, output in [("SHA", "sha"), ("DEPLOYMENT", "deployment_id"), ("EXPIRES_AT", "expires_at")]}
        self.assertEqual(metadata, {
            "name": "Reconstruct the original grant metadata", "id": "grant", "continue-on-error": "true",
            "env": grant_env, "run": 'mkdir -p "$RUNNER_TEMP/voice-evidence"\n'
            'node wrangler/scripts/voice-measurement-metadata.mjs "$RUNNER_TEMP/voice-evidence/grant.json"\n',
        })
        evidence = "${{ steps.cleanup.outcome == 'success' && steps.candidate.outcome == 'success' && " \
                   "steps.first.outcome == 'success' && steps.grant.outcome == 'success' && " \
                   "(needs.measurement-after.outputs.second_required == 'false' || steps.second.outcome == 'success') && " \
                   "needs.measurement-after.outputs.second_required || '' }}"
        self.assertEqual(evaluate, {
            "name": "Evaluate terminal evidence",
            "env": {**grant_env, "EVIDENCE_DIR": "${{ runner.temp }}/voice-evidence", "SECOND_REQUIRED": evidence,
                    **{"RESULT_" + name.upper().replace("-", "_"): "${{ needs." + name + ".result }}"
                       for name in job["needs"]}},
            "run": 'python3 scripts/ci/voice_measurement_terminal.py "$RUNNER_TEMP/voice-terminal.json"',
        })
        self.assertEqual(upload, {
            "name": "Retain the terminal verdict", "if": "always()",
            "uses": "actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9",
            "with": {"name": "voice-measurement-terminal-${{ github.run_id }}-${{ github.run_attempt }}",
                     "path": "${{ runner.temp }}/voice-terminal.json", "if-no-files-found": "error", "retention-days": "14"},
        })

    def test_both_policy_consumers_reject_lock_or_guard_drift(self):
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
                job["if"] = "${{ true }}"
                self.assertTrue(policy.workflow_policy_errors(changed))
                self.assertTrue(lifecycle.repository_errors(changed, canonical))
        queue = workflows[WORKFLOW]["concurrency"]
        for field, value in (("on", {"push": ""}), ("permissions", {"contents": "write"}),
                             ("concurrency", dict(queue, group=canonical))):
            with self.subTest(field=field, value=value):
                changed = deepcopy(workflows)
                changed[WORKFLOW][field] = value
                self.assertTrue(policy.workflow_policy_errors(changed))
                self.assertTrue(lifecycle.repository_errors(changed, canonical))
        changed = deepcopy(workflows)
        changed[WORKFLOW]["jobs"]["voice-first"]["uses"] = "./.github/workflows/other.yml"
        self.assertTrue(policy.workflow_policy_errors(changed))
        self.assertTrue(lifecycle.repository_errors(changed, canonical))

    def test_policy_rejects_unapproved_worker_url_bindings(self):
        workflows = policy.load_workflows()
        production = "${{ vars.PRODUCTION_WORKER_URL }}"
        for job_id, step, field, value in (
            ("staging-window", 2, "PRODUCTION_WORKER_URL", production),
            ("staging-use-release", 2, "PRODUCTION_WORKER_URL", production),
            ("measurement-before", 0, "PRODUCTION_WORKER_URL", production),
            ("measurement-after", None, "STAGING_WORKER_URL", "${{ vars.STAGING_WORKER_URL }}"),
            ("measurement-final", 6, "PRODUCTION_WORKER_URL", "${{ vars.STAGING_WORKER_URL }}"),
        ):
            with self.subTest(job=job_id, step=step, field=field):
                changed = deepcopy(workflows)
                job = changed[WORKFLOW]["jobs"][job_id]
                target = job if step is None else job["steps"][step]
                target.setdefault("env", {})[field] = value
                self.assertIn(f"{WORKFLOW}:{job_id}: deployment/probe alternative outside approved deploy workflows",
                              policy.workflow_policy_errors(changed))

    def test_policy_keeps_scanning_the_capture_script(self):
        workflows = policy.load_workflows()
        step = workflows[WORKFLOW]["jobs"]["measurement-before"]["steps"][2]
        step["run"] += 'test -n "$PRODUCTION_WORKER_URL"\n'
        self.assertIn(f"{WORKFLOW}:measurement-before: deployment/probe alternative outside approved deploy workflows",
                      policy.workflow_policy_errors(workflows))

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
            self.assertEqual(len(steps), 4 if steps is before else 7)
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
            self.assertEqual(steps.index(capture), 2 if steps is before else 5)
            self.assertNotIn("if", capture)
            self.assertNotIn("continue-on-error", capture)
            self.assertEqual(capture["env"], {
                "STAGING_USE_SHA": "${{ needs.staging-window.outputs.sha }}",
                "STAGING_USE_DEPLOYMENT": "${{ needs.staging-window.outputs.deployment_id }}",
                "STAGING_USE_EXPIRES_AT": "${{ needs.staging-window.outputs.expires_at }}",
                "STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}",
                "PRODUCTION_WORKER_URL": "${{ vars.PRODUCTION_WORKER_URL }}",
                "METRICS_SCRAPE_TOKEN": "${{ secrets.STAGING_METRICS_SCRAPE_TOKEN }}",
                "OWNERSHIP_CONTROL_TOKEN": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}",
                "STAGING_USE_RECEIPT": "${{ runner.temp }}/staging-use-measurement-" + ("before" if steps is before else "after") + ".jsonl",
            })
            commands = capture["run"].splitlines()
            assertion = 'node wrangler/scripts/staging-use-pipeline.mjs consume'
            self.assertEqual(commands[0], assertion)
            self.assertEqual(commands[1], 'node wrangler/scripts/voice-measurement-metadata.mjs "$RUNNER_TEMP/voice-metadata.json"')
            self.assertEqual(commands[2], 'node wrangler/scripts/voice-rest-measurement.mjs capture --metadata "$RUNNER_TEMP/voice-metadata.json" --metrics-url "$STAGING_WORKER_URL" --out "$RUNNER_TEMP/voice-' + ("before" if steps is before else "after") + '.json"')
            self.assertEqual(commands[3], assertion)
            self.assertEqual(len(commands), 4 if steps is before else 8)
            if steps is after:
                self.assertEqual(capture["id"], "decision")
                self.assertEqual(jobs["measurement-after"]["outputs"], {"second_required": "${{ steps.decision.outputs.second_required }}"})
                self.assertEqual(commands[4], 'second_required=$(node wrangler/scripts/voice-measurement-decision.mjs "$RUNNER_TEMP/voice-before/voice-before.json" "$RUNNER_TEMP/voice-after.json" "$RUNNER_TEMP/voice-decision.json")')
                self.assertEqual(commands[5:7], ['test -s "$RUNNER_TEMP/voice-lifecycle-first.json"',
                                                  'test -s "$RUNNER_TEMP/voice-decision.json"'])
                self.assertEqual(commands[7], 'printf \'second_required=%s\\n\' "$second_required" >> "$GITHUB_OUTPUT"')
            upload = steps[3] if steps is before else steps[6]
            self.assertNotIn("if", upload)
            self.assertNotIn("continue-on-error", upload)
            self.assertEqual(upload["uses"], "actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9")
            suffix = ("before" if steps is before else
                      "${{ steps.decision.outputs.second_required == 'false' && 'candidate' || 'intermediate' }}")
            path = ("${{ runner.temp }}/voice-before.json" if steps is before else
                    "".join("${{ runner.temp }}/" + item + "\n" for item in (
                        "voice-decision.json", "voice-lifecycle-first.json", "voice-before/voice-before.json", "voice-after.json")))
            self.assertEqual(upload["with"], {
                "name": "voice-measurement-" + suffix + "-${{ github.run_id }}-${{ github.run_attempt }}",
                "path": path,
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
        self.assertEqual(after[3], {
            "uses": "actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333",
            "with": {"name": "voice-synthetic-${{ github.run_id }}-first",
                     "path": "${{ runner.temp }}/voice-first"},
        })
        self.assertEqual(after[4], {
            "name": "Validate the first lifecycle",
            "run": 'python3 scripts/ci/voice_lifecycle_evidence.py "$RUNNER_TEMP/voice-first" "$GITHUB_RUN_ID" "$RUNNER_TEMP/voice-lifecycle-first.json"',
        })

    def test_second_interval_evidence_is_fail_closed(self):
        jobs = policy.load_workflows()[WORKFLOW]["jobs"]
        final, after = jobs["measurement-final"], jobs["measurement-after"]
        steps, first_steps = final["steps"], after["steps"]
        for key in ("permissions", "environment", "runs-on", "timeout-minutes"):
            self.assertEqual(final[key], after[key], key)
        self.assertEqual((len(steps), steps[:2]), (8, first_steps[:2]))
        for step, name, path in zip(steps[2:5],
                ("voice-measurement-before-${{ github.run_id }}-${{ github.run_attempt }}",
                 "voice-measurement-intermediate-${{ github.run_id }}-${{ github.run_attempt }}",
                 "voice-synthetic-${{ github.run_id }}-second"),
                ("voice-before", "voice-middle", "voice-second")):
            self.assertEqual(step, {
                "uses": "actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333",
                "with": {"name": name, "path": "${{ runner.temp }}/" + path},
            })
        self.assertEqual(steps[5], {
            "name": "Validate the second lifecycle",
            "run": 'python3 scripts/ci/voice_lifecycle_evidence.py "$RUNNER_TEMP/voice-second" "$GITHUB_RUN_ID" "$RUNNER_TEMP/voice-lifecycle-second.json"',
        })
        capture = steps[6]
        self.assertNotIn("if", capture)
        self.assertEqual(capture["env"], dict(first_steps[5]["env"],
            STAGING_USE_RECEIPT="${{ runner.temp }}/staging-use-measurement-final.jsonl"))
        bookend = first_steps[5]["run"].splitlines()[:4]
        self.assertEqual(capture["run"].splitlines(), [
            *(command.replace('/voice-after.json', '/voice-final.json') for command in bookend),
            'test -s "$RUNNER_TEMP/voice-middle/voice-lifecycle-first.json"',
            'test -s "$RUNNER_TEMP/voice-lifecycle-second.json"',
            'node wrangler/scripts/voice-measurement-decision.mjs "$RUNNER_TEMP/voice-before/voice-before.json" "$RUNNER_TEMP/voice-middle/voice-after.json" "$RUNNER_TEMP/voice-final.json" "$RUNNER_TEMP/voice-combined.json"',
        ])
        self.assertEqual(steps[-1], {"uses": first_steps[-1]["uses"], "with": {
            "name": "voice-measurement-candidate-${{ github.run_id }}-${{ github.run_attempt }}",
            "if-no-files-found": "error", "retention-days": "14",
            "path": "".join("${{ runner.temp }}/" + item + "\n" for item in (
                "voice-combined.json", "voice-middle/voice-lifecycle-first.json", "voice-lifecycle-second.json",
                "voice-before/voice-before.json", "voice-middle/voice-after.json", "voice-final.json")),
        }})

    def test_only_fresh_protected_main_dispatch_can_hold_the_grant(self):
        workflows = policy.load_workflows()
        workflow = workflows[WORKFLOW]
        self.assertEqual(set(workflow["on"]), {"workflow_dispatch"})
        self.assertEqual(workflow["permissions"], {})
        canonical = workflows[lifecycle.DEPLOY]["concurrency"]["group"]
        self.assertEqual(workflow["concurrency"], {
            "group": "${{ github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' "
            "&& github.ref_protected && github.run_attempt == 1 && '" + canonical + "' || "
            "format('voice-rest-measurement-refused-{0}-{1}', github.run_id, github.run_attempt) }}",
            "cancel-in-progress": "false", "queue": "max",
        })
        jobs = workflow["jobs"]
        self.assertNotIn("defaults", workflow)
        for name, job in jobs.items():
            with self.subTest(job=name):
                self.assertNotIn("continue-on-error", job)
                self.assertNotIn("defaults", job)
                for index, step in enumerate(job.get("steps", [])):
                    if name == "measurement-terminal" and index in range(2, 7):
                        self.assertEqual(step.get("continue-on-error"), "true")
                    else:
                        self.assertNotIn("continue-on-error", step)
                    self.assertNotIn("shell", step)
        first = "github.ref == 'refs/heads/main' && github.ref_protected && github.run_attempt == 1"
        guards = {
            "staging-window": first,
            "measurement-before": first + " && needs.staging-window.result == 'success'",
            "voice-first": first + " && needs.staging-window.result == 'success' && needs.measurement-before.result == 'success'",
            "measurement-after": first + " && needs.voice-first.result == 'success'",
            "voice-second": first + " && needs.measurement-after.result == 'success' && needs.measurement-after.outputs.second_required == 'true'",
            "measurement-final": first + " && needs.voice-second.result == 'success'",
            "staging-use-release": "always() && " + first + " && needs.staging-window.outputs.expires_at != ''",
            "measurement-terminal": "always() && " + first,
            "rerun-refusal": "always() && !(" + first + ")",
        }
        self.assertEqual({name: job["if"] for name, job in jobs.items()},
                         {name: "${{ " + guard + " }}" for name, guard in guards.items()})
        refusal = jobs["rerun-refusal"]
        self.assertEqual(refusal["needs"], "measurement-terminal")
        self.assertEqual(refusal["permissions"], {})
        self.assertEqual(refusal["steps"], [{"run": "exit 1"}])
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
        self.assertEqual(voice["with"]["deploy_staging_call"], "true")
        for name, output in (("staging_use_expires_at", "expires_at"),
                             ("staging_use_sha", "sha"),
                             ("staging_use_deployment", "deployment_id")):
            self.assertEqual(voice["with"][name], "${{ needs.staging-window.outputs." + output + " }}")
        second = jobs["voice-second"]
        self.assertEqual(second["uses"], "./.github/workflows/voice-synthetic.yml")
        self.assertEqual(second["permissions"], {"contents": "read"})
        self.assertEqual(second["with"], {
            **{key: value for key, value in voice["with"].items() if key != "artifact_invocation"},
            "artifact_invocation": "second",
        })
        expected_secrets = {
            "TWO_VOICE_SYNTHETIC_BOT_TOKEN": "${{ secrets.TWO_VOICE_SYNTHETIC_BOT_TOKEN }}",
            "STAGING_OWNERSHIP_CONTROL_TOKEN": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}",
        }
        self.assertEqual(voice["secrets"], expected_secrets)
        self.assertEqual(second["secrets"], expected_secrets)
        release = jobs["staging-use-release"]
        self.assertEqual(set(jobs["voice-second"]["needs"]), {"staging-window", "measurement-after"})
        self.assertEqual(set(jobs["measurement-final"]["needs"]), {"staging-window", "measurement-after", "voice-second"})
        self.assertEqual(set(release["needs"]), {"staging-window", "measurement-before", "voice-first", "measurement-after", "voice-second", "measurement-final"})
        finish = next(s for s in release["steps"] if s.get("name") == "Release only this run's window")
        self.assertNotIn("if", finish)
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
