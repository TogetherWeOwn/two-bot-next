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
            "sample.py": ("class TestDemo:\n    def test_actual(self): pass\n"
                          "# def test_stale(): pass\n", "TestDemo.test_actual"),
            "sample.rs": ('mod checks { #[tokio::test] async fn actual() {} }'
                          'const SAMPLE: &str = r#"#[test] fn stale() {}"#;'
                          'fn ordinary() {}', "checks::actual"),
            "sample.ts": ("test('actual', () => {}); /* test('stale', () => {}); */", "actual"),
        }
        for path, (source, name) in fixtures.items():
            with self.subTest(path=path):
                self.assertEqual(list(evidence.named_tests(path, source)), [name])

    def test_stale_evidence_and_test_name_block(self):
        body = "\n".join([
            "## Verification", "- Ran the evidence fixtures.",
            "## Attestation", "risk_tier: T1", "### Claims", "- C1: Verifies a declaration.",
            "### Tests", "- C1 | test=test_sample.py::test_present | failed_before=not-run | proof=fixture",
            "### High-risk checklist", *(f"- [ ] D{i}: Fixture only." for i in range(1, 9)),
        ])
        files = [{"filename": "test_sample.py", "status": "modified"}]
        read = lambda _: "def test_present(): pass"
        evidence.validate(policy, body, files, read)
        for changed, problem in (
                (body.replace("test_present", "test_gone"), "no unique matching test"),
                (body.replace("Ran the evidence fixtures.", f"https://github.com/{REPO}/actions/runs/12/job/34"),
                 "can go stale"),
                (body.replace("- Ran the evidence fixtures.", "-"), "Verification")):
            with self.assertRaisesRegex(evidence.EvidenceError, problem):
                evidence.validate(policy, changed, files, read)
        with self.assertRaisesRegex(evidence.EvidenceError, "test_file_not_in_head_diff"):
            evidence.validate(policy, body, [{"filename": "test_sample.py", "status": "removed"}], read)
