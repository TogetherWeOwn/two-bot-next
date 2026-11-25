"""Offline consumer of a trusted PR evidence verdict at a merge attempt."""
from pathlib import Path
import sys
import unittest
from unittest.mock import ANY, Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / ".github/scripts"))
import pr_evidence as evidence


class MergeAttemptTests(unittest.TestCase):
    def test_merge_attempt_requires_current_verdict(self):
        repo, base, head = "example/repo", "a" * 40, "b" * 40
        original = {"number": 1, "state": "open", "base": {"repo": {"full_name": repo}, "sha": base},
                    "head": {"sha": head}, "body": "Original valid evidence"}
        edited = dict(original, body="Revised valid evidence")
        identity = (1, repo, base, head)
        reader, accept_merge, policy = Mock(), Mock(), object()
        reader.get.return_value = original
        # Isolate the consumer. The advisory fixtures exercise the actual evidence policy.
        self.enterContext(patch.object(evidence, "sources", return_value=[]))
        validate = self.enterContext(patch.object(evidence, "validate"))

        def attempt_merge(recorded):
            try:
                current = evidence.evaluate_current(policy, reader, *identity)
            except evidence.EvidenceError:
                return
            if recorded == current and recorded.matches_current(reader, *identity):
                accept_merge(recorded.head)

        prior = evidence.evaluate_current(policy, reader, *identity)
        for case, reads in (
                ("edit before validation", [edited, edited]),
                ("edit during validation", [original, edited]),
                ("edit after validation", [original, original, edited])):
            with self.subTest(case=case), patch.object(reader, "get", side_effect=reads) as get:
                attempt_merge(prior)
                self.assertEqual(get.call_count, len(reads))
                accept_merge.assert_not_called()

        reader.get.return_value = edited
        fresh = evidence.evaluate_current(policy, reader, *identity)
        self.assertEqual(prior.head, fresh.head)
        attempt_merge(fresh)
        validate.assert_called_with(policy, edited["body"], [], ANY)
        accept_merge.assert_called_once_with(head)
