import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location(
    "codeowners", Path(__file__).with_name("verify-codeowners.py")
)
codeowners = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(codeowners)


class CodeownersTests(unittest.TestCase):
    def test_root_owner_and_teams(self):
        self.assertEqual(codeowners.verify("# Advisory\n* @owner @org/security\n/docs/ docs@example.com # docs\n"), [])

    def test_missing_catchall(self):
        self.assertTrue(codeowners.verify("/src/ @owner\n"))
        self.assertTrue(codeowners.verify("# empty\n"))

    def test_invalid_owner(self):
        self.assertTrue(codeowners.verify("* owner\n"))
        self.assertTrue(codeowners.verify("* @\n"))

    def test_later_rule_cannot_erase_coverage(self):
        self.assertTrue(codeowners.verify("* @owner\n.github/workflows/\n"))

    def test_unsupported_patterns(self):
        for pattern in ["!private/", "[ab].rs", r"\#file"]:
            self.assertTrue(codeowners.verify(f"* @owner\n{pattern} @owner\n"))

    def test_specific_overrides_with_valid_owner(self):
        self.assertEqual(codeowners.verify("* @owner\n/crates/** @org/rust-dev\n"), [])


if __name__ == "__main__":
    unittest.main()
