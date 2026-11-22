import io
import json
import unittest
from collections import Counter

import check_sqlx_pool as gate


def diagnostic(path, line, *, message="raw SQLx", code=gate.LINT, text="", expansion=None):
    return {
        "reason": "compiler-message",
        "message": {
            "code": {"code": code},
            "message": message,
            "rendered": f"{path}:{line} {message}\n",
            "spans": [{
                "file_name": path, "line_start": line, "line_end": line,
                "is_primary": True, "text": [{"text": text}], "expansion": expansion,
            }],
        },
    }


class CheckSqlxPoolTests(unittest.TestCase):
    def check(self, added=(), diagnostics=(), success=True):
        owner = diagnostic(gate.OWNER, 24, message=gate.LAZY_METHOD, text=".connect_lazy_with(options))")
        records = [
            owner, diagnostic(gate.FIXTURE, 8), *diagnostics,
            {"reason": "build-finished", "success": success},
        ]
        output = io.StringIO()
        result = gate.check(map(json.dumps, records), set(added), Counter({8: 1}), output)
        return result, output.getvalue()

    def test_added_raw_constructor_fails_even_within_owner_module(self):
        added = {(gate.OWNER, 30)}
        result, output = self.check(added, [diagnostic(gate.OWNER, 30, message=gate.LAZY_METHOD)])
        self.assertTrue(result)
        self.assertIn(f"{gate.OWNER}:30", output)

    def test_grandfathered_call_passes_and_unrelated_diagnostic_is_visible(self):
        path = "crates/store/tests/legacy.rs"
        result, output = self.check(
            {(path, 10), (gate.OWNER, 24)},
            [diagnostic(path, 7), diagnostic(path, 10, message="unrelated error", code="E0308")],
        )
        self.assertFalse(result)
        self.assertNotIn(f"{path}:7", output)
        self.assertIn(f"{path}:10 unrelated error", output)

    def test_new_macro_invocation_is_checked_at_the_call_site(self):
        path = "crates/store/src/new.rs"
        expansion = {"span": {"file_name": path, "line_start": 9, "line_end": 9}}
        result, _ = self.check({(path, 9)}, [diagnostic("crates/store/src/macros.rs", 12, expansion=expansion)])
        self.assertTrue(result)

    def test_blind_gate_fails_if_compiler_does_not_report_expected_fixture(self):
        with self.assertRaisesRegex(ValueError, "fixture expected"):
            self.check(diagnostics=[diagnostic(gate.FIXTURE, 15)])
        with self.assertRaisesRegex(ValueError, "successful build"):
            self.check(success=False)
