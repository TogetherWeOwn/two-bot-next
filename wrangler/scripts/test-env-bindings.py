#!/usr/bin/env python3
"""Offline regressions for optional alert bindings. No Worker API calls."""
import importlib.util
import re
import subprocess
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

    def test_eventsub_ingress_is_staging_only_and_deployed_dark(self):
        staging = BASE.split("[env.staging.vars]", 1)[1].split("\n[", 1)[0]
        self.assertIn('EVENTSUB_INGRESS = "1"', staging)
        self.assertEqual(self.check_config(BASE), [])
        for section in ["vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(f"[{section}]", f'[{section}]\nEVENTSUB_INGRESS = "1"', 1)
                errors = self.check_config(config)
                self.assertTrue(any("EVENTSUB_INGRESS is staging-only" in e for e in errors), errors)
        # Absent from staging is also fine: the route is simply dark.
        self.assertEqual(
            self.check_config(BASE.replace('EVENTSUB_INGRESS = "1"\n', "")), []
        )

    def test_eventsub_ingress_value_must_be_exactly_one(self):
        for value in ['"0"', '"true"', '""', '"1 "', "1"]:
            with self.subTest(value=value):
                config = BASE.replace('EVENTSUB_INGRESS = "1"', f"EVENTSUB_INGRESS = {value}")
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

    def test_voice_presence_is_staging_only(self):
        staging = BASE.split("[env.staging.vars]", 1)[1].split("\n[", 1)[0]
        self.assertIn('TWO_VOICE_PRESENCE = "1"', staging)
        for section in ["vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(f"[{section}]", f'[{section}]\nTWO_VOICE_PRESENCE = "1"', 1)
                errors = self.check_config(config)
                self.assertTrue(any("TWO_VOICE_PRESENCE is staging-only" in e for e in errors), errors)
        self.assertEqual(self.check_config(BASE.replace('TWO_VOICE_PRESENCE = "1"\n', "")), [])
        for value in ['"0"', '"true"', '""', "1"]:
            with self.subTest(value=value):
                config = BASE.replace('TWO_VOICE_PRESENCE = "1"', f"TWO_VOICE_PRESENCE = {value}")
                self.assertTrue(any('must be exactly "1"' in e for e in self.check_config(config)))

    def test_music_gate_is_staging_only(self):
        # Staging placement is accepted; top-level and production are refused.
        # The spike commits no staging var yet, so the fixture inserts it.
        staging_ok = BASE.replace("[env.staging.vars]", '[env.staging.vars]\nTWO_MUSIC = "1"', 1)
        self.assertEqual(self.check_config(staging_ok), [])
        for section in ["vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(f"[{section}]", f'[{section}]\nTWO_MUSIC = "1"', 1)
                errors = self.check_config(config)
                self.assertTrue(any("TWO_MUSIC is staging-only" in e for e in errors), errors)

    def test_music_gate_value_must_be_exactly_one(self):
        for value in ['"0"', '"true"', '""', '"1 "', "1"]:
            with self.subTest(value=value):
                config = BASE.replace("[env.staging.vars]", f"[env.staging.vars]\nTWO_MUSIC = {value}", 1)
                self.assertTrue(any('must be exactly "1"' in e for e in self.check_config(config)))

    def test_deploy_env_marker_is_staging_only_and_exact(self):
        staging = BASE.split("[env.staging.vars]", 1)[1].split("\n[", 1)[0]
        self.assertIn('TWO_DEPLOY_ENV = "staging"', staging)
        self.assertEqual(self.check_config(BASE), [])
        for section in ["vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(f"[{section}]", f'[{section}]\nTWO_DEPLOY_ENV = "staging"', 1)
                errors = self.check_config(config)
                self.assertTrue(any("TWO_DEPLOY_ENV is staging-only" in e for e in errors), errors)
        # Absent is fine (the drill then simply cannot arm); any other
        # spelling is not the fence marker.
        self.assertEqual(self.check_config(BASE.replace('TWO_DEPLOY_ENV = "staging"\n', "")), [])
        for value in ['"production"', '"Staging"', '"staging "', '""', "1"]:
            with self.subTest(value=value):
                config = BASE.replace('TWO_DEPLOY_ENV = "staging"', f"TWO_DEPLOY_ENV = {value}")
                self.assertTrue(any('must be exactly "staging"' in e for e in self.check_config(config)))

    def test_gateway_failure_injection_is_never_a_committed_var(self):
        # Even staging must not commit it: only the generated per-deploy
        # config from a manual drill dispatch may carry the switch.
        for section in ["vars", "env.staging.vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(
                    f"[{section}]", f'[{section}]\nTWO_GATEWAY_READINESS_FAIL_INJECTION = "1"', 1
                )
                errors = self.check_config(config)
                self.assertTrue(
                    any("TWO_GATEWAY_READINESS_FAIL_INJECTION must never be a committed var" in e for e in errors),
                    errors,
                )
        self.assertEqual(self.check_config(BASE), [])

    def test_remediation_fault_switches_are_never_committed_vars(self):
        # Same generated-only rule as the rollback-drill switch: either
        # remediation drill switch committed anywhere fails.
        for key in ["TWO_VOICE_STALL_FAULT_INJECTION", "TWO_GATEWAY_UNPAIRED_FAULT_INJECTION"]:
            for section in ["vars", "env.staging.vars", "env.production.vars"]:
                with self.subTest(key=key, section=section):
                    config = BASE.replace(f"[{section}]", f'[{section}]\n{key} = "1"', 1)
                    errors = self.check_config(config)
                    self.assertTrue(
                        any(f"{key} must never be a committed var" in e for e in errors),
                        errors,
                    )
        self.assertEqual(self.check_config(BASE), [])

    def test_idle_sleep_is_staging_only(self):
        # Staging idle sleep: production keeps the always-on keepalive, so
        # neither switch may appear at the top level (required everywhere)
        # or in the production vars table.
        staging = BASE.split("[env.staging.vars]", 1)[1].split("\n[", 1)[0]
        self.assertIn('KEEPALIVE_ENABLED = "0"', staging)
        self.assertIn('IDLE_SLEEP_AFTER = "15m"', staging)
        self.assertEqual(self.check_config(BASE), [])
        for key, value in [("KEEPALIVE_ENABLED", '"0"'), ("IDLE_SLEEP_AFTER", '"15m"')]:
            for section in ["vars", "env.production.vars"]:
                with self.subTest(key=key, section=section):
                    config = BASE.replace(f"[{section}]", f"[{section}]\n{key} = {value}", 1)
                    errors = self.check_config(config)
                    self.assertTrue(any(f"{key} is staging-only" in e for e in errors), errors)
        # Absent from staging is fine: the keepalive simply stays on (rollback).
        rollback = BASE.replace('KEEPALIVE_ENABLED = "0"\n', "").replace('IDLE_SLEEP_AFTER = "15m"\n', "")
        self.assertEqual(self.check_config(rollback), [])

    def test_idle_sleep_values_are_exact(self):
        for value in ['"1"', '"false"', '"off"', '""', '"0 "', "0", "false"]:
            with self.subTest(value=value):
                config = BASE.replace('KEEPALIVE_ENABLED = "0"', f"KEEPALIVE_ENABLED = {value}")
                self.assertTrue(any('KEEPALIVE_ENABLED must be exactly "0"' in e for e in self.check_config(config)))
        for value in ['"4m"', '"61m"', '"1h"', '"900s"', '"15"', '""', '"015m"', "15"]:
            with self.subTest(value=value):
                config = BASE.replace('IDLE_SLEEP_AFTER = "15m"', f"IDLE_SLEEP_AFTER = {value}")
                self.assertTrue(any("IDLE_SLEEP_AFTER must be" in e for e in self.check_config(config)))
        for value in ['"5m"', '"60m"']:
            with self.subTest(value=value):
                config = BASE.replace('IDLE_SLEEP_AFTER = "15m"', f"IDLE_SLEEP_AFTER = {value}")
                self.assertEqual(self.check_config(config), [])
        # The idle window means nothing while the keepalive runs.
        orphan = BASE.replace('KEEPALIVE_ENABLED = "0"\n', "")
        self.assertTrue(any("IDLE_SLEEP_AFTER needs KEEPALIVE_ENABLED" in e for e in self.check_config(orphan)))

    def test_dashboard_secrets_are_never_toml_vars(self):
        # TOG-22861: the dashboard service-token pair gates human access; a
        # committed value would hand out that access. Both keys must stay
        # Worker secrets, like the metrics scrape token.
        for section in ["vars", "env.staging.vars", "env.production.vars"]:
            for key in ["OPS_DASHBOARD_CLIENT_ID", "OPS_DASHBOARD_CLIENT_SECRET"]:
                with self.subTest(section=section, key=key):
                    config = BASE.replace(f"[{section}]", f'[{section}]\n{key} = "synthetic-secret"', 1)
                    errors = self.check_config(config)
                    self.assertTrue(any("must be an optional Worker secret" in e for e in errors), errors)
                    self.assertFalse(any("synthetic-secret" in e for e in errors))
        self.assertEqual(self.check_config(BASE), [])

    def test_ops_config_writes_is_staging_only(self):
        enabled = BASE.replace(
            "[env.staging.vars]", '[env.staging.vars]\nOPS_CONFIG_WRITES_ENABLED = "1"', 1
        )
        self.assertEqual(self.check_config(enabled), [])
        for section in ["vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(
                    f"[{section}]", f'[{section}]\nOPS_CONFIG_WRITES_ENABLED = "1"', 1
                )
                errors = self.check_config(config)
                self.assertTrue(
                    any("OPS_CONFIG_WRITES_ENABLED is staging-only" in e for e in errors), errors
                )
        # Absent from staging is also fine: config writes stay dark.
        self.assertEqual(self.check_config(BASE), [])

    def test_ops_config_writes_value_must_be_exactly_one(self):
        enabled = BASE.replace(
            "[env.staging.vars]", '[env.staging.vars]\nOPS_CONFIG_WRITES_ENABLED = "1"', 1
        )
        for value in ['"0"', '"true"', '""', '"1 "', "1"]:
            with self.subTest(value=value):
                config = enabled.replace(
                    'OPS_CONFIG_WRITES_ENABLED = "1"', f"OPS_CONFIG_WRITES_ENABLED = {value}"
                )
                self.assertTrue(any('must be exactly "1"' in e for e in self.check_config(config)))

    def test_ops_killswitch_is_staging_only(self):
        enabled = BASE.replace(
            "[env.staging.vars]", '[env.staging.vars]\nOPS_KILLSWITCH_ENABLED = "1"', 1
        )
        self.assertEqual(self.check_config(enabled), [])
        for section in ["vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(
                    f"[{section}]", f'[{section}]\nOPS_KILLSWITCH_ENABLED = "1"', 1
                )
                errors = self.check_config(config)
                self.assertTrue(
                    any("OPS_KILLSWITCH_ENABLED is staging-only" in e for e in errors), errors
                )
        # Absent from staging is also fine: the kill-switch stays dark.
        self.assertEqual(self.check_config(BASE), [])

    def test_ops_killswitch_value_must_be_exactly_one(self):
        enabled = BASE.replace(
            "[env.staging.vars]", '[env.staging.vars]\nOPS_KILLSWITCH_ENABLED = "1"', 1
        )
        for value in ['"0"', '"true"', '""', '"1 "', "1"]:
            with self.subTest(value=value):
                config = enabled.replace(
                    'OPS_KILLSWITCH_ENABLED = "1"', f"OPS_KILLSWITCH_ENABLED = {value}"
                )
                self.assertTrue(any('must be exactly "1"' in e for e in self.check_config(config)))

    def test_ops_control_token_is_never_a_toml_var(self):
        for section in ["vars", "env.staging.vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(
                    f"[{section}]", f'[{section}]\nOPS_CONTROL_TOKEN = "synthetic-secret"', 1
                )
                errors = self.check_config(config)
                self.assertTrue(
                    any("OPS_CONTROL_TOKEN must be an optional Worker secret" in e for e in errors),
                    errors,
                )
                self.assertFalse(any("synthetic-secret" in e for e in errors))

    def test_mcp_token_is_never_a_toml_var(self):
        # TOG-29201: MCP_TOKEN is an optional Worker secret (docs/mcp.md);
        # a plain var in any section must be refused without echoing the
        # synthetic value, while absence stays valid and introduces no
        # required binding.
        for section in ["vars", "env.staging.vars", "env.production.vars"]:
            with self.subTest(section=section):
                config = BASE.replace(
                    f"[{section}]", f'[{section}]\nMCP_TOKEN = "synthetic-secret"', 1
                )
                errors = self.check_config(config)
                self.assertTrue(
                    any("MCP_TOKEN must be an optional Worker secret" in e for e in errors),
                    errors,
                )
                self.assertFalse(any("synthetic-secret" in e for e in errors))
        extra = BASE + '\n[env.preview]\nworkers_dev = false\n[env.preview.vars]\nTWO_GUILD_NAME = "Preview"\nMCP_TOKEN = "synthetic-secret"\n'
        errors = self.check_config(extra)
        self.assertTrue(
            any(
                "[env.preview.vars]: MCP_TOKEN must be an optional Worker secret" in e
                for e in errors
            ),
            errors,
        )
        self.assertFalse(any("synthetic-secret" in e for e in errors))
        self.assertEqual(self.check_config(BASE), [])

    def test_receiver_config_is_never_a_toml_var(self):
        for section in ["vars", "env.staging.vars", "env.production.vars"]:
            for key in ["TWO_INTERNAL_KEYS", "TWO_INTERNAL_ACTIONS", "TWO_INTERNAL_BIND", "TWO_INTERNAL_CONTAINER"]:
                with self.subTest(section=section, key=key):
                    config = BASE.replace(f"[{section}]", f'[{section}]\n{key} = "synthetic-secret"', 1)
                    errors = self.check_config(config)
                    self.assertTrue(any(f"{key} is an Operator-set Worker secret" in e for e in errors), errors)
                    self.assertFalse(any("synthetic-secret" in e for e in errors))

    def test_eventsub_operator_switch_is_never_a_toml_var(self):
        # TOG-23364: TWO_EVENTSUB=1 is the Operator's last switch for the
        # public Twitch ingress; a committed var would turn the route on from
        # a toml change alone, without the Operator step. TOG-23387: the
        # private receiver enable, bind and signing secret plus the Twitch
        # client pair are Operator-set Worker secrets for the same reason.
        for section in ["vars", "env.staging.vars", "env.production.vars"]:
            for key in [
                "TWO_EVENTSUB",
                "TWO_EVENTSUB_BIND",
                "TWO_TWITCH_WEBHOOK_SECRET",
                "TWO_TWITCH_CLIENT_ID",
                "TWO_TWITCH_CLIENT_SECRET",
            ]:
                with self.subTest(section=section, key=key):
                    config = BASE.replace(f"[{section}]", f'[{section}]\n{key} = "synthetic-secret"', 1)
                    errors = self.check_config(config)
                    self.assertTrue(any(f"{key} is an Operator-set Worker secret" in e for e in errors), errors)
                    self.assertFalse(any("synthetic-secret" in e for e in errors))
        self.assertEqual(self.check_config(BASE), [])


def drop_env(content, env):
    """Remove every [env.<env>.*] section header and its body lines."""
    out = []
    skip = False
    for line in content.splitlines(keepends=True):
        stripped = line.lstrip()
        if stripped.startswith("["):
            skip = stripped == f"[env.{env}]\n" or stripped.startswith(
                (f"[env.{env}.", f"[[env.{env}.")
            )
            if skip:
                continue
        if skip:
            continue
        out.append(line)
    return "".join(out)


def top_level_prefix(content):
    """Lines before the first [env.*] section (top-level config only)."""
    out = []
    for line in content.splitlines(keepends=True):
        if line.lstrip().startswith(("[env.", "[[env.")):
            break
        out.append(line)
    return "".join(out)


class RequiredEnvironmentTests(unittest.TestCase):
    def check_config(self, content):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "wrangler.toml"
            path.write_text(content)
            return checker.check(path)

    def run_cli(self, path):
        return subprocess.run(
            [sys.executable, str(Path(__file__).with_name("check-env-bindings.py")), str(path)],
            capture_output=True,
            text=True,
        )

    def test_missing_staging_is_rejected(self):
        errors = self.check_config(drop_env(BASE, "staging"))
        required = [e for e in errors if "missing required environment(s)" in e]
        self.assertEqual(len(required), 1)
        self.assertIn("['staging']", required[0])

    def test_missing_production_is_rejected(self):
        errors = self.check_config(drop_env(BASE, "production"))
        self.assertTrue(any("missing required environment(s)" in e for e in errors), errors)
        required = [e for e in errors if "missing required environment(s)" in e]
        self.assertEqual(len(required), 1)
        self.assertIn("['production']", required[0])

    def test_missing_both_envs_is_rejected(self):
        errors = self.check_config(drop_env(drop_env(BASE, "staging"), "production"))
        required = [e for e in errors if "missing required environment(s)" in e]
        self.assertEqual(len(required), 1)
        self.assertIn("staging", required[0])
        self.assertIn("production", required[0])

    def test_additional_env_is_still_checked(self):
        config = BASE + '\n[env.preview.vars]\nTWO_GUILD_NAME = "Preview"\n'
        errors = self.check_config(config)
        self.assertTrue(any("[env.preview]" in e for e in errors), errors)
        self.assertFalse(any("missing required environment(s)" in e for e in errors), errors)

    def test_malformed_toml_is_controlled(self):
        marker = "SYNTHETIC_MALFORMED_MARKER_XYZ"
        errors = self.check_config(f"NOT VALID TOML [[[\n{marker} = \"unclosed\n")
        self.assertEqual(len(errors), 1)
        self.assertIn("invalid Wrangler config", errors[0])
        self.assertNotIn(marker, errors[0])

    def test_malformed_toml_cli_has_no_traceback(self):
        marker = "SYNTHETIC_MALFORMED_MARKER_XYZ"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "wrangler.toml"
            path.write_text(f"NOT VALID TOML [[[\n{marker} = \"unclosed\n")
            proc = self.run_cli(path)
        self.assertNotEqual(proc.returncode, 0)
        combined = proc.stdout + proc.stderr
        self.assertNotIn("Traceback", combined)
        self.assertNotIn(marker, combined)

    def test_missing_file_is_controlled(self):
        with tempfile.TemporaryDirectory() as directory:
            missing = Path(directory) / "does-not-exist.toml"
            errors = checker.check(missing)
            proc = self.run_cli(missing)
        self.assertEqual(len(errors), 1)
        self.assertIn("unreadable Wrangler config", errors[0])
        self.assertNotEqual(proc.returncode, 0)
        self.assertNotIn("Traceback", proc.stdout + proc.stderr)

    def test_duplicate_table_never_echoes_names(self):
        # tomllib quotes the offending table name ("Cannot declare ..."),
        # so this pins that the diagnostic discards the parser error.
        marker = "SYNTHETIC_DUP_MARKER_XYZ"
        errors = self.check_config(f"[{marker}]\n[{marker}]\n")
        self.assertEqual(errors, [checker.INVALID_TOML_ERROR])

    def test_non_table_env_is_controlled(self):
        config = 'env = "not-a-table"\n' + top_level_prefix(BASE)
        errors = self.check_config(config)
        self.assertEqual(errors, [checker.INVALID_TOML_ERROR])

    def test_wrong_shape_tables_are_controlled(self):
        config = top_level_prefix(BASE) + "[env]\nstaging = 1\nproduction = 2\n"
        errors = self.check_config(config)
        self.assertEqual(errors, [checker.INVALID_TOML_ERROR])


class WorkersDevBreakGlassTests(unittest.TestCase):
    """TOG-22861: staging + production are the workers.dev break-glass
    origins until a custom domain exists; every other route (including bare
    deploys) must not serve the public suffix."""

    def check_config(self, content):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "wrangler.toml"
            path.write_text(content)
            return checker.check(path)

    def test_base_declares_staging_and_production_break_glass(self):
        self.assertEqual(self.check_config(BASE), [])
        self.assertIn("workers_dev = true", BASE.split("[env.staging]", 1)[1].split("[env.staging.", 1)[0])
        self.assertIn("workers_dev = true", BASE.split("[env.production]", 1)[1].split("[env.production.", 1)[0])

    def test_top_level_workers_dev_must_be_explicitly_false(self):
        for variant in ['workers_dev = true', '']:
            with self.subTest(variant=variant or "absent"):
                config = BASE.replace("workers_dev = false\n", f"{variant}\n" if variant else "")
                errors = self.check_config(config)
                self.assertTrue(any("top-level workers_dev must be explicitly false" in e for e in errors), errors)

    def test_staging_workers_dev_must_be_explicitly_true(self):
        for variant in ['workers_dev = false', '']:
            with self.subTest(variant=variant or "absent"):
                staging = BASE.split("[env.staging]\n", 1)
                config = staging[0] + "[env.staging]\n" + staging[1].replace("workers_dev = true\n", f"{variant}\n" if variant else "", 1)
                errors = self.check_config(config)
                self.assertTrue(any("[env.staging]: workers_dev must be explicitly true" in e for e in errors), errors)

    def test_production_workers_dev_must_be_explicitly_true(self):
        for variant in ['workers_dev = false', '']:
            with self.subTest(variant=variant or "absent"):
                production = BASE.split("[env.production]\n", 1)
                config = production[0] + "[env.production]\n" + production[1].replace("workers_dev = true\n", f"{variant}\n" if variant else "", 1)
                errors = self.check_config(config)
                self.assertTrue(any("[env.production]: workers_dev must be explicitly true" in e for e in errors), errors)

    def test_additional_env_must_declare_workers_dev_false(self):
        config = BASE + '\n[env.preview.vars]\nTWO_GUILD_NAME = "Preview"\n'
        errors = self.check_config(config)
        self.assertTrue(any("[env.preview]: workers_dev must be explicitly false" in e for e in errors), errors)
        config = BASE + '\n[env.preview]\nworkers_dev = false\n[env.preview.vars]\nTWO_GUILD_NAME = "Preview"\n'
        errors = self.check_config(config)
        self.assertFalse(any("workers_dev" in e for e in errors), errors)


if __name__ == "__main__":
    unittest.main()
