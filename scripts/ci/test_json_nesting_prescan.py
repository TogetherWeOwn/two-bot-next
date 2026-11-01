"""Regressions for the lexical JSON nesting pre-scan copied into each parser.

The scan must stay linear on hostile strings (an unterminated string used
to rescan to the end from every later quote), and bytes must be scanned in
the same encoding json.loads parses, so UTF-16/32 input cannot slip a deep
document past the bound.
"""

import importlib.util
import sys
import time
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent.parent

# Helpers that take a str and a limit and return True when it is too deep.
DEPTH_EXCEEDS = ["production_deploy.py", "soak_evidence.py",
                 "release-sbom-target.py", "vulnerability-preflight.py"]
# Helpers that wrap json.loads and raise RecursionError past MAX_JSON_DEPTH.
LOADS_BOUNDED = ["staging_rollout.py", "synthetic_alert_history.py",
                 "staging_automation_read_smoke.py", "staging_guild_e2e_prep.py",
                 "rollback_readiness_probe.py", "slow_burn_tracker.py",
                 "staging_voice_smoke.py", "ci/schema_gate_common.py"]

HOSTILE = ['"' + '\\"' * 40_000,
           '"' + '\\"' * 40_000 + '\\',
           '"a' * 40_000]
# The linear scan takes milliseconds; the old quadratic one took ~10 s here
# and close to an hour at the 1 MiB input caps.
SECONDS = 2


def load(relative):
    path = SCRIPTS / relative
    name = "prescan_" + relative.replace("/", "_").replace("-", "_")[:-3]
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.path.insert(0, str(path.parent))
    try:
        spec.loader.exec_module(module)
    finally:
        sys.path.remove(str(path.parent))
    return module


class DepthExceedsTests(unittest.TestCase):
    def test_hostile_strings_scan_in_linear_time(self):
        for relative in DEPTH_EXCEEDS:
            module = load(relative)
            for text in HOSTILE:
                with self.subTest(relative, size=len(text)):
                    started = time.monotonic()
                    self.assertFalse(module._json_depth_exceeds(text, 100))
                    self.assertLess(time.monotonic() - started, SECONDS)

    def test_brackets_inside_strings_do_not_count(self):
        for relative in DEPTH_EXCEEDS:
            module = load(relative)
            with self.subTest(relative):
                self.assertFalse(module._json_depth_exceeds('["[[[\\"[[", "]"]', 2))
                self.assertTrue(module._json_depth_exceeds("[[[1]]]", 2))


class LoadsBoundedTests(unittest.TestCase):
    def test_hostile_strings_scan_in_linear_time(self):
        for relative in LOADS_BOUNDED:
            module = load(relative)
            for text in HOSTILE:
                with self.subTest(relative, size=len(text)):
                    started = time.monotonic()
                    with self.assertRaises(ValueError):
                        module._loads_bounded(text)
                    self.assertLess(time.monotonic() - started, SECONDS)

    def test_utf16_and_utf32_bytes_are_bounded_too(self):
        levels = 1001
        # Misread as UTF-8, the escaped quote ends the string early, and the
        # quote after "deep" then swallows the brackets up to "z".
        deep = '{"a":"\\"", "deep":' + "[" * levels + "0" + "]" * levels + ', "z": 0}'
        for relative in LOADS_BOUNDED:
            module = load(relative)
            for encoding in ("utf-8", "utf-16-le", "utf-16-be", "utf-32-le", "utf-32-be"):
                with self.subTest(relative, encoding=encoding):
                    with self.assertRaises(RecursionError):
                        module._loads_bounded(deep.encode(encoding))

    def test_bytes_parse_as_json_loads_would(self):
        for relative in LOADS_BOUNDED:
            module = load(relative)
            for encoding in ("utf-8", "utf-8-sig", "utf-16", "utf-32-le"):
                with self.subTest(relative, encoding=encoding):
                    self.assertEqual(module._loads_bounded('{"k": ["]"]}'.encode(encoding)),
                                     {"k": ["]"]})


if __name__ == "__main__":
    unittest.main()
