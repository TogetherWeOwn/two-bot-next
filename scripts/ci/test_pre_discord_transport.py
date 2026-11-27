"""Exercise the hosted preflight as the protected caller would invoke it."""

from pathlib import Path
import subprocess
import sys
import unittest


PREFLIGHT = Path(__file__).resolve().parents[1] / "pre_discord_transport.py"


class HostedDiscordTransportTests(unittest.TestCase):
    def test_hosted_transport_refuses_before_provisioning(self):
        for env in ({}, {"DISCORD_GATEWAY_URL": "ws://127.0.0.1:8081",
                         "DISCORD_API_BASE": "http://127.0.0.1:8081"}):
            with self.subTest(env=env):
                result = subprocess.run([sys.executable, "-I", str(PREFLIGHT)],
                                        env=env, capture_output=True, text=True, timeout=5, check=False)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertTrue(result.stderr.startswith("pre_hosted_discord_transport_unavailable:"))
                for requirement in ("DISCORD_GATEWAY_URL", "DISCORD_API_BASE", "same-namespace fixtures",
                                    "public Discord egress denied on every start"):
                    self.assertIn(requirement, result.stderr)
