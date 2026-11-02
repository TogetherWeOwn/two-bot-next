//! Acceptance: the tombstone migration for the scheduled-event mirror.
//!
//! PROTECTED test-author file. The Builder builds on this file's immutable
//! head without editing it: a Builder change here voids the handoff.
//!
//! Offline pins over the repository's own migration chains plus controls for
//! the surface the migration must not disturb. No database, no network, no
//! Discord: every probe reads migration files and `migrations.lock` from the
//! working tree and drives pure comparisons.
//!
//! Contract (tombstone layer): one new table per chain,
//! `scheduled_events_tombstone (guild_id TEXT, event_id TEXT, deleted_at TEXT,
//! PRIMARY KEY (guild_id, event_id))`, in both the cutover chain (fixtures)
//! and the store chain (boot runner). `deleted_at` is the deleting tick's
//! `observed_at` in the same fixed-width UTC-millis rendering the mirror and
//! the claim epoch use, so TEXT comparison stays chronological. The migration
//! writes only the new table — no write may touch the old mirror or the claim
//! table — and every tombstone file is locked with a matching checksum. The
//! four migration tests below are RED until the Builder lands; the remaining
//! tests are controls.
//!
//! The behavioral half lives in `tombstone_reinsert_guard_acceptance.rs`
//! (disposable database): delete-then-stale-replay at equal stamps, the 24 h
//! / 1,000-per-guild purge caps, genuinely-new inserts, and the ungated
//! mutation path. The migration number is the Builder's choice (next free in
//! each chain); these probes match any file mentioning the table.

/// Chains carrying the website tables, in apply order: cutover first, then
/// the store chain staging applies on top under its own ledger.
const CHAINS: [(&str, &str); 2] = [
    ("cutover", "../cutover/migrations"),
    ("store", "../store/migrations"),
];

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
/// the given table.
fn table_migrations(chain_dir: &str, table: &str) -> Vec<(String, String)> {
    sql_files(chain_dir)
        .iter()
        .filter_map(|path| {
            let sql = std::fs::read_to_string(path).expect("read migration");
            let body = normalize_sql(&strip_sql_comments(&sql));
            body.contains(table).then(|| {
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

fn tombstone_migrations(chain_dir: &str) -> Vec<(String, String)> {
    table_migrations(chain_dir, "scheduled_events_tombstone")
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
fn tombstone_migration_exists_in_both_chains() {
    for (name, dir) in CHAINS {
        assert!(
            !tombstone_migrations(dir).is_empty(),
            "no scheduled_events_tombstone migration in the {name} chain; \
             the re-INSERT guard needs one per chain \
             (cutover for fixtures, store for the boot runner)"
        );
    }
}

#[test]
fn tombstone_migration_creates_guild_event_deleted_table() {
    let mut checked = 0;
    for (name, dir) in CHAINS {
        for (file, sql) in tombstone_migrations(dir) {
            let body = normalize_sql(&strip_sql_comments(&sql));
            assert!(
                body.contains("create table"),
                "{name}/{file}: tombstone migration must create the tombstone table"
            );
            assert_eq!(
                body.matches("create table").count(),
                1,
                "{name}/{file}: tombstone migration creates exactly one table"
            );
            assert!(
                body.contains("if not exists"),
                "{name}/{file}: repo convention is CREATE TABLE IF NOT EXISTS"
            );
            assert!(
                body.contains("guild_id") && body.contains("primary key"),
                "{name}/{file}: tombstone table needs a guild_id primary key"
            );
            assert!(
                body.contains("event_id"),
                "{name}/{file}: tombstone table needs the event_id column"
            );
            assert!(
                body.contains("deleted_at"),
                "{name}/{file}: tombstone table needs the deleted_at column"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no tombstone migration to check");
}

#[test]
fn tombstone_migration_writes_only_the_new_table() {
    // The old mirror and the claim table are write-frozen: the migration must
    // leave every snapshot row, every epoch row and every read path exactly
    // as it found them. A tombstone exists precisely when its mirror row is
    // gone, so no foreign key into the mirror is wanted either. A covering
    // index on the new table is fine.
    let mut checked = 0;
    for (name, dir) in CHAINS {
        for (file, sql) in tombstone_migrations(dir) {
            let body = normalize_sql(&strip_sql_comments(&sql));
            let words: Vec<&str> = body
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .filter(|word| !word.is_empty())
                .collect();
            for verb in ["alter", "drop", "truncate", "delete", "update"] {
                assert!(
                    !words.contains(&verb),
                    "{name}/{file}: tombstone migration must never {verb} (old tables are write-frozen)"
                );
            }
            assert!(
                !body.contains("references"),
                "{name}/{file}: tombstone table takes no foreign keys (its row outlives the mirror row)"
            );
            for segment in body.split("insert into").skip(1) {
                let target = segment
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ");
                assert!(
                    target.contains("scheduled_events_tombstone"),
                    "{name}/{file}: INSERT targets only the new tombstone table, found {target:?}"
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no tombstone migration to check");
}

#[test]
fn tombstone_migrations_are_locked_with_matching_checksums() {
    let lock_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations.lock");
    let lock_text = std::fs::read_to_string(&lock_path).expect("read migrations.lock");
    let lock: serde_json::Value = serde_json::from_str(&lock_text).expect("parse migrations.lock");
    assert_eq!(lock["version"], serde_json::json!(1));
    let mut checked = 0;
    for (name, dir) in CHAINS {
        for path in sql_files(dir) {
            let sql = std::fs::read_to_string(&path).expect("read migration");
            if !normalize_sql(&strip_sql_comments(&sql)).contains("scheduled_events_tombstone") {
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
    assert!(checked > 0, "no tombstone migration to check");
}

#[test]
fn retention_comparison_purges_only_stamps_older_than_the_cutoff() {
    // Control documenting the purge direction the database suite pins
    // behaviorally: fixed-width UTC-millis stamps compare chronologically as
    // TEXT, and only a tombstone strictly older than observed-minus-24h goes.
    // The exact-boundary row is deliberately unpinned (Builder's choice).
    let observed = "2026-08-25T21:00:00.000Z";
    let cutoff = "2026-08-24T21:00:00.000Z";
    assert!(cutoff < observed, "cutoff is 24 h before the tick");
    assert!(
        "2026-08-24T20:00:00.000Z" < cutoff,
        "a 25 h old tombstone is older than the cutoff, so the tick purges it"
    );
    assert!(
        "2026-08-25T20:00:00.000Z" >= cutoff,
        "a 1 h old tombstone is newer than the cutoff, so the tick keeps it"
    );
}

#[test]
fn epoch_claim_migration_still_present_in_both_chains() {
    // Control: the tombstone layer adds to the epoch-claim layer, it never
    // replaces it. The claim migration stays one per chain.
    for (name, dir) in CHAINS {
        assert!(
            !table_migrations(dir, "scheduled_events_claim").is_empty(),
            "the {name} chain lost its scheduled_events_claim migration; \
             the tombstone layer must not disturb the epoch gate"
        );
    }
}
