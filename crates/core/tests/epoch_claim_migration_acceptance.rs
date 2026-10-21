//! Acceptance: the epoch-claim migration for the scheduled-event mirror.
//!
//! Offline pins over the repository's own migration chains plus controls for
//! the read surface the migration must not disturb. No database, no network,
//! no Discord: every probe reads migration files and `migrations.lock` from
//! the working tree and drives the pure normalization API.
//!
//! Contract (bounded-claim design, epoch layer): one new table per chain,
//! `scheduled_events_claim (guild_id PK, epoch TEXT)`, in both the cutover
//! chain (fixtures) and the store chain (boot runner). The migration writes
//! only the new table — reads of pre-existing snapshots may backfill the
//! claim, but no write may touch the old mirror — and every claim file is
//! locked with a matching checksum. The four migration tests below are RED
//! until the builder lands; the remaining tests are controls pinning the
//! read/normalize surface byte-identical.

use two_bot_core::{
    normalize_events, EventStatus, RawScheduledEvent, ScheduledEventsSkip,
    SCHEDULED_EVENTS_INTERVAL_MS,
};

/// Chains carrying the website tables, in apply order: cutover first, then
/// the store chain staging applies on top under its own ledger.
const CHAINS: [(&str, &str); 2] = [
    ("cutover", "../cutover/migrations"),
    ("store", "../store/migrations"),
];

const T1: &str = "2026-08-25T20:00:00.000Z";
const T2: &str = "2026-08-25T21:00:00.000Z";

/// The contracted epoch-claim rule, as an executable spec: only a snapshot
/// strictly older than the committed epoch rolls back. Genesis (no stored
/// epoch), newer, and equal stamps all commit.
#[derive(Debug, PartialEq, Eq)]
enum ClaimOutcome {
    Committed,
    StaleSnapshot,
}

fn classify_claim(stored_epoch: Option<&str>, observed_at: &str) -> ClaimOutcome {
    match stored_epoch {
        None => ClaimOutcome::Committed,
        Some(stored) if observed_at < stored => ClaimOutcome::StaleSnapshot,
        Some(_) => ClaimOutcome::Committed,
    }
}

fn sql_files(chain_dir: &str) -> Vec<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(chain_dir);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("migrations directory")
        .map(|entry| entry.expect("migration entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no migrations found");
    entries
}

/// Strip `--` line comments and `/* */` block comments, quote-aware so a
/// marker inside a string literal never eats executable SQL. Probes must see
/// only executable SQL: a comment explaining that the migration never alters
/// the old mirror must not itself trip the no-ALTER probe.
fn strip_sql_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    let mut in_string = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while i < bytes.len() {
        let byte = bytes[i];
        let next = bytes.get(i + 1).copied();
        if in_line_comment {
            if byte == b'\n' {
                in_line_comment = false;
                out.push('\n');
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if byte == b'*' && next == Some(b'/') {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_string {
            out.push(byte as char);
            if byte == b'\'' {
                if next == Some(b'\'') {
                    out.push('\'');
                    i += 1;
                } else {
                    in_string = false;
                }
            }
            i += 1;
            continue;
        }
        if byte == b'-' && next == Some(b'-') {
            in_line_comment = true;
            i += 2;
            continue;
        }
        if byte == b'/' && next == Some(b'*') {
            in_block_comment = true;
            i += 2;
            continue;
        }
        if byte == b'\'' {
            in_string = true;
        }
        out.push(byte as char);
        i += 1;
    }
    out
}

/// Lowercase with collapsed whitespace so multi-line DDL matches single-line
/// needles regardless of formatting.
fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// `(file name, file bytes)` for every migration in the chain that mentions
/// the claim table.
fn claim_migrations(chain_dir: &str) -> Vec<(String, String)> {
    sql_files(chain_dir)
        .iter()
        .filter_map(|path| {
            let sql = std::fs::read_to_string(path).expect("read migration");
            let body = normalize_sql(&strip_sql_comments(&sql));
            body.contains("scheduled_events_claim").then(|| {
                let name = path
                    .file_name()
                    .expect("migration file name")
                    .to_string_lossy()
                    .into_owned();
                (name, sql)
            })
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn claim_migration_exists_in_both_chains() {
    for (name, dir) in CHAINS {
        assert!(
            !claim_migrations(dir).is_empty(),
            "no scheduled_events_claim migration in the {name} chain; \
             the mirror epoch gate needs one per chain \
             (cutover for fixtures, store for the boot runner)"
        );
    }
}

#[test]
fn claim_migration_creates_guild_epoch_table() {
    let mut checked = 0;
    for (name, dir) in CHAINS {
        for (file, sql) in claim_migrations(dir) {
            let body = normalize_sql(&strip_sql_comments(&sql));
            assert!(
                body.contains("create table"),
                "{name}/{file}: claim migration must create the claim table"
            );
            assert_eq!(
                body.matches("create table").count(),
                1,
                "{name}/{file}: claim migration creates exactly one table"
            );
            assert!(
                body.contains("if not exists"),
                "{name}/{file}: repo convention is CREATE TABLE IF NOT EXISTS"
            );
            assert!(
                body.contains("guild_id") && body.contains("primary key"),
                "{name}/{file}: claim table needs a guild_id primary key"
            );
            assert!(
                body.contains("epoch"),
                "{name}/{file}: claim table needs the epoch column"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no claim migration to check");
}

#[test]
fn claim_migration_writes_only_the_new_table() {
    // Reads of the pre-existing mirror may backfill the claim, but no write
    // may touch an old table: the migration must leave every snapshot row
    // and every read path exactly as it found them.
    let mut checked = 0;
    for (name, dir) in CHAINS {
        for (file, sql) in claim_migrations(dir) {
            let body = normalize_sql(&strip_sql_comments(&sql));
            let words: Vec<&str> = body
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .filter(|word| !word.is_empty())
                .collect();
            for verb in ["alter", "drop", "truncate", "delete", "update"] {
                assert!(
                    !words.contains(&verb),
                    "{name}/{file}: claim migration must never {verb} (old mirror is write-frozen)"
                );
            }
            for segment in body.split("insert into").skip(1) {
                let target = segment
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ");
                assert!(
                    target.contains("scheduled_events_claim"),
                    "{name}/{file}: INSERT targets only the new claim table, found {target:?}"
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no claim migration to check");
}

#[test]
fn claim_migrations_are_locked_with_matching_checksums() {
    let lock_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations.lock");
    let lock_text = std::fs::read_to_string(&lock_path).expect("read migrations.lock");
    let lock: serde_json::Value = serde_json::from_str(&lock_text).expect("parse migrations.lock");
    assert_eq!(lock["version"], serde_json::json!(1));
    let mut checked = 0;
    for (name, dir) in CHAINS {
        for path in sql_files(dir) {
            let sql = std::fs::read_to_string(&path).expect("read migration");
            if !normalize_sql(&strip_sql_comments(&sql)).contains("scheduled_events_claim") {
                continue;
            }
            let file = path
                .file_name()
                .expect("migration file name")
                .to_string_lossy();
            let key = format!("crates/{name}/migrations/{file}");
            let entry = lock["migrations"][key.as_str()].as_object().unwrap_or_else(|| {
                panic!("{key} has no migrations.lock entry; feature migrations need reviewed checksums")
            });
            let expected = entry["sha256"].as_str().expect("lock sha256");
            let bytes = std::fs::read(&path).expect("read migration bytes");
            assert_eq!(
                sha256_hex(&bytes),
                expected,
                "{key}: checksum mismatch; never edit an applied migration"
            );
            let justification = entry["justification"].as_str().expect("lock justification");
            assert_eq!(
                justification.lines().count(),
                1,
                "{key}: justification must be one nonempty line"
            );
            assert!(
                !justification.trim().is_empty(),
                "{key}: justification must be one nonempty line"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no claim migration to check");
}

#[test]
fn claim_rule_commits_genesis_newer_and_equal_only_stale_rolls_back() {
    use ClaimOutcome::{Committed, StaleSnapshot};
    assert_eq!(classify_claim(None, T2), Committed);
    assert_eq!(classify_claim(Some(T2), T1), StaleSnapshot);
    assert_eq!(classify_claim(Some(T1), T2), Committed);
    assert_eq!(classify_claim(Some(T2), T2), Committed);
}

fn raw_event(id: &str) -> RawScheduledEvent {
    RawScheduledEvent {
        id: Some(id.to_owned()),
        name: Some("Sunday Squad".to_owned()),
        scheduled_start_time: Some("2026-09-06T18:30:00+01:00".to_owned()),
        channel_id: Some("voice-1".to_owned()),
        description: Some("Join the weekly games night.".to_owned()),
        status: Some(1),
    }
}

#[test]
fn snapshot_normalization_is_unchanged() {
    assert_eq!(
        normalize_events(&[])
            .expect("empty snapshot is valid")
            .len(),
        0,
        "a successful empty response stays a valid empty mirror"
    );
    let normalized = normalize_events(&[raw_event("event-1")]).expect("valid row normalizes");
    assert_eq!(normalized.len(), 1);
    assert_eq!(normalized[0].starts_at, "2026-09-06T17:30:00.000Z");
    assert_eq!(normalized[0].status, EventStatus::Scheduled);
    assert!(
        normalize_events(&[raw_event("event-1"), raw_event("event-1")]).is_none(),
        "duplicate ids still refuse the whole snapshot"
    );
}

#[test]
fn event_status_words_are_unchanged() {
    for (api, word, status) in [
        (1, "scheduled", EventStatus::Scheduled),
        (2, "active", EventStatus::Active),
        (3, "completed", EventStatus::Completed),
        (4, "cancelled", EventStatus::Cancelled),
    ] {
        assert_eq!(EventStatus::from_api(api), Some(status));
        assert_eq!(EventStatus::parse(word), Some(status));
        assert_eq!(status.as_str(), word);
    }
    assert_eq!(EventStatus::parse("bogus"), None);
    assert_eq!(EventStatus::from_api(99), None);
}

#[test]
fn existing_skip_outcomes_keep_their_names() {
    assert_ne!(
        ScheduledEventsSkip::DiscordReadFailed,
        ScheduledEventsSkip::InvalidResponse,
        "transport failure and malformed body stay distinct outcomes"
    );
    assert_eq!(
        format!("{:?}", ScheduledEventsSkip::DiscordReadFailed),
        "DiscordReadFailed"
    );
    assert_eq!(
        format!("{:?}", ScheduledEventsSkip::InvalidResponse),
        "InvalidResponse"
    );
}

#[test]
fn poll_cadence_is_unchanged() {
    assert_eq!(SCHEDULED_EVENTS_INTERVAL_MS, 600_000);
}
