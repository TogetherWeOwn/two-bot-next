//! Sanitized error-chain contract for backfill + retention, typed for the
//! gateway [`FailureClass`] vocabulary.
//!
//! Every backfill/retention failure event carries `class` + `chain`
//! (sanitized) + `step`. The step vocabularies and the shared redaction live
//! in [`two_bot_core::error_chain`] (one vocabulary for the bot binary, the
//! store error Displays, and the cutover retention paths); this module keeps
//! the `crate::backfill_retention_chains` import path the frozen acceptance
//! suites use and binds the class parameter to [`FailureClass`].
//!
//! The wired call sites (`ops_backfill` snapshot failure, `retention`
//! guild-purge failure + deferral warns, `retention_jobs` database-error
//! mapping) feed the renders classified causes only (see `db_log`,
//! `DiscordError::cause`), never free-form text: display names and message
//! bodies never enter a chain.
//!
//! ```text
//! render_backfill_failure(class, step, chain)
//!     = "backfill_failed class=<class> step=<step> chain=<sanitize_chain(chain)>"
//! render_retention_failure(class, step, chain)
//!     = "retention_failed class=<class> step=<step> chain=<sanitize_chain(chain)>"
//! ```

pub use two_bot_core::error_chain::{sanitize_chain, BackfillStep, RetentionStep};

use crate::gateway_failure::FailureClass;

/// One backfill failure line: the gateway class, the stage step, and the
/// sanitized chain. The tests assert substrings, not this exact layout.
#[must_use]
pub fn render_backfill_failure(class: FailureClass, step: BackfillStep, chain: &str) -> String {
    two_bot_core::error_chain::render_backfill_failure(class.as_str(), step, &sanitize_chain(chain))
}

/// One retention failure line: the gateway class, the stage step, and the
/// sanitized chain. The tests assert substrings, not this exact layout.
#[must_use]
pub fn render_retention_failure(class: FailureClass, step: RetentionStep, chain: &str) -> String {
    two_bot_core::error_chain::render_retention_failure(
        class.as_str(),
        step,
        &sanitize_chain(chain),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use two_bot_core::error_chain::STORE_UNAVAILABLE_CLASS;

    #[test]
    fn store_unavailable_token_matches_core_shared_const() {
        // The cutover retention paths render with the shared core const; this
        // fails the moment the bot gateway token drifts from it.
        assert_eq!(
            FailureClass::StoreUnavailable.as_str(),
            STORE_UNAVAILABLE_CLASS,
            "bot gateway token drifted from the shared core const cutover uses",
        );
    }
}
