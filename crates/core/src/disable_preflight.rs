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

/// Releases still owed while a disable is in effect. Ids are request,
/// channel, or schedule ids only — never reasons, bodies, or user content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwedReleases {
    /// `moderation_scheduled_unbans.request_id` in non-terminal states.
    pub unbans: Vec<String>,
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

    fn section(label: &str, ids: &[String], out: &mut Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let mut named = ids.iter().take(MAX_NAMED_IDS).cloned().collect::<Vec<_>>();
        if ids.len() > MAX_NAMED_IDS {
            named.push(format!("+{} more", ids.len() - MAX_NAMED_IDS));
        }
        out.push(format!("{} {}: {}", ids.len(), label, named.join(", ")));
    }

    /// Single-line refusal detail: exact counts plus outstanding ids.
    /// Never echoes caller-supplied values beyond the stored ids.
    #[must_use]
    pub fn report(&self) -> String {
        let mut sections = Vec::new();
        Self::section("pending unban(s)", &self.unbans, &mut sections);
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
        owed.unbans = pending_unbans(pool).await?;
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

/// `request_id`s of unban schedules that still owe a release, ordered.
/// Terminal states (`cancelled`, `done`, `superseded`) owe nobody anything;
/// every other state — `staged`, `pending`, `running`, `quarantined`, or
/// anything a future migration adds — still owes the member their release.
/// A missing table reads as empty (see module docs).
#[cfg(feature = "db")]
async fn pending_unbans(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String,)> = match sqlx::query_as(
        "SELECT request_id FROM moderation_scheduled_unbans
          WHERE state NOT IN ('cancelled', 'done', 'superseded') ORDER BY request_id",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) if is_missing_table(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(rows.into_iter().map(|(id,)| id).collect())
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
            scheduled: vec![],
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
            lockdowns: vec![],
            scheduled: vec![],
        };
        let report = owed.report();
        assert!(report.contains("13 pending unban(s): "));
        assert!(report.contains("+3 more"));
        assert!(!report.contains("req-12"));
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
