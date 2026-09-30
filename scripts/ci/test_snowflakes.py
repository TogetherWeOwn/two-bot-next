import importlib.util
import os
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "snowflakes", Path(__file__).with_name("check-src-snowflakes.py")
)
snowflakes = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(snowflakes)


class SnowflakeTests(unittest.TestCase):
    def values(self, source):
        return [value for _, value, _ in snowflakes.occurrences(source)]

    def test_strings_numbers_raw_and_separators(self):
        self.assertEqual(self.values('''
const A: &str = "<@123456789012345678>";
const B: u64 = 123_456_789_012_345_679u64;
const C: &str = r##"https://discord.com/channels/123456789012345680"##;
const D: &[u8] = br#"123456789012345681"#;
'''), [str(n) for n in range(123456789012345678, 123456789012345682)])

    def test_comments_are_not_literals(self):
        self.assertEqual(self.values('''
// 123456789012345678
/* outer /* 123456789012345679 */ 123456789012345680 */
const A: &str = "// 123456789012345681";
'''), ["123456789012345681"])

    def test_digit_lengths(self):
        self.assertEqual(self.values('''
let a = "1234567890123456";
let b = "12345678901234567";
let c = "12345678901234567890";
let d = "123456789012345678901";
let e = 1_420_070_400_000;
'''), ["12345678901234567", "12345678901234567890"])

    def test_only_exact_test_module_is_excluded(self):
        self.assertEqual(self.values('''
#[cfg(test)]
mod tests {
    fn fixture() { let x = r#"} /* 123456789012345678"#; }
    mod nested { const ID: &str = "123456789012345679"; }
}
const AFTER: &str = "123456789012345680";
#[cfg(feature = "test")]
mod production { const ID: &str = "123456789012345681"; }
'''), ["123456789012345680", "123456789012345681"])

    def test_test_attribute_with_other_attribute(self):
        self.assertEqual(self.values('''
#[cfg(test)]
#[allow(dead_code)]
pub mod fixtures { const ID: &str = "123456789012345678"; }
'''), [])

    def test_string_mentioning_test_attribute_does_not_exempt_source(self):
        self.assertEqual(self.values('''
let x = "#[cfg(test)] mod tests {";
const ID: &str = "123456789012345678";
'''), ["123456789012345678"])

    def test_allowance_is_path_line_value_and_occurrence_scoped(self):
        source = 'pub const LIVE_GUILD_ID: &str = "123456789012345678";'
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as temp:
            root = Path(temp)
            path = root / "crates/cutover/src/lib.rs"
            path.parent.mkdir(parents=True)
            path.write_text(source + "\n")
            allowance = [{"path": "crates/cutover/src/lib.rs", "value": "123456789012345678",
                          "source": source, "reason": "live-guild refusal guard"}]
            self.assertEqual(snowflakes.check(root, allowance), [])
            path.write_text(source + "\n" + source + "\n")
            self.assertEqual(len(snowflakes.check(root, allowance)), 1)
            path.write_text(source.replace("LIVE_GUILD_ID", "OTHER_ID"))
            self.assertEqual(len(snowflakes.check(root, allowance)), 2)
            path.write_text("")
            self.assertEqual(len(snowflakes.check(root, allowance)), 1)
            path.write_text(source)
            (path.parent / "other.rs").write_text(source)
            self.assertEqual(len(snowflakes.check(root, allowance)), 1)

    def test_allowance_requires_reason_and_source_path(self):
        for entry in [
            {"reason": ""},
            {"reason": "fixture", "path": "../outside.rs"},
        ]:
            with self.assertRaises(ValueError):
                snowflakes.check(Path("."), [entry])


if __name__ == "__main__":
    unittest.main()
