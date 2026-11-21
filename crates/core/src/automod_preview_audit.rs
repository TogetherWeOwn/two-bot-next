//! Fail-closed replay of supported automod preview rows.
//!
//! This helper is not wired to the preview handler.

use crate::automod::AutomodFilter;
use crate::automod_preview::{
    deciding_knobs, exemption_knobs, PreviewExemption, PreviewResult, PreviewVerdict,
};
use crate::automod_preview_report::{render_preview_reply, PREVIEW_RATE_LIMITED_REPLY};

/// `action` column value for every preview audit row.
pub const PREVIEW_AUDIT_ACTION: &str = "automod.preview";

/// Audit-row (and advisory-lock) id for one preview attempt:
/// `automod-preview:<interaction_id>`. The lock key and the row id are the
/// same string, so one interaction serializes on exactly the row it claims.
/// Callers must supply a non-empty interaction id for the id to be parseable.
#[must_use]
pub fn preview_audit_id(interaction_id: &str) -> String {
    format!("automod-preview:{interaction_id}")
}

/// Inverse of [`preview_audit_id`] for non-empty interaction ids: the
/// interaction id, or `None` when the id is not a preview claim. An empty
/// remainder refuses; interaction ids containing colons are preserved.
#[must_use]
pub fn parse_preview_audit_id(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("automod-preview:")?;
    (!rest.is_empty()).then_some(rest)
}

/// One visible preview audit row: outcome, reason, and the stored
/// `created_at` text. The timestamp text is used verbatim as the reply's
/// invocation receipt time, so the first reply and every replay render the
/// same string at database precision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewAuditRow {
    pub outcome: String,
    pub reason: Option<String>,
    pub created_at: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedPreviewReason<'a> {
    filter: &'a str,
    exemption: &'a str,
    dry_run: bool,
}

fn parse_preview_reason(reason: &str) -> Option<ParsedPreviewReason<'_>> {
    let mut parts = reason.split(';');
    let filter = parts.next()?.strip_prefix("filter=")?;
    let exemption = parts.next()?.strip_prefix("exemption=")?;
    let mode = parts.next()?.strip_prefix("mode=")?;
    let len = parts.next()?.strip_prefix("len=")?;
    if parts.next().is_some() || !matches!(len, "0_50" | "51_500" | "501_2000") {
        return None;
    }
    let dry_run = match mode {
        "dry_run" => true,
        "enforce" => false,
        _ => return None,
    };
    Some(ParsedPreviewReason {
        filter,
        exemption,
        dry_run,
    })
}

/// Reconstruct a supported reply from a committed row without evaluation.
/// `created_at` is the pre-rendered stored invocation receipt, not a new clock
/// reading. Unsupported outcomes, invalid reasons, and empty timestamps fail closed.
#[must_use]
pub fn replay_preview_reply(
    outcome: &str,
    reason: Option<&str>,
    created_at: &str,
) -> Option<String> {
    if created_at.is_empty() {
        return None;
    }
    let parsed = parse_preview_reason(reason?)?;
    let verdict = match (outcome, parsed.filter, parsed.exemption) {
        ("matched", name, "none") => {
            let filter = AutomodFilter::from_persisted_name(name)?;
            PreviewVerdict {
                result: PreviewResult::Matched,
                filter: Some(filter),
                knobs: deciding_knobs(filter),
                exemption: None,
                dry_run: parsed.dry_run,
            }
        }
        ("rate_limited", "none", "none") => return Some(PREVIEW_RATE_LIMITED_REPLY.to_owned()),
        ("no_match", "none", "none") => PreviewVerdict {
            result: PreviewResult::NoMatch,
            filter: None,
            knobs: &[],
            exemption: None,
            dry_run: parsed.dry_run,
        },
        ("partial", "none", "none") => PreviewVerdict::partial(parsed.dry_run),
        ("exempt", "none", class) => {
            let exemption = match class {
                "exempt_channel" => PreviewExemption::ExemptChannel,
                "bypass_role" => PreviewExemption::BypassRole,
                _ => return None,
            };
            PreviewVerdict {
                result: PreviewResult::Exempt,
                filter: None,
                knobs: exemption_knobs(exemption),
                exemption: Some(exemption),
                dry_run: parsed.dry_run,
            }
        }
        _ => return None,
    };
    Some(render_preview_reply(&verdict, created_at))
}

/// Build the [`crate::custom_commands::AuditRecord`] for one evaluated
/// preview or in-scope rate refusal. `outcome`/`reason` come from the
/// `preview_audit_outcome` / `audit_reason_for_verdict` builders (or the
/// rate-limited builders). This function does not validate those strings;
/// callers must not pass preview content. The caller supplies the winning
/// attempt's captured UTC receipt time separately to `insert_preview_audit` (§3.1).
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
/// (spec §6). Commit or rollback releases the claim. Acquisition waits for a
/// competing transaction: the caller must set a database timeout before calling.
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
/// pool. `Ok(None)` means no row is visible to that executor; a transaction
/// can also see its own uncommitted insert. The existing `load_audit` takes
/// the pool only and drops the timestamp.
#[cfg(feature = "db")]
pub async fn load_preview_audit<'c>(
    executor: impl sqlx::Executor<'c, Database = sqlx::Postgres>,
    id: &str,
) -> Result<Option<PreviewAuditRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT outcome, reason,
                to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS.US') || '+00' AS created_at_text
         FROM automation_audit_log WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await?;
    use sqlx::Row;
    row.map(|row| {
        Ok(PreviewAuditRow {
            outcome: row.try_get("outcome")?,
            reason: row.try_get("reason")?,
            created_at: row.try_get("created_at_text")?,
        })
    })
    .transpose()
}

/// Insert one preview audit row and read back the stored timestamp
/// (`INSERT ... RETURNING` in UTC). The caller renders both the first reply
/// and every replay from that returned text, independent of session timezone
/// and at database precision.
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
         RETURNING to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS.US') || '+00'",
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
    use crate::automod_preview_report::{audit_reason_for_verdict, preview_audit_outcome};

    #[test]
    fn rate_limit_replays_each_emitted_mode_and_bucket() {
        for mode in ["dry_run", "enforce"] {
            for len in ["0_50", "51_500", "501_2000"] {
                let reason = format!("filter=none;exemption=none;mode={mode};len={len}");
                assert_eq!(
                    replay_preview_reply("rate_limited", Some(&reason), "stored timestamp"),
                    Some(PREVIEW_RATE_LIMITED_REPLY.to_owned())
                );
            }
        }
    }

    #[test]
    fn malformed_or_contradictory_rate_limit_rows_fail_closed() {
        for reason in [
            "",
            "filter=none",
            "filter=none;exemption=none;mode=dry_run",
            "filter=none;exemption=none;mode=dry_run;len=0_50;extra=x",
            "filter=none;exemption=none;mode=live;len=0_50",
            "filter=none;exemption=none;mode=dry_run;len=",
            "filter=none;exemption=none;mode=dry_run;len=unexpected",
            "exemption=none;filter=none;mode=dry_run;len=0_50",
            "filter:none;exemption=none;mode=dry_run;len=0_50",
            "filter=bad_words;exemption=none;mode=dry_run;len=0_50",
            "filter=none;exemption=bypass_role;mode=dry_run;len=0_50",
        ] {
            assert_eq!(
                replay_preview_reply("rate_limited", Some(reason), "t"),
                None
            );
        }
        assert_eq!(replay_preview_reply("rate_limited", None, "t"), None);
        assert_eq!(
            replay_preview_reply(
                "rate_limited",
                Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
                ""
            ),
            None
        );
    }

    #[test]
    fn no_match_and_partial_replay_the_original_reply() {
        let created_at = "2026-11-02T08:00:00.123Z";
        for dry_run in [true, false] {
            let no_match = PreviewVerdict {
                result: PreviewResult::NoMatch,
                filter: None,
                knobs: &[],
                exemption: None,
                dry_run,
            };
            for verdict in [no_match, PreviewVerdict::partial(dry_run)] {
                let outcome = preview_audit_outcome(&verdict);
                for len in [50, 51, 501] {
                    let reason = audit_reason_for_verdict(&verdict, len);
                    assert_eq!(
                        replay_preview_reply(outcome, Some(&reason), created_at),
                        Some(render_preview_reply(&verdict, created_at)),
                        "{outcome}: {reason}"
                    );
                }
            }
        }
    }

    #[test]
    fn malformed_or_contradictory_verdict_rows_fail_closed() {
        let valid = Some("filter=none;exemption=none;mode=dry_run;len=0_50");
        for outcome in ["no_match", "partial"] {
            for reason in [
                None,
                Some("filter=none;exemption=none;mode=dry_run"),
                Some("filter=none;exemption=none;mode=live;len=0_50"),
                Some("filter=none;exemption=none;mode=dry_run;len=unexpected"),
                Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50"),
                Some("filter=none;exemption=bypass_role;mode=dry_run;len=0_50"),
            ] {
                assert_eq!(replay_preview_reply(outcome, reason, "t"), None);
            }
            assert_eq!(replay_preview_reply(outcome, valid, ""), None);
        }
    }

    #[test]
    fn unknown_outcomes_and_missing_exemption_fail_closed() {
        let reason = Some("filter=none;exemption=none;mode=dry_run;len=0_50");
        for outcome in ["exempt", "bogus", ""] {
            assert_eq!(replay_preview_reply(outcome, reason, "t"), None);
        }
        assert_eq!(
            replay_preview_reply(
                "bogus",
                Some("filter=none;exemption=bypass_role;mode=dry_run;len=0_50"),
                "t",
            ),
            None
        );
    }

    #[test]
    fn matched_replays_the_original_reply_for_each_filter_and_mode() {
        let created_at = "2026-11-02T08:00:00.123Z";
        for filter in [
            AutomodFilter::BadWords,
            AutomodFilter::RepeatedMessage,
            AutomodFilter::MentionSpam,
            AutomodFilter::InviteLink,
            AutomodFilter::ExternalLink,
            AutomodFilter::AttachmentType,
        ] {
            for dry_run in [true, false] {
                let verdict = PreviewVerdict {
                    result: PreviewResult::Matched,
                    filter: Some(filter),
                    knobs: deciding_knobs(filter),
                    exemption: None,
                    dry_run,
                };
                let outcome = preview_audit_outcome(&verdict);
                for len in [50, 51, 501] {
                    let reason = audit_reason_for_verdict(&verdict, len);
                    assert_eq!(
                        replay_preview_reply(outcome, Some(&reason), created_at),
                        Some(render_preview_reply(&verdict, created_at)),
                        "{outcome}: {reason}"
                    );
                }
            }
        }
    }

    #[test]
    fn malformed_or_contradictory_matched_rows_fail_closed() {
        let valid = Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50");
        for reason in [
            None,
            Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
            Some("filter=unknown;exemption=none;mode=dry_run;len=0_50"),
            Some("filter=bad_words;exemption=bypass_role;mode=dry_run;len=0_50"),
            Some("filter=bad_words;exemption=exempt_channel;mode=enforce;len=0_50"),
            Some("filter=bad_words;exemption=none;mode=live;len=0_50"),
            Some("filter=bad_words;exemption=none;mode=dry_run;len=unexpected"),
            Some("filter=bad_words;exemption=none;mode=dry_run"),
            Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50;filter=invite_link"),
        ] {
            assert_eq!(
                replay_preview_reply("matched", reason, "t"),
                None,
                "{reason:?}"
            );
        }
        assert_eq!(replay_preview_reply("matched", valid, ""), None);
    }

    #[test]
    fn exempt_replays_the_original_reply_for_each_class_and_mode() {
        let created_at = "2026-11-02T08:00:00.123Z";
        for exemption in [
            PreviewExemption::ExemptChannel,
            PreviewExemption::BypassRole,
        ] {
            for dry_run in [true, false] {
                let verdict = PreviewVerdict {
                    result: PreviewResult::Exempt,
                    filter: None,
                    knobs: exemption_knobs(exemption),
                    exemption: Some(exemption),
                    dry_run,
                };
                let outcome = preview_audit_outcome(&verdict);
                for len in [50, 51, 501] {
                    let reason = audit_reason_for_verdict(&verdict, len);
                    assert_eq!(
                        replay_preview_reply(outcome, Some(&reason), created_at),
                        Some(render_preview_reply(&verdict, created_at)),
                        "{outcome}: {reason}"
                    );
                }
            }
        }
    }

    #[test]
    fn malformed_or_contradictory_exempt_rows_fail_closed() {
        let valid = Some("filter=none;exemption=bypass_role;mode=dry_run;len=0_50");
        for reason in [
            None,
            Some("filter=none;exemption=none;mode=dry_run;len=0_50"),
            Some("filter=none;exemption=admin;mode=dry_run;len=0_50"),
            Some("filter=bad_words;exemption=bypass_role;mode=dry_run;len=0_50"),
            Some("filter=none;exemption=exempt_channel;mode=live;len=0_50"),
            Some("filter=none;exemption=bypass_role;mode=dry_run;len=unexpected"),
            Some("filter=none;exemption=exempt_channel;mode=enforce"),
        ] {
            assert_eq!(
                replay_preview_reply("exempt", reason, "t"),
                None,
                "{reason:?}"
            );
        }
        assert_eq!(replay_preview_reply("exempt", valid, ""), None);
    }
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
            "filter=bad_words;exemption=none;mode=dry_run;len=unexpected",
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
    fn replay_preserves_opaque_stored_timestamp() {
        let verdict = matched_bad_words_verdict(true);
        let reason = audit_reason_for_verdict(&verdict, 12);
        let timestamp = "stored timestamp with microseconds 123456";
        assert_eq!(
            replay_preview_reply("matched", Some(&reason), timestamp),
            Some(render_preview_reply(&verdict, timestamp))
        );
        for dry_run in [true, false] {
            let verdicts = [
                PreviewVerdict {
                    result: PreviewResult::NoMatch,
                    filter: None,
                    knobs: &[],
                    exemption: None,
                    dry_run,
                },
                PreviewVerdict::partial(dry_run),
                PreviewVerdict {
                    result: PreviewResult::Exempt,
                    filter: None,
                    knobs: crate::automod_preview::exemption_knobs(PreviewExemption::BypassRole),
                    exemption: Some(PreviewExemption::BypassRole),
                    dry_run,
                },
            ];
            for verdict in verdicts {
                let reason = audit_reason_for_verdict(&verdict, 12);
                let replayed =
                    replay_preview_reply(preview_audit_outcome(&verdict), Some(&reason), timestamp)
                        .expect("stored verdict replays");
                assert_eq!(replayed, render_preview_reply(&verdict, timestamp));
                if verdict.exemption == Some(PreviewExemption::BypassRole) {
                    assert!(replayed.contains("TWO_AUTOMOD_BYPASS_ROLE_IDS"));
                }
            }
        }
    }

    #[test]
    fn replay_matches_a_live_evaluation_for_the_same_verdict() {
        use crate::automod::{AutomodConfig, AutomodMessage, AutomodPolicy, RepeatTracker};
        use crate::automod_preview::preview_automod;

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
    fn audit_record_carries_claim_fields() {
        let record = preview_audit_record(
            "guild-1",
            "actor-1",
            "channel-1",
            "matched",
            "filter=bad_words;exemption=none;mode=dry_run;len=0_50".to_owned(),
        );
        assert_eq!(record.action, "automod.preview");
        assert_eq!(record.guild_id, "guild-1");
        assert_eq!(record.actor_id.as_deref(), Some("actor-1"));
        assert_eq!(record.target_key.as_deref(), Some("channel-1"));
        assert_eq!(record.outcome, "matched");
        assert_eq!(
            record.reason.as_deref(),
            Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50")
        );
    }

    #[test]
    fn replay_never_consults_live_config_or_roles() {
        use crate::automod::{AutomodConfig, AutomodPolicy};

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
}
