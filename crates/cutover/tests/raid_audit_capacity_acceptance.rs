//! Offline acceptance for capacity preflight and resumed audit byte accounting.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use two_bot_core::raid_removal::RemovalMode;
use two_bot_cutover::raid_tools::{FileAudit, RemovalOutcome, RemovalRecord};

const CAP: usize = 8 * 1024 * 1024;
const HEADROOM: usize = 1000;
const GUILD: &str = "100000000000037010";
const MEMBERS: [&str; 6] = [
    "100000000000037001",
    "100000000000037002",
    "100000000000037003",
    "100000000000037004",
    "100000000000037005",
    "100000000000037006",
];

// Serialize CLI forks with the lifecycle test's flock: an inherited audit fd
// must not keep the lock alive after that test drops its FileAudit.
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    root: PathBuf,
    audit: PathBuf,
}

impl Fixture {
    fn new(label: &str, records: &[RemovalRecord]) -> Self {
        let base = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let root = base.join(format!(
            "raid-capacity-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let audit = root.join("audit.jsonl");
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend(serde_json::to_vec(record).unwrap());
            bytes.push(b'\n');
        }
        assert!(bytes.len() < CAP - HEADROOM);
        bytes.resize(CAP - HEADROOM, b'\n');
        fs::write(&audit, bytes).unwrap();
        assert_eq!(fs::metadata(&audit).unwrap().len(), (CAP - HEADROOM) as u64);
        Self { root, audit }
    }

    fn execute_without_token(&self, ids: &[&str]) -> Output {
        fs::write(self.root.join("ids.txt"), ids.join("\n")).unwrap();
        Command::new(env!("CARGO_BIN_EXE_raid-remove"))
            .current_dir(&self.root)
            .args([
                "--guild",
                GUILD,
                "--ids-from",
                "ids.txt",
                "--audit",
                "audit.jsonl",
                "--execute",
                "--expect",
                &ids.len().to_string(),
                "--reason",
                "offline capacity acceptance",
            ])
            .env_remove("DISCORD_TOKEN")
            .output()
            .unwrap()
    }

    fn finish(self) {
        // Only remove this test's unique fixture, and retain it on any panic.
        fs::remove_dir_all(self.root).unwrap();
    }
}

fn completed(member: &str, outcome: RemovalOutcome) -> RemovalRecord {
    RemovalRecord {
        v: 1,
        ts: "2026-10-21T00:00:00Z".into(),
        run_id: "offline-capacity-acceptance".into(),
        guild_id: GUILD.into(),
        member_id: member.into(),
        mode: RemovalMode::Execute,
        action: "kick".into(),
        status: Some(match outcome {
            RemovalOutcome::AlreadyGone => 404,
            _ => 204,
        }),
        outcome,
        attempts: 1,
    }
}

fn assert_capacity_refusal(error: &str) {
    assert!(error.contains("8 MiB"), "missing cap diagnostic: {error}");
    assert!(
        error.contains("split the cohort"),
        "missing capacity remedy: {error}"
    );
}

#[test]
fn insufficient_pending_capacity_refuses_before_token_access_without_writing() {
    let _guard = FIXTURE_LOCK.lock().unwrap();
    let fixture = Fixture::new(
        "preflight",
        &[completed("100000000000037099", RemovalOutcome::Kicked)],
    );
    // Four pending records need 1280 bytes, but the valid audit has only 1000.
    let audit = FileAudit::open(&fixture.audit, GUILD).unwrap();
    assert!(audit.ensure_capacity(3).is_ok());
    drop(audit);
    let before = fs::read(&fixture.audit).unwrap();
    let output = fixture.execute_without_token(&MEMBERS[..4]);
    let error = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2), "{error}");
    assert_capacity_refusal(&error);
    assert!(
        !error.contains("DISCORD_TOKEN"),
        "capacity must be checked before token access: {error}"
    );
    assert!(
        output.stdout.is_empty(),
        "refusal printed a success summary"
    );
    assert!(
        fs::read(&fixture.audit).unwrap() == before,
        "capacity refusal changed the audit"
    );
    fixture.finish();
}

#[test]
fn resumed_capacity_counts_only_pending_targets_before_missing_token_refusal() {
    let _guard = FIXTURE_LOCK.lock().unwrap();
    let fixture = Fixture::new(
        "pending-only",
        &[
            completed(MEMBERS[0], RemovalOutcome::Kicked),
            completed(MEMBERS[1], RemovalOutcome::AlreadyGone),
            completed(MEMBERS[2], RemovalOutcome::Kicked),
            completed(MEMBERS[3], RemovalOutcome::AlreadyGone),
        ],
    );
    // Six ids would need 1920 bytes; only the two pending ids need 640.
    let audit = FileAudit::open(&fixture.audit, GUILD).unwrap();
    assert!(audit.ensure_capacity(2).is_ok());
    assert!(audit.ensure_capacity(6).is_err());
    drop(audit);
    let before = fs::read(&fixture.audit).unwrap();
    let output = fixture.execute_without_token(&MEMBERS);
    let error = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2), "{error}");
    assert!(
        error.contains("DISCORD_TOKEN"),
        "fitting pending targets must reach the missing-token refusal: {error}"
    );
    assert!(
        !error.contains("8 MiB"),
        "unexpected capacity refusal: {error}"
    );
    assert!(
        !error.contains("split the cohort"),
        "completed ids consumed capacity: {error}"
    );
    assert!(
        output.stdout.is_empty(),
        "refusal printed a success summary"
    );
    assert!(
        fs::read(&fixture.audit).unwrap() == before,
        "missing-token refusal changed the audit"
    );
    fixture.finish();
}

#[test]
fn replayed_bytes_and_each_append_count_toward_the_real_file_backstop() {
    let _guard = FIXTURE_LOCK.lock().unwrap();
    let fixture = Fixture::new(
        "lifecycle",
        &[completed("100000000000037098", RemovalOutcome::Kicked)],
    );
    let mut audit = FileAudit::open(&fixture.audit, GUILD).unwrap();
    assert!(audit.ensure_capacity(0).is_ok());
    assert!(audit.ensure_capacity(3).is_ok());
    assert_capacity_refusal(&audit.ensure_capacity(4).unwrap_err());

    let mut appended = Vec::new();
    let mut refused = false;
    for attempt in 0..16 {
        let member = format!("100000000000038{attempt:03}");
        let record = completed(&member, RemovalOutcome::Kicked);
        let mut serialized = serde_json::to_vec(&record).unwrap();
        serialized.push(b'\n');
        assert!((64..=320).contains(&serialized.len()));
        let before = fs::read(&fixture.audit).unwrap();
        let result = audit.append(&record);
        let after = fs::read(&fixture.audit).unwrap();
        let length = fs::metadata(&fixture.audit).unwrap().len();
        assert!(length <= CAP as u64, "append exceeded the 8 MiB cap");
        match result {
            Ok(()) => {
                assert!(
                    before.len() + serialized.len() <= CAP,
                    "append accepted bytes beyond the cap"
                );
                let mut expected = before;
                expected.extend(serialized);
                assert!(
                    after == expected,
                    "append did not persist exactly one record"
                );
                appended.push(member);
                let fitting = (CAP - length as usize) / 320;
                assert!(audit.ensure_capacity(fitting).is_ok());
                assert!(audit.ensure_capacity(fitting + 1).is_err());
            }
            Err(error) => {
                assert!(
                    before.len() + serialized.len() > CAP,
                    "early refusal: {error}"
                );
                assert!(error.contains("8 MiB"), "{error}");
                assert!(after == before, "refused append changed the audit");
                assert!(audit.append(&record).is_err());
                assert!(
                    fs::read(&fixture.audit).unwrap() == before,
                    "repeated refused append changed the audit"
                );
                refused = true;
                break;
            }
        }
    }
    assert!(
        appended.len() >= 2,
        "must exercise cumulative append accounting"
    );
    assert!(
        refused,
        "backstop did not refuse within 16 bounded attempts"
    );
    drop(audit);

    let before_reopen = fs::read(&fixture.audit).unwrap();
    let reopened = FileAudit::open(&fixture.audit, GUILD).unwrap();
    for member in appended {
        assert!(
            reopened.done.contains(&member),
            "lost appended record on replay"
        );
    }
    assert!(reopened.ensure_capacity(1).is_err());
    drop(reopened);
    assert!(
        fs::read(&fixture.audit).unwrap() == before_reopen,
        "reopen changed the audit"
    );
    fixture.finish();
}
