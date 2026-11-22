import sys
import unittest
from unittest.mock import patch

import pr_evidence as evidence

sys.path.insert(0, "attestation-policy/.github/scripts")
import pr_attestation as policy

HEAD, OLD = "a" * 40, "b" * 40
REPO = "TogetherWeOwn/two-bot-next"


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
        body = "\n".join([
            "## Verification", "- Ran the evidence fixtures.",
            "## Attestation", "risk_tier: T1", "```md\n## Example heading\n```",
            "### Claims", "- C1: Verifies a declaration.",
            "### Tests", "- C1 | test=test_sample.py::test_present | failed_before=not-run | proof=fixture",
            "### High-risk checklist", *(f"- [ ] D{i}: Fixture only." for i in range(1, 9)),
        ])
        files = [{"filename": "test_sample.py", "status": "modified"}]
        read = lambda _: "class ExampleTests(TestCase):\n    def test_present(self): pass"
        evidence.validate(policy, body, files, read)
        fenced = "\n## Notes\n```md\n## Verification\n- Looks verified.\n```"
        link = f"https://github.com/{REPO}/actions/runs/12/job/34"
        for changed, problem in (
                (body.replace("test_present", "test_gone"), "no unique matching test"),
                (body.replace("test_present", "test_gone") + "\n## attestation", "no unique matching test"),
                (body + "\n## verification\n- Alternate account.", "Verification"),
                (body.replace("Ran the evidence fixtures.", link) + fenced, "can go stale"),
                (body.replace("- Ran the evidence fixtures.", "```\npython3 -m unittest\n```") + fenced, "Verification"),
                (body.replace("- Ran the evidence fixtures.", "-\n##\tNotes\n- Ran tests."), "Verification")):
            with self.assertRaisesRegex(evidence.EvidenceError, problem):
                evidence.validate(policy, changed, files, read)
        with self.assertRaisesRegex(evidence.EvidenceError, "test_file_not_in_head_diff"):
            evidence.validate(policy, body, [{"filename": "test_sample.py", "status": "removed"}], read)
        for heading in ("## Verification\n", "## Attestation\n"):
            for block in (f"> [Old CI]({link})", f"```\n{link}\n```", f"    {link}"):
                with self.assertRaisesRegex(evidence.EvidenceError, "can go stale"):
                    evidence.validate(policy, body.replace(heading, f"{heading}{block}\n"), files, read)
