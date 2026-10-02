//! Audit `flock` durability test, isolated in its own binary.
//!
//! `FileAudit::open` takes a non-blocking `flock`. Sibling tests in
//! `raid_removal.rs` spawn `raid-remove` via `std::process::Command`; a fork
//! that lands while the audit fd is open hands the child a duplicate of the
//! fd, so an immediate drop + reopen with `try_lock` can spuriously fail with
//! "audit lock unavailable" on a loaded runner. This binary spawns nothing, so
//! no sibling thread can fork while the audit fd is open. Do not add
//! CLI-spawning tests to this binary.

use two_bot_core::raid_removal::RemovalMode;
use two_bot_cutover::raid_tools::{FileAudit, RemovalOutcome, RemovalRecord};

const G: &str = "100000000000000010";
const A: &str = "100000000000000001";

#[test]
fn file_audit_is_durable_locked_and_terminal_ever_scoped_by_guild() {
    let root = std::env::var_os("PAPERCLIP_SCRATCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = root.join(format!(
        "raid-audit-test-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut audit = FileAudit::open(&path, G).unwrap();
    assert!(FileAudit::open(&path, G).is_err());
    let hard_alias = path.with_extension("hard-link");
    std::fs::hard_link(&path, &hard_alias).unwrap();
    assert!(FileAudit::open(&hard_alias, G).is_err());
    std::fs::remove_file(&hard_alias).unwrap();
    #[cfg(unix)]
    {
        let sym_alias = path.with_extension("symbolic-link");
        std::os::unix::fs::symlink(&path, &sym_alias).unwrap();
        assert!(FileAudit::open(&sym_alias, G).is_err());
        std::fs::remove_file(&sym_alias).unwrap();
    }
    let mut r = RemovalRecord {
        v: 1,
        ts: "2026-09-01T00:00:00Z".into(),
        run_id: "test".into(),
        guild_id: G.into(),
        member_id: A.into(),
        mode: RemovalMode::Execute,
        action: "kick".into(),
        outcome: RemovalOutcome::Kicked,
        status: Some(204),
        attempts: 1,
    };
    audit.append(&r).unwrap();
    r.mode = RemovalMode::DryRun;
    r.outcome = RemovalOutcome::WouldKick;
    r.status = None;
    r.attempts = 0;
    audit.append(&r).unwrap();
    drop(audit);
    let audit = FileAudit::open(&path, G).unwrap();
    assert!(audit.done.contains(A));
    drop(audit);
    let audit = FileAudit::open(&path, "100000000000000011").unwrap();
    assert!(audit.done.is_empty());
    drop(audit);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 2);
    std::fs::write(&path, text.trim_end_matches('\n')).unwrap();
    for _ in 0..2 {
        // Refusal must also release the lock so repaired evidence can resume.
        assert!(FileAudit::open(&path, G)
            .err()
            .unwrap()
            .contains("unterminated audit record"));
    }
    std::fs::write(&path, text).unwrap();
    let audit = FileAudit::open(&path, G).unwrap();
    assert!(audit.done.contains(A));
    drop(audit);
    std::fs::remove_file(path).unwrap();
}
