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
def fixture_base(content):
    return re.sub(
        r"^[ \t]*(?:UNREADY_ALERT_FAILURES|OPS_ALERT_WEBHOOK_URL)[ \t]*=.*(?:\n|$)",
        "",
        content,
        flags=re.MULTILINE,
    )


BASE = fixture_base(Path(__file__).parents[1].joinpath("wrangler.toml").read_text())


class AlertBindingTests(unittest.TestCase):
    def check_config(self, content):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "wrangler.toml"
            path.write_text(content)
            return checker.check(path)

    def test_missing_optional_secret_is_log_only(self):
        self.assertEqual(self.check_config(BASE), [])

    def test_optional_fixture_keys_can_be_indented(self):
        for section in ["vars", "env.staging.vars", "env.production.vars"]:
            for indent in ["", "  ", "\t"]:
                with self.subTest(section=section, indent=repr(indent)):
                    config = BASE.replace(
                        f"[{section}]",
                        f'[{section}]\n{indent}UNREADY_ALERT_FAILURES = "5" # operator tuning',
                    )
                    self.assertEqual(self.check_config(config), [], "indented tuning is valid TOML")
                    self.assertEqual(fixture_base(config), BASE)
                    config = config.replace(
                        f"[{section}]",
                        f'[{section}]\n{indent}OPS_ALERT_WEBHOOK_URL = "synthetic-secret"',
                    )
                    normalized = fixture_base(config)
                    self.assertEqual(normalized, BASE)
                    replacement = normalized.replace(
                        f"[{section}]", f'[{section}]\nUNREADY_ALERT_FAILURES = "3"'
                    )
                    self.assertEqual(self.check_config(replacement), [], "fixtures must not duplicate tuning keys")

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

    def test_ingress_is_staging_only_and_deployed_dark(self):
        staging = BASE.split("[env.staging.vars]", 1)[1].split("\n[", 1)[0]
        self.assertIn('INTERNAL_ACTIONS_INGRESS = "1"', staging)
        self.assertEqual(self.check_config(BASE), [])
        for section in ["vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(f"[{section}]", f'[{section}]\nINTERNAL_ACTIONS_INGRESS = "1"', 1)
                errors = self.check_config(config)
                self.assertTrue(any("INTERNAL_ACTIONS_INGRESS is staging-only" in e for e in errors), errors)
        # Absent from staging is also fine: the route is simply dark.
        self.assertEqual(
            self.check_config(BASE.replace('INTERNAL_ACTIONS_INGRESS = "1"\n', "")), []
        )

    def test_ingress_value_must_be_exactly_one(self):
        for value in ['"0"', '"true"', '""', '"1 "', "1"]:
            with self.subTest(value=value):
                config = BASE.replace('INTERNAL_ACTIONS_INGRESS = "1"', f"INTERNAL_ACTIONS_INGRESS = {value}")
                self.assertTrue(any('must be exactly "1"' in e for e in self.check_config(config)))

    def test_boot_publish_is_staging_only(self):
        staging = BASE.split("[env.staging.vars]", 1)[1].split("\n[", 1)[0]
        self.assertIn('TWO_COMMANDS_PUBLISH_ON_BOOT = "1"', staging)
        self.assertEqual(self.check_config(BASE), [])
        for section in ["vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(f"[{section}]", f'[{section}]\nTWO_COMMANDS_PUBLISH_ON_BOOT = "1"', 1)
                errors = self.check_config(config)
                self.assertTrue(any("TWO_COMMANDS_PUBLISH_ON_BOOT is staging-only" in e for e in errors), errors)
        # Absent from staging is also fine: boot publication is simply off.
        self.assertEqual(
            self.check_config(BASE.replace('TWO_COMMANDS_PUBLISH_ON_BOOT = "1"\n', "")), []
        )

    def test_boot_publish_value_must_be_exactly_one(self):
        for value in ['"0"', '"true"', '""', '"1 "', "1"]:
            with self.subTest(value=value):
                config = BASE.replace(
                    'TWO_COMMANDS_PUBLISH_ON_BOOT = "1"', f"TWO_COMMANDS_PUBLISH_ON_BOOT = {value}"
                )
                self.assertTrue(any('must be exactly "1"' in e for e in self.check_config(config)))

    def test_voice_gate_is_staging_only(self):
        staging = BASE.split("[env.staging.vars]", 1)[1].split("\n[", 1)[0]
        self.assertIn('TWO_VOICE = "1"', staging)
        self.assertEqual(self.check_config(BASE), [])
        for section in ["vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(f"[{section}]", f'[{section}]\nTWO_VOICE = "1"', 1)
                errors = self.check_config(config)
                self.assertTrue(any("TWO_VOICE is staging-only" in e for e in errors), errors)
        # Absent from staging is also fine: voice is simply off (rollback).
        self.assertEqual(self.check_config(BASE.replace('TWO_VOICE = "1"\n', "")), [])

    def test_voice_gate_value_must_be_exactly_one(self):
        for value in ['"0"', '"true"', '""', '"1 "', "1"]:
            with self.subTest(value=value):
                config = BASE.replace('TWO_VOICE = "1"', f"TWO_VOICE = {value}")
                self.assertTrue(any('must be exactly "1"' in e for e in self.check_config(config)))

    def test_receiver_config_is_never_a_toml_var(self):
        for section in ["vars", "env.staging.vars", "env.production.vars"]:
            for key in ["TWO_INTERNAL_KEYS", "TWO_INTERNAL_ACTIONS", "TWO_INTERNAL_BIND", "TWO_INTERNAL_CONTAINER"]:
                with self.subTest(section=section, key=key):
                    config = BASE.replace(f"[{section}]", f'[{section}]\n{key} = "synthetic-secret"', 1)
                    errors = self.check_config(config)
                    self.assertTrue(any(f"{key} is an Operator-set Worker secret" in e for e in errors), errors)
                    self.assertFalse(any("synthetic-secret" in e for e in errors))


if __name__ == "__main__":
    unittest.main()
