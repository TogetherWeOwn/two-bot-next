#!/usr/bin/env python3
"""Offline regressions for optional alert bindings. No Worker API calls."""
import importlib.util
import re
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "check_env_bindings", Path(__file__).with_name("check-env-bindings.py")
)
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)
# Optional operator tuning in the real config must not create duplicate keys
# in our synthetic fixtures or change the omitted-binding test's meaning.
BASE = re.sub(
    r"^(?:UNREADY_ALERT_FAILURES|OPS_ALERT_WEBHOOK_URL)\s*=.*(?:\n|$)",
    "",
    Path(__file__).parents[1].joinpath("wrangler.toml").read_text(),
    flags=re.MULTILINE,
)


class AlertBindingTests(unittest.TestCase):
    def check_config(self, content):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "wrangler.toml"
            path.write_text(content)
            return checker.check(path)

    def test_missing_optional_secret_is_log_only(self):
        self.assertEqual(self.check_config(BASE), [])

    def test_plaintext_webhook_is_rejected_without_echoing_value(self):
        for section in ["vars", "env.staging.vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(
                    f"[{section}]", f'[{section}]\nOPS_ALERT_WEBHOOK_URL = "synthetic-secret"'
                )
                errors = self.check_config(config)
                self.assertEqual(len(errors), 1)
                self.assertIn("must be an optional Worker secret", errors[0])
                self.assertNotIn("synthetic-secret", errors[0])

    def test_threshold_can_be_env_specific_or_omitted(self):
        for section in ["vars", "env.staging.vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(f"[{section}]", f'[{section}]\nUNREADY_ALERT_FAILURES = "5"')
                self.assertEqual(self.check_config(config), [])

    def test_invalid_threshold_is_rejected(self):
        for value in ['"0"', '"-1"', '"1.5"', '" 5 "', '"1e1"', '""', '"9007199254740992"', '"٣"', "3"]:
            with self.subTest(value=value):
                config = BASE.replace("[env.staging.vars]", f"[env.staging.vars]\nUNREADY_ALERT_FAILURES = {value}")
                self.assertTrue(any("positive safe-integer string" in e for e in self.check_config(config)))

    def test_optional_handling_does_not_weaken_required_bindings(self):
        config = BASE.replace('KEEPALIVE_SECONDS = "60"\nTWO_GUILD_NAME', "TWO_GUILD_NAME", 1)
        self.assertTrue(any("missing vars ['KEEPALIVE_SECONDS']" in e for e in self.check_config(config)))


if __name__ == "__main__":
    unittest.main()
