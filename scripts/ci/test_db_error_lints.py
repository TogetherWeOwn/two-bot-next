"""Never log raw database errors (TOG-19028).

`docs/logging.md` forbids logging raw database errors: a `sqlx::Error`
Display/Debug chain can carry connection URLs and statement detail, and
several store wrappers embed one (`feed store database error: {0}`).
`crates/bot/src/db_log.rs` classifies every in-scope failure to a fixed
token; this lint rejects new `error = %err`-style sites in the four
converted files so the next failure log reaches for a helper instead of
formatting the error.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]

# Every file converted by TOG-19028. `main.rs` keeps zero `error = %` sites
# (config/gate failures log under `reason`); the other three classify every
# database failure through `db_log` and every Discord failure through
# `DiscordError::cause`.
SCOPED_FILES = [
    "crates/bot/src/command_runtime.rs",
    "crates/bot/src/schedule_runtime.rs",
    "crates/bot/src/scheduled_jobs.rs",
    "crates/bot/src/main.rs",
]

# `error = %err`, `error=%error`, `error = ?err` — any raw error value in the
# structured `error` field.
RAW_ERROR_FIELD = re.compile(r"\berror\s*=\s*[?%]")


class DbErrorLintTests(unittest.TestCase):
    def test_no_raw_error_field_in_scoped_files(self):
        for rel in SCOPED_FILES:
            with self.subTest(file=rel):
                source = (ROOT / rel).read_text()
                hits = RAW_ERROR_FIELD.findall(source)
                self.assertEqual(
                    hits,
                    [],
                    f"{rel} logs a raw error value; classify it with "
                    "crates/bot/src/db_log.rs and log the token under `reason`",
                )

    def test_db_log_helpers_exist(self):
        source = (ROOT / "crates/bot/src/db_log.rs").read_text()
        for helper in [
            "pub(crate) fn db_class",
            "pub(crate) fn feed_store_class",
            "pub(crate) fn quota_write_class",
            "pub(crate) fn leveling_class",
            "pub(crate) fn scheduled_store_class",
            "pub(crate) fn channel_response_class",
        ]:
            with self.subTest(helper=helper):
                self.assertIn(helper, source)

    def test_scoped_files_use_the_classifiers(self):
        expectations = {
            "crates/bot/src/command_runtime.rs": [
                "db_class",
                "feed_store_class",
                "leveling_class",
                "channel_response_class",
            ],
            "crates/bot/src/schedule_runtime.rs": ["db_class", "quota_write_class"],
            "crates/bot/src/scheduled_jobs.rs": ["db_class", "scheduled_store_class"],
        }
        for rel, helpers in expectations.items():
            source = (ROOT / rel).read_text()
            for helper in helpers:
                with self.subTest(file=rel, helper=helper):
                    self.assertIn(
                        helper,
                        source,
                        f"{rel} stopped using {helper}; new failure logs must "
                        "classify through crates/bot/src/db_log.rs",
                    )


if __name__ == "__main__":
    unittest.main()
