#![cfg(feature = "db")]

//! Acceptance: giveaways slice 1 — domain, store and migration land with a
//! verifiable draw.
//!
//! Slice 1 of the giveaways design (timed entry, fair draw, reroll): the pure
//! domain module (`giveaway.rs`: validation, eligibility, the pure draw, the
//! status state machine), the sqlx store (`giveaway_store.rs` behind the crate
//! `db` feature: CRUD, entry upsert, claim-lease, draw insert) and the numbered
//! migration (three guild-scoped tables in both chains) land together, and any
//! member can replay the draw from the published inputs.
//!
//! RED on the baseline, GREEN once the builder lands the slice — this file is
//! the protected acceptance corpus and the implementation PR must make it pass
//! without editing it:
//!
//! - RED (fail until the slice lands): the four migration probes, the two
//!   module-registration probes, and the five disposable-database contract
//!   tests. The database tests fail with `table public.giveaways is missing`
//!   style assertions, never transport errors.
//! - Controls (pass on the baseline and after): the two `reference_*` replay
//!   vectors plus `giveaway_domain_vocabulary_contract`. The `reference_*`
//!   tests pin the byte-exact replay contract the builder's domain code must
//!   reproduce; the builder's own unit tests assert the same vectors.
//!
//! Replay contract (byte-exact; anything else is a design change, not a fix
//! shape): `seed = sha256("{draw_nonce_hex}:{draw_no}")` over ASCII,
//! `eligible_hash = sha256(comma_join(sorted_user_ids))` over ASCII (Discord
//! ids are digit strings, so the comma join is unambiguous), and winners come
//! from a partial Fisher–Yates over the lexicographically sorted eligible list
//! with index stream `u64::from_be_bytes(sha256("{seed_hex}:{i}")[..8])`.
//! A member replays with `printf '%s' 'nonce:0' | sha256sum` and the few lines
//! of Fisher–Yates below. ASCII constructions are deliberate: no byte-order or
//! hex-decoding ambiguity for a verifier.
//!
//! Deliberately NOT pinned here: secondary indexes (a plain `CREATE INDEX`
//! after `CREATE TABLE IF NOT EXISTS` fails the required squawk-strict lint
//! and `CONCURRENTLY` cannot run in the transactional migrators, so the index
//! strategy is the builder's call with the reviewer); claim-lease contention
//! between two workers (needs the builder's claim SQL, so it belongs to the
//! builder's store tests); the command/button/ticker surface (later slices).
//!
//! Run the offline probes with no database:
//! `python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db
//! --test giveaway_domain_store_acceptance reference_ giveaway_migration_ giveaway_domain_ giveaway_store_`
//! Run everything against the disposable test service only (never staging or
//! production):
//! `TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local
//! python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db
//! --test giveaway_domain_store_acceptance`
//! An absent `TWO_TEST_DATABASE_URL` skips the database tests vacuously on a
//! workstation; CI must configure it (the helper asserts so).

use two_bot_testsupport::TestDatabase;

/// Chains carrying new tables, in apply order: cutover first (fixtures run
/// this chain), then the store chain (the boot runner applies it under its own
/// ledger). Every feature table needs one migration per chain.
const CHAINS: [(&str, &str); 2] = [
    ("cutover", "../cutover/migrations"),
    ("store", "../store/migrations"),
];

/// The three tables slice 1 owns.
const GIVEAWAY_TABLES: [&str; 3] = ["giveaways", "giveaway_entries", "giveaway_draws"];

/// Spec §3 `giveaways` columns, in spec order.
const GIVEAWAYS_COLUMNS: [&str; 16] = [
    "guild_id",
    "id",
    "prize",
    "channel_id",
    "message_id",
    "created_by",
    "winners_count",
    "ends_at",
    "status",
    "requirements_json",
    "max_entries",
    "draw_nonce",
    "claim_token",
    "claimed_at",
    "announced_at",
    "created_at",
];
/// Spec §3 `giveaways` columns, continued (`updated_at` keeps the array small
/// enough to read; the probe concatenates both).
const GIVEAWAYS_COLUMNS_TAIL: [&str; 1] = ["updated_at"];
/// Spec §3 `giveaway_entries` columns.
const ENTRIES_COLUMNS: [&str; 5] = ["guild_id", "giveaway_id", "user_id", "weight", "entered_at"];
/// Spec §3 `giveaway_draws` columns.
const DRAWS_COLUMNS: [&str; 9] = [
    "guild_id",
    "giveaway_id",
    "draw_no",
    "drawn_at",
    "eligible_count",
    "eligible_hash",
    "seed",
    "winner_ids",
    "reroll_kind",
];

/// Status vocabulary from the design: `open` | `paused` | `drawing` | `drawn`
/// | `cancelled`.
const STATUSES: [&str; 5] = ["open", "paused", "drawing", "drawn", "cancelled"];
/// First draw of a giveaway is draw 0; each reroll inserts `draw_no + 1`.
const FIRST_DRAW_NO: i64 = 0;
/// Winner-count bounds (Giveaway Boat parity on the ceiling).
const WINNERS_MIN: i64 = 1;
const WINNERS_MAX: i64 = 100;

/// Fixed 128-bit draw nonce (hex) for the reference vectors.
const VECTOR_NONCE: &str = "9f2c4a7e1b5d8f03a6c9e2b4d7f1a83c";
/// Ten fixed eligible member ids for the reference vectors.
const VECTOR_ELIGIBLE: [&str; 10] = [
    "100000000000000001",
    "100000000000000002",
    "100000000000000003",
    "100000000000000004",
    "100000000000000005",
    "100000000000000006",
    "100000000000000007",
    "100000000000000008",
    "100000000000000009",
    "100000000000000010",
];
/// `reference_seed(VECTOR_NONCE, 0)`.
const VECTOR_SEED_0: &str = "fdab72cb4823dbf292f0ed3fea3abef20dc268169a90f6aea0a1cd4c1a8c15ab";
/// `reference_seed(VECTOR_NONCE, 1)` — the full-reroll seed.
const VECTOR_SEED_1: &str = "16688231019cd5f768dcb0e2a604881b365c0ba7b242433b3e7ff96bde0bdf88";
/// `reference_eligible_hash(VECTOR_ELIGIBLE)`.
const VECTOR_ELIGIBLE_HASH: &str =
    "deebe3bffe4ba1543bd568b42a31508a96ef5c0ba98464a3f1e572286ead0299";
/// `reference_draw(VECTOR_ELIGIBLE, VECTOR_SEED_0, 2)`.
const VECTOR_WINNERS_10_OF_2: [&str; 2] = ["100000000000000009", "100000000000000002"];
/// `reference_draw(first 3 of VECTOR_ELIGIBLE, VECTOR_SEED_0, 5)` — fewer
/// entrants than winners draws everybody, in draw order.
const VECTOR_WINNERS_3_OF_5: [&str; 3] = [
    "100000000000000001",
    "100000000000000003",
    "100000000000000002",
];
/// `reference_draw(VECTOR_ELIGIBLE, VECTOR_SEED_1, 2)` — the full reroll.
const VECTOR_REROLL_WINNERS: [&str; 2] = ["100000000000000005", "100000000000000002"];

const GUILD: &str = "1545644954272137297";
const GIVEAWAY: &str = "gw01h455k";
const CHANNEL: &str = "200000000000000001";
const MESSAGE: &str = "200000000000000002";
const MOD: &str = "100000000000000099";
const ENDS_AT: &str = "2026-11-01T00:00:00.000Z";
const STAMP: &str = "2026-10-21T00:00:00.000Z";

// ---------------------------------------------------------------------------
// Reference replay contract (executable spec; the builder reproduces these).
// ---------------------------------------------------------------------------

fn sha256_hex(input: impl AsRef<[u8]>) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(input.as_ref());
    hex::encode(hasher.finalize())
}

fn sha256_raw(input: &str) -> [u8; 32] {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(input.as_bytes());
    hasher.finalize().into()
}

/// Reference seed: `sha256("{draw_nonce_hex}:{draw_no}")` over ASCII. The
/// nonce is minted at create (128-bit hex) and revealed at draw; the seed is
/// published in the draw row so anyone can replay.
fn reference_seed(draw_nonce_hex: &str, draw_no: i64) -> String {
    assert_eq!(
        draw_nonce_hex.len(),
        32,
        "draw nonces are 128-bit hex, got {draw_nonce_hex:?}"
    );
    assert!(
        draw_nonce_hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "draw nonces are 128-bit hex, got {draw_nonce_hex:?}"
    );
    sha256_hex(format!("{draw_nonce_hex}:{draw_no}"))
}

/// Reference eligible-hash: `sha256` over the ASCII comma-join of the
/// lexicographically sorted eligible user ids. Sorting inside makes the
/// canonical order part of the contract: entry order never affects the hash.
fn reference_eligible_hash(eligible_user_ids: &[String]) -> String {
    let mut sorted = eligible_user_ids.to_vec();
    sorted.sort();
    sha256_hex(sorted.join(","))
}

/// Reference draw: partial Fisher–Yates over the lexicographically sorted
/// eligible list. Position `i` swaps with `i + (stream(i) mod (n - i))` where
/// `stream(i)` is the first 8 bytes, big-endian, of
/// `sha256("{seed_hex}:{i}")`. Total over edge cases: zero entrants draws
/// nobody, and fewer entrants than winners draws everybody in draw order.
fn reference_draw(
    eligible_user_ids: &[String],
    seed_hex: &str,
    winners_count: usize,
) -> Vec<String> {
    let mut order = eligible_user_ids.to_vec();
    order.sort();
    let total = order.len();
    let take = winners_count.min(total);
    for i in 0..take {
        let digest = sha256_raw(&format!("{seed_hex}:{i}"));
        let rand =
            u64::from_be_bytes(digest[0..8].try_into().expect("sha256 is 32 bytes")) as usize;
        let pick = i + rand % (total - i);
        order.swap(i, pick);
    }
    order.truncate(take);
    order
}

/// Reference status parse: exactly the five design statuses.
fn parse_giveaway_status(value: &str) -> Option<&'static str> {
    STATUSES.into_iter().find(|status| *status == value)
}

/// Reference winner-count bounds: 1–100.
fn validate_winners_count(count: i64) -> bool {
    (WINNERS_MIN..=WINNERS_MAX).contains(&count)
}

/// Reference reroll-kind shapes: NULL for the first draw, `full`, or
/// `single:<winner user id>`.
fn reroll_kind_single(user_id: &str) -> String {
    format!("single:{user_id}")
}

// ---------------------------------------------------------------------------
// Offline file probes (migration chains, module registration).
// ---------------------------------------------------------------------------

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
/// an old table must not itself trip the no-ALTER probe.
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

/// `(file name, file bytes)` for every migration in the chain that carries
/// giveaway DDL: the draws table name is unique to this slice, and the file
/// stem is the fallback for a split migration.
fn giveaway_migrations(chain_dir: &str) -> Vec<(String, String)> {
    sql_files(chain_dir)
        .iter()
        .filter_map(|path| {
            let name = path
                .file_name()
                .expect("migration file name")
                .to_string_lossy()
                .into_owned();
            let sql = std::fs::read_to_string(path).expect("read migration");
            let body = normalize_sql(&strip_sql_comments(&sql));
            (body.contains("giveaway_draws") || name.contains("giveaway")).then_some((name, sql))
        })
        .collect()
}

fn core_src_path(file: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file)
}

#[test]
fn giveaway_migration_exists_in_both_chains() {
    for (name, dir) in CHAINS {
        assert!(
            !giveaway_migrations(dir).is_empty(),
            "no giveaway migration in the {name} chain; slice 1 needs one per chain \
             (cutover for fixtures, store for the boot runner)"
        );
    }
}

#[test]
fn giveaway_migration_creates_three_guild_scoped_tables() {
    for (name, dir) in CHAINS {
        let migrations = giveaway_migrations(dir);
        assert!(
            !migrations.is_empty(),
            "no giveaway migration to check in the {name} chain"
        );
        let bodies: Vec<String> = migrations
            .iter()
            .map(|(_, sql)| normalize_sql(&strip_sql_comments(sql)))
            .collect();
        let all = bodies.join("\n");
        for table in GIVEAWAY_TABLES {
            assert!(
                all.contains("create table") && all.contains(table),
                "{name}: giveaway migration must CREATE TABLE {table}"
            );
        }
        assert!(
            all.contains("if not exists"),
            "{name}: repo convention is CREATE TABLE IF NOT EXISTS"
        );
        for column in GIVEAWAYS_COLUMNS
            .iter()
            .chain(GIVEAWAYS_COLUMNS_TAIL.iter())
            .chain(ENTRIES_COLUMNS.iter())
            .chain(DRAWS_COLUMNS.iter())
            .copied()
        {
            assert!(
                all.contains(column),
                "{name}: giveaway migration is missing column {column}"
            );
        }
        // Primary keys, whitespace-insensitive so formatting never matters.
        let nospace: String = all.chars().filter(|c| !c.is_whitespace()).collect();
        for (table, key) in [
            ("giveaways", "primarykey(guild_id,id)"),
            (
                "giveaway_entries",
                "primarykey(guild_id,giveaway_id,user_id)",
            ),
            ("giveaway_draws", "primarykey(guild_id,giveaway_id,draw_no)"),
        ] {
            assert!(
                nospace.contains(key),
                "{name}: {table} needs PRIMARY KEY {key}"
            );
        }
        assert!(
            all.contains("timestamptz"),
            "{name}: ends_at is a timestamptz deadline"
        );
    }
}

#[test]
fn giveaway_migration_writes_only_new_tables() {
    // Additive only: the slice owns three new tables and must leave every
    // existing table exactly as it found them. Secondary indexes are
    // deliberately not probed here (see the module docs).
    let mut checked = 0;
    for (name, dir) in CHAINS {
        for (file, sql) in giveaway_migrations(dir) {
            let body = normalize_sql(&strip_sql_comments(&sql));
            let words: Vec<&str> = body
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .filter(|word| !word.is_empty())
                .collect();
            for verb in ["alter", "drop", "truncate", "delete", "update"] {
                assert!(
                    !words.contains(&verb),
                    "{name}/{file}: giveaway migration must never {verb} (additive only)"
                );
            }
            for segment in body.split("create table").skip(1) {
                let target = segment
                    .split_whitespace()
                    .take(4)
                    .collect::<Vec<_>>()
                    .join(" ");
                assert!(
                    GIVEAWAY_TABLES.iter().any(|table| target.contains(*table)),
                    "{name}/{file}: CREATE TABLE targets only the three giveaway tables, found {target:?}"
                );
            }
            for segment in body.split("insert into").skip(1) {
                let target = segment
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ");
                assert!(
                    GIVEAWAY_TABLES.iter().any(|table| target.contains(*table)),
                    "{name}/{file}: INSERT targets only the new giveaway tables, found {target:?}"
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no giveaway migration to check");
}

#[test]
fn giveaway_migrations_are_locked_with_matching_checksums() {
    let lock_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations.lock");
    let lock_text = std::fs::read_to_string(&lock_path).expect("read migrations.lock");
    let lock: serde_json::Value = serde_json::from_str(&lock_text).expect("parse migrations.lock");
    assert_eq!(lock["version"], serde_json::json!(1));
    let mut checked = 0;
    for (name, dir) in CHAINS {
        for (file, _) in giveaway_migrations(dir) {
            let key = format!("crates/{name}/migrations/{file}");
            let entry = lock["migrations"][key.as_str()].as_object().unwrap_or_else(|| {
                panic!("{key} has no migrations.lock entry; feature migrations need reviewed checksums")
            });
            let expected = entry["sha256"].as_str().expect("lock sha256");
            let bytes = std::fs::read(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(dir)
                    .join(&file),
            )
            .expect("read migration bytes");
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
    assert!(checked > 0, "no giveaway migration to check");
}

#[test]
fn giveaway_domain_module_is_registered() {
    let path = core_src_path("giveaway.rs");
    assert!(
        path.is_file(),
        "crates/core/src/giveaway.rs is missing; slice 1 puts the pure domain \
         (validation, eligibility, pure draw, state machine) there"
    );
    let lib = std::fs::read_to_string(core_src_path("lib.rs")).expect("read lib.rs");
    assert!(
        lib.contains("pub mod giveaway;"),
        "lib.rs must register `pub mod giveaway;`"
    );
    // Coarse presence pins so an empty file cannot read as landed; the exact
    // API is reviewed by a human against the design.
    let domain = std::fs::read_to_string(&path).expect("read giveaway.rs");
    assert!(
        domain.contains("draw"),
        "giveaway.rs must carry the pure draw"
    );
    assert!(
        domain.contains("eligib"),
        "giveaway.rs must carry eligibility adjudication"
    );
}

#[test]
fn giveaway_store_module_is_registered_behind_db_feature() {
    let path = core_src_path("giveaway_store.rs");
    assert!(
        path.is_file(),
        "crates/core/src/giveaway_store.rs is missing; slice 1 puts the sqlx \
         reads and writes (CRUD, entry upsert, claim-lease, draw insert) there"
    );
    let lib = std::fs::read_to_string(core_src_path("lib.rs")).expect("read lib.rs");
    let decl = "pub mod giveaway_store;";
    let pos = lib.find(decl).unwrap_or_else(|| {
        panic!("lib.rs must register `{decl}`");
    });
    let preceding: String = lib[..pos]
        .lines()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        preceding.contains("cfg(feature") && preceding.contains("\"db\""),
        "giveaway_store must sit behind #[cfg(feature = \"db\")] like leveling_store"
    );
    let store = std::fs::read_to_string(&path).expect("read giveaway_store.rs");
    assert!(
        store.contains("giveaway"),
        "giveaway_store.rs must move giveaway rows"
    );
    assert!(
        store.contains("entry") || store.contains("entries") || store.contains("draw"),
        "giveaway_store.rs must cover entries and draws"
    );
}

// ---------------------------------------------------------------------------
// Executable-spec controls (pass before and after; the builder reproduces them).
// ---------------------------------------------------------------------------

#[test]
fn reference_seed_and_eligible_hash_contract() {
    assert_eq!(reference_seed(VECTOR_NONCE, 0), VECTOR_SEED_0);
    assert_eq!(reference_seed(VECTOR_NONCE, 1), VECTOR_SEED_1);
    assert_ne!(
        VECTOR_SEED_0, VECTOR_SEED_1,
        "each draw round needs its own seed"
    );
    for seed in [VECTOR_SEED_0, VECTOR_SEED_1] {
        assert_eq!(seed.len(), 64, "seeds are sha256 hex");
        assert!(
            seed.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "seeds are lowercase hex"
        );
    }
    let eligible: Vec<String> = VECTOR_ELIGIBLE.iter().map(ToString::to_string).collect();
    assert_eq!(reference_eligible_hash(&eligible), VECTOR_ELIGIBLE_HASH);
    let mut shuffled = eligible.clone();
    shuffled.reverse();
    assert_eq!(
        reference_eligible_hash(&shuffled),
        VECTOR_ELIGIBLE_HASH,
        "entry order never affects the eligible hash"
    );
    let mut tampered = eligible.clone();
    tampered.pop();
    assert_ne!(
        reference_eligible_hash(&tampered),
        VECTOR_ELIGIBLE_HASH,
        "the hash commits to the full eligible list"
    );
}

#[test]
fn reference_draw_vectors_pin_verifiable_draw() {
    let eligible: Vec<String> = VECTOR_ELIGIBLE.iter().map(ToString::to_string).collect();
    let winners = reference_draw(&eligible, VECTOR_SEED_0, 2);
    assert_eq!(
        winners,
        VECTOR_WINNERS_10_OF_2
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        reference_draw(&eligible, VECTOR_SEED_0, 2),
        winners,
        "the same seed replays to the same winners"
    );
    let short: Vec<String> = eligible.iter().take(3).cloned().collect();
    assert_eq!(
        reference_draw(&short, VECTOR_SEED_0, 5),
        VECTOR_WINNERS_3_OF_5
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "fewer entrants than winners draws everybody, in draw order"
    );
    assert!(
        reference_draw(&[], VECTOR_SEED_0, 2).is_empty(),
        "zero entrants draws nobody"
    );
    assert_eq!(
        reference_draw(&eligible, VECTOR_SEED_1, 2),
        VECTOR_REROLL_WINNERS
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "a full reroll replays from the next round seed"
    );
    assert_ne!(
        reference_draw(&eligible, VECTOR_SEED_0, 2),
        reference_draw(&eligible, VECTOR_SEED_1, 2),
        "distinct round seeds must decorrelate the fixed vectors"
    );
    let mut shuffled = eligible.clone();
    shuffled.reverse();
    assert_eq!(
        reference_draw(&shuffled, VECTOR_SEED_0, 2),
        winners,
        "entry order never affects the draw"
    );
}

#[test]
fn giveaway_domain_vocabulary_contract() {
    for status in STATUSES {
        assert_eq!(parse_giveaway_status(status), Some(status));
    }
    for bogus in ["", "OPEN", "ended", "deleted"] {
        assert_eq!(
            parse_giveaway_status(bogus),
            None,
            "{bogus:?} is outside the status vocabulary"
        );
    }
    assert!(validate_winners_count(1));
    assert!(validate_winners_count(100));
    assert!(!validate_winners_count(0));
    assert!(!validate_winners_count(101));
    assert_eq!(FIRST_DRAW_NO, 0);
    assert_eq!(
        reroll_kind_single("100000000000000008"),
        "single:100000000000000008"
    );
}

// ---------------------------------------------------------------------------
// Disposable-database contract (skips vacuously without TWO_TEST_DATABASE_URL).
// ---------------------------------------------------------------------------

async fn connect(test: &str) -> Option<TestDatabase> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(
            TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                .await
                .expect("create migrated test database"),
        ),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure giveaway acceptance ({test})"
            );
            eprintln!(
                "SKIP giveaway_domain_store_acceptance ({test}): TWO_TEST_DATABASE_URL not set"
            );
            None
        }
    }
}

/// Fail RED with a missing-table assertion (never a transport error) until the
/// builder's migration applies.
async fn require_tables(pool: &sqlx::PgPool) {
    for table in GIVEAWAY_TABLES {
        let reg: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(format!("public.{table}"))
            .fetch_one(pool)
            .await
            .expect("to_regclass probe");
        assert!(
            reg.is_some(),
            "table public.{table} is missing; the giveaway migration has not landed"
        );
    }
}

async fn column_names(pool: &sqlx::PgPool, table: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = $1",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("information_schema probe")
}

async fn column_type(pool: &sqlx::PgPool, table: &str, column: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2",
    )
    .bind(table)
    .bind(column)
    .fetch_optional(pool)
    .await
    .expect("information_schema probe")
}

/// True when the column stores JSON natively (json/jsonb) rather than TEXT.
/// The design says "JSON array" without fixing the storage type, so inserts
/// branch on the live type instead of pinning one.
async fn is_json_column(pool: &sqlx::PgPool, table: &str, column: &str) -> bool {
    matches!(
        column_type(pool, table, column).await.as_deref(),
        Some("json") | Some("jsonb")
    )
}

fn assert_sqlstate(error: &sqlx::Error, code: &str) {
    assert_eq!(
        error
            .as_database_error()
            .expect("database constraint error")
            .code()
            .as_deref(),
        Some(code),
        "{error}"
    );
}

/// Insert the full spec §3 `giveaways` row. Every spec column is bound except
/// the lease/announce trio, which are nullable by design (`announced_at` NULL
/// means announce is still owed). Timestamps bind as ISO strings with explicit
/// `::timestamptz` casts (sqlx `&str` binds are typed TEXT, which Postgres
/// will not implicitly cast); `ends_at` itself is pinned to timestamptz by the
/// columns test.
async fn insert_giveaway(pool: &sqlx::PgPool, guild_id: &str, giveaway_id: &str) {
    if is_json_column(pool, "giveaways", "requirements_json").await {
        sqlx::query(
            "INSERT INTO giveaways
               (guild_id, id, prize, channel_id, message_id, created_by, winners_count,
                ends_at, status, requirements_json, max_entries, draw_nonce,
                created_at, updated_at)
             VALUES ($1, $2, 'Two game keys', $3, $4, $5, 2,
                     $6::timestamptz, 'open', $7::jsonb, 500, $8, $9::timestamptz, $9::timestamptz)",
        )
        .bind(guild_id)
        .bind(giveaway_id)
        .bind(CHANNEL)
        .bind(MESSAGE)
        .bind(MOD)
        .bind(ENDS_AT)
        .bind("{}")
        .bind(VECTOR_NONCE)
        .bind(STAMP)
        .execute(pool)
        .await
        .expect("insert giveaway");
    } else {
        sqlx::query(
            "INSERT INTO giveaways
               (guild_id, id, prize, channel_id, message_id, created_by, winners_count,
                ends_at, status, requirements_json, max_entries, draw_nonce,
                created_at, updated_at)
             VALUES ($1, $2, 'Two game keys', $3, $4, $5, 2,
                     $6::timestamptz, 'open', $7, 500, $8, $9::timestamptz, $9::timestamptz)",
        )
        .bind(guild_id)
        .bind(giveaway_id)
        .bind(CHANNEL)
        .bind(MESSAGE)
        .bind(MOD)
        .bind(ENDS_AT)
        .bind("{}")
        .bind(VECTOR_NONCE)
        .bind(STAMP)
        .execute(pool)
        .await
        .expect("insert giveaway");
    }
}

async fn insert_entry(pool: &sqlx::PgPool, guild_id: &str, giveaway_id: &str, user_id: &str) {
    sqlx::query(
        "INSERT INTO giveaway_entries (guild_id, giveaway_id, user_id, weight, entered_at)
         VALUES ($1, $2, $3, 1, $4::timestamptz)",
    )
    .bind(guild_id)
    .bind(giveaway_id)
    .bind(user_id)
    .bind(STAMP)
    .execute(pool)
    .await
    .expect("insert entry");
}

/// Insert one draw row; the seed/hash inputs are the reference vectors so the
/// stored row ties back to the replay contract.
async fn insert_draw(
    pool: &sqlx::PgPool,
    guild_id: &str,
    giveaway_id: &str,
    draw_no: i64,
    reroll_kind: Option<&str>,
    seed_hex: &str,
    winners_json: &str,
) {
    if is_json_column(pool, "giveaway_draws", "winner_ids").await {
        sqlx::query(
            "INSERT INTO giveaway_draws
               (guild_id, giveaway_id, draw_no, drawn_at, eligible_count,
                eligible_hash, seed, winner_ids, reroll_kind)
             VALUES ($1, $2, $3, $4::timestamptz, 10, $5, $6, $7::jsonb, $8)",
        )
        .bind(guild_id)
        .bind(giveaway_id)
        .bind(draw_no)
        .bind(STAMP)
        .bind(VECTOR_ELIGIBLE_HASH)
        .bind(seed_hex)
        .bind(winners_json)
        .bind(reroll_kind)
        .execute(pool)
        .await
        .expect("insert draw");
    } else {
        sqlx::query(
            "INSERT INTO giveaway_draws
               (guild_id, giveaway_id, draw_no, drawn_at, eligible_count,
                eligible_hash, seed, winner_ids, reroll_kind)
             VALUES ($1, $2, $3, $4::timestamptz, 10, $5, $6, $7, $8)",
        )
        .bind(guild_id)
        .bind(giveaway_id)
        .bind(draw_no)
        .bind(STAMP)
        .bind(VECTOR_ELIGIBLE_HASH)
        .bind(seed_hex)
        .bind(winners_json)
        .bind(reroll_kind)
        .execute(pool)
        .await
        .expect("insert draw");
    }
}

#[tokio::test]
async fn giveaway_tables_exist_with_spec_columns() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("giveaway_tables_exist_with_spec_columns").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    require_tables(&pool).await;
    for column in GIVEAWAYS_COLUMNS
        .iter()
        .chain(GIVEAWAYS_COLUMNS_TAIL.iter())
    {
        assert!(
            column_names(&pool, "giveaways")
                .await
                .contains(&column.to_string()),
            "giveaways is missing column {column}"
        );
    }
    for column in ENTRIES_COLUMNS {
        assert!(
            column_names(&pool, "giveaway_entries")
                .await
                .contains(&column.to_string()),
            "giveaway_entries is missing column {column}"
        );
    }
    for column in DRAWS_COLUMNS {
        assert!(
            column_names(&pool, "giveaway_draws")
                .await
                .contains(&column.to_string()),
            "giveaway_draws is missing column {column}"
        );
    }
    assert_eq!(
        column_type(&pool, "giveaways", "ends_at").await.as_deref(),
        Some("timestamp with time zone"),
        "ends_at is the timestamptz due predicate"
    );
    for (table, column) in [
        ("giveaways", "winners_count"),
        ("giveaways", "max_entries"),
        ("giveaway_entries", "weight"),
        ("giveaway_draws", "draw_no"),
        ("giveaway_draws", "eligible_count"),
    ] {
        let data_type = column_type(&pool, table, column).await;
        assert!(
            matches!(
                data_type.as_deref(),
                Some("smallint") | Some("integer") | Some("bigint")
            ),
            "{table}.{column} is an integer, got {data_type:?}"
        );
    }
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn giveaway_entry_pk_collapses_double_entry() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("giveaway_entry_pk_collapses_double_entry").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    require_tables(&pool).await;
    insert_giveaway(&pool, GUILD, GIVEAWAY).await;
    let entrant = VECTOR_ELIGIBLE[0];
    insert_entry(&pool, GUILD, GIVEAWAY, entrant).await;
    // A double-click / double POST is a second plain insert of the same key:
    // the PK refuses it, so the store's upsert has exactly one row to find.
    let duplicate = sqlx::query(
        "INSERT INTO giveaway_entries (guild_id, giveaway_id, user_id, weight, entered_at)
         VALUES ($1, $2, $3, 1, $4::timestamptz)",
    )
    .bind(GUILD)
    .bind(GIVEAWAY)
    .bind(entrant)
    .bind(STAMP)
    .execute(&pool)
    .await
    .expect_err("duplicate entry must fail");
    assert_sqlstate(&duplicate, "23505");
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaway_entries WHERE guild_id = $1 AND giveaway_id = $2",
    )
    .bind(GUILD)
    .bind(GIVEAWAY)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 1, "one row per entrant");
    // A second entrant is unaffected by the first entrant's key.
    insert_entry(&pool, GUILD, GIVEAWAY, VECTOR_ELIGIBLE[1]).await;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaway_entries WHERE guild_id = $1 AND giveaway_id = $2",
    )
    .bind(GUILD)
    .bind(GIVEAWAY)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 2);
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn giveaway_draw_pk_forbids_double_draw_of_same_round() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("giveaway_draw_pk_forbids_double_draw_of_same_round").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    require_tables(&pool).await;
    insert_giveaway(&pool, GUILD, GIVEAWAY).await;
    let first_winners = serde_json::to_string(&VECTOR_WINNERS_10_OF_2).expect("winners JSON");
    insert_draw(
        &pool,
        GUILD,
        GIVEAWAY,
        0,
        None,
        VECTOR_SEED_0,
        &first_winners,
    )
    .await;
    // Two workers racing the same round collide on the draw PK: the second
    // draw of the same round is impossible even if both pass the lease. The
    // winner_ids cast follows the live storage type so the PK refusal is
    // asserted on both TEXT and native-JSON storage.
    let winners_cast = if is_json_column(&pool, "giveaway_draws", "winner_ids").await {
        "$6::jsonb"
    } else {
        "$6"
    };
    // Audit for the AssertSqlSafe wrap below: the only interpolation is the
    // static cast fragment chosen above — no test input reaches SQL text.
    let raced = sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO giveaway_draws
           (guild_id, giveaway_id, draw_no, drawn_at, eligible_count,
            eligible_hash, seed, winner_ids, reroll_kind)
         VALUES ($1, $2, 0, $3::timestamptz, 10, $4, $5, {winners_cast}, NULL)"
    )))
    .bind(GUILD)
    .bind(GIVEAWAY)
    .bind(STAMP)
    .bind(VECTOR_ELIGIBLE_HASH)
    .bind(VECTOR_SEED_0)
    .bind(&first_winners)
    .execute(&pool)
    .await
    .expect_err("second draw of round 0 must fail");
    assert_sqlstate(&raced, "23505");
    // Rerolls insert new rounds, never update: full then single.
    let reroll_winners = serde_json::to_string(&VECTOR_REROLL_WINNERS).expect("winners JSON");
    insert_draw(
        &pool,
        GUILD,
        GIVEAWAY,
        1,
        Some("full"),
        VECTOR_SEED_1,
        &reroll_winners,
    )
    .await;
    let single_winners = serde_json::to_string(&["100000000000000008"]).expect("winners JSON");
    insert_draw(
        &pool,
        GUILD,
        GIVEAWAY,
        2,
        Some(&reroll_kind_single("100000000000000001")),
        VECTOR_SEED_1,
        &single_winners,
    )
    .await;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaway_draws WHERE guild_id = $1 AND giveaway_id = $2",
    )
    .bind(GUILD)
    .bind(GIVEAWAY)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 3, "first draw plus two reroll rows");
    // The first draw row is intact with its replay inputs.
    let stored: (String, String, String) =
        if is_json_column(&pool, "giveaway_draws", "winner_ids").await {
            sqlx::query_as(
                "SELECT eligible_hash, seed, winner_ids::text FROM giveaway_draws
              WHERE guild_id = $1 AND giveaway_id = $2 AND draw_no = 0",
            )
            .bind(GUILD)
            .bind(GIVEAWAY)
            .fetch_one(&pool)
            .await?
        } else {
            sqlx::query_as(
                "SELECT eligible_hash, seed, winner_ids FROM giveaway_draws
              WHERE guild_id = $1 AND giveaway_id = $2 AND draw_no = 0",
            )
            .bind(GUILD)
            .bind(GIVEAWAY)
            .fetch_one(&pool)
            .await?
        };
    assert_eq!(stored.0, VECTOR_ELIGIBLE_HASH);
    assert_eq!(stored.1, VECTOR_SEED_0);
    let stored_winners: serde_json::Value =
        serde_json::from_str(&stored.2).expect("stored winners parse as JSON");
    assert_eq!(
        stored_winners,
        serde_json::to_value(VECTOR_WINNERS_10_OF_2).expect("winners value")
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn giveaway_claim_lease_columns_support_draw_ticker_claim() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("giveaway_claim_lease_columns_support_draw_ticker_claim").await
    else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    require_tables(&pool).await;
    insert_giveaway(&pool, GUILD, GIVEAWAY).await;
    let fresh: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT claim_token, claimed_at::text, announced_at::text FROM giveaways
          WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(GIVEAWAY)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        fresh,
        (None, None, None),
        "a fresh giveaway holds no claim and still owes its announcement"
    );
    // Claim, then release: the lease round-trips through the two columns.
    sqlx::query(
        "UPDATE giveaways SET claim_token = $3, claimed_at = $4::timestamptz WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(GIVEAWAY)
    .bind("lease-token-1")
    .bind(STAMP)
    .execute(&pool)
    .await?;
    let claimed: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT claim_token, claimed_at::text FROM giveaways WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(GIVEAWAY)
    .fetch_one(&pool)
    .await?;
    assert_eq!(claimed.0.as_deref(), Some("lease-token-1"));
    assert!(
        claimed
            .1
            .as_deref()
            .is_some_and(|at| at.starts_with("2026-10-21")),
        "claimed_at round-trips, got {claimed:?}"
    );
    sqlx::query("UPDATE giveaways SET claim_token = NULL, claimed_at = NULL WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(GIVEAWAY)
        .execute(&pool)
        .await?;
    let released: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT claim_token, claimed_at::text FROM giveaways WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(GIVEAWAY)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        released,
        (None, None),
        "releasing the claim clears the lease"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn giveaway_migration_reruns_cleanly() -> Result<(), sqlx::Error> {
    let Some(fixture) = connect("giveaway_migration_reruns_cleanly").await else {
        return Ok(());
    };
    let pool = fixture.pool().clone();
    require_tables(&pool).await;
    // Audit for the AssertSqlSafe wraps below: the SQL is the repository's own
    // committed cutover migration file(s) discovered at runtime — no test
    // input, env or network content reaches SQL text. Re-running proves the
    // IF NOT EXISTS convention at runtime: cross-running the store and
    // cutover chains against one schema stays safe in either order.
    let mut reran = 0;
    for (file, _) in giveaway_migrations(CHAINS[0].1) {
        let contents = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(CHAINS[0].1)
                .join(&file),
        )
        .expect("read giveaway migration");
        for _ in 0..2 {
            sqlx::raw_sql(sqlx::AssertSqlSafe(contents.clone()))
                .execute(&pool)
                .await
                .expect("giveaway migration re-runs cleanly");
        }
        reran += 1;
    }
    assert!(reran > 0, "no giveaway migration to re-run");
    fixture.close().await.expect("drop test database");
    Ok(())
}
