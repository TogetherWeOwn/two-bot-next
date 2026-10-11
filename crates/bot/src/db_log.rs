//! Bounded database-error classification for structured logs.
//!
//! `docs/logging.md` forbids logging raw database errors: a `sqlx::Error`
//! Display/Debug chain can carry connection URLs and statement detail, and
//! several store wrappers embed one (`feed store database error: {0}`).
//! Every `tracing` call in the command surface therefore logs a fixed token,
//! never the error value:
//!
//! - [`db_class`] classifies a bare `sqlx::Error`;
//! - [`feed_store_class`], [`quota_write_class`] and [`leveling_class`]
//!   classify the store wrappers that embed one;
//! - [`channel_response_class`] classifies the channel responder's fixed
//!   three-variant error;
//! - Discord failures use the existing fixed vocabulary
//!   (`DiscordError::cause`); config/gate failures already render as fixed
//!   strings and are logged under `reason`, never `error`.
//!
//! `scripts/ci/test_db_error_lints.py` rejects new raw-error-field log
//! sites in the four in-scope files, so add new failure logs with one of
//! these helpers instead of formatting the error.

use sqlx::error::ErrorKind;
use two_bot_core::{
    automation_quota::QuotaWriteError, feeds_store::FeedStoreError,
    leveling_store::LevelingStoreError,
};
use two_bot_discord::{leveling_runtime::LevelingRuntimeError, ChannelResponseError};

/// Fixed token naming a database failure without its internals.
///
/// Constraint violations keep their kind; pool/connection/row states keep
/// theirs; everything else (protocol, decode, configuration, transaction
/// misuse) collapses to `query`. SQLSTATE codes, messages and severities
/// are never surfaced: rerun the failing operation path for detail.
#[must_use]
pub(crate) fn db_class(error: &sqlx::Error) -> &'static str {
    match error {
        sqlx::Error::Database(db) => match db.kind() {
            ErrorKind::UniqueViolation => "unique_violation",
            ErrorKind::ForeignKeyViolation => "foreign_key_violation",
            ErrorKind::NotNullViolation => "not_null_violation",
            ErrorKind::CheckViolation => "check_violation",
            ErrorKind::ExclusionViolation => "exclusion_violation",
            // `ErrorKind::Other` and any future kind: never the code/message.
            _ => "database",
        },
        sqlx::Error::PoolTimedOut => "pool_timeout",
        sqlx::Error::PoolClosed => "pool_closed",
        sqlx::Error::WorkerCrashed => "worker_crashed",
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) => "connection",
        sqlx::Error::RowNotFound => "row_not_found",
        _ => "query",
    }
}

/// [`FeedStoreError`] without its SQLx detail. Capacity refusals already
/// reply with the quota text and log nothing; the token stays for
/// exhaustiveness.
#[must_use]
pub(crate) fn feed_store_class(error: &FeedStoreError) -> &'static str {
    match error {
        FeedStoreError::Db(inner) => db_class(inner),
        FeedStoreError::Domain(_) => "domain",
        FeedStoreError::EmptyClaimToken => "empty_claim_token",
        FeedStoreError::Capacity(_) => "capacity",
    }
}

/// [`QuotaWriteError`] without its SQLx detail. Same posture as
/// [`feed_store_class`]: capacity is a reply, not a log line.
#[must_use]
pub(crate) fn quota_write_class(error: &QuotaWriteError) -> &'static str {
    match error {
        QuotaWriteError::Capacity(_) => "capacity",
        QuotaWriteError::Database(inner) => db_class(inner),
    }
}

/// [`LevelingRuntimeError`] without its SQLx or Discord detail. Database
/// halves classify through [`db_class`]; Discord halves reuse the fixed
/// `cause` vocabulary; malformed input is one token.
#[must_use]
pub(crate) fn leveling_class(error: &LevelingRuntimeError) -> &'static str {
    match error {
        LevelingRuntimeError::Store(LevelingStoreError::Db(inner)) => db_class(inner),
        LevelingRuntimeError::Store(LevelingStoreError::InvalidTimestamp(_)) => "invalid_timestamp",
        LevelingRuntimeError::Store(LevelingStoreError::InvalidRewards(_)) => "invalid_rewards",
        LevelingRuntimeError::Discord(discord) => discord.cause(),
        LevelingRuntimeError::InvalidInteraction => "invalid_interaction",
    }
}

/// [`ChannelResponseError`] as a fixed token. All three variants render as
/// authored constants with no payload, but the token keeps the log surface
/// stable if a payload is ever added.
#[must_use]
pub(crate) fn channel_response_class(error: &ChannelResponseError) -> &'static str {
    match error {
        ChannelResponseError::Defer => "defer",
        ChannelResponseError::Persistence => "persistence",
        ChannelResponseError::Edit => "edit",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::error::DatabaseError;
    use std::borrow::Cow;
    use std::fmt;

    /// Stand-in database error whose Display carries a fake secret: proves
    /// the classifier never surfaces it.
    #[derive(Debug)]
    struct FakeDbError {
        code: &'static str,
    }

    impl fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "fake database error with postgres://secret@localhost/db")
        }
    }

    impl std::error::Error for FakeDbError {}

    impl DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            "fake"
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.code))
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> ErrorKind {
            match self.code {
                "23505" => ErrorKind::UniqueViolation,
                "23503" => ErrorKind::ForeignKeyViolation,
                "23502" => ErrorKind::NotNullViolation,
                "23514" => ErrorKind::CheckViolation,
                "23P01" => ErrorKind::ExclusionViolation,
                _ => ErrorKind::Other,
            }
        }
    }

    fn database(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDbError { code }))
    }

    #[test]
    fn constraint_kinds_keep_their_token() {
        for (code, token) in [
            ("23505", "unique_violation"),
            ("23503", "foreign_key_violation"),
            ("23502", "not_null_violation"),
            ("23514", "check_violation"),
            ("23P01", "exclusion_violation"),
        ] {
            assert_eq!(db_class(&database(code)), token, "SQLSTATE {code}");
        }
    }

    #[test]
    fn unmapped_database_errors_collapse_to_database() {
        assert_eq!(db_class(&database("XX000")), "database");
    }

    #[test]
    fn pool_and_connection_states_keep_their_token() {
        assert_eq!(db_class(&sqlx::Error::PoolTimedOut), "pool_timeout");
        assert_eq!(db_class(&sqlx::Error::PoolClosed), "pool_closed");
        assert_eq!(db_class(&sqlx::Error::WorkerCrashed), "worker_crashed");
        assert_eq!(
            db_class(&sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "fake"
            ))),
            "connection"
        );
        assert_eq!(
            db_class(&sqlx::Error::Tls("fake TLS failure".into())),
            "connection"
        );
        assert_eq!(db_class(&sqlx::Error::RowNotFound), "row_not_found");
    }

    #[test]
    fn programming_errors_collapse_to_query() {
        assert_eq!(db_class(&sqlx::Error::Protocol("fake".to_owned())), "query");
        assert_eq!(
            db_class(&sqlx::Error::Configuration("fake".into())),
            "query"
        );
        assert_eq!(
            db_class(&sqlx::Error::ColumnNotFound("fake".to_owned())),
            "query"
        );
        assert_eq!(db_class(&sqlx::Error::BeginFailed), "query");
    }

    #[test]
    fn classification_never_surfaces_error_text() {
        let class = db_class(&database("23505"));
        assert_eq!(class, "unique_violation");
        assert!(!class.contains("secret"), "classifier leaked error text");
    }

    #[test]
    fn feed_wrapper_classifies_without_sqlx_detail() {
        assert_eq!(
            feed_store_class(&FeedStoreError::Db(sqlx::Error::PoolTimedOut)),
            "pool_timeout"
        );
        assert_eq!(
            feed_store_class(&FeedStoreError::Domain(
                two_bot_core::feeds::FeedError::InvalidKind
            )),
            "domain"
        );
        assert_eq!(
            feed_store_class(&FeedStoreError::EmptyClaimToken),
            "empty_claim_token"
        );
        assert_eq!(
            feed_store_class(&FeedStoreError::Capacity(
                two_bot_core::automation_quota::AutomationQuota::Feeds
            )),
            "capacity"
        );
    }

    #[test]
    fn quota_wrapper_classifies_without_sqlx_detail() {
        assert_eq!(
            quota_write_class(&QuotaWriteError::Database(sqlx::Error::RowNotFound)),
            "row_not_found"
        );
        assert_eq!(
            quota_write_class(&QuotaWriteError::Capacity(
                two_bot_core::automation_quota::AutomationQuota::Schedules
            )),
            "capacity"
        );
    }

    #[test]
    fn leveling_wrapper_classifies_without_store_or_discord_detail() {
        assert_eq!(
            leveling_class(&LevelingRuntimeError::Store(
                LevelingStoreError::InvalidTimestamp("not-a-timestamp".to_owned())
            )),
            "invalid_timestamp"
        );
        assert_eq!(
            leveling_class(&LevelingRuntimeError::Store(LevelingStoreError::Db(
                sqlx::Error::PoolClosed
            ))),
            "pool_closed"
        );
        assert_eq!(
            leveling_class(&LevelingRuntimeError::Discord(
                two_bot_discord::executor::DiscordError::Timeout
            )),
            "timeout"
        );
        assert_eq!(
            leveling_class(&LevelingRuntimeError::InvalidInteraction),
            "invalid_interaction"
        );
    }

    #[test]
    fn channel_response_maps_each_variant_to_a_fixed_token() {
        assert_eq!(
            channel_response_class(&two_bot_discord::ChannelResponseError::Defer),
            "defer"
        );
        assert_eq!(
            channel_response_class(&two_bot_discord::ChannelResponseError::Persistence),
            "persistence"
        );
        assert_eq!(
            channel_response_class(&two_bot_discord::ChannelResponseError::Edit),
            "edit"
        );
    }
}
