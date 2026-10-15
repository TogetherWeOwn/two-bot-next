//! Sanitized error-chain contract for backfill + retention failure lines.
//!
//! Every backfill/retention failure event carries `class` + `chain`
//! (sanitized) + `step`. The class token comes from the caller (the bot
//! crate passes its gateway `FailureClass` token); the step comes from
//! [`BackfillStep`] / [`RetentionStep`]. Every token matches
//! `[a-z0-9_]{1,32}`, the shape `scripts/staging_rollout.py` and the Worker
//! accept.
//!
//! The chain must never carry raw error text: a `sqlx::Error` Display/Debug
//! chain can embed connection URLs, statement detail, and member snowflakes.
//! [`sanitize_chain`] redacts the hostile token shapes while leaving
//! fixed-vocabulary causes byte-identical; [`db_cause`] classifies a bare
//! `sqlx::Error` to a fixed token so chain inputs are classified causes,
//! never free-form text: display names and message bodies never enter a
//! chain.
//!
//! This lives in core (a library, where `pub` vocabulary carries no
//! dead-code lint) so the bot binary, the store error Displays, and the
//! cutover retention paths all share one vocabulary instead of drifting
//! string literals.

/// One variant per fallible backfill stage: rate-limit client, cursors,
/// audit catch-up, plus the history put/count and the progress-snapshot
/// read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillStep {
    /// Slice-1 rate-limit client (`ActionExecutor::get_json_observed` waits).
    RateLimitClient,
    /// Per-family cursor checkpoint (`backfill_cursors` store).
    CursorCheckpoint,
    /// Audit-log 45d catch-up writes.
    AuditCatchUp,
    /// Channel-history message metadata writes.
    HistoryPut,
    /// Per-channel history aggregate counts.
    HistoryCount,
    /// Progress-snapshot read (`ops_backfill`).
    SnapshotRead,
}

impl BackfillStep {
    /// Every variant, for the vocabulary tests.
    pub const ALL: [Self; 6] = [
        Self::RateLimitClient,
        Self::CursorCheckpoint,
        Self::AuditCatchUp,
        Self::HistoryPut,
        Self::HistoryCount,
        Self::SnapshotRead,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimitClient => "rate_limit_client",
            Self::CursorCheckpoint => "cursor_checkpoint",
            Self::AuditCatchUp => "audit_catch_up",
            Self::HistoryPut => "history_put",
            Self::HistoryCount => "history_count",
            Self::SnapshotRead => "snapshot_read",
        }
    }
}

/// One variant per fallible retention stage: expiry, deletion intake,
/// dry-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionStep {
    /// Rolling-window deletes (presence/voice raw, audit cache).
    Expiry,
    /// User-deletion intake + fulfilment (`retention_deletion_requests`,
    /// including guild-purge drains).
    DeletionIntake,
    /// Dry-run projection (counts, deferrals, overdue signal).
    DryRun,
}

impl RetentionStep {
    /// Every variant, for the vocabulary tests.
    pub const ALL: [Self; 3] = [Self::Expiry, Self::DeletionIntake, Self::DryRun];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Expiry => "expiry",
            Self::DeletionIntake => "deletion_intake",
            Self::DryRun => "dry_run",
        }
    }
}

/// One backfill failure line: the caller-supplied class token, the stage
/// step, and the sanitized chain.
#[must_use]
pub fn render_backfill_failure(class: &str, step: BackfillStep, chain: &str) -> String {
    format!(
        "backfill_failed class={class} step={} chain={}",
        step.as_str(),
        sanitize_chain(chain)
    )
}

/// One retention failure line: the caller-supplied class token, the stage
/// step, and the sanitized chain.
#[must_use]
pub fn render_retention_failure(class: &str, step: RetentionStep, chain: &str) -> String {
    format!(
        "retention_failed class={class} step={} chain={}",
        step.as_str(),
        sanitize_chain(chain)
    )
}

/// Redact the hostile shapes a raw error chain can carry. Whitespace-separated
/// tokens are dropped to `[redacted]` when they contain `@` (emails,
/// credential URLs), a credential word (`secret`, `bearer`, `password`), or a
/// run of ten or more digits (member/guild/channel snowflakes). Everything
/// else — classified causes like `pool_timeout` or `rate_limited` — passes
/// through byte-identical, so fixed-vocabulary chain content is never mangled.
#[must_use]
pub fn sanitize_chain(raw: &str) -> String {
    raw.split_whitespace()
        .map(|token| {
            if hostile_token(token) {
                "[redacted]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn hostile_token(token: &str) -> bool {
    if token.contains('@') {
        return true;
    }
    let lower = token.to_lowercase();
    if lower.contains("secret") || lower.contains("bearer") || lower.contains("password") {
        return true;
    }
    let mut run = 0u32;
    for byte in token.bytes() {
        if byte.is_ascii_digit() {
            run += 1;
            if run >= 10 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Fixed token naming a database failure without its internals.
///
/// Constraint violations keep their kind; pool/connection/row states keep
/// theirs; everything else (protocol, decode, configuration, transaction
/// misuse) collapses to `query`. SQLSTATE codes, messages and severities
/// are never surfaced: rerun the failing operation path for detail.
#[cfg(feature = "db")]
#[must_use]
pub fn db_cause(error: &sqlx::Error) -> &'static str {
    use sqlx::error::ErrorKind;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_tokens_are_a_distinct_short_vocabulary() {
        fn is_token(value: &str) -> bool {
            (1..=32).contains(&value.len())
                && value
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        }
        let mut seen = std::collections::BTreeSet::new();
        for step in BackfillStep::ALL
            .into_iter()
            .map(BackfillStep::as_str)
            .chain(RetentionStep::ALL.into_iter().map(RetentionStep::as_str))
        {
            assert!(is_token(step), "{step:?}");
            assert!(seen.insert(step), "duplicate token {step:?}");
        }
    }

    #[test]
    fn renders_carry_class_step_and_sanitized_chain() {
        let line = render_backfill_failure(
            "store_unavailable",
            BackfillStep::CursorCheckpoint,
            "pool_timeout",
        );
        assert!(line.contains("store_unavailable"), "{line:?}");
        assert!(line.contains("cursor_checkpoint"), "{line:?}");
        assert!(line.contains("pool_timeout"), "{line:?}");
        let line = render_retention_failure(
            "store_unavailable",
            RetentionStep::DeletionIntake,
            "operator@example.com",
        );
        assert!(!line.contains("operator@example.com"), "{line:?}");
        assert!(line.contains("deletion_intake"), "{line:?}");
    }

    #[test]
    fn redacts_secret_token_pii_and_member_id() {
        for hostile in [
            "postgres://user:hunter2@db.internal/app",
            "Bearer test-bearer-xxxx",
            "operator@example.com",
            "987654321098765432",
        ] {
            let clean = sanitize_chain(hostile);
            assert!(!clean.contains(hostile), "sanitizer leaked {hostile:?}");
            assert!(clean.contains("[redacted]"), "must mark redaction");
        }
        let embedded = "fetch 987654321098765432 failed for operator@example.com";
        assert!(!sanitize_chain(embedded).contains("987654321098765432"));
        assert!(!sanitize_chain(embedded).contains("operator@example.com"));
    }

    #[test]
    fn keeps_classified_causes_byte_identical() {
        for cause in [
            "pool_timeout",
            "rate_limited",
            "connection",
            "row_not_found",
        ] {
            assert_eq!(sanitize_chain(cause), cause, "cause {cause:?} mangled");
        }
    }

    #[cfg(feature = "db")]
    #[test]
    fn pool_and_connection_states_keep_their_token() {
        assert_eq!(db_cause(&sqlx::Error::PoolTimedOut), "pool_timeout");
        assert_eq!(db_cause(&sqlx::Error::PoolClosed), "pool_closed");
        assert_eq!(db_cause(&sqlx::Error::WorkerCrashed), "worker_crashed");
        assert_eq!(db_cause(&sqlx::Error::RowNotFound), "row_not_found");
        assert_eq!(db_cause(&sqlx::Error::Protocol("fake".to_owned())), "query");
    }
}
