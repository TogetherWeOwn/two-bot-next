//! Inactivity flagging domain: quiet-member sweep without outbound messages.
//!
//! Ports `flagInactive()` from `src/jobs/inactivity.ts` in legacy two-bot
//! (frozen `main` @ `d5d11793`) as framework-free data plus pure functions, in
//! the same style as `leveling.rs`/`moderation.rs`. No Discord client, no SQL
//! here: the caller supplies member rows, this module returns a plain-data
//! outcome, and the store records `member_inactive` events — so the sweep
//! unit-tests without Discord or Postgres.
//!
//! The read-only contract is structural: [`FlaggedMember`] carries no
//! channel, message, or DM field, so no future edit can route this outcome
//! into a send path without changing the type. Any outbound DM or ping needs
//! CEO sign-off first (legacy `docs/PRIVACY.md`); this module never DMs.
//!
//! Source files (legacy `two-bot`):
//! - `src/jobs/inactivity.ts` (`flagInactive`, `joinedNeverPosted`, hourly
//!   `setInterval` from `src/index.ts`)
//! - `src/core/config.ts` (`TWO_INACTIVITY_DAYS ?? 14`)
//! - `src/core/events.ts` (`idempotencyKey`, `member_inactive` projection)

use serde::Serialize;

use super::funnel::format_iso_millis;

/// Hourly sweep cadence (legacy `setInterval(..., 60 * 60 * 1000)` in
/// `src/index.ts`). A cheap query; no outbound messages.
pub const INACTIVITY_SWEEP_INTERVAL_MS: u64 = 60 * 60 * 1000;

/// Default quiet threshold (legacy `TWO_INACTIVITY_DAYS ?? 14`).
pub const DEFAULT_INACTIVITY_DAYS: u64 = 14;

/// Event source recorded on the sweep's rows (legacy `source: 'job:inactivity'`).
pub const INACTIVITY_EVENT_SOURCE: &str = "job:inactivity";
/// Funnel event type the sweep records (legacy `eventType: 'member_inactive'`).
pub const INACTIVITY_EVENT_TYPE: &str = "member_inactive";

/// Invalid inactivity-gate environment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InactivityGateError {
    #[error("TWO_INACTIVITY_DAYS must be a non-negative integer of days, got {0:?}")]
    InvalidDays(String),
}

/// Parse the quiet threshold (legacy `Number(src.get('TWO_INACTIVITY_DAYS') ?? 14)`).
pub fn parse_inactivity_days(raw: Option<&str>) -> Result<u64, InactivityGateError> {
    match raw {
        None => Ok(DEFAULT_INACTIVITY_DAYS),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .map_err(|_| InactivityGateError::InvalidDays(v.to_owned())),
    }
}

/// One member row as the sweep consumes it (legacy `members` projection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InactivityCandidate {
    pub guild_id: String,
    pub member_id: String,
    /// `COALESCE(last_active_at, joined_at)`: `None` when neither is known —
    /// SQL three-valued logic excludes such rows, and so do we.
    pub last_seen_ms: Option<i64>,
    /// `inactive_flagged_at`: `None` when never flagged.
    pub flagged_at_ms: Option<i64>,
    pub is_bot: bool,
    /// `left_at IS NOT NULL`.
    pub has_left: bool,
}

/// Quiet cutoff: members seen before `now - days` are candidates (legacy
/// `new Date(Date.now() - days * 86_400_000)`).
#[must_use]
pub fn inactivity_cutoff_ms(now_ms: i64, days: u64) -> i64 {
    let span = i64::try_from(days)
        .unwrap_or(i64::MAX)
        .saturating_mul(86_400_000);
    now_ms.saturating_sub(span)
}

/// Whether one member trips the sweep (legacy `flagInactive` WHERE clause):
/// still present, not a bot, last seen before the cutoff, and either never
/// flagged or last flagged before the cutoff (so a re-run never double-flags
/// the same quiet spell).
#[must_use]
pub fn should_flag(candidate: &InactivityCandidate, cutoff_ms: i64) -> bool {
    if candidate.is_bot || candidate.has_left {
        return false;
    }
    let Some(seen) = candidate.last_seen_ms else {
        return false;
    };
    if seen >= cutoff_ms {
        return false;
    }
    candidate.flagged_at_ms.is_none_or(|f| f < cutoff_ms)
}

/// Select the members to flag (legacy row query, minus the SQL).
pub fn select_inactive(
    candidates: &[InactivityCandidate],
    cutoff_ms: i64,
) -> Vec<&InactivityCandidate> {
    candidates
        .iter()
        .filter(|c| should_flag(c, cutoff_ms))
        .collect()
}

/// Idempotency key for the sweep's event (legacy `idempotencyKey()`:
/// `member_inactive` is repeatable, so the key carries member + time).
#[must_use]
pub fn member_inactive_event_key(guild_id: &str, member_id: &str, occurred_at: &str) -> String {
    format!("{guild_id}:{member_id}:{INACTIVITY_EVENT_TYPE}:{occurred_at}")
}

/// One flagged member. Deliberately message-free: there is no channel, body,
/// or DM field to render, so this outcome cannot feed a send path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlaggedMember {
    pub guild_id: String,
    pub member_id: String,
    pub occurred_at: String,
    pub threshold_days: u64,
}

/// Sweep outcome: the flagged members and nothing else. The store records one
/// `member_inactive` event per entry; nothing here messages anybody.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InactivityOutcome {
    pub flagged: Vec<FlaggedMember>,
}

/// One hourly sweep as a pure outcome (legacy `flagInactive` minus the DB):
/// select the quiet members and stamp them with `now`.
#[must_use]
pub fn flag_inactive(
    candidates: &[InactivityCandidate],
    now_ms: i64,
    days: u64,
) -> InactivityOutcome {
    let cutoff_ms = inactivity_cutoff_ms(now_ms, days);
    let occurred_at = format_iso_millis(now_ms);
    let flagged = select_inactive(candidates, cutoff_ms)
        .into_iter()
        .map(|c| FlaggedMember {
            guild_id: c.guild_id.clone(),
            member_id: c.member_id.clone(),
            occurred_at: occurred_at.clone(),
            threshold_days: days,
        })
        .collect();
    InactivityOutcome { flagged }
}

/// On-demand reengagement list (parity §4/§9: on-demand CLI only, never
/// scheduled): render the selector's flagged members row-for-row as CSV.
/// The header always prints; an empty outcome yields the header plus one
/// `#`-prefixed hint line the CSV parser skips, and the CLI exits 0.
///
/// The outcome carries no channel/message/DM field, so this renderer cannot
/// feed a send path (parity forbids DMs). IDs are validated as snowflakes
/// before emission, mirroring the `raid_tools::cohort_csv` gate: a bad ID is
/// a hard error, not a skipped row.
pub fn render_reengagement_csv(outcome: &InactivityOutcome) -> Result<String, &'static str> {
    let mut out = "guild_id,member_id,occurred_at,threshold_days\n".to_owned();
    if outcome.flagged.is_empty() {
        out.push_str("# no members past the inactivity cutoff; list is empty\n");
        return Ok(out);
    }
    for f in &outcome.flagged {
        if !is_snowflake_like(&f.guild_id) || !is_snowflake_like(&f.member_id) {
            return Err("reengagement CSV emits snowflake IDs only");
        }
        out.push_str(&format!(
            "{},{},{},{}\n",
            f.guild_id, f.member_id, f.occurred_at, f.threshold_days
        ));
    }
    Ok(out)
}

/// Canonical Discord snowflake shape (17–20 ASCII digits, no leading zero),
/// mirroring [`crate::raid_removal::validate_targets`] without its
/// non-emptiness/dedup list semantics.
#[must_use]
pub fn is_snowflake_like(id: &str) -> bool {
    (17..=20).contains(&id.len())
        && !id.starts_with('0')
        && id.bytes().all(|b| b.is_ascii_digit())
        && id.parse::<u64>().is_ok()
}

/// Parse the CSV [`render_reengagement_csv`] emits back into flagged rows
/// (round-trip acceptance: CSV parseable, row-for-row identical to the
/// selector). `#`-prefixed lines are skipped (the empty-state hint).
/// Returns an error for a bad header, a bad row, or a non-snowflake ID.
pub fn parse_reengagement_csv(text: &str) -> Result<Vec<FlaggedMember>, &'static str> {
    let mut lines = text.lines();
    match lines.next() {
        Some("guild_id,member_id,occurred_at,threshold_days") => {}
        _ => return Err("reengagement CSV has an unexpected header"),
    }
    let mut rows = Vec::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cells: Vec<_> = line.split(',').collect();
        if cells.len() != 4 {
            return Err("reengagement CSV row must have 4 cells");
        }
        let [guild_id, member_id, occurred_at, threshold_days] = cells[..] else {
            unreachable!();
        };
        if !is_snowflake_like(guild_id) || !is_snowflake_like(member_id) {
            return Err("reengagement CSV row has a non-snowflake ID");
        }
        if super::funnel::parse_iso_millis(occurred_at).is_none() {
            return Err("reengagement CSV row has an invalid timestamp");
        }
        let threshold_days: u64 = threshold_days
            .parse()
            .map_err(|_| "reengagement CSV row has an invalid threshold")?;
        rows.push(FlaggedMember {
            guild_id: guild_id.to_owned(),
            member_id: member_id.to_owned(),
            occurred_at: occurred_at.to_owned(),
            threshold_days,
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::funnel::parse_iso_millis;

    const DAY: i64 = 86_400_000;

    fn ms(iso: &str) -> i64 {
        parse_iso_millis(iso).expect("fixture timestamp parses")
    }

    fn candidate(
        member: &str,
        last_seen: Option<i64>,
        flagged: Option<i64>,
    ) -> InactivityCandidate {
        InactivityCandidate {
            guild_id: "guild-a".to_owned(),
            member_id: member.to_owned(),
            last_seen_ms: last_seen,
            flagged_at_ms: flagged,
            is_bot: false,
            has_left: false,
        }
    }

    #[test]
    fn cadence_and_default_threshold_match_legacy() {
        assert_eq!(INACTIVITY_SWEEP_INTERVAL_MS, 3_600_000);
        assert_eq!(DEFAULT_INACTIVITY_DAYS, 14);
        assert_eq!(parse_inactivity_days(None), Ok(14));
        assert_eq!(parse_inactivity_days(Some("30")), Ok(30));
        assert!(parse_inactivity_days(Some("soon")).is_err());
    }

    #[test]
    fn cutoff_is_now_minus_days() {
        let now = ms("2026-09-07T06:15:00.000Z");
        assert_eq!(
            inactivity_cutoff_ms(now, 14),
            ms("2026-08-24T06:15:00.000Z")
        );
    }

    #[test]
    fn cutoff_before_epoch_goes_negative_without_panic() {
        // `now_ms = 0` is before any real activity: the cutoff is just
        // `-days` in millis, negative without any saturation or panic.
        assert_eq!(inactivity_cutoff_ms(0, 14), -14 * DAY);
        assert_eq!(inactivity_cutoff_ms(0, 14), -1_209_600_000);
        // Zero days is the identity: cutoff == now, even at the epoch.
        assert_eq!(inactivity_cutoff_ms(0, 0), 0);
    }

    #[test]
    fn cutoff_saturates_instead_of_wrapping_at_extreme_days() {
        // `i64::MAX / 86_400_000 = 106_751_991_167`: the largest whole-day
        // span that still fits in an `i64` of millis. One more day overflows
        // the span, so `saturating_mul` pins it to `i64::MAX` instead of
        // wrapping (a wrapping refactor would flip the sweep to flag the
        // active or flag nobody at all).
        const MAX_EXACT_DAYS: u64 = 106_751_991_167;
        let now = ms("2026-09-07T00:00:00.000Z");
        let exact_span = (MAX_EXACT_DAYS as i64) * DAY;
        assert_eq!(exact_span, 9_223_372_036_828_800_000);
        assert_eq!(inactivity_cutoff_ms(now, MAX_EXACT_DAYS), now - exact_span);
        // One day past the boundary saturates the span to `i64::MAX` ...
        assert_eq!(
            inactivity_cutoff_ms(now, MAX_EXACT_DAYS + 1),
            now.saturating_sub(i64::MAX)
        );
        // ... and so does any larger value, including `u64::MAX` (which never
        // fits in an `i64` at all). The cutoff stays deeply negative — below
        // any real activity — instead of wrapping positive.
        let saturated = inactivity_cutoff_ms(now, u64::MAX);
        assert_eq!(saturated, now.saturating_sub(i64::MAX));
        assert!(
            saturated < 0,
            "saturated cutoff {saturated} stays below real activity"
        );
        // The floor is `i64::MIN`-bounded: a saturating subtraction can never
        // go below `i64::MIN`, wherever `now` sits.
        assert_eq!(inactivity_cutoff_ms(0, u64::MAX), i64::MIN + 1);
        assert_eq!(inactivity_cutoff_ms(i64::MIN, u64::MAX), i64::MIN);
    }

    #[test]
    fn sweep_inclusion_stays_strict_at_saturated_boundary() {
        // At `days = u64::MAX` the cutoff saturates to `i64::MIN + 1`: the
        // only representable older instant is `i64::MIN` itself. Inclusion
        // stays strict-`<` there exactly as on the happy path — a refactor
        // that flips the comparison (or the saturation) flags the wrong set.
        let cutoff = inactivity_cutoff_ms(0, u64::MAX);
        assert_eq!(cutoff, i64::MIN + 1);
        assert!(
            !should_flag(&candidate("edge", Some(cutoff), None), cutoff),
            "seen exactly at the saturated cutoff is not flagged"
        );
        assert!(
            should_flag(&candidate("older", Some(i64::MIN), None), cutoff),
            "the one instant below the saturated cutoff still flags"
        );
        assert!(
            !should_flag(&candidate("new", Some(0), None), cutoff),
            "anything newer than the saturated cutoff never flags"
        );
    }

    #[test]
    fn quiet_members_flagged_active_ones_not() {
        let now = ms("2026-09-07T00:00:00.000Z");
        let cutoff = inactivity_cutoff_ms(now, 14);
        // Seen 15 days ago: flag.
        assert!(should_flag(
            &candidate("quiet", Some(now - 15 * DAY), None),
            cutoff
        ));
        // Seen exactly at the cutoff: `<` is strict, no flag.
        assert!(!should_flag(&candidate("edge", Some(cutoff), None), cutoff));
        // Seen yesterday: no flag.
        assert!(!should_flag(
            &candidate("active", Some(now - DAY), None),
            cutoff
        ));
        // Never seen at all (neither activity nor join): SQL NULL logic
        // excludes the row, and so do we.
        assert!(!should_flag(&candidate("ghost", None, None), cutoff));
    }

    #[test]
    fn bots_departed_and_reruns_never_flag() {
        let now = ms("2026-09-07T00:00:00.000Z");
        let cutoff = inactivity_cutoff_ms(now, 14);
        let mut bot = candidate("bot", Some(now - 30 * DAY), None);
        bot.is_bot = true;
        assert!(!should_flag(&bot, cutoff));
        let mut left = candidate("left", Some(now - 30 * DAY), None);
        left.has_left = true;
        assert!(!should_flag(&left, cutoff));
        // Flagged after the cutoff: the same quiet spell, no double-flag.
        assert!(!should_flag(
            &candidate("again", Some(now - 30 * DAY), Some(now - DAY)),
            cutoff
        ));
        // Flagged before the cutoff: a new quiet spell, flag again.
        assert!(should_flag(
            &candidate("spell", Some(now - 30 * DAY), Some(cutoff - 1)),
            cutoff
        ));
    }

    #[test]
    fn sweep_returns_flagged_members_with_sweep_time() {
        let now = ms("2026-09-07T00:00:00.000Z");
        let rows = vec![
            candidate("quiet-1", Some(now - 20 * DAY), None),
            candidate("active-1", Some(now - DAY), None),
            candidate("quiet-2", Some(now - 60 * DAY), Some(now - 30 * DAY)),
        ];
        let outcome = flag_inactive(&rows, now, 14);
        let ids: Vec<_> = outcome
            .flagged
            .iter()
            .map(|f| f.member_id.as_str())
            .collect();
        assert_eq!(ids, ["quiet-1", "quiet-2"]);
        assert!(outcome
            .flagged
            .iter()
            .all(|f| f.occurred_at == "2026-09-07T00:00:00.000Z" && f.threshold_days == 14));
    }

    #[test]
    fn outcome_carries_no_notification_surface() {
        // Read-only by construction: the serialized outcome has exactly the
        // record fields, so no DM/channel/message key can creep in.
        let flagged = FlaggedMember {
            guild_id: "g".to_owned(),
            member_id: "m".to_owned(),
            occurred_at: "2026-09-07T00:00:00.000Z".to_owned(),
            threshold_days: 14,
        };
        let json = serde_json::to_value(&flagged).expect("serializes");
        let keys: Vec<_> = json.as_object().expect("object").keys().cloned().collect();
        assert_eq!(
            keys,
            ["guild_id", "member_id", "occurred_at", "threshold_days"]
        );
    }

    #[test]
    fn event_key_matches_legacy_repeatable_shape() {
        assert_eq!(
            member_inactive_event_key("g", "m", "2026-09-07T00:00:00.000Z"),
            "g:m:member_inactive:2026-09-07T00:00:00.000Z"
        );
    }

    fn snowflake_candidate(member: &str, last_seen: i64) -> InactivityCandidate {
        InactivityCandidate {
            guild_id: "100000000000000010".to_owned(),
            member_id: member.to_owned(),
            last_seen_ms: Some(last_seen),
            flagged_at_ms: None,
            is_bot: false,
            has_left: false,
        }
    }

    #[test]
    fn reengagement_csv_matches_selector_row_for_row() {
        // The CSV renders exactly the selector's flagged set, in order: the
        // acceptance row-for-row check.
        let now = ms("2026-09-07T00:00:00.000Z");
        let rows = vec![
            snowflake_candidate("100000000000000001", now - 20 * DAY),
            snowflake_candidate("100000000000000002", now - DAY),
            snowflake_candidate("100000000000000003", now - 60 * DAY),
        ];
        let outcome = flag_inactive(&rows, now, 14);
        let expected: Vec<String> = select_inactive(&rows, inactivity_cutoff_ms(now, 14))
            .into_iter()
            .map(|c| c.member_id.clone())
            .collect();
        assert_eq!(expected, ["100000000000000001", "100000000000000003"]);
        let csv = render_reengagement_csv(&outcome).unwrap();
        let lines: Vec<_> = csv.lines().collect();
        assert_eq!(lines[0], "guild_id,member_id,occurred_at,threshold_days");
        let body: Vec<_> = lines[1..].to_vec();
        assert_eq!(body.len(), expected.len());
        for (line, id) in body.iter().zip(&expected) {
            let cells: Vec<_> = line.split(',').collect();
            assert_eq!(
                cells,
                [
                    "100000000000000010",
                    id.as_str(),
                    "2026-09-07T00:00:00.000Z",
                    "14"
                ]
            );
        }
        // Round-trip: the CSV parses back to the selector's rows exactly.
        assert_eq!(parse_reengagement_csv(&csv).unwrap(), outcome.flagged);
    }

    #[test]
    fn reengagement_csv_empty_state_hint_parses_to_nothing() {
        let outcome = InactivityOutcome { flagged: vec![] };
        let csv = render_reengagement_csv(&outcome).unwrap();
        let lines: Vec<_> = csv.lines().collect();
        assert_eq!(lines[0], "guild_id,member_id,occurred_at,threshold_days");
        assert_eq!(lines.len(), 2, "header plus one hint line");
        assert!(lines[1].starts_with('#'), "hint is a CSV comment");
        assert!(parse_reengagement_csv(&csv).unwrap().is_empty());
    }

    #[test]
    fn reengagement_csv_rejects_bad_header_rows_and_ids() {
        assert!(parse_reengagement_csv("guild_id,member_id\n").is_err());
        assert!(
            parse_reengagement_csv("guild_id,member_id,occurred_at,threshold_days\n1,2,3\n")
                .is_err()
        );
        assert!(parse_reengagement_csv(
            "guild_id,member_id,occurred_at,threshold_days\nnot-an-id,100000000000000001,2026-09-07T00:00:00.000Z,14\n"
        )
        .is_err());
        assert!(parse_reengagement_csv(
            "guild_id,member_id,occurred_at,threshold_days\n100000000000000010,100000000000000001,not-a-time,14\n"
        )
        .is_err());
        assert!(!is_snowflake_like("guild-a"));
        assert!(is_snowflake_like("100000000000000010"));
        // Render fails closed on a non-snowflake ID instead of emitting it.
        let bad = InactivityOutcome {
            flagged: vec![FlaggedMember {
                guild_id: "guild-a".to_owned(),
                member_id: "m".to_owned(),
                occurred_at: "2026-09-07T00:00:00.000Z".to_owned(),
                threshold_days: 14,
            }],
        };
        assert!(render_reengagement_csv(&bad).is_err());
    }
}
