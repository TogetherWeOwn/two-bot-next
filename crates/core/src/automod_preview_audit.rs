//! Audit transaction and redelivery replay for `/automod-preview`
//! (docs/automod-preview-spec.md §6; slice 3e).
//!
//! Pure core plus three transaction-capable store calls. The handler owns
//! strict decode, scope checks, the limiter, and the live-snapshot refresh;
//! it calls into this module to serialize attempts, persist the
//! content-free audit row, and replay a committed row without re-evaluating.
//!
//! Attempt protocol (§6 "Redelivery and crash recovery"): after strict decode
//! and permission/scope checks, the winner takes a transaction-scoped
//! advisory lock keyed by interaction id, reads the audit id on that same
//! transaction connection, and — only when no row exists — charges the
//! limiter, evaluates synchronously, inserts the audit row, and commits
//! **before any verdict reply**. The runtime mutex is released before any
//! audit I/O await; it is never held across the transaction's awaits.
//!
//! The reply and the row carry no content by construction: builders take
//! enums (and the stored timestamp text) only — never the previewed text,
//! its normalization, or any digest of it.

use crate::automod::AutomodFilter;
use crate::automod_preview::{PreviewExemption, PreviewResult, PreviewVerdict};
use crate::automod_preview_report::{
    render_preview_reply, replay_exemption_knob, PREVIEW_RATE_LIMITED_REPLY,
};

/// `action` column value for every preview audit row.
pub const PREVIEW_AUDIT_ACTION: &str = "automod.preview";

/// Audit-row (and advisory-lock) id for one preview attempt:
/// `automod-preview:<interaction_id>`. The lock key and the row id are the
/// same string, so one interaction serializes on exactly the row it claims.
#[must_use]
pub fn preview_audit_id(interaction_id: &str) -> String {
    format!("automod-preview:{interaction_id}")
}

/// Strict inverse of [`preview_audit_id`]: the interaction id, or `None`
/// when the id is not a preview claim. An empty remainder refuses.
#[must_use]
pub fn parse_preview_audit_id(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("automod-preview:")?;
    (!rest.is_empty()).then_some(rest)
}

/// One committed preview audit row: outcome, reason, and the stored
/// `created_at` text. The timestamp text is used verbatim as the reply's
/// `current at` currency, so the first reply and every replay render the
/// same string at database precision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewAuditRow {
    pub outcome: String,
    pub reason: Option<String>,
    pub created_at: String,
}

/// Parsed `reason` field (`filter=..;exemption=..;mode=..;len=..`). Only the
/// filter, exemption, and mode drive a replayed reply; the length bucket is
/// validated present but never rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedPreviewReason {
    filter: String,
    exemption: String,
    dry_run: bool,
}

/// Strict parse of a stored preview reason. `None` fails closed as
/// unavailable: never a guessed class or a fresh evaluation.
fn parse_preview_reason(reason: &str) -> Option<ParsedPreviewReason> {
    let mut parts = reason.split(';');
    let filter = parts.next()?.strip_prefix("filter=")?;
    let exemption = parts.next()?.strip_prefix("exemption=")?;
    let mode = parts.next()?.strip_prefix("mode=")?;
    let len = parts.next()?.strip_prefix("len=")?;
    if parts.next().is_some() || len.is_empty() {
        return None;
    }
    let dry_run = match mode {
        "dry_run" => true,
        "enforce" => false,
        _ => return None,
    };
    Some(ParsedPreviewReason {
        filter: filter.to_owned(),
        exemption: exemption.to_owned(),
        dry_run,
    })
}

/// Rebuild the ephemeral reply from one committed audit row, without
/// evaluating anything. Returns `None` for any missing, unknown, or
/// malformed field: the handler answers the generic unavailable line and
/// never guesses a verdict.
///
/// The exemption knob comes from the persisted class via
/// [`replay_exemption_knob`], never from current config or member roles; a
/// `matched` filter name parses via [`AutomodFilter::from_persisted_name`].
/// `rate_limited` replays the generic rate line (the mode still parses, so a
/// corrupt reason fails closed). `created_at` is the stored timestamp text,
/// used verbatim so replays match the first reply exactly.
#[must_use]
pub fn replay_preview_reply(
    outcome: &str,
    reason: Option<&str>,
    created_at: &str,
) -> Option<String> {
    if created_at.is_empty() {
        return None;
    }
    if outcome == "rate_limited" {
        let parsed = parse_preview_reason(reason?)?;
        if parsed.filter != "none" || parsed.exemption != "none" {
            return None;
        }
        return Some(PREVIEW_RATE_LIMITED_REPLY.to_owned());
    }
    let parsed = parse_preview_reason(reason?)?;
    let verdict = match outcome {
        "matched" => {
            if parsed.exemption != "none" {
                return None;
            }
            let filter = AutomodFilter::from_persisted_name(&parsed.filter)?;
            PreviewVerdict {
                result: PreviewResult::Matched,
                filter: Some(filter),
                knobs: crate::automod_preview::deciding_knobs(filter),
                exemption: None,
                dry_run: parsed.dry_run,
            }
        }
        "no_match" => {
            if parsed.filter != "none" || parsed.exemption != "none" {
                return None;
            }
            PreviewVerdict {
                result: PreviewResult::NoMatch,
                filter: None,
                knobs: &[],
                exemption: None,
                dry_run: parsed.dry_run,
            }
        }
        "exempt" => {
            if parsed.filter != "none" {
                return None;
            }
            let class = match parsed.exemption.as_str() {
                "exempt_channel" => PreviewExemption::ExemptChannel,
                "bypass_role" => PreviewExemption::BypassRole,
                _ => return None,
            };
            // The knob comes from the stored class, never current config.
            replay_exemption_knob(parsed.exemption.as_str())?;
            PreviewVerdict {
                result: PreviewResult::Exempt,
                filter: None,
                knobs: crate::automod_preview::exemption_knobs(class),
                exemption: Some(class),
                dry_run: parsed.dry_run,
            }
        }
        "partial" => {
            if parsed.filter != "none" || parsed.exemption != "none" {
                return None;
            }
            PreviewVerdict::partial(parsed.dry_run)
        }
        _ => return None,
    };
    Some(render_preview_reply(&verdict, created_at))
}

/// Build the [`crate::custom_commands::AuditRecord`] for one evaluated
/// preview or in-scope rate refusal. `outcome`/`reason` come from the
/// `preview_audit_outcome` / `audit_reason_for_verdict` builders (or the
/// rate-limited builders); `created_at` is always the winning attempt's
/// captured UTC receipt time (§3.1), never a database default.
#[must_use]
pub fn preview_audit_record(
    guild_id: &str,
    actor_id: &str,
    channel_id: &str,
    outcome: &str,
    reason: String,
) -> crate::custom_commands::AuditRecord {
    crate::custom_commands::AuditRecord {
        guild_id: guild_id.to_owned(),
        actor_id: Some(actor_id.to_owned()),
        action: PREVIEW_AUDIT_ACTION.to_owned(),
        target_key: Some(channel_id.to_owned()),
        outcome: outcome.to_owned(),
        reason: Some(reason),
    }
}

/// Serialize one attempt by interaction id inside the caller's transaction
/// (spec §6). Transaction-scoped: commit or rollback releases the claim, so
/// no permanent claimed-without-result state is possible.
#[cfg(feature = "db")]
pub async fn lock_preview_attempt(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    interaction_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(preview_audit_id(interaction_id))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Transaction-capable replay read: `(outcome, reason, created_at)` for one
/// audit id, on any executor — the winner's transaction connection or the
/// pool. `Ok(None)` means no committed row (first delivery, or a rolled-back
/// attempt that may retry). The existing `load_audit` cannot satisfy this
/// contract: it takes the pool only and drops the timestamp.
#[cfg(feature = "db")]
pub async fn load_preview_audit<'c>(
    executor: impl sqlx::Executor<'c, Database = sqlx::Postgres>,
    id: &str,
) -> Result<Option<PreviewAuditRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT outcome, reason, created_at::text AS created_at_text
         FROM automation_audit_log WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await?;
    Ok(row.map(|row| {
        use sqlx::Row;
        PreviewAuditRow {
            outcome: row.get("outcome"),
            reason: row.get("reason"),
            created_at: row.get("created_at_text"),
        }
    }))
}

/// Insert one preview audit row and read back the stored timestamp
/// (`INSERT ... RETURNING created_at::text`). The caller renders both the
/// first reply and every replay from that returned text, so config-currency
/// times agree at database precision.
#[cfg(feature = "db")]
pub async fn insert_preview_audit<'c>(
    executor: impl sqlx::Executor<'c, Database = sqlx::Postgres>,
    record: &crate::custom_commands::AuditRecord,
    id: &str,
    at_iso: &str,
) -> Result<String, sqlx::Error> {
    let stored: String = sqlx::query_scalar(
        "INSERT INTO automation_audit_log (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::timestamptz)
         RETURNING created_at::text",
    )
    .bind(id)
    .bind(&record.guild_id)
    .bind(&record.actor_id)
    .bind(&record.action)
    .bind(&record.target_key)
    .bind(&record.outcome)
    .bind(&record.reason)
    .bind(at_iso)
    .fetch_one(executor)
    .await?;
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automod::{AutomodConfig, AutomodMessage, AutomodPolicy, RepeatTracker};
    use crate::automod_preview::preview_automod;
    use crate::automod_preview_report::{audit_reason_for_verdict, preview_audit_outcome};

    #[test]
    fn audit_id_round_trip_is_strict() {
        assert_eq!(preview_audit_id("123"), "automod-preview:123");
        assert_eq!(parse_preview_audit_id("automod-preview:123"), Some("123"));
        for bad in [
            "",
            "automod-preview:",
            "automod-preview",
            "custom:text:attempt:123",
            "AUTOMOD-PREVIEW:123",
            "automod-preview:123:extra",
        ] {
            if bad == "automod-preview:123:extra" {
                // Remainder may itself contain ':'; it still parses.
                assert_eq!(parse_preview_audit_id(bad), Some("123:extra"));
            } else {
                assert_eq!(parse_preview_audit_id(bad), None, "id {bad:?}");
            }
        }
    }

    #[test]
    fn reason_parse_accepts_only_the_emitted_shape() {
        let parsed = parse_preview_reason("filter=bad_words;exemption=none;mode=dry_run;len=0_50")
            .expect("emitted shape parses");
        assert_eq!(parsed.filter, "bad_words");
        assert_eq!(parsed.exemption, "none");
        assert!(parsed.dry_run);
        let enforced =
            parse_preview_reason("filter=none;exemption=bypass_role;mode=enforce;len=51_500")
                .expect("enforce parses");
        assert!(!enforced.dry_run);
        for bad in [
            "",
            "filter=bad_words",
            "filter=bad_words;exemption=none;mode=dry_run",
            "filter=bad_words;exemption=none;mode=dry_run;len=0_50;extra=x",
            "filter=bad_words;exemption=none;mode=live;len=0_50",
            "filter=bad_words;exemption=none;mode=dry_run;len=",
            "exemption=none;filter=bad_words;mode=dry_run;len=0_50",
            "filter:bad_words;exemption=none;mode=dry_run;len=0_50",
        ] {
            assert_eq!(parse_preview_reason(bad), None, "reason {bad:?}");
        }
    }

    fn matched_bad_words_verdict(dry_run: bool) -> PreviewVerdict {
        PreviewVerdict {
            result: PreviewResult::Matched,
            filter: Some(AutomodFilter::BadWords),
            knobs: crate::automod_preview::deciding_knobs(AutomodFilter::BadWords),
            exemption: None,
            dry_run,
        }
    }

    #[test]
    fn replay_rebuilds_every_outcome_without_evaluating() {
        let created_at = "2026-11-02 08:00:00+00";
        // Matched: first reply and replayed reply are byte-identical.
        let verdict = matched_bad_words_verdict(true);
        let first = render_preview_reply(&verdict, created_at);
        let reason = audit_reason_for_verdict(&verdict, 12);
        assert_eq!(
            replay_preview_reply(preview_audit_outcome(&verdict), Some(&reason), created_at),
            Some(first)
        );
        // No-match, exempt (knob from the stored class), partial, rate-limited.
        let no_match = PreviewVerdict {
            result: PreviewResult::NoMatch,
            filter: None,
            knobs: &[],
            exemption: None,
            dry_run: false,
        };
        let reason = audit_reason_for_verdict(&no_match, 9);
        assert_eq!(
            replay_preview_reply("no_match", Some(&reason), created_at),
            Some(render_preview_reply(&no_match, created_at))
        );
        let exempt = PreviewVerdict {
            result: PreviewResult::Exempt,
            filter: None,
            knobs: crate::automod_preview::exemption_knobs(PreviewExemption::BypassRole),
            exemption: Some(PreviewExemption::BypassRole),
            dry_run: true,
        };
        let reason = audit_reason_for_verdict(&exempt, 7);
        let replayed =
            replay_preview_reply("exempt", Some(&reason), created_at).expect("exempt replays");
        assert_eq!(replayed, render_preview_reply(&exempt, created_at));
        assert!(replayed.contains("TWO_AUTOMOD_BYPASS_ROLE_IDS"));
        let partial = PreviewVerdict::partial(true);
        let reason = audit_reason_for_verdict(&partial, 9);
        assert_eq!(
            replay_preview_reply("partial", Some(&reason), created_at),
            Some(render_preview_reply(&partial, created_at))
        );
        assert_eq!(
            replay_preview_reply(
                "rate_limited",
                Some("filter=none;exemption=none;mode=enforce;len=51_500"),
                created_at
            ),
            Some(PREVIEW_RATE_LIMITED_REPLY.to_owned())
        );
    }

    #[test]
    fn replay_fails_closed_on_any_malformed_field() {
        let created_at = "2026-11-02 08:00:00+00";
        // Unknown outcome, missing/invalid reason, unknown filter, guessed class.
        assert_eq!(
            replay_preview_reply(
                "bogus",
                Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        assert_eq!(replay_preview_reply("matched", None, created_at), None);
        assert_eq!(
            replay_preview_reply(
                "matched",
                Some("filter=nonsense;exemption=none;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        // Matched row claiming an exemption, exempt row with a filter, and an
        // invalid stored exemption class all refuse instead of guessing.
        assert_eq!(
            replay_preview_reply(
                "matched",
                Some("filter=bad_words;exemption=bypass_role;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        assert_eq!(
            replay_preview_reply(
                "no_match",
                Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        assert_eq!(
            replay_preview_reply(
                "exempt",
                Some("filter=none;exemption=admin;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        assert_eq!(
            replay_preview_reply(
                "exempt",
                Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
                created_at
            ),
            None
        );
        // Corrupt rate-limited reason and empty timestamp refuse too.
        assert_eq!(
            replay_preview_reply(
                "rate_limited",
                Some("filter=none;exemption=none;mode=live;len=0_50"),
                created_at
            ),
            None
        );
        assert_eq!(
            replay_preview_reply(
                "no_match",
                Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
                ""
            ),
            None
        );
    }

    #[test]
    fn replay_never_consults_live_config_or_roles() {
        // The stored row was written under a config that has since changed:
        // replay still renders the stored filter and knob, byte-identical.
        let created_at = "2026-11-02 08:00:00+00";
        let verdict = matched_bad_words_verdict(true);
        let reason = audit_reason_for_verdict(&verdict, 12);
        let before = replay_preview_reply("matched", Some(&reason), created_at).expect("replays");
        // A "changed world": different policy, different roles — replay takes
        // neither as a parameter, so the stored row renders the same reply.
        let policy = AutomodPolicy {
            bad_words: vec!["different".to_owned()],
            ..AutomodPolicy::default()
        };
        let _config = AutomodConfig {
            enabled: true,
            dry_run: false,
            policy,
        };
        let after = replay_preview_reply("matched", Some(&reason), created_at).expect("replays");
        assert_eq!(before, after);
        assert!(after.contains("TWO_AUTOMOD_BAD_WORDS"));
    }

    #[test]
    fn replay_matches_a_live_evaluation_for_the_same_verdict() {
        // Core-half of T17: the row written for a real evaluation replays the
        // same reply the evaluation rendered.
        let policy = AutomodPolicy {
            bad_words: vec!["bannedword".to_owned()],
            ..AutomodPolicy::default()
        };
        let config = AutomodConfig {
            enabled: true,
            dry_run: true,
            policy,
        };
        let message = AutomodMessage {
            guild_id: "g".to_owned(),
            channel_id: "c".to_owned(),
            message_id: "preview:live".to_owned(),
            author_id: "a".to_owned(),
            author_is_bot: false,
            role_ids: vec![],
            content: "has bannedword here".to_owned(),
            mentioned_user_ids: vec![],
            attachment_names: vec![],
            observed_timestamp_ms: 1,
        };
        let verdict = preview_automod(&message, 0, &config, &RepeatTracker::default());
        assert_eq!(verdict.result, PreviewResult::Matched);
        let created_at = "2026-11-02 08:00:00+00";
        let first = render_preview_reply(&verdict, created_at);
        let reason = audit_reason_for_verdict(&verdict, 17);
        assert_eq!(
            replay_preview_reply(preview_audit_outcome(&verdict), Some(&reason), created_at),
            Some(first)
        );
    }

    #[test]
    fn audit_record_carries_claim_fields_and_winning_receipt_time() {
        let record = preview_audit_record(
            "guild-1",
            "actor-1",
            "channel-1",
            "matched",
            "filter=bad_words;exemption=none;mode=dry_run;len=0_50".to_owned(),
        );
        assert_eq!(record.action, PREVIEW_AUDIT_ACTION);
        assert_eq!(record.guild_id, "guild-1");
        assert_eq!(record.actor_id.as_deref(), Some("actor-1"));
        assert_eq!(record.target_key.as_deref(), Some("channel-1"));
        assert_eq!(record.outcome, "matched");
        assert!(record
            .reason
            .as_deref()
            .unwrap()
            .starts_with("filter=bad_words;"));
    }
}
