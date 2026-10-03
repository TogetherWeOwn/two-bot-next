//! Timer equivalents, pinned (TOG-9881).
//!
//! The parity matrix promises: nightly DB backup (daily 04:17),
//! guild-config backup (daily 04:31 UTC), monthly restore drill. A schedule
//! that silently changes is a backup that silently stops, so the unit files
//! are asserted here, not just installed. Mirrors legacy
//! `test/unit.guildconfigtimer.test.ts`.

use std::path::PathBuf;

fn deploy_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("deploy")
}

fn read(name: &str) -> String {
    std::fs::read_to_string(deploy_dir().join(name))
        .unwrap_or_else(|_| panic!("deploy/{name} must exist"))
}

#[test]
fn nightly_db_backup_runs_daily_at_04_17() {
    let timer = read("two-bot-next-backup.timer");
    assert!(timer.contains("OnCalendar=*-*-* 04:17:00"), "{timer}");
    assert!(timer.contains("Persistent=true"), "{timer}");
    let service = read("two-bot-next-backup.service");
    assert!(
        service.contains("ExecStart=/opt/two-bot-next/two-bot backup"),
        "{service}"
    );
    // The backup unit must not receive the Discord token.
    assert!(!service.contains("DISCORD"), "{service}");
    assert!(
        service.contains("Environment=TWO_BACKUP_KEEP=14"),
        "{service}"
    );
}

#[test]
fn guild_config_backup_runs_daily_at_04_31_utc() {
    let timer = read("two-bot-next-guild-config-backup.timer");
    assert!(timer.contains("OnCalendar=*-*-* 04:31:00 UTC"), "{timer}");
    assert!(
        timer.contains("Unit=two-bot-next-guild-config-backup.service"),
        "{timer}"
    );
    let service = read("two-bot-next-guild-config-backup.service");
    assert!(
        service.contains("ExecStart=/opt/two-bot-next/two-bot guild-config-snapshot"),
        "{service}"
    );
    assert!(
        service.contains(
            "LoadCredential=discord_staging_token:/etc/two-bot-next/credentials/discord_staging_token"
        ),
        "{service}"
    );
    assert!(
        service.contains("ReadWritePaths=/var/backups/two-bot-next/guild-config"),
        "{service}"
    );
}

#[test]
fn restore_drill_runs_monthly_after_the_nightly_backup() {
    let timer = read("two-bot-next-restore-drill.timer");
    // The 1st of each month, well after the 04:17 nightly backup.
    assert!(timer.contains("OnCalendar=*-*-01 05:30:00"), "{timer}");
    let service = read("two-bot-next-restore-drill.service");
    assert!(service.contains("two-bot restore-drill"), "{service}");
    assert!(service.contains("--confirm-scratch"), "{service}");
    assert!(!service.contains("--force"), "{service}");
    assert!(!service.contains("TWO_RESTORE_URL="), "{service}");
    assert!(
        service.contains("TWO_RESTORE_DRILL_BOOTSTRAP_URL"),
        "{service}"
    );
    assert!(
        service.contains("EnvironmentFile=/etc/two-bot-next/restore-drill.env"),
        "{service}"
    );
    assert!(
        service.contains("StateDirectory=two-bot-next-restore-drills"),
        "{service}"
    );
    for line in service.lines().filter(|l| l.starts_with("ExecStart=")) {
        assert!(!line.contains("TWO_DATABASE_URL"), "{line}");
    }
}
