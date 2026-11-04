"""Offline adoption contract for the public-only reusable security gate."""

from pathlib import Path
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
PIN = "feb1e0e2111da007f38ca75ee95c6f6944723d36"
PUBLIC_PR = "github.event_name == 'pull_request' && github.event.repository.private == false"


class PublicPrSecurityTests(unittest.TestCase):
    def setUp(self):
        self.workflow = yaml.load(
            (ROOT / ".github/workflows/check.yml").read_text(), Loader=yaml.BaseLoader)

    def test_caller_is_public_pr_only_and_immutable(self):
        job = self.workflow["jobs"]["public-pr-security"]
        self.assertEqual(job, {
            "name": "Public PR security",
            "if": PUBLIC_PR,
            "permissions": {"actions": "read", "contents": "read", "security-events": "write"},
            "uses": f"TogetherWeOwn/.github/.github/workflows/public-pr-security.yml@{PIN}",
        })
        self.assertEqual(self.workflow["permissions"], {})
        self.assertEqual(self.workflow["on"]["pull_request"]["types"],
                         ["opened", "synchronize", "reopened"])
        self.assertNotIn("pull_request_target", self.workflow["on"])
        self.assertEqual(self.workflow["concurrency"], {
            "group": "check-${{ github.event.pull_request.number || github.sha }}",
            "cancel-in-progress": "${{ github.event_name == 'pull_request' }}",
        })

    def test_existing_required_aggregate_consumes_the_security_verdict(self):
        job = self.workflow["jobs"]["ci-ok"]
        self.assertIn("public-pr-security", job["needs"])
        self.assertEqual(job["if"], "${{ always() }}")
        step = next(step for step in job["steps"] if step["name"] == "Aggregate CI verdict")
        self.assertEqual(step["env"]["SECURITY_RESULT"], "${{ needs.public-pr-security.result }}")
        self.assertEqual(step["env"]["SECURITY_REQUIRED"], "${{ " + PUBLIC_PR + " }}")
        self.assertIn('os.environ.get("SECURITY_RESULT", "")', step["run"])
        self.assertIn('os.environ.get("SECURITY_REQUIRED", "")', step["run"])

    def test_actions_dependabot_preserves_weekly_grouped_version_updates(self):
        config = yaml.load((ROOT / ".github/dependabot.yml").read_text(), Loader=yaml.BaseLoader)
        updates = config["updates"]
        self.assertEqual([(u["package-ecosystem"], u["directory"]) for u in updates],
                         [("cargo", "/"), ("cargo", "/fuzz"), ("docker", "/"),
                          ("github-actions", "/"), ("npm", "/wrangler")])
        actions = next(u for u in updates if u["package-ecosystem"] == "github-actions")
        self.assertEqual(actions["schedule"], {
            "interval": "weekly", "day": "monday", "time": "06:00", "timezone": "UTC"})
        self.assertEqual(actions["commit-message"], {"prefix": "ci(deps)"})
        self.assertEqual(actions["labels"], ["dependencies"])
        self.assertEqual(actions["open-pull-requests-limit"], "5")
        self.assertEqual(actions["groups"], {
            "actions-minor-patch": {"applies-to": "version-updates", "update-types": ["minor", "patch"]},
            "actions-major": {"applies-to": "version-updates", "update-types": ["major"]},
        })


if __name__ == "__main__":
    unittest.main()
