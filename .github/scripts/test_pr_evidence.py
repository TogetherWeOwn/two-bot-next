import sys
import unittest
from unittest.mock import Mock, patch

import pr_evidence as evidence

sys.path.insert(0, "attestation-policy/.github/scripts")
import pr_attestation as policy

HEAD, OLD = "a" * 40, "b" * 40
REPO = "TogetherWeOwn/two-bot-next"
FILES = [{"filename": "test_sample.py", "status": "modified"}]
SOURCE = "class ExampleTests(TestCase):\n    def test_present(self): pass"
BODY = "\n".join([
    "## Verification", "- Ran the evidence fixtures.",
    "## Attestation", "risk_tier: T1", "```md\n## Example heading\n```",
    "### Claims", "- C1: Verifies a declaration.",
    "### Tests", "- C1 | test=test_sample.py::test_present | failed_before=not-run | proof=fixture",
    "### High-risk checklist", *(f"- [ ] D{i}: Fixture only." for i in range(1, 9)),
])
PR = {"number": 1, "state": "open", "base": {"repo": {"full_name": REPO}, "sha": OLD},
      "head": {"sha": HEAD}, "body": BODY}


class EvidenceTests(unittest.TestCase):
    def test_attempt_urls_only_after_head_and_job_match(self):
        run = f"{HEAD}\tpull_request\t.github/workflows/check.yml\t12\t2"
        job = f"34\t{HEAD}\t12\t2\tsuccess\tworker check"
        for actual_run, actual_job in ((run.replace(HEAD, OLD), job),
                                       (run, job.replace(HEAD, OLD)),
                                       (run, job.replace("\t2\t", "\t3\t"))):
            with patch.object(evidence, "command", side_effect=[actual_run, actual_job]):
                with self.assertRaisesRegex(evidence.EvidenceError, "head"):
                    evidence.publish(REPO, "pull_request", HEAD, "12", "2", "check.yml")
        with patch.object(evidence, "command", side_effect=[run, job.replace("\t2\t", "\t1\t")]):
            summary = evidence.publish(REPO, "pull_request", HEAD, "12", "2", "check.yml")
        self.assertIn(HEAD, summary)
        self.assertIn("/actions/runs/12/attempts/2", summary)
        self.assertIn("/actions/runs/12/job/34", summary)

    def test_test_name_matches_declaration_only(self):
        fixtures = {
            "sample.py": ("class DemoBase(TestCase): pass\nclass TestDemo(DemoBase):\n    def test_actual(self): pass\n"
                          "class HelperTests:\n    def test_helper(self): pass\n"
                          "# def test_stale(): pass\n", "TestDemo.test_actual"),
            "sample.rs": ('mod checks { #[tokio::test] async fn actual() {} }'
                          'const SAMPLE: &str = r#"#[test] fn stale() {}"#;'
                          'fn ordinary() {}', "checks::actual"),
            "sample.ts": ("test('actual', () => {}); /* test('stale', () => {}); */", "actual"),
        }
        for path, (source, name) in fixtures.items():
            self.assertEqual(list(evidence.named_tests(path, source)), [name], path)

    def test_stale_evidence_and_test_name_block(self):
        body, files = BODY, FILES
        read = lambda _: SOURCE
        evidence.validate(policy, body, files, read)
        fenced = "\n## Notes\n```md\n## Verification\n- Looks verified.\n```"
        link = f"https://github.com/{REPO}/actions/runs/12/job/34"
        policy_only = body.replace("### Tests", "### Tests\n    ```\n## Notes\n    ```")
        for changed, problem in (
                (body.replace("test_present", "test_gone"), "no unique matching test"),
                (body.replace("test_present", "test_gone") + "\n## attestation", "canonical Verification"),
                (body.replace("Ran the evidence fixtures.", link) + fenced, "can go stale"),
                (policy_only.replace("proof=fixture", f"proof={link}"), "can go stale"),
                (body.replace("- Ran the evidence fixtures.", "```\npython3 -m unittest\n```") + fenced, "Verification"),
                (body.replace("- Ran the evidence fixtures.", "    python3 -m unittest\n##\tNotes\n- Ran tests."), "Verification")):
            with self.assertRaisesRegex(evidence.EvidenceError, problem):
                evidence.validate(policy, changed, files, read)
        with self.assertRaisesRegex(evidence.EvidenceError, "test_file_not_in_head_diff"):
            evidence.validate(policy, body, [{"filename": "test_sample.py", "status": "removed"}], read)
        for alias in ("## verification", "## Verification ##", "# Verification", "## **Verification**"):
            with self.assertRaisesRegex(evidence.EvidenceError, "canonical Verification"):
                evidence.validate(policy, body + f"\n{alias}\n{link}", files, read)
        for heading in ("## Verification\n", "## Attestation\n"):
            blocks = (f"> `<!--` {link} `-->`",
                      f"```\n    ```\n## Notes\n```\n\n    ```\n\n{link}",
                      f"<details>\n## Notes\n</details>\n\n    {link}")
            for block in blocks:
                with self.assertRaisesRegex(evidence.EvidenceError, "can go stale"):
                    evidence.validate(policy, body.replace(heading, f"{heading}{block}\n"), files, read)


class CurrentEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.reader = Mock()
        self.reader.get.return_value = PR
        self.identity = (1, REPO, OLD, HEAD)
        self.enterContext(patch.object(evidence, "sources", return_value=FILES))
        self.enterContext(patch.object(evidence, "source_blob", return_value=SOURCE))

    def test_description_edit_evaluates_current_pair(self):
        def receive(event):
            # Model the advisory subscription for the two deliveries in this fixture.
            self.assertIn(event["action"], {"opened", "edited"})
            pr = event["pull_request"]
            return evidence.evaluate_current(policy, self.reader, event["number"],
                                             pr["base"]["repo"]["full_name"],
                                             pr["base"]["sha"], pr["head"]["sha"])

        prior = receive({"action": "opened", "number": 1, "pull_request": PR})
        self.assertTrue(prior.matches_current(self.reader, *self.identity))

        invalid = dict(PR, body=BODY.replace("test_present", "test_gone"))
        event = {"action": "edited", "number": 1, "changes": {"body": {"from": BODY}},
                 "pull_request": invalid}
        self.reader.get.return_value = invalid
        self.assertFalse(prior.matches_current(self.reader, *self.identity))
        with self.assertRaisesRegex(evidence.EvidenceError, "no unique matching test"):
            receive(event)

        updated = BODY.replace("proof=fixture", "proof=updated synthetic evidence")
        self.reader.get.return_value = dict(PR, body=updated)
        # The delayed edit payload still contains the invalid body.
        fresh = receive(event)
        self.assertEqual(prior.head, fresh.head)
        self.assertTrue(fresh.matches_current(self.reader, *self.identity))
        self.assertFalse(prior.matches_current(self.reader, *self.identity))
        self.assertNotIn(updated, repr(fresh))
        self.reader.get.assert_called_with("pulls/1")

    def test_merge_attempt_requires_current_verdict(self):
        accept_merge = Mock()

        def attempt_merge(recorded):
            try:
                current = evidence.evaluate_current(policy, self.reader, *self.identity)
            except evidence.EvidenceError:
                return
            if recorded == current and recorded.matches_current(self.reader, *self.identity):
                accept_merge(recorded.head)

        prior = evidence.evaluate_current(policy, self.reader, *self.identity)
        edited = dict(PR, body=BODY.replace("proof=fixture", "proof=updated synthetic evidence"))

        for case, reads in (
                ("edit before validation", [edited, edited]),
                ("edit during validation", [PR, edited]),
                ("edit after validation", [PR, PR, edited])):
            with self.subTest(case=case), patch.object(self.reader, "get", side_effect=reads) as get:
                attempt_merge(prior)
                self.assertEqual(get.call_count, len(reads))
                accept_merge.assert_not_called()

        self.reader.get.return_value = edited
        fresh = evidence.evaluate_current(policy, self.reader, *self.identity)
        self.assertEqual(prior.head, fresh.head)
        attempt_merge(fresh)
        accept_merge.assert_called_once_with(HEAD)

    def test_formatting_and_identity_changes_invalidate_verdict(self):
        prior = evidence.evaluate_current(policy, self.reader, *self.identity)
        for body in (BODY + "\n", BODY.replace("\n", "\r\n"), BODY + "\n<!-- updated -->"):
            with self.subTest(body=body[-25:]):
                self.reader.get.return_value = dict(PR, body=body)
                self.assertFalse(prior.matches_current(self.reader, *self.identity))
        for metadata, identity in (
                (dict(PR, head={"sha": OLD}), (1, REPO, OLD, OLD)),
                (dict(PR, number=2), (2, REPO, OLD, HEAD)),
                (dict(PR, base=dict(PR["base"], sha=HEAD)), (1, REPO, HEAD, HEAD)),
                (dict(PR, base=dict(PR["base"], repo={"full_name": "example/other"})),
                 (1, "example/other", OLD, HEAD)),
                (dict(PR, state="closed"), self.identity),
        ):
            self.reader.get.return_value = metadata
            self.assertFalse(prior.matches_current(self.reader, *self.identity))
            with self.assertRaisesRegex(evidence.EvidenceError, "PR identity moved"):
                evidence.evaluate_current(policy, self.reader, *self.identity)
            self.assertFalse(prior.matches_current(self.reader, *identity))

    def test_unavailable_metadata_and_concurrent_edit_refuse(self):
        prior = evidence.evaluate_current(policy, self.reader, *self.identity)
        for metadata in (None, {key: value for key, value in PR.items() if key != "body"},
                         dict(PR, body=None), dict(PR, body=["invalid"]), dict(PR, body="\ud800")):
            with self.subTest(metadata_type=type(metadata)):
                self.reader.get.return_value = metadata
                self.assertFalse(prior.matches_current(self.reader, *self.identity))
                with self.assertRaises(evidence.EvidenceError):
                    evidence.evaluate_current(policy, self.reader, *self.identity)

        self.reader.get.side_effect = PermissionError("synthetic credential")
        self.assertFalse(prior.matches_current(self.reader, *self.identity))
        with self.assertRaisesRegex(evidence.EvidenceError, r"^Could not read current PR metadata\.$"):
            evidence.evaluate_current(policy, self.reader, *self.identity)

        self.reader.get.side_effect = [PR, dict(PR, body=BODY + "\n")]
        with self.assertRaisesRegex(evidence.EvidenceError, "still current"):
            evidence.evaluate_current(policy, self.reader, *self.identity)
