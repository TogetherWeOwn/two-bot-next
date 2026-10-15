//! Sanitized error-chain contract for backfill + retention (TOG-25282 scaffold).
//!
//! M7.3 of the TOG-25262 roadmap: every backfill/retention failure event must
//! carry `class` + `chain` (sanitized) + `step` in the gateway [`FailureClass`]
//! vocabulary (TOG-25069 scope item 1, voice/gateway v1 itself out of scope and
//! read-only here). `FailureClass` has no backfill/retention variant today, and
//! today's failure surfaces render raw error text: the store errors below pass
//! `sqlx::Error` through `{0}`, and `retention` logs `format!("{err:?}")`.
//!
//! What is real and pinned green in this module:
//!
//! * [`BackfillStep`] / [`RetentionStep`]: the fixed step vocabularies. Every
//!   token matches `[a-z0-9_]{1,32}`, the shape `scripts/staging_rollout.py`
//!   and the Worker accept.
//! * [`sanitize_chain`]: the redaction the renders must apply. Final: the
//!   builder reuses it, never replaces it.
//!
//! What is scaffold (red until TOG-25285 lands):
//!
//! * [`render_backfill_failure`] / [`render_retention_failure`] render today's
//!   semantics — class plus the RAW chain, no step — so the frozen acceptance
//!   suites (`backfill_error_chain_acceptance_tests`,
//!   `retention_error_chain_acceptance_tests`) compile and fail red. The module
//!   is `#[cfg(test)]`-gated, so shipping it changes no runtime behavior and
//!   raises no dead-code lint.
//!
//! Builder wiring (TOG-25285; keep every signature here, never edit the frozen
//! acceptance files; promote this module from `#[cfg(test)]` to the production
//! build as part of the wiring):
//!
//! ```text
//! render_backfill_failure(class, step, chain)
//!     = "backfill_failed class=<class> step=<step> chain=<sanitize_chain(chain)>"
//! render_retention_failure(class, step, chain)
//!     = "retention_failed class=<class> step=<step> chain=<sanitize_chain(chain)>"
//! ```
//!
//! The frozen tests assert substrings, not exact formats: any layout carrying
//! `class=<FailureClass token>`, `step=<step token>` and a sanitized `chain`
//! passes. Wire the call sites (`ops_backfill` snapshot failure, `retention`
//! guild-purge failure + deferral warns, `retention_jobs` database-error
//! mapping), add the prod `FailureClass` variants the lines need, and keep the
//! class token in the output. Chain inputs must be classified causes (see
//! `db_log`, `DiscordError::cause`), never free-form text: display names and
//! message bodies never enter a chain.

use crate::gateway_failure::FailureClass;

/// One variant per fallible backfill stage (TOG-25285 build card: rate-limit
/// client, cursors, audit catch-up, plus the history put/count and the
/// progress-snapshot read).
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
    #[cfg(test)]
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

/// One variant per fallible retention stage (TOG-25285 build card: expiry,
/// deletion intake, dry-run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionStep {
    /// Rolling-window deletes (presence/voice raw, audit cache).
    Expiry,
    /// User-deletion intake + fulfilment (`retention_deletion_requests`).
    DeletionIntake,
    /// Dry-run projection (counts, deferrals, overdue signal).
    DryRun,
}

impl RetentionStep {
    /// Every variant, for the vocabulary tests.
    #[cfg(test)]
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

/// Scaffold with today's semantics: class plus the RAW chain, no step, no
/// sanitization. The builder replaces this body per the module docs; the
/// signature stays.
#[must_use]
pub fn render_backfill_failure(class: FailureClass, _step: BackfillStep, chain: &str) -> String {
    format!("backfill_failed class={} chain={chain}", class.as_str())
}

/// Scaffold with today's semantics: class plus the RAW chain, no step, no
/// sanitization. The builder replaces this body per the module docs; the
/// signature stays.
#[must_use]
pub fn render_retention_failure(class: FailureClass, _step: RetentionStep, chain: &str) -> String {
    format!("retention_failed class={} chain={chain}", class.as_str())
}
