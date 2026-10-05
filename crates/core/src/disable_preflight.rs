//! Refuse to run with moderation/automation disabled while releases are owed.
//!
//! Ports legacy two-bot `src/moderation/shutdownPreflight.ts`,
//! `scripts/moderation-disable-preflight.ts` and `src/automations/disable.ts`:
//! boot hard-refuses when moderation is disabled but Postgres still holds
//! pending tempban unbans or active lockdowns (counts + ids, state read from
//! the database, never Discord), and the automations side does the same for
//! enabled scheduled messages. Without this guard members stay banned and
//! channels stay locked silently after a disable or cutover.
//!
//! This module is pure policy plus three read-only queries: it never writes,
//! never migrates, and never contacts Discord. Missing tables are read as
//! empty — a database that never ran a slice's migrations cannot hold that
//! slice's owed releases — while any other database failure is reported as
//! unknown, never as clear.
//!
//! Outcomes mirror the legacy script: `CLEAR` (exit 0), `REFUSED` (exit 1,
//! names outstanding ids), `UNKNOWN` (exit 2, state could not be read). An
//! explicit override (`--allow-owed` or `TWO_ALLOW_OWED_RELEASES=1`) proceeds
//! and must be logged loudly by the caller; it never skips the read.
//!
//! Out of scope: changing moderation behaviour. This only observes.

use std::collections::HashMap;

#[cfg(feature = "db")]
use sqlx::PgPool;

/// Env override for the disable guard. Forwarded into the Container (see
/// `wrangler/src/container-env.ts`): an operator escape hatch, unset by
/// default so the refusal is the default.
pub const OVERRIDE_ENV: &str = "TWO_ALLOW_OWED_RELEASES";

/// CLI override flag, accepted by both `two-bot moderation preflight` and the
/// bare boot command. Never a `TWO_*` name, so it stays out of the
/// Worker-forwarded flag allowlist by construction.
pub const OVERRIDE_FLAG: &str = "--allow-owed";

/// How many outstanding ids each section of the refusal report names.
/// Counts are always exact; only the listed ids truncate.
pub const MAX_NAMED_IDS: usize = 10;

/// Where the operator recovery steps for a `running` unban claim live. Named
/// in the refusal report so the boot log and the CLI point at the same place.
pub const STRANDED_RUNNING_DOC: &str =
    "docs/moderation-disable-preflight.md#stranded-running-unban-claims";

/// Releases still owed while a disable is in effect. Ids are request,
/// channel, or schedule ids only — never reasons, bodies, or user content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwedReleases {
    /// `moderation_scheduled_unbans.request_id` in non-terminal states,
    /// `running` claims included.
    pub unbans: Vec<String>,
    /// The subset of [`Self::unbans`] whose state is `running`: a worker
    /// dispatched the Discord DELETE and never recorded the outcome. Nothing
    /// reclaims these by age, so they never drain on their own. Always a
    /// subset of `unbans`; ids absent from `unbans` are not reported.
    pub running_unbans: Vec<String>,
    /// `moderation_lockdowns.channel_id` rows still holding recovery state.
    pub lockdowns: Vec<String>,
    /// Enabled `scheduled_messages.id` rows that would stop firing.
    pub scheduled: Vec<String>,
}

impl OwedReleases {
    /// True when nothing is owed: disabling strands nobody.
    #[must_use]
    pub fn is_clear(&self) -> bool {
        self.unbans.is_empty() && self.lockdowns.is_empty() && self.scheduled.is_empty()
    }

    /// Total owed releases across all three sections.
    #[must_use]
    pub fn total(&self) -> usize {
        self.unbans.len() + self.lockdowns.len() + self.scheduled.len()
    }

    /// `MAX_NAMED_IDS` ids, then `+N more` only when ids were actually cut.
    fn named_ids(ids: &[String]) -> String {
        let mut named = ids.iter().take(MAX_NAMED_IDS).cloned().collect::<Vec<_>>();
        if ids.len() > MAX_NAMED_IDS {
            named.push(format!("+{} more", ids.len() - MAX_NAMED_IDS));
        }
        named.join(", ")
    }

    fn section(label: &str, ids: &[String], out: &mut Vec<String>) {
        if ids.is_empty() {
            return;
        }
        out.push(format!("{} {}: {}", ids.len(), label, Self::named_ids(ids)));
    }

    /// Owed unbans that are not `running`: staged, pending or quarantined.
    fn unbans_not_running(&self) -> Vec<String> {
        self.unbans
            .iter()
            .filter(|id| !self.running_unbans.contains(id))
            .cloned()
            .collect()
    }

    /// `running` unbans named in [`Self::unbans`], in the same order.
    fn running_named(&self) -> Vec<String> {
        self.unbans
            .iter()
            .filter(|id| self.running_unbans.contains(id))
            .cloned()
            .collect()
    }

    /// Single-line refusal detail: exact counts plus outstanding ids.
    /// `running` unbans get their own tagged section so a stranded claim is
    /// never hidden behind `+N more` of the pending list. Never echoes
    /// caller-supplied values beyond the stored ids; never names a claim
    /// token, which fences the close.
    #[must_use]
    pub fn report(&self) -> String {
        let mut sections = Vec::new();
        Self::section(
            "pending unban(s)",
            &self.unbans_not_running(),
            &mut sections,
        );
        let running = self.running_named();
        if !running.is_empty() {
            sections.push(format!(
                "{} running unban claim(s) [running]: {} (a claim left by a stopped worker \
                 never drains on its own, see {STRANDED_RUNNING_DOC})",
                running.len(),
                Self::named_ids(&running),
            ));
        }
        Self::section("active lockdown(s)", &self.lockdowns, &mut sections);
        Self::section(
            "enabled scheduled message(s)",
            &self.scheduled,
            &mut sections,
        );
        if sections.is_empty() {
            return "CLEAR: no owed releases".to_owned();
        }
        format!("REFUSED: {}", sections.join("; "))
    }
}

/// Which disable guards apply at boot. Raw flag reads only: a malformed
/// companion value (feed poll, Owen id) must not change disable semantics;
/// the typed loaders still reject those values on their own paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisableGates {
    /// `TWO_MODERATION=1` — moderation commands are served.
    pub moderation: bool,
    /// `TWO_AUTOMATIONS=1` — automation commands are served.
    pub automations: bool,
}

impl DisableGates {
    /// Read both gates from an explicit map (tests, staged config).
    #[must_use]
    pub fn from_map(vars: &HashMap<String, String>) -> Self {
        let flag = |key: &str| vars.get(key).is_some_and(|v| v == "1");
        Self {
            moderation: flag("TWO_MODERATION"),
            automations: flag("TWO_AUTOMATIONS"),
        }
    }

    /// Read both gates from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_map(&std::env::vars().collect())
    }
}

/// The explicit escape hatch is active when the env override is exactly `1`
/// or the CLI flag is present. Both are checked; either suffices.
#[must_use]
pub fn override_active(vars: &HashMap<String, String>, args: &[String]) -> bool {
    vars.get(OVERRIDE_ENV).is_some_and(|v| v == "1") || args.iter().any(|arg| arg == OVERRIDE_FLAG)
}

/// Boot decision after the disable guard runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootVerdict {
    /// All applicable gates are enabled, or every disabled slice is clear.
    Proceed,
    /// A disabled slice still owes releases; boot must stop.
    Refused(OwedReleases),
    /// Owed releases exist but the operator overrode explicitly. The caller
    /// must log this loudly; the owed set rides along for that log line.
    Overridden(OwedReleases),
}

/// Read the owed releases for the disabled slices only. Enabled gates
/// short-circuit before any database read, so `TWO_MODERATION=1` (or
/// `TWO_AUTOMATIONS=1`) behaviour is unchanged by this guard.
#[cfg(feature = "db")]
pub async fn outstanding_for_gates(
    pool: &PgPool,
    gates: &DisableGates,
) -> Result<OwedReleases, sqlx::Error> {
    let mut owed = OwedReleases::default();
    if !gates.moderation {
        (owed.unbans, owed.running_unbans) = pending_unbans(pool).await?;
        owed.lockdowns = active_lockdowns(pool).await?;
    }
    if !gates.automations {
        owed.scheduled = enabled_scheduled(pool).await?;
    }
    Ok(owed)
}

/// Read every section regardless of gates (the `moderation preflight`
/// subcommand reports the full picture).
#[cfg(feature = "db")]
pub async fn outstanding(pool: &PgPool) -> Result<OwedReleases, sqlx::Error> {
    outstanding_for_gates(
        pool,
        &DisableGates {
            moderation: false,
            automations: false,
        },
    )
    .await
}

/// Apply the guard at boot: `Proceed` when nothing relevant is owed,
/// otherwise `Refused` — or `Overridden` when the operator said so loudly.
#[cfg(feature = "db")]
pub async fn boot_check(
    pool: &PgPool,
    gates: &DisableGates,
    override_requested: bool,
) -> Result<BootVerdict, sqlx::Error> {
    if gates.moderation && gates.automations {
        return Ok(BootVerdict::Proceed);
    }
    let owed = outstanding_for_gates(pool, gates).await?;
    if owed.is_clear() {
        return Ok(BootVerdict::Proceed);
    }
    if override_requested {
        return Ok(BootVerdict::Overridden(owed));
    }
    Ok(BootVerdict::Refused(owed))
}

/// `request_id`s of unban schedules that still owe a release, ordered, plus
/// the subset in `running`.
/// Terminal states (`cancelled`, `done`, `superseded`) owe nobody anything;
/// every other state — `staged`, `pending`, `running`, `quarantined`, or
/// anything a future migration adds — still owes the member their release.
/// A missing table reads as empty (see module docs).
#[cfg(feature = "db")]
async fn pending_unbans(pool: &PgPool) -> Result<(Vec<String>, Vec<String>), sqlx::Error> {
    let rows: Vec<(String, String)> = match sqlx::query_as(
        "SELECT request_id, state::text FROM moderation_scheduled_unbans
          WHERE state NOT IN ('cancelled', 'done', 'superseded') ORDER BY request_id",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) if is_missing_table(&error) => return Ok((Vec::new(), Vec::new())),
        Err(error) => return Err(error),
    };
    let running = rows
        .iter()
        .filter(|(_, state)| state == "running")
        .map(|(id, _)| id.clone())
        .collect();
    Ok((rows.into_iter().map(|(id, _)| id).collect(), running))
}

/// `channel_id`s still holding lockdown recovery state, ordered.
#[cfg(feature = "db")]
async fn active_lockdowns(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String,)> =
        match sqlx::query_as("SELECT channel_id FROM moderation_lockdowns ORDER BY channel_id")
            .fetch_all(pool)
            .await
        {
            Ok(rows) => rows,
            Err(error) if is_missing_table(&error) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Enabled scheduled-message ids, ordered. The table postdates the
/// moderation slices, so a database without it simply owes nothing here.
#[cfg(feature = "db")]
async fn enabled_scheduled(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String,)> =
        match sqlx::query_as("SELECT id FROM scheduled_messages WHERE enabled ORDER BY id")
            .fetch_all(pool)
            .await
        {
            Ok(rows) => rows,
            Err(error) if is_missing_table(&error) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Postgres `undefined_table` (SQLSTATE 42P01): the slice's migrations never
/// ran on this database, so no rows of that shape can exist.
#[cfg(feature = "db")]
fn is_missing_table(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.code().map(|code| code.into_owned()).as_deref() == Some("42P01"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn clear_report_is_not_a_refusal() {
        let owed = OwedReleases::default();
        assert!(owed.is_clear());
        assert_eq!(owed.total(), 0);
        assert_eq!(owed.report(), "CLEAR: no owed releases");
    }

    #[test]
    fn report_names_counts_and_ids_per_section() {
        let owed = OwedReleases {
            unbans: vec!["req-b".to_owned(), "req-a".to_owned()],
            lockdowns: vec!["chan-1".to_owned()],
            ..OwedReleases::default()
        };
        assert!(!owed.is_clear());
        assert_eq!(owed.total(), 3);
        let report = owed.report();
        assert!(report.starts_with("REFUSED: "));
        assert!(report.contains("2 pending unban(s): req-b, req-a"));
        assert!(report.contains("1 active lockdown(s): chan-1"));
        assert!(!report.contains("scheduled"));
    }

    #[test]
    fn report_truncates_ids_but_keeps_exact_counts() {
        let owed = OwedReleases {
            unbans: (0..13).map(|n| format!("req-{n:02}")).collect(),
            ..OwedReleases::default()
        };
        let report = owed.report();
        assert!(report.contains("13 pending unban(s): "));
        assert!(report.contains("+3 more"));
        assert!(!report.contains("req-12"));
    }

    /// `count` ids named `{prefix}-00`, `{prefix}-01`, ...
    fn ids(prefix: &str, count: usize) -> Vec<String> {
        (0..count).map(|n| format!("{prefix}-{n:02}")).collect()
    }

    /// One `OwedReleases` per section sharing the truncation helper.
    fn owed_with(section: &str, count: usize) -> OwedReleases {
        let mut owed = OwedReleases::default();
        match section {
            "pending unban(s)" => owed.unbans = ids("id", count),
            "running unban claim(s) [running]" => {
                owed.unbans = ids("id", count);
                owed.running_unbans = ids("id", count);
            }
            "active lockdown(s)" => owed.lockdowns = ids("id", count),
            "enabled scheduled message(s)" => owed.scheduled = ids("id", count),
            other => panic!("unknown section {other}"),
        }
        owed
    }

    const TRUNCATING_SECTIONS: [&str; 4] = [
        "pending unban(s)",
        "running unban claim(s) [running]",
        "active lockdown(s)",
        "enabled scheduled message(s)",
    ];

    #[test]
    fn exactly_the_limit_names_every_id_and_reports_no_truncation() {
        for section in TRUNCATING_SECTIONS {
            let report = owed_with(section, MAX_NAMED_IDS).report();
            assert!(
                report.contains(&format!("{MAX_NAMED_IDS} {section}: id-00, id-01")),
                "{section}: exact count and first ids: {report}"
            );
            assert!(
                report.contains("id-09"),
                "{section}: the last id at the limit is named: {report}"
            );
            assert!(
                !report.contains("more"),
                "{section}: nothing was cut at exactly the limit: {report}"
            );
        }
    }

    #[test]
    fn one_past_the_limit_cuts_one_id_and_keeps_the_exact_count() {
        for section in TRUNCATING_SECTIONS {
            let report = owed_with(section, MAX_NAMED_IDS + 1).report();
            assert!(
                report.contains(&format!("{} {section}: id-00", MAX_NAMED_IDS + 1)),
                "{section}: exact count 11: {report}"
            );
            assert!(
                report.contains("id-09, +1 more"),
                "{section}: first {MAX_NAMED_IDS} named then +1 more: {report}"
            );
            assert!(
                !report.contains("id-10"),
                "{section}: the eleventh id is the one cut: {report}"
            );
        }
    }

    #[test]
    fn running_claims_get_their_own_tagged_section_with_the_recovery_pointer() {
        let owed = OwedReleases {
            unbans: vec![
                "req-pending".to_owned(),
                "req-running".to_owned(),
                "req-staged".to_owned(),
            ],
            running_unbans: vec!["req-running".to_owned()],
            ..OwedReleases::default()
        };
        assert_eq!(owed.total(), 3);
        let report = owed.report();
        assert!(
            report.contains("2 pending unban(s): req-pending, req-staged"),
            "running id leaves the pending section: {report}"
        );
        assert!(
            report.contains("1 running unban claim(s) [running]: req-running"),
            "{report}"
        );
        assert!(report.contains("never drains on its own"), "{report}");
        assert!(report.contains(STRANDED_RUNNING_DOC), "{report}");
        // Exactly one mention per id: the split never double-reports a claim.
        assert_eq!(report.matches("req-running").count(), 1, "{report}");
    }

    #[test]
    fn a_stranded_claim_is_never_hidden_behind_the_pending_truncation() {
        let mut unbans = ids("req", MAX_NAMED_IDS + 3);
        unbans.push("req-zz-running".to_owned());
        let owed = OwedReleases {
            unbans,
            running_unbans: vec!["req-zz-running".to_owned()],
            ..OwedReleases::default()
        };
        let report = owed.report();
        assert!(report.contains("13 pending unban(s)"), "{report}");
        assert!(report.contains("+3 more"), "{report}");
        assert!(
            report.contains("1 running unban claim(s) [running]: req-zz-running"),
            "the running claim sorts past the cut yet is still named: {report}"
        );
    }

    #[test]
    fn only_running_unbans_refuse_without_a_pending_section() {
        let owed = OwedReleases {
            unbans: vec!["req-running".to_owned()],
            running_unbans: vec!["req-running".to_owned()],
            ..OwedReleases::default()
        };
        assert!(!owed.is_clear());
        let report = owed.report();
        assert!(report.starts_with("REFUSED: 1 running unban claim(s) [running]"));
        assert!(!report.contains("pending unban(s)"), "{report}");
    }

    #[test]
    fn pending_only_refusal_carries_no_running_tag_or_recovery_pointer() {
        let owed = OwedReleases {
            unbans: vec!["req-pending".to_owned(), "req-staged".to_owned()],
            lockdowns: vec!["chan-1".to_owned()],
            ..OwedReleases::default()
        };
        let report = owed.report();
        assert!(report.contains("2 pending unban(s): req-pending, req-staged"));
        for absent in ["running", "[running]", STRANDED_RUNNING_DOC, "drain"] {
            assert!(!report.contains(absent), "{absent:?} in {report}");
        }
    }

    #[test]
    fn gates_default_off_and_read_exact_one() {
        assert_eq!(
            DisableGates::from_map(&vars(&[])),
            DisableGates {
                moderation: false,
                automations: false,
            }
        );
        assert_eq!(
            DisableGates::from_map(&vars(&[("TWO_MODERATION", "1"), ("TWO_AUTOMATIONS", "1")])),
            DisableGates {
                moderation: true,
                automations: true,
            }
        );
        // Anything but exactly "1" is off; a malformed companion never flips it.
        assert!(!DisableGates::from_map(&vars(&[("TWO_MODERATION", "true")])).moderation);
        assert!(!DisableGates::from_map(&vars(&[("TWO_MODERATION", "")])).moderation);
    }

    #[test]
    fn override_needs_exact_env_or_flag() {
        let flag = |s: &str| vec![s.to_owned()];
        assert!(override_active(&vars(&[(OVERRIDE_ENV, "1")]), &[]));
        assert!(override_active(&vars(&[]), &flag(OVERRIDE_FLAG)));
        assert!(!override_active(&vars(&[]), &[]));
        assert!(!override_active(&vars(&[(OVERRIDE_ENV, "true")]), &[]));
        assert!(!override_active(&vars(&[]), &flag("--allow-owed-releases")));
    }
}
