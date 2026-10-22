//! Acceptance: `web_v1` voice columns + privacy (SPEC card 4, R15/R16/R17).
//!
//! Failing-first contract for the view card. The bot-owned `web_v1` view is
//! the website's only voice-channel source, so every property below is pinned
//! here before the Builder writes the migration, the view edit, or the
//! CHANNEL_UPDATE/DELETE sync. The Builder builds on this card's branch and
//! never edits this file.
//!
//! Contract (from the epic spec + the view card):
//! - `web_v1.upcoming_events` (and `next_event` via `SELECT *`) exposes
//!   `voice_channel_id, voice_channel_name, state, after_party, tbd_reason`,
//!   appended after the five legacy columns, never reordered or retyped.
//! - A rename write is visible on the very next view read (the CHANNEL_UPDATE
//!   half); a delete-clear write removes the link on the next read (the
//!   CHANNEL_DELETE half). The gateway arms must produce exactly these writes.
//! - The public shape carries no identity or occupancy: no user/member ids, no
//!   occupant names/counts, no owner id (R16/R17).
//! - The TBD vocabulary is exactly `opens_soon, rate_limited, perm_denied,
//!   no_category, unavailable`; unknown reasons never render.
//! - The link is keyed by the `event_voice_channels` row + flag, never by
//!   parsing channel names (R15).
//! - The migration is locked; the role matrix covers the new tables; the web
//!   reader reads the view but never the base tables.
//!
//! Test-author pins (the spec is silent here; these choices are binding):
//! - `state` vocabulary is `tbd | live | ended` (lowercase, matching the
//!   mirror `status` style). After-party display is `ended + after_party`;
//!   hidden is exclusion from the view. The table column (if any) and the view
//!   agree on these values for the arranged cases, so both pass-through and
//!   view-side derivation satisfy the tests.
//! - An event with no `event_voice_channels` row serves `tbd / opens_soon`.
//! - A cleared link on a live event serves `tbd / unavailable`.
//! - `after_party` is BOOLEAN, the voice ids/names/reason are TEXT.
//! - `event_voice_channels` carries `voice_channel_id, voice_channel_name,
//!   after_party, tbd_reason` (plus card 1's `event_id, instance_starts_at`
//!   key). `state` may live on the table or be derived in the view; every
//!   other card-1 column is that card's business and is tolerated here.
//! - Enforcement of the TBD vocabulary is a CHECK constraint or view-level
//!   normalization: Rust-only validation cannot fence direct SQL writers.
//!
//! RED map (all fail before the Builder lands, for the named reason):
//! - O1/O3/O6/O7: the table migration, view edit, matrix rows and sync path
//!   do not exist yet.
//! - D1-D8, D10-D12: the view columns / tables do not exist yet.
//! - O2/O4/O5/O8, D9 (+ the hidden half of D12): controls that pass now and
//!   must stay green.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Shared offline helpers (no database).
// ---------------------------------------------------------------------------

/// Migration chains in apply order, mirroring `database_roles.rs`.
const CHAINS: [(&str, &str); 2] = [
    ("cutover", "../cutover/migrations"),
    ("store", "../store/migrations"),
];

/// Both copies of the contract SQL. They are executable-identical (O4); the
/// Builder must edit both.
const VIEW_SQL_FILES: [&str; 2] = ["../../sql/web_v1.sql", "../store/sql/web_v1.sql"];

/// Rust source roots that may hold the channel-sync write path.
const RUST_SRC_DIRS: [&str; 3] = ["../bot/src", "../core/src", "../store/src"];

fn sql_files(chain_dir: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(chain_dir);
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
/// marker inside a string literal never eats executable SQL. Ported from
/// `epoch_claim_migration_acceptance.rs`: probes must see only executable
/// SQL, so a comment naming a forbidden verb must not trip the probe.
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
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// Whole-word token scan so `updated_at` never trips a `update` probe.
fn sql_words(body: &str) -> Vec<&str> {
    body.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .collect()
}

fn read_view_sql(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).expect("read contract SQL")
}

/// The `upcoming_events` CREATE VIEW statement, comment-stripped. The view
/// body holds no inner `;`, so the statement ends at the first semicolon.
fn upcoming_statement(sql: &str) -> &str {
    let stripped = strip_sql_comments(sql);
    // Deliberately leaked: the region borrow must outlive this helper and the
    // probe runs once per test process.
    let owned: &'static str = Box::leak(stripped.into_boxed_str());
    let anchor = owned
        .find("upcoming_events")
        .expect("upcoming_events view exists");
    let start = owned[..anchor].rfind("CREATE").unwrap_or(0);
    let rest = &owned[start..];
    let end = rest.find(';').map(|i| start + i).unwrap_or(owned.len());
    &owned[start..end]
}

/// Recursive `.rs` files under a source root, excluding test modules. Test
/// files must never satisfy a wiring probe: sibling acceptance files name the
/// same tables.
fn rust_files(root: &str) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(dir).expect("read source dir");
        for entry in entries {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                walk(&path, out);
                continue;
            }
            if !path.extension().is_some_and(|ext| ext == "rs") {
                continue;
            }
            let name = path
                .file_stem()
                .expect("file stem")
                .to_string_lossy()
                .into_owned();
            if name.ends_with("_tests")
                || name.ends_with("_test")
                || name.starts_with("test_")
                || name.contains("acceptance")
                || path.to_string_lossy().contains("/tests/")
            {
                continue;
            }
            out.push(path);
        }
    }
    let mut out = Vec::new();
    walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join(root), &mut out);
    out.sort();
    out
}

/// Naive Rust comment strip for existence probes only: `//` to end of line
/// plus `/* */` spans. A builder comment naming the sync must not satisfy the
/// wiring probe; mangling inside string literals is accepted and documented.
fn strip_rust_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        let next = bytes.get(i + 1).copied();
        if byte == b'/' && next == Some(b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if byte == b'/' && next == Some(b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        out.push(byte as char);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// O1: the table migration carries the voice columns.
// ---------------------------------------------------------------------------

#[test]
fn migration_creates_event_voice_channels_with_voice_columns() {
    // Card 1 owns the key/timestamptz/lease shape; this probe owns only what
    // the view card needs: the table exists with the id + name columns the
    // CHANNEL_UPDATE sync writes. RED until either builder lands.
    let mut checked = 0;
    for (name, dir) in CHAINS {
        for path in sql_files(dir) {
            let sql = std::fs::read_to_string(&path).expect("read migration");
            let body = normalize_sql(&strip_sql_comments(&sql)).to_lowercase();
            if !body.contains("event_voice_channels") {
                continue;
            }
            let file = path.file_name().expect("file name").to_string_lossy();
            for needle in [
                "create table",
                "event_voice_channels",
                "voice_channel_id",
                "voice_channel_name",
            ] {
                assert!(
                    body.contains(needle),
                    "{name}/{file}: event_voice migration must contain {needle}"
                );
            }
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "no migration creates event_voice_channels with voice columns"
    );
}

// ---------------------------------------------------------------------------
// O2: every migration is locked (control now, live guard for the builder).
// ---------------------------------------------------------------------------

#[test]
fn every_migration_is_locked_with_matching_checksum() {
    let lock_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations.lock");
    let lock_text = std::fs::read_to_string(&lock_path).expect("read migrations.lock");
    let lock: serde_json::Value = serde_json::from_str(&lock_text).expect("parse migrations.lock");
    assert_eq!(lock["version"], serde_json::json!(1));
    let mut checked = 0;
    for (name, dir) in CHAINS {
        for path in sql_files(dir) {
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
    assert!(checked > 0, "no migrations to check");
}

// ---------------------------------------------------------------------------
// O3: both contract-SQL copies expose the voice view columns.
// ---------------------------------------------------------------------------

#[test]
fn contract_sql_exposes_voice_view_columns() {
    for relative in VIEW_SQL_FILES {
        let sql = read_view_sql(relative);
        let body = normalize_sql(upcoming_statement(&sql)).to_lowercase();
        for needle in [
            "voice_channel_id",
            "voice_channel_name",
            "state",
            "after_party",
            "tbd_reason",
            "event_voice_channels",
        ] {
            assert!(
                body.contains(needle),
                "{relative}: upcoming_events must select {needle} from event_voice_channels"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// O4: the two contract copies stay executable-identical (control).
// ---------------------------------------------------------------------------

#[test]
fn contract_sql_copies_stay_executable_identical() {
    // Normalized comparison: the two headers carry different comments (and
    // therefore different blank runs once stripped), so only the token
    // stream must match. Any executable drift still fails loudly.
    let bodies: Vec<String> = VIEW_SQL_FILES
        .iter()
        .map(|relative| normalize_sql(&strip_sql_comments(&read_view_sql(relative))))
        .collect();
    assert_eq!(
        bodies[0], bodies[1],
        "contract SQL copies diverged: edit sql/web_v1.sql and crates/store/sql/web_v1.sql together"
    );
}

// ---------------------------------------------------------------------------
// O5: contract SQL is CREATE OR REPLACE only, so any version re-applies
// cleanly over any other (that re-applicability IS the view's reversibility).
// ---------------------------------------------------------------------------

#[test]
fn contract_sql_uses_create_or_replace_only() {
    for relative in VIEW_SQL_FILES {
        let body = normalize_sql(&strip_sql_comments(&read_view_sql(relative))).to_lowercase();
        let words = sql_words(&body);
        for verb in ["alter", "drop", "truncate", "delete", "update"] {
            assert!(
                !words.contains(&verb),
                "{relative}: contract SQL must never {verb}"
            );
        }
        assert!(
            body.contains("create or replace view web_v1.upcoming_events"),
            "{relative}: upcoming_events must be CREATE OR REPLACE"
        );
    }
}

// ---------------------------------------------------------------------------
// O6: the role matrix covers the new tables.
// ---------------------------------------------------------------------------

#[test]
fn role_matrix_covers_event_voice_tables() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sql/database_role_matrix.sql");
    let matrix = std::fs::read_to_string(&path).expect("read role matrix");
    for table in ["event_voice_channels", "event_voice_presence"] {
        assert!(
            matrix.contains(table),
            "role matrix has no row for {table}; the builder registers both tables"
        );
    }
}

// ---------------------------------------------------------------------------
// O7: the Rust sync path exists on both sides of the dispatch boundary.
// ---------------------------------------------------------------------------

#[test]
fn rust_references_the_event_voice_channel_sync() {
    // Wiring-existence tripwire, placement-agnostic by design: the rename /
    // clear write must live in Rust sqlx (any of bot/core/store), and the bot
    // dispatch side must reference the event-voice surface (the gateway
    // CHANNEL_UPDATE/DELETE arms are bot-owned per spec 1.5). The DB tests
    // prove the writes' effects; this proves the wiring exists.
    let mut table_refs: Vec<String> = Vec::new();
    let mut bot_refs: Vec<String> = Vec::new();
    for root in RUST_SRC_DIRS {
        for path in rust_files(root) {
            let source = std::fs::read_to_string(&path).expect("read rust source");
            let body = strip_rust_comments(&source).to_lowercase();
            let display = path
                .strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")).join(".."))
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if body.contains("event_voice_channels") {
                table_refs.push(display.clone());
            }
            if root == "../bot/src" && body.contains("event_voice") {
                bot_refs.push(display);
            }
        }
    }
    assert!(
        !table_refs.is_empty(),
        "no non-test Rust references event_voice_channels: the sync write path is missing"
    );
    assert!(
        !bot_refs.is_empty(),
        "no non-test bot source references event_voice: the gateway dispatch arm is missing"
    );
}

// ---------------------------------------------------------------------------
// O8: the view never matches channel names (R15, control now + guard later).
// ---------------------------------------------------------------------------

#[test]
fn view_sql_never_matches_channel_names() {
    for relative in VIEW_SQL_FILES {
        let body = normalize_sql(&strip_sql_comments(&read_view_sql(relative))).to_lowercase();
        assert!(
            !body.contains("after-party"),
            "{relative}: the view must not match the rename template"
        );
        assert!(
            !sql_words(&body).contains(&"like"),
            "{relative}: the view must key links by id/row, never LIKE on names"
        );
    }
}

// ---------------------------------------------------------------------------
// DB tests (need `--features two-bot-core/db` + TWO_TEST_DATABASE_URL).
// ---------------------------------------------------------------------------

#[cfg(feature = "db")]
mod db_tests {
    use super::sql_files;
    use sqlx::PgPool;
    use std::collections::HashSet;
    use two_bot_testsupport::TestDatabase;

    /// The shared TBD-reason vocabulary (spec optional-7). Binding and closed.
    const TBD_VOCABULARY: [&str; 5] = [
        "opens_soon",
        "rate_limited",
        "perm_denied",
        "no_category",
        "unavailable",
    ];

    const GUILD: &str = "326474832151838730";
    const OBSERVED_AT: &str = "2026-08-25T20:00:00.000Z";

    /// Pinned `state` vocabulary served by the view (test-author decision;
    /// the spec names the website states but not their stored values).
    const STATE_TBD: &str = "tbd";
    const STATE_LIVE: &str = "live";
    const STATE_ENDED: &str = "ended";

    /// Exact `upcoming_events` / `next_event` shape: the five legacy columns
    /// first, in order, then the five voice columns. This exact list IS the
    /// additive-only guard.
    const EXPECTED_COLUMNS: [&str; 10] = [
        "event_id",
        "name",
        "starts_at",
        "channel_id",
        "description",
        "voice_channel_id",
        "voice_channel_name",
        "state",
        "after_party",
        "tbd_reason",
    ];

    /// Column-name fragments that must never appear on the public views
    /// (R16/R17). Checked against the served columns, so the required voice
    /// columns must — and do — pass this matcher.
    const FORBIDDEN_FRAGMENTS: [&str; 12] = [
        "owner",
        "user",
        "member",
        "occup",
        "presence",
        "count",
        "creator",
        "author",
        "login",
        "email",
        "avatar",
        "discriminator",
    ];

    fn db_url() -> Option<String> {
        match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => Some(url),
            Err(_) if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") => {
                panic!("TWO_TEST_DATABASE_URL is required in CI")
            }
            Err(_) => None, // Offline: the O-probes still run.
        }
    }

    /// Scratch database with BOTH migration chains plus the `web_v1` views.
    /// Both chains apply so the tests pass wherever the builder's migration
    /// lands; a new migration file is picked up without a hardcoded list.
    async fn fixture(url: &str) -> TestDatabase {
        let fixture = TestDatabase::create(url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated test database");
        for path in sql_files("../store/migrations") {
            let sql = std::fs::read_to_string(&path).expect("read store migration");
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(fixture.pool())
                .await
                .expect("apply store migration");
        }
        two_bot_core::apply_web_contract(fixture.pool())
            .await
            .expect("apply web_v1 views");
        fixture
    }

    fn future_iso(days: i64) -> String {
        let t = time::OffsetDateTime::now_utc() + time::Duration::days(days);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute(),
            t.second(),
            t.nanosecond() / 1_000_000
        )
    }

    async fn table_columns(pool: &PgPool, table: &str) -> HashSet<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT column_name FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = $1",
        )
        .bind(table)
        .fetch_all(pool)
        .await
        .expect("read table columns")
        .into_iter()
        .collect()
    }

    async fn seed_mirror_event(
        pool: &PgPool,
        event_id: &str,
        starts_at: &str,
        status: &str,
        channel_id: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO scheduled_events
               (guild_id, event_id, name, starts_at, channel_id, description, status, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(GUILD)
        .bind(event_id)
        .bind(format!("Event {event_id}"))
        .bind(starts_at)
        .bind(channel_id)
        .bind(format!("Description for {event_id}"))
        .bind(status)
        .bind(OBSERVED_AT)
        .execute(pool)
        .await
        .expect("seed mirror event");
    }

    /// Link (or TBD-mark) an event in `event_voice_channels`. Only columns
    /// present on the table are written, so card-1 shape choices the view
    /// card does not own can never break these tests — but the six columns
    /// the view contract needs are asserted present with explicit errors.
    /// Returns the write outcome so the invalid-reason test can observe a
    /// CHECK rejection instead of asserting success.
    #[allow(clippy::too_many_arguments)]
    async fn exec_link(
        pool: &PgPool,
        event_id: &str,
        instance_starts_at: &str,
        channel_id: Option<&str>,
        channel_name: Option<&str>,
        state: Option<&str>,
        after_party: Option<bool>,
        tbd_reason: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let present = table_columns(pool, "event_voice_channels").await;
        for required in [
            "event_id",
            "instance_starts_at",
            "voice_channel_id",
            "voice_channel_name",
            "after_party",
            "tbd_reason",
        ] {
            assert!(
                present.contains(required),
                "event_voice_channels.{required} is missing: the card-1 migration and the view contract disagree"
            );
        }
        // (column, cast, text value, bool value): exactly one of the values
        // is inspected per column kind.
        let mut cells: Vec<(&str, &str, Option<String>, Option<bool>)> = vec![
            ("event_id", "", Some(event_id.to_owned()), None),
            (
                "instance_starts_at",
                "::timestamptz",
                Some(instance_starts_at.to_owned()),
                None,
            ),
            ("voice_channel_id", "", channel_id.map(str::to_owned), None),
            (
                "voice_channel_name",
                "",
                channel_name.map(str::to_owned),
                None,
            ),
            ("state", "", state.map(str::to_owned), None),
            ("after_party", "::boolean", None, after_party),
            ("tbd_reason", "", tbd_reason.map(str::to_owned), None),
            // Card-1-owned columns: supplied when present so NOT NULL shapes
            // keep working; never asserted here.
            (
                "starts_at",
                "::timestamptz",
                Some(instance_starts_at.to_owned()),
                None,
            ),
            ("ends_at", "::timestamptz", None, None),
            ("owner_tick_lease", "", None, None),
        ];
        cells.retain(|(column, _, _, _)| present.contains(*column));
        for (column, _, _, _) in &cells {
            assert!(
                column
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "unsafe catalog column name: {column}"
            );
        }
        let columns: Vec<String> = cells
            .iter()
            .map(|(c, _, _, _)| format!("\"{c}\""))
            .collect();
        let values: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, (_, cast, _, _))| format!("${}${cast}", i + 1))
            .collect();
        let sql = format!(
            "INSERT INTO event_voice_channels ({}) VALUES ({})",
            columns.join(", "),
            values.join(", ")
        );
        // Audit: column identifiers above are restricted to [a-z0-9_] and
        // double-quoted; all values go through bind parameters, so the
        // assembled statement cannot carry injected SQL.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for (_, _, text, flag) in &cells {
            if text.is_some() || flag.is_none() {
                query = query.bind(text.clone());
            } else {
                query = query.bind(*flag);
            }
        }
        query.execute(pool).await.map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    async fn link_voice_row(
        pool: &PgPool,
        event_id: &str,
        instance_starts_at: &str,
        channel_id: Option<&str>,
        channel_name: Option<&str>,
        state: Option<&str>,
        after_party: Option<bool>,
        tbd_reason: Option<&str>,
    ) {
        exec_link(
            pool,
            event_id,
            instance_starts_at,
            channel_id,
            channel_name,
            state,
            after_party,
            tbd_reason,
        )
        .await
        .expect("link voice row");
    }

    type VoiceRow = (Option<String>, Option<String>, String, bool, Option<String>);

    async fn read_voice(pool: &PgPool, event_id: &str) -> Option<VoiceRow> {
        sqlx::query_as::<_, VoiceRow>(
            "SELECT voice_channel_id, voice_channel_name, state, after_party, tbd_reason
             FROM web_v1.upcoming_events WHERE event_id = $1",
        )
        .bind(event_id)
        .fetch_optional(pool)
        .await
        .expect("view must expose the voice columns")
    }

    async fn view_shape(pool: &PgPool, view: &str) -> Vec<(String, String)> {
        sqlx::query_as::<_, (String, String)>(
            "SELECT column_name, data_type FROM information_schema.columns
             WHERE table_schema = 'web_v1' AND table_name = $1
             ORDER BY ordinal_position",
        )
        .bind(view)
        .fetch_all(pool)
        .await
        .expect("read view shape")
    }

    async fn view_definition(pool: &PgPool, view: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT pg_get_viewdef($1::regclass, true)")
            .bind(format!("web_v1.{view}"))
            .fetch_one(pool)
            .await
            .expect("read view definition")
    }

    #[tokio::test]
    async fn live_event_serves_voice_columns() {
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        let starts_at = future_iso(7);
        seed_mirror_event(&pool, "evt-live", &starts_at, "scheduled", Some("111"));
        link_voice_row(
            &pool,
            "evt-live",
            &starts_at,
            Some("111"),
            Some("Friday 8s"),
            Some(STATE_LIVE),
            Some(false),
            None,
        )
        .await;
        assert_eq!(
            read_voice(&pool, "evt-live").await,
            Some((
                Some("111".to_owned()),
                Some("Friday 8s".to_owned()),
                STATE_LIVE.to_owned(),
                false,
                None,
            ))
        );
        let next: Option<(String,)> = sqlx::query_as("SELECT event_id FROM web_v1.next_event")
            .fetch_optional(&pool)
            .await
            .expect("next view");
        assert_eq!(next.map(|(id,)| id), Some("evt-live".to_owned()));
        fixture.close().await.expect("drop test database");
    }

    #[tokio::test]
    async fn unlinked_event_serves_tbd_with_opens_soon() {
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        let starts_at = future_iso(7);
        seed_mirror_event(&pool, "evt-tbd", &starts_at, "scheduled", None);
        assert_eq!(
            read_voice(&pool, "evt-tbd").await,
            Some((
                None,
                None,
                STATE_TBD.to_owned(),
                false,
                Some("opens_soon".to_owned())
            ))
        );
        fixture.close().await.expect("drop test database");
    }

    #[tokio::test]
    async fn linked_tbd_event_serves_each_known_reason() {
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        for (i, reason) in TBD_VOCABULARY.into_iter().enumerate() {
            let event_id = format!("evt-tbd-{i}");
            let starts_at = future_iso(7);
            seed_mirror_event(&pool, &event_id, &starts_at, "scheduled", None);
            link_voice_row(
                &pool,
                &event_id,
                &starts_at,
                None,
                None,
                Some(STATE_TBD),
                Some(false),
                Some(reason),
            )
            .await;
            assert_eq!(
                read_voice(&pool, &event_id).await,
                Some((
                    None,
                    None,
                    STATE_TBD.to_owned(),
                    false,
                    Some(reason.to_owned()),
                )),
                "tbd reason {reason} must round-trip through the view"
            );
        }
        fixture.close().await.expect("drop test database");
    }

    #[tokio::test]
    async fn unknown_tbd_reason_is_rejected_or_normalized() {
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        let starts_at = future_iso(7);
        seed_mirror_event(&pool, "evt-bogus", &starts_at, "scheduled", None);
        // A full consistent row varying only the reason, so a rejection can
        // only be the vocabulary CHECK. A missing table/column panics inside
        // exec_link with the schema-missing message (the RED signal).
        let outcome = exec_link(
            &pool,
            "evt-bogus",
            &starts_at,
            None,
            None,
            Some(STATE_TBD),
            Some(false),
            Some("bogus_reason_zzz"),
        )
        .await;
        match outcome {
            Err(error) => {
                let code = error.as_database_error().and_then(|e| e.code());
                assert_eq!(
                    code.as_deref(),
                    Some("23514"),
                    "want the vocabulary CHECK (23514), got: {error:?}"
                );
            }
            Ok(_) => {
                let served = read_voice(&pool, "evt-bogus").await;
                let reason = served
                    .expect("row must stay served")
                    .4
                    .expect("reason must stay served");
                assert!(
                    TBD_VOCABULARY.contains(&reason.as_str()),
                    "unknown reason leaked to the view: {reason}"
                );
            }
        }
        fixture.close().await.expect("drop test database");
    }

    #[tokio::test]
    async fn channel_rename_is_visible_on_the_next_view_read() {
        // The CHANNEL_UPDATE half: the gateway arm performs the rename write;
        // the view serves it on the very next read ("within one tick"), with
        // no second write and no deploy.
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        let starts_at = future_iso(7);
        seed_mirror_event(&pool, "evt-rename", &starts_at, "active", Some("222"));
        link_voice_row(
            &pool,
            "evt-rename",
            &starts_at,
            Some("222"),
            Some("Before rename"),
            Some(STATE_LIVE),
            Some(false),
            None,
        )
        .await;
        sqlx::query("UPDATE event_voice_channels SET voice_channel_name = $1 WHERE event_id = $2")
            .bind("After rename")
            .bind("evt-rename")
            .execute(&pool)
            .await
            .expect("the CHANNEL_UPDATE write");
        assert_eq!(
            read_voice(&pool, "evt-rename").await,
            Some((
                Some("222".to_owned()),
                Some("After rename".to_owned()),
                STATE_LIVE.to_owned(),
                false,
                None,
            ))
        );
        fixture.close().await.expect("drop test database");
    }

    #[tokio::test]
    async fn channel_delete_clears_the_link() {
        // The CHANNEL_DELETE half: the gateway arm clears the link; the next
        // read shows no stale id or name, back to TBD with `unavailable`.
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        let starts_at = future_iso(7);
        seed_mirror_event(&pool, "evt-delete", &starts_at, "scheduled", Some("333"));
        link_voice_row(
            &pool,
            "evt-delete",
            &starts_at,
            Some("333"),
            Some("Doomed room"),
            Some(STATE_LIVE),
            Some(false),
            None,
        )
        .await;
        sqlx::query(
            "UPDATE event_voice_channels
             SET voice_channel_id = NULL, voice_channel_name = NULL,
                 state = $1, after_party = FALSE, tbd_reason = $2
             WHERE event_id = $3",
        )
        .bind(STATE_TBD)
        .bind("unavailable")
        .bind("evt-delete")
        .execute(&pool)
        .await
        .expect("the CHANNEL_DELETE write");
        assert_eq!(
            read_voice(&pool, "evt-delete").await,
            Some((
                None,
                None,
                STATE_TBD.to_owned(),
                false,
                Some("unavailable".to_owned()),
            ))
        );
        fixture.close().await.expect("drop test database");
    }

    #[tokio::test]
    async fn arbitrary_rename_preserves_the_link() {
        // R15: the link is keyed by row/id, never by parsing names. A mod
        // rename to an unrelated name — or to a colliding template lookalike
        // — must not break or re-route the link, and the bot never renames
        // back (the view keeps serving exactly what was written).
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        let starts_at = future_iso(7);
        seed_mirror_event(&pool, "evt-r15", &starts_at, "active", Some("444"));
        link_voice_row(
            &pool,
            "evt-r15",
            &starts_at,
            Some("444"),
            Some("Original name"),
            Some(STATE_LIVE),
            Some(false),
            None,
        )
        .await;
        for name in [
            "lobby-remake-2",
            "after-party \u{b7} friday-8s \u{b7} 0606-1900",
        ] {
            sqlx::query(
                "UPDATE event_voice_channels SET voice_channel_name = $1 WHERE event_id = $2",
            )
            .bind(name)
            .bind("evt-r15")
            .execute(&pool)
            .await
            .expect("rename write");
            assert_eq!(
                read_voice(&pool, "evt-r15").await,
                Some((
                    Some("444".to_owned()),
                    Some(name.to_owned()),
                    STATE_LIVE.to_owned(),
                    false,
                    None,
                )),
                "rename to {name:?} must preserve the id-keyed link"
            );
        }
        fixture.close().await.expect("drop test database");
    }

    #[tokio::test]
    async fn public_view_shape_contains_no_identity_or_occupancy() {
        // R16/R17: exact shape (this assertion IS the additive-only guard for
        // the new columns) plus the privacy contract on served names and on
        // the view definition itself.
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        for view in ["upcoming_events", "next_event"] {
            let shape = view_shape(&pool, view).await;
            let names: Vec<&str> = shape.iter().map(|(name, _)| name.as_str()).collect();
            assert_eq!(
                names,
                EXPECTED_COLUMNS.to_vec(),
                "shape drift on web_v1.{view}: additive appends only"
            );
            let types: Vec<&str> = shape.iter().map(|(_, ty)| ty.as_str()).collect();
            assert_eq!(
                types,
                vec![
                    "text", "text", "text", "text", "text", "text", "text", "text", "boolean",
                    "text"
                ],
                "type drift on web_v1.{view}"
            );
            for name in &names {
                let lower = name.to_lowercase();
                for fragment in FORBIDDEN_FRAGMENTS {
                    assert!(
                        !lower.contains(fragment),
                        "web_v1.{view}.{name} leaks identity/occupancy ({fragment})"
                    );
                }
            }
            let definition = view_definition(&pool, view).await.to_lowercase();
            for needle in [
                "event_voice_presence",
                "owner_id",
                "user_id",
                "member_id",
                "occupant",
                "after-party",
            ] {
                assert!(
                    !definition.contains(needle),
                    "web_v1.{view} definition references {needle}"
                );
            }
        }
        fixture.close().await.expect("drop test database");
    }

    #[tokio::test]
    async fn legacy_columns_keep_position_and_passthrough_types() {
        // Additive-only control: the five legacy columns stay first, in
        // order, passing the mirror's types through untouched.
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        let shape = view_shape(&pool, "upcoming_events").await;
        let names: Vec<&str> = shape.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            &names[..5],
            &EXPECTED_COLUMNS[..5],
            "legacy columns must stay first, in order"
        );
        let mirror: Vec<(String, String)> = sqlx::query_as(
            "SELECT column_name, data_type FROM information_schema.columns
             WHERE table_schema = 'public' AND table_name = 'scheduled_events'",
        )
        .fetch_all(&pool)
        .await
        .expect("read mirror types");
        let mirror_type = |column: &str| {
            mirror
                .iter()
                .find(|(name, _)| name == column)
                .map(|(_, ty)| ty.as_str())
        };
        for (name, ty) in shape.iter().take(5) {
            assert_eq!(
                Some(ty.as_str()),
                mirror_type(name),
                "web_v1.upcoming_events.{name} must pass the mirror type through"
            );
        }
        fixture.close().await.expect("drop test database");
    }

    #[tokio::test]
    async fn reader_reads_voice_columns_but_not_base_tables() {
        // Role-matrix surface for the new columns: the web reader selects the
        // voice columns through the views and is denied the base tables, so
        // no future column can leak around the view.
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        sqlx::raw_sql("CREATE TABLE public._sqlx_migrations (version bigint PRIMARY KEY);")
            .execute(&pool)
            .await
            .expect("ledger table");
        sqlx::raw_sql("CREATE TABLE public._two_bot_migrations (version bigint PRIMARY KEY);")
            .execute(&pool)
            .await
            .expect("ledger table");
        let roles = role_names();
        let plan = two_bot_core::database_roles::plan();
        execute(&pool, isolated(&plan, &roles)).await;
        execute(&pool, isolated(&plan, &roles)).await;
        let outcome: Result<(), String> = async {
            for view in ["upcoming_events", "next_event"] {
                as_role(
                    &pool,
                    &roles[2],
                    &format!(
                        "SELECT voice_channel_id, voice_channel_name, state, after_party, tbd_reason \
                         FROM web_v1.{view}"
                    ),
                )
                .await
                .map_err(|error| format!("reader cannot read web_v1.{view} voice columns: {error:?}"))?;
            }
            for table in [
                "event_voice_channels",
                "event_voice_presence",
                "scheduled_events",
            ] {
                denied(&pool, &roles[2], &format!("SELECT * FROM public.{table}"))
                    .await
                    .map_err(|error| format!("reader reached public.{table}: {error:?}"))?;
            }
            Ok(())
        }
        .await;
        for role in &roles {
            let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP ROLE IF EXISTS {role}")))
                .execute(&pool)
                .await;
        }
        outcome.expect("role probes");
        fixture.close().await.expect("drop test database");
    }

    #[tokio::test]
    async fn contract_reapplies_idempotently_with_voice_columns() {
        // Reversibility: any contract version re-applies cleanly (O5 pins no
        // DROP), so a rollback is just the previous file. Re-applying must
        // keep the voice columns and their data.
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        let starts_at = future_iso(7);
        seed_mirror_event(&pool, "evt-reapply", &starts_at, "scheduled", Some("555"));
        link_voice_row(
            &pool,
            "evt-reapply",
            &starts_at,
            Some("555"),
            Some("Sticky room"),
            Some(STATE_LIVE),
            Some(false),
            None,
        )
        .await;
        for _ in 0..2 {
            two_bot_core::apply_web_contract(&pool)
                .await
                .expect("re-apply contract");
            let shape = view_shape(&pool, "upcoming_events").await;
            let names: Vec<&str> = shape.iter().map(|(name, _)| name.as_str()).collect();
            assert_eq!(names, EXPECTED_COLUMNS.to_vec(), "columns lost on re-apply");
            assert_eq!(
                read_voice(&pool, "evt-reapply").await.map(|row| row.1),
                Some(Some("Sticky room".to_owned())),
                "data lost on re-apply"
            );
        }
        fixture.close().await.expect("drop test database");
    }

    #[tokio::test]
    async fn ended_after_party_row_stays_visible_until_the_room_goes() {
        // Website state 3 ("Event ended · after-party in <channel>") is only
        // renderable if the view keeps ended rows while the after-party flag
        // is set; without the flag an ended event stays hidden (control half).
        let Some(url) = db_url() else { return };
        let fixture = fixture(&url).await;
        let pool = fixture.pool().clone();
        let starts_at = future_iso(-1);
        seed_mirror_event(&pool, "evt-party", &starts_at, "completed", Some("666"));
        link_voice_row(
            &pool,
            "evt-party",
            &starts_at,
            Some("666"),
            Some("After-party room"),
            Some(STATE_ENDED),
            Some(true),
            None,
        )
        .await;
        assert_eq!(
            read_voice(&pool, "evt-party").await,
            Some((
                Some("666".to_owned()),
                Some("After-party room".to_owned()),
                STATE_ENDED.to_owned(),
                true,
                None,
            ))
        );
        seed_mirror_event(&pool, "evt-hidden", &starts_at, "completed", None);
        assert_eq!(read_voice(&pool, "evt-hidden").await, None);
        fixture.close().await.expect("drop test database");
    }

    // -- Least-privilege probe helpers (ported from database_roles.rs). ------

    static NEXT_ROLES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn role_names() -> Vec<String> {
        use std::sync::atomic::Ordering;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROLES.fetch_add(1, Ordering::Relaxed);
        let base = format!("evv{}_{stamp}_{sequence}", std::process::id());
        ["m", "b", "w", "r"]
            .map(|suffix| format!("{base}_{suffix}"))
            .to_vec()
    }

    fn isolated(sql: &str, roles: &[String]) -> String {
        sql.replace("two_bot_migrator_ro", &roles[3])
            .replace("two_bot_migrator", &roles[0])
            .replace("two_bot_runtime", &roles[1])
            .replace("two_web_reader", &roles[2])
    }

    async fn execute(pool: &PgPool, sql: String) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(pool)
            .await
            .expect("execute role sql");
    }

    async fn as_role(pool: &PgPool, role: &str, sql: &str) -> Result<(), sqlx::Error> {
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {role}; {sql}")))
            .execute(&mut *tx)
            .await?;
        tx.rollback().await?;
        Ok(())
    }

    async fn denied(pool: &PgPool, role: &str, sql: &str) -> Result<(), sqlx::Error> {
        match as_role(pool, role, sql).await {
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42501") => Ok(()),
            Ok(()) => Err(sqlx::Error::InvalidArgument(format!(
                "expected permission denial for {sql}"
            ))),
            Err(error) => Err(error),
        }
    }
}
