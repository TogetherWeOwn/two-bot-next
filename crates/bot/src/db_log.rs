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
//! - [`discord_class`] classifies a Discord executor failure (the fixed
//!   `DiscordError::cause` vocabulary plus send-admission refusals);
//! - config/gate failures render as authored strings with no secret (an
//!   invalid `TWO_FEED_POLL_SECONDS` value is echoed) and are logged under
//!   `reason`, never `error`.
//!
//! `scripts/ci/test_db_error_lints.py` rejects new raw-error-field log
//! sites in the four in-scope files, so add new failure logs with one of
//! these helpers instead of formatting the error.

use two_bot_core::{
    automation_quota::QuotaWriteError, feeds_store::FeedStoreError,
    leveling_store::LevelingStoreError, send_admission::AdmissionError,
};
use two_bot_discord::{
    executor::DiscordError,
    leveling_runtime::{LevelingRuntimeError, RoleApplicationError},
    ChannelResponseError,
};

/// Fixed token naming a database failure without its internals.
///
/// Constraint violations keep their kind; pool/connection/row states keep
/// theirs; everything else (protocol, decode, configuration, transaction
/// misuse) collapses to `query`. SQLSTATE codes, messages and severities
/// are never surfaced: rerun the failing operation path for detail.
#[must_use]
pub(crate) fn db_class(error: &sqlx::Error) -> &'static str {
    two_bot_core::error_chain::db_cause(error)
}

/// Reason text for the gateway failure log line (TOG-25067): the
/// `InvalidArgument` message verbatim, the [`db_class`] token for every
/// other variant. Every `InvalidArgument` payload constructed on the gateway
/// path is a string literal naming the failed inner step (never a URL,
/// token or remote value); keep it that way — any other variant's `Display`
/// can carry a connection URL or statement detail and must stay unlogged.
#[must_use]
pub(crate) fn failure_reason(error: &sqlx::Error) -> std::borrow::Cow<'_, str> {
    match error {
        sqlx::Error::InvalidArgument(message) => std::borrow::Cow::Borrowed(message),
        _ => std::borrow::Cow::Borrowed(db_class(error)),
    }
}

/// Postgres SQLSTATE for database errors (`42P01`, `42501`, …), `None`
/// otherwise. The 5-character code is fixed vocabulary from our own database
/// server — it names the failure without the message, severity or statement
/// — so the gateway failure line can carry it where the raw error cannot go.
#[must_use]
pub(crate) fn error_code(error: &sqlx::Error) -> Option<String> {
    match error {
        sqlx::Error::Database(error) => error.code().map(|code| code.into_owned()),
        _ => None,
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
/// halves classify through [`db_class`]; Discord halves through
/// [`discord_class`]; malformed input is one token.
#[must_use]
pub(crate) fn leveling_class(error: &LevelingRuntimeError) -> &'static str {
    match error {
        LevelingRuntimeError::Store(LevelingStoreError::Db(inner)) => db_class(inner),
        LevelingRuntimeError::Store(LevelingStoreError::InvalidTimestamp(_)) => "invalid_timestamp",
        LevelingRuntimeError::Store(LevelingStoreError::InvalidRewards(_)) => "invalid_rewards",
        LevelingRuntimeError::Discord(discord) => discord_class(discord),
        LevelingRuntimeError::InvalidInteraction => "invalid_interaction",
        LevelingRuntimeError::LevelingPoisoned => "deferred_leveling_poisoned",
        LevelingRuntimeError::CommunityFactsPoisoned => "deferred_community_facts_poisoned",
        LevelingRuntimeError::RoleApplication { cause, .. } => match cause {
            RoleApplicationError::Store(LevelingStoreError::Db(inner)) => db_class(inner),
            RoleApplicationError::Store(LevelingStoreError::InvalidTimestamp(_)) => {
                "invalid_timestamp"
            }
            RoleApplicationError::Store(LevelingStoreError::InvalidRewards(_)) => "invalid_rewards",
            RoleApplicationError::Discord(discord) => discord_class(discord),
            RoleApplicationError::Plan(_) => "role_cap_exceeded",
            RoleApplicationError::Refused { .. } => "role_write_refused",
        },
    }
}

/// [`DiscordError`] as a fixed token. `Unavailable` can carry transport
/// detail and `Rejected` provider text, so neither is rendered. The transport
/// stringifies send-admission refusals into `Unavailable`; matching the typed
/// [`AdmissionError`] displays recovers them, so a storage failure still reads
/// differently from a Discord outage (the 2026-10-05 boot crash loop needed
/// exactly that). Everything else is [`DiscordError::cause`], which already
/// names `admission_blocked`.
#[must_use]
pub(crate) fn discord_class(error: &DiscordError) -> &'static str {
    if let DiscordError::Unavailable(detail) = error {
        for (admission, token) in [
            (AdmissionError::Storage, "admission_storage_unavailable"),
            (
                AdmissionError::Configuration,
                "admission_configuration_invalid",
            ),
            (AdmissionError::StaleClaim, "admission_stale_claim"),
        ] {
            if detail == &admission.to_string() {
                return token;
            }
        }
    }
    error.cause()
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
    use sqlx::error::{DatabaseError, ErrorKind};
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
    fn failure_reason_keeps_inner_step_and_code_without_error_text() {
        // `InvalidArgument` literals name the failed inner gateway step.
        assert_eq!(
            failure_reason(&sqlx::Error::InvalidArgument(
                "leveling gateway dispatch failed; checkpoint unchanged".into()
            )),
            "leveling gateway dispatch failed; checkpoint unchanged"
        );
        // Every other variant collapses to the fixed token, never the text.
        let gap = database("42P01");
        let reason = failure_reason(&gap);
        assert_eq!(reason, "database");
        assert!(!reason.contains("secret"), "reason leaked error text");
        // The SQLSTATE rides along separately for database errors only.
        assert_eq!(error_code(&database("42P01")), Some("42P01".to_owned()));
        assert_eq!(error_code(&database("42501")), Some("42501".to_owned()));
        assert_eq!(error_code(&sqlx::Error::PoolTimedOut), None);
        assert_eq!(
            error_code(&sqlx::Error::InvalidArgument("literal".into())),
            None
        );
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
        assert_eq!(
            leveling_class(&LevelingRuntimeError::LevelingPoisoned),
            "deferred_leveling_poisoned"
        );
        assert_eq!(
            leveling_class(&LevelingRuntimeError::CommunityFactsPoisoned),
            "deferred_community_facts_poisoned"
        );
    }

    #[test]
    fn role_application_failures_classify_after_the_committed_award() {
        let role = |cause| LevelingRuntimeError::RoleApplication {
            cause,
            awards: Vec::new(),
        };
        assert_eq!(
            leveling_class(&role(RoleApplicationError::Store(LevelingStoreError::Db(
                sqlx::Error::PoolClosed
            )))),
            "pool_closed"
        );
        assert_eq!(
            leveling_class(&role(RoleApplicationError::Store(
                LevelingStoreError::InvalidTimestamp("x".to_owned())
            ))),
            "invalid_timestamp"
        );
        assert_eq!(
            leveling_class(&role(RoleApplicationError::Store(
                LevelingStoreError::InvalidRewards(
                    two_bot_core::leveling::RewardConfigError::InvalidLevel
                )
            ))),
            "invalid_rewards"
        );
        assert_eq!(
            leveling_class(&role(RoleApplicationError::Discord(
                two_bot_discord::executor::DiscordError::Timeout
            ))),
            "timeout"
        );
        assert_eq!(
            leveling_class(&role(RoleApplicationError::Plan(
                two_bot_core::leveling::RewardPlanError::CapExceeded {
                    list: "grant",
                    count: 11,
                    cap: 10
                }
            ))),
            "role_cap_exceeded"
        );
        assert_eq!(
            leveling_class(&role(RoleApplicationError::Refused {
                role_id: "1".to_owned(),
                reason: "r".to_owned(),
                applied: Vec::new(),
                skipped: Vec::new()
            })),
            "role_write_refused"
        );
    }

    #[test]
    fn discord_class_keeps_admission_refusals_and_drops_transport_text() {
        for (error, token) in [
            (
                DiscordError::Unavailable(AdmissionError::Storage.to_string()),
                "admission_storage_unavailable",
            ),
            (
                DiscordError::Unavailable(AdmissionError::Configuration.to_string()),
                "admission_configuration_invalid",
            ),
            (
                DiscordError::Unavailable(AdmissionError::StaleClaim.to_string()),
                "admission_stale_claim",
            ),
            (
                DiscordError::Unavailable(AdmissionError::Blocked.to_string()),
                "admission_blocked",
            ),
            (
                DiscordError::Unavailable(
                    "error sending request for url (https://user:secret@example.invalid/)".into(),
                ),
                "unavailable",
            ),
            (
                DiscordError::Rejected("provider text with secret".into()),
                "rejected",
            ),
            (DiscordError::Timeout, "timeout"),
        ] {
            assert_eq!(discord_class(&error), token, "{error:?}");
        }
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
