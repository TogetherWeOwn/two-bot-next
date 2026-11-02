"""Never log raw database errors.

`docs/logging.md` forbids logging raw database errors: a `sqlx::Error`
Display/Debug chain can carry connection URLs and statement detail, and
several store wrappers embed one (`feed store database error: {0}`).
`crates/bot/src/db_log.rs` classifies every in-scope failure to a fixed
token; this lint rejects new `error = %err`-style sites (and the `%error`
shorthand) in the five converted files so the next failure log reaches for
a helper instead of formatting the error.

It is a name-based tripwire, not a proof: it matches the field and variable
names a failure log reaches for, not every way to render an error value.
`reason` is reserved for helper tokens and authored, secret-free text.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]

# Every file converted to the classifiers. `main.rs` keeps zero `error = %` sites
# (config/gate failures log under `reason`); the other three classify every
# database failure through `db_log` and every Discord failure through
# `DiscordError::cause`.
SCOPED_FILES = [
    "crates/bot/src/command_runtime.rs",
    "crates/bot/src/schedule_runtime.rs",
    "crates/bot/src/scheduled_jobs.rs",
    "crates/bot/src/main.rs",
    "crates/bot/src/moderation_recent_command.rs",
]

# `error = %err`, `error=%error`, `error = ?err` — any raw error value in the
# structured `error` field.
RAW_ERROR_FIELD = re.compile(r"\berror\s*=\s*[?%]")
# `warn!(..., %error, ...)` / `?err` / `%e` — the shorthand that records the
# value under its own name. `reason = %err` and `?error.class()` stay legal.
RAW_ERROR_SHORTHAND = re.compile(r"[(,]\s*[%?](?:e|err|error)\s*[,)]")


class DbErrorLintTests(unittest.TestCase):
    def test_patterns_catch_raw_sites_and_spare_classified_ones(self):
        raw = [
            'warn!(error = %err, "x");',
            'warn!(error=?error, "x");',
            'warn!(id = 1, %error, "x");',
            'warn!(?err, "x");',
            'warn!(%e, "x");',
        ]
        classified = [
            'warn!(reason = db_class(&err), "x");',
            'warn!(reason = err.cause(), "x");',
            'warn!(reason = %err, "config invalid");',
            'warn!(error_class = ?error.class(), "x");',
        ]
        for line in raw:
            with self.subTest(raw=line):
                self.assertTrue(
                    RAW_ERROR_FIELD.search(line) or RAW_ERROR_SHORTHAND.search(line)
                )
        for line in classified:
            with self.subTest(classified=line):
                self.assertIsNone(RAW_ERROR_FIELD.search(line))
                self.assertIsNone(RAW_ERROR_SHORTHAND.search(line))

    def test_no_raw_error_field_in_scoped_files(self):
        for rel in SCOPED_FILES:
            with self.subTest(file=rel):
                source = (ROOT / rel).read_text()
                hits = RAW_ERROR_FIELD.findall(source) + RAW_ERROR_SHORTHAND.findall(
                    source
                )
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
            "pub(crate) fn channel_response_class",
            "pub(crate) fn discord_class",
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
                "discord_class",
            ],
            "crates/bot/src/schedule_runtime.rs": [
                "db_class",
                "quota_write_class",
                "discord_class",
            ],
            "crates/bot/src/scheduled_jobs.rs": ["db_class"],
            "crates/bot/src/moderation_recent_command.rs": [
                "db_class",
                "discord_class",
            ],
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
