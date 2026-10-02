//! The v4 backup envelope (also reads v3): manifest / rows / end marker, gzipped NDJSON.
//!
//! Port of the file half of legacy `src/store/dump.ts`. The writer side lives
//! in [`super::dump`]; everything here touches no database, so every refusal
//! below is pinned by a test that runs without Postgres.
//!
//! ## The format
//!
//! Gzipped NDJSON, one JSON object per line, in three kinds:
//!
//! ```text
//! {"kind":"manifest", version, createdAt, tables:[{name,columns,columnTypes,count}], ...}
//! {"kind":"row", table, data:{...}}
//! {"kind":"end", rows}
//! ```
//!
//! Values are stored in Postgres text-output form (`SELECT col::text`, one
//! `Option<String>` per cell) with the column type recorded per table in the
//! manifest. Restore re-applies the value with a `$n::type` cast, so the
//! round trip is faithful for every type the bot owns (text, integers,
//! timestamptz, booleans, json/jsonb, bytea hex, arrays, enums, intervals)
//! without per-type Rust decoding. `column_types` is additive to legacy v3.
//! Frozen legacy dumps omit it and contain native numbers/booleans; restore
//! converts these to PostgreSQL input text using the target's column types.
//! JSON-looking TEXT stays unchanged; native JSON cells are serialized only
//! for a JSON/JSONB target. Unsupported composite cells refuse, never become NULL.

use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::path::Path;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

/// Durable bot-owned tables, parents before children. The first 22 names are
/// frozen v3 coverage; append new tables after that prefix. Tables retired from
/// the Rust migration set remain supported when present on a legacy target.
/// The bot-owned website-contract backing tables ARE included; derived views
/// and the website service's own database are not application data here.
pub const DUMP_TABLES: &[&str] = &[
    "events",
    "members",
    "invite_snapshots",
    "operational_audit_log",
    "moderation_warnings",
    "moderation_scheduled_unbans",
    "moderation_audit",
    "moderation_lockdowns",
    "moderation_idempotency",
    "containment_events",
    "containment_incidents",
    "join_risk_flags",
    "automation_commands",
    "scheduled_messages",
    "sticky_messages",
    "automation_audit_log",
    "tickets",
    "ticket_transcripts",
    "automod_violations",
    "automod_processed_messages",
    "self_role_audit",
    "self_role_panel_claims",
    "self_role_exchanges",
    "self_role_exchange_baselines",
    // Appended after the frozen v3 prefix: per-delivery automod arbitration
    // (0221 + counted/preserved-match columns in 0222/0223), one-shot gateway
    // boot directives (0321) and the erasure accountability log (0410). None
    // carries foreign keys, so order among them is free.
    "automod_delivery_claims",
    "gateway_boot_directives",
    "member_erasure_audit",
    "member_levels",
    "xp_awards",
    "level_role_rewards",
    "level_import_runs",
    "event_rsvps",
    "announcements_audit_log",
    "community_facts",
    "lfg_posts",
    "lfg_roles",
    "lfg_signups",
    "feed_relays",
    "feed_deliveries",
    "web_contract_meta",
    "guild_counters",
    "rank_ladder",
    "rank_snapshots",
    "member_ranks",
    "scheduled_events",
    "counter_snapshots",
    "member_exclusions",
    "presence_probe",
    "community_stream_heartbeats",
    "community_scorecard_runs",
    "community_scorecard_alerts",
    "gateway_sessions",
    "guild_settings_revision",
    "guild_settings",
    "guild_settings_audit",
    "audit_kill_switch",
    "internal_nonces",
    "internal_clock_high_water",
    "internal_idempotency",
    "internal_action_log",
    "internal_discord_events",
    "moderation_channel_executions",
];

/// Frozen v3 tables no longer created by cutover migrations. Keep their data
/// when they exist, but do not require nonexistent legacy subsystems on Rust.
pub const OPTIONAL_LEGACY_TABLES: &[&str] = &[
    "moderation_warnings",
    "moderation_scheduled_unbans",
    "containment_events",
    "containment_incidents",
    "automation_commands",
];

/// Explicit migrated-schema exclusions, checked by the schema coverage test.
pub const EXCLUDED_TABLES: &[&str] = &[
    // Short-lived XP award throttles, not XP totals/history. Never replay a
    // pre-restore cooldown into a recovered process.
    "xp_cooldowns",
    // Durable send admission is per-credential runtime lane state: occupancy
    // generations and Discord cooldown timing, not application data. Never
    // replay a pre-restore lane hold into a recovered process; a restored
    // database re-admits from generation zero and re-learns cooldowns.
    "discord_send_admission",
    // Migration ledgers describe target DDL; replacing them would falsely mark
    // unapplied migrations as applied. Legacy schema_migrations is diagnostic
    // manifest metadata only, never restored application data.
    "_sqlx_migrations",
    "schema_migrations",
];

/// Columns the destination allocates itself: never archived, never restored.
pub const DESTINATION_OWNED_COLUMNS: &[(&str, &str)] = &[
    // CAS tokens come from the never-reseeded guild_settings_cas_seq, and
    // trg_guild_settings_version replaces supplied tokens on every write. A
    // restore therefore allocates fresh tokens that invalidate every token
    // issued before it, as legacy copy does; archiving them would only record
    // values that restore cannot and must not reproduce.
    ("guild_settings", "cas_version"),
];

/// True when `table.column` is allocated by the destination, see
/// [`DESTINATION_OWNED_COLUMNS`].
#[must_use]
pub fn is_destination_owned(table: &str, column: &str) -> bool {
    DESTINATION_OWNED_COLUMNS
        .iter()
        .any(|(owned_table, owned_column)| *owned_table == table && *owned_column == column)
}

/// Write the complete-schema envelope; the reader also accepts frozen v3.
pub const DUMP_VERSION: u32 = 4;

/// A table name in the dump, validated against [`DUMP_TABLES`].
pub type DumpTable = String;

/// Per-table manifest entry: live column order, Postgres type per column
/// (`format_type()` spelling, used as the restore cast), and the row count
/// taken inside the same snapshot as the rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpTableInfo {
    pub name: String,
    pub columns: Vec<String>,
    /// Postgres type per column, parallel to `columns`. Absent on dumps
    /// written by legacy `two-bot`; restore then casts as `text`.
    #[serde(default)]
    pub column_types: Vec<String>,
    pub count: u64,
}

/// The leading manifest line of a dump.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpManifest {
    pub kind: String,
    pub version: u32,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub tables: Vec<DumpTableInfo>,
    /// So a restore can put the id sequence back where it belongs.
    #[serde(rename = "eventsSequence")]
    pub events_sequence: i64,
    /// Which migrations the source had applied, for diagnosing an old backup.
    #[serde(rename = "schemaMigrations")]
    pub schema_migrations: Vec<String>,
    /// Source allocator positions. Absent from v3 and from v4 archives written
    /// before marks existed; restore then knows only the restored rows and the
    /// target's own allocators, which cannot see deleted top rows.
    #[serde(
        rename = "sequenceMarks",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub sequence_marks: Vec<SequenceMark>,
}

/// A sequence's position when the dump was taken, keyed by the column it
/// feeds. Sequences are not MVCC, so the dump reads the live position: it is
/// at or beyond every value in the archived snapshot, never behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceMark {
    pub table: String,
    pub column: String,
    #[serde(rename = "lastValue")]
    pub last_value: i64,
    #[serde(rename = "isCalled")]
    pub is_called: bool,
    pub increment: i64,
}

impl SequenceMark {
    /// The value the source would have allocated next; `None` past `i64`.
    #[must_use]
    pub fn next(&self) -> Option<i64> {
        if self.is_called {
            self.last_value.checked_add(self.increment)
        } else {
            Some(self.last_value)
        }
    }
}

impl DumpManifest {
    /// The archived position of the allocator feeding `table.column`, if any.
    #[must_use]
    pub fn sequence_mark(&self, table: &str, column: &str) -> Option<&SequenceMark> {
        self.sequence_marks
            .iter()
            .find(|mark| mark.table == table && mark.column == column)
    }

    /// Old v3 archives predate complete table coverage. Restore clears these
    /// tables too, rather than silently retaining unrelated target contents.
    #[must_use]
    pub fn missing_tables(&self) -> Vec<&'static str> {
        DUMP_TABLES
            .iter()
            .copied()
            .filter(|name| !OPTIONAL_LEGACY_TABLES.contains(name))
            .filter(|name| !self.tables.iter().any(|t| t.name == *name))
            .collect()
    }
}

/// Maximum compressed AND decoded bytes. Files are streamed rather than
/// loading the compressed input alongside every retained row.
pub const MAX_DUMP_BYTES: u64 = 1024 * 1024 * 1024;
/// Check before growing the line buffer, including on highly compressible input.
pub const MAX_DUMP_LINE_BYTES: u64 = 8 * 1024 * 1024;
/// Conservative retained-value budget, counting keys and per-value overhead,
/// not merely their serialized bytes. Restore still buffers validated rows.
pub const MAX_DUMP_RETAINED_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
struct InspectLimits {
    compressed: u64,
    decoded: u64,
    line: u64,
    retained: u64,
}

const INSPECT_LIMITS: InspectLimits = InspectLimits {
    compressed: MAX_DUMP_BYTES,
    decoded: MAX_DUMP_BYTES,
    line: MAX_DUMP_LINE_BYTES,
    retained: MAX_DUMP_RETAINED_BYTES,
};

// Both paths count the newline, not just the JSON payload. Check before
// growing a buffer or handing another line to the compressor.
fn reserve_decoded_bytes(
    decoded: &mut u64,
    line_len: u64,
    added: u64,
    limits: InspectLimits,
) -> Result<(), DumpError> {
    if added > limits.line.saturating_sub(line_len) {
        return Err(refuse(format!(
            "dump decoded line exceeds {}-byte cap",
            limits.line
        )));
    }
    if added > limits.decoded.saturating_sub(*decoded) {
        return Err(refuse(format!(
            "dump decoded content exceeds {}-byte cap",
            limits.decoded
        )));
    }
    *decoded += added;
    Ok(())
}

fn read_capped_line(
    reader: &mut impl BufRead,
    decoded: &mut u64,
    limits: InspectLimits,
) -> Result<Option<Vec<u8>>, DumpError> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let n = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        reserve_decoded_bytes(decoded, line.len() as u64, n as u64, limits)?;
        let complete = bytes[n - 1] == b'\n';
        line.extend_from_slice(&bytes[..n]);
        reader.consume(n);
        if complete {
            return Ok(Some(line));
        }
    }
}

// Deliberately over-count value/map/vector bookkeeping so tiny native JSON
// nodes cannot evade the cumulative cap. Per-line input is bounded separately.
fn retained_weight(value: &Value) -> u64 {
    128 + match value {
        Value::String(s) => s.capacity() as u64,
        Value::Array(items) => items.iter().map(retained_weight).sum(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| k.capacity() as u64 + retained_weight(v))
            .sum(),
        _ => 0,
    }
}

fn retain_within_cap(retained: &mut u64, value: &Value, limit: u64) -> Result<(), DumpError> {
    *retained += retained_weight(value);
    if *retained > limit {
        return Err(refuse(format!(
            "dump retained data exceeds {limit}-byte cap"
        )));
    }
    Ok(())
}

fn ensure_within_size_cap(len: u64, what: &str) -> Result<(), DumpError> {
    if len > MAX_DUMP_BYTES {
        return Err(refuse(format!(
            "{what} is {len} bytes, past the {MAX_DUMP_BYTES}-byte dump cap; \
             refusing rather than buffering it"
        )));
    }
    Ok(())
}

/// A file-level refusal: the dump is not ours, truncated, or internally
/// inconsistent. Never a database error; see [`super::dump`] for those.
#[derive(Debug, Error)]
pub enum DumpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("gzip: {0}")]
    Gzip(String),
    #[error("json on line {line}: {message}")]
    Json { line: u64, message: String },
    #[error("{0}")]
    Refused(String),
}

/// True when `name` is a table this backup format owns.
///
/// The restore interpolates table names into SQL, and a backup file is not a
/// trusted input — bytes off a disk someone else may have written. Without
/// this gate a crafted dump naming `website_users` would be
/// truncated-and-inserted like one of ours.
#[must_use]
pub fn is_dump_table(name: &str) -> bool {
    DUMP_TABLES.contains(&name)
}

fn refuse(message: impl Into<String>) -> DumpError {
    DumpError::Refused(message.into())
}

/// Write one dump line (already-serialised JSON + newline) into the encoder.
pub fn write_line(enc: &mut GzEncoder<Vec<u8>>, value: &Value) -> Result<(), DumpError> {
    let mut line = serde_json::to_vec(value)
        .map_err(|e| refuse(format!("cannot serialise dump line: {e}")))?;
    line.push(b'\n');
    enc.write_all(&line)?;
    Ok(())
}

/// Finish a gzip stream and return the compressed bytes.
pub fn finish_gzip(enc: GzEncoder<Vec<u8>>) -> Result<Vec<u8>, DumpError> {
    enc.finish().map_err(|e| DumpError::Gzip(e.to_string()))
}

// Unlike the fixture helpers above, the live writer is bounded while encoding
// and never writes under a backup filename until the exact reader accepts it.
#[cfg(any(feature = "db", test))]
struct CappedWrite<W> {
    inner: W,
    written: u64,
    limit: u64,
    what: &'static str,
}

#[cfg(any(feature = "db", test))]
impl<W: Write> Write for CappedWrite<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.limit.saturating_sub(self.written) {
            return Err(std::io::Error::other(format!(
                "dump {} exceeds {}-byte cap",
                self.what, self.limit
            )));
        }
        let n = self.inner.write(bytes)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(any(feature = "db", test))]
struct TemporaryDump(std::path::PathBuf);

#[cfg(any(feature = "db", test))]
impl Drop for TemporaryDump {
    fn drop(&mut self) {
        // Error/cancellation unwinding removes only our exclusively-created
        // temporary. A killed process can leave a .tmp, never a backup candidate.
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Bounded, unpublished live archive. Dropping it never publishes a prefix.
#[cfg(any(feature = "db", test))]
pub(crate) struct DumpWriter {
    enc: GzEncoder<CappedWrite<std::fs::File>>,
    temporary: TemporaryDump,
    destination: std::path::PathBuf,
    decoded: u64,
    retained: u64,
    limits: InspectLimits,
}

#[cfg(any(feature = "db", test))]
impl DumpWriter {
    pub(crate) fn new(destination: &Path) -> Result<Self, DumpError> {
        Self::with_limits(destination, INSPECT_LIMITS)
    }

    fn with_limits(destination: &Path, limits: InspectLimits) -> Result<Self, DumpError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        loop {
            // Neither retention's two-funnel-*.ndjson.gz predicate nor the
            // restore drill's glob can match this name. Same directory = same FS.
            let path = parent.join(format!(
                ".dump-writing-{}-{stamp}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            // Exclusive creation also refuses symlinks (including dangling ones).
            // https://doc.rust-lang.org/std/fs/struct.OpenOptions.html#method.create_new
            let file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            };
            return Ok(Self {
                enc: GzEncoder::new(
                    CappedWrite {
                        inner: file,
                        written: 0,
                        limit: limits.compressed,
                        what: "compressed content",
                    },
                    Compression::new(9),
                ),
                temporary: TemporaryDump(path),
                destination: destination.to_owned(),
                decoded: 0,
                retained: 0,
                limits,
            });
        }
    }

    pub(crate) fn write_line(&mut self, value: &Value) -> Result<(), DumpError> {
        // Bound JSON escaping before allocation (a source cell can expand on
        // serialization). Reserve room for the newline in the same line cap.
        let mut line = CappedWrite {
            inner: Vec::new(),
            written: 0,
            limit: self.limits.line,
            what: "decoded line",
        };
        serde_json::to_writer(&mut line, value)
            .map_err(|e| refuse(format!("cannot serialise dump line: {e}")))?;
        line.write_all(b"\n")?;
        reserve_decoded_bytes(&mut self.decoded, 0, line.written, self.limits)?;
        // The reader budgets allocated capacities, not just string lengths.
        // Parse the actual bytes so writer/reader accounting sees the same
        // capacities, rather than capacities inherited from database values.
        let parsed: Value = serde_json::from_slice(&line.inner)
            .map_err(|e| refuse(format!("cannot parse encoded dump line: {e}")))?;
        if matches!(
            parsed.get("kind").and_then(Value::as_str),
            Some("manifest" | "row")
        ) {
            retain_within_cap(&mut self.retained, &parsed, self.limits.retained)?;
        }
        self.enc.write_all(&line.inner)?;
        Ok(())
    }

    pub(crate) fn publish(self) -> Result<(), DumpError> {
        let Self {
            enc,
            temporary,
            destination,
            limits,
            ..
        } = self;
        // finish includes the gzip trailer and propagates short/failed writes.
        // https://docs.rs/flate2/1.1.10/flate2/write/struct.GzEncoder.html#method.finish
        let mut output = enc.finish()?;
        output.flush()?;
        // Drop alone ignores close-time errors; sync before validation/rename.
        // https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all
        output.inner.sync_all()?;
        drop(output);
        // The full structural/count/gzip validation is exactly restore's reader,
        // not merely the writer's expectation of what it emitted.
        drop(inspect_with_limits(&temporary.0, limits)?);
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let directory = std::fs::File::open(parent)?;
        rename_no_replace(&temporary.0, &destination)?;
        // File fsync does not persist the renamed directory entry. Require
        // that durability too before CLI retention can remove recovery points.
        // https://man7.org/linux/man-pages/man2/fsync.2.html
        directory.sync_all()?;
        Ok(())
    }
}

// std::fs::rename replaces existing destinations on Unix. Use Linux's atomic
// no-replace rename instead: even racing publishers cannot destroy a good dump.
// https://man7.org/linux/man-pages/man2/rename.2.html (RENAME_NOREPLACE)
#[cfg(all(any(feature = "db", test), target_os = "linux"))]
fn rename_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn renameat2(
            oldfd: i32,
            old: *const std::ffi::c_char,
            newfd: i32,
            new: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    let path = |p: &Path| {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    };
    let source = path(source)?;
    let destination = path(destination)?;
    // Linux UAPI: AT_FDCWD=-100; RENAME_NOREPLACE=1. CString pointers remain
    // alive through the call; this function neither reads nor owns Rust memory.
    let result = unsafe { renameat2(-100, source.as_ptr(), -100, destination.as_ptr(), 1) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(any(feature = "db", test), not(target_os = "linux")))]
fn rename_no_replace(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    // Fail closed, rather than fall back to an overwrite-prone check + rename.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace dump publication requires Linux renameat2",
    ))
}

/// A row line, with the table name already gated.
#[derive(Debug, Clone)]
pub struct DumpRow {
    pub table: String,
    pub data: Map<String, Value>,
}

/// A fully validated dump: manifest plus rows keyed by table, with the total
/// already checked against the file's own `end` marker.
#[derive(Debug)]
pub struct DumpContents {
    pub manifest: DumpManifest,
    /// Rows read off the file, keyed by table, in file order.
    pub buffers: BTreeMap<String, Vec<Map<String, Value>>>,
    /// Total rows read, already checked against the file's own `end` marker.
    pub rows: u64,
}

/// Read a dump and check that it is internally consistent, touching no database.
///
/// Everything knowable from the file alone is decided here: the format
/// version, that the table names are ours, that the end marker is present,
/// and that the row counts match what the writer declared. `restore()` calls
/// this first so a bad file is refused before a transaction opens — and
/// `restore --dry-run` calls it *instead*, which is what makes the dry run a
/// real check of the backup rather than a check that a URL parses.
pub fn inspect_bytes(bytes: &[u8]) -> Result<DumpContents, DumpError> {
    ensure_within_size_cap(bytes.len() as u64, "dump")?;
    inspect_reader(bytes, INSPECT_LIMITS)
}

fn inspect_reader(input: impl Read, limits: InspectLimits) -> Result<DumpContents, DumpError> {
    let decoder = GzDecoder::new(input);
    let mut reader = std::io::BufReader::new(decoder);
    let mut manifest: Option<DumpManifest> = None;
    let mut saw_end = false;
    let mut declared_rows: u64 = 0;
    let mut buffers: BTreeMap<String, Vec<Map<String, Value>>> = BTreeMap::new();
    let mut decoded = 0;
    let mut retained = 0;
    let mut line_no = 0;

    while let Some(line) = read_capped_line(&mut reader, &mut decoded, limits)? {
        line_no += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut obj: Value = serde_json::from_slice(&line).map_err(|e| DumpError::Json {
            line: line_no,
            message: e.to_string(),
        })?;
        if saw_end {
            return Err(refuse("dump contains data after its end marker"));
        }
        if !obj.is_object() {
            return Err(refuse(format!("dump line {line_no} is not an object")));
        }
        let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
        match kind {
            "manifest" => {
                if manifest.is_some() {
                    return Err(refuse("dump contains more than one manifest"));
                }
                let version = obj
                    .get("version")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| refuse("manifest has an invalid version"))?;
                if version != 3 && version != u64::from(DUMP_VERSION) {
                    return Err(refuse(format!(
                        "dump version {version}, this build reads 3 and {DUMP_VERSION}"
                    )));
                }
                retain_within_cap(&mut retained, &obj, limits.retained)?;
                manifest = Some(validate_manifest(&obj)?);
            }
            "row" => {
                let Some(m) = manifest.as_ref() else {
                    return Err(refuse("dump row appears before the manifest"));
                };
                let table = obj
                    .get("table")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if !is_dump_table(&table) {
                    return Err(refuse(format!(
                        "row: {table:?} is not a table this backup format owns \
                         (expected one of {})",
                        DUMP_TABLES.join(", ")
                    )));
                }
                let info = m.tables.iter().find(|t| t.name == table).ok_or_else(|| {
                    refuse(format!("row table {table} is not declared in the manifest"))
                })?;
                retain_within_cap(&mut retained, &obj, limits.retained)?;
                let data = match obj.get_mut("data").map(Value::take) {
                    Some(Value::Object(data)) => data,
                    _ => return Err(refuse("dump row has no data object")),
                };
                // Frozen v3 has native driver-decoded cells and no type metadata.
                // This port's text-output encoding must remain strings/nulls.
                if !info.column_types.is_empty() {
                    for (col, value) in &data {
                        if !(value.is_string() || value.is_null()) {
                            return Err(refuse(format!(
                                "{table} row on line {line_no}: column {col:?} is not a string or null \
                                 in a text-encoded dump"
                            )));
                        }
                    }
                }
                let rows = buffers.entry(table).or_default();
                if rows.len() as u64 >= info.count {
                    return Err(refuse(format!(
                        "{}: rows exceed manifest count {}",
                        info.name, info.count
                    )));
                }
                rows.push(data);
            }
            "end" => {
                // as_u64 refuses nulls, strings, negatives and floating-point
                // values. Missing metadata must not verify an empty archive.
                // https://docs.rs/serde_json/1.0.151/serde_json/value/enum.Value.html#method.as_u64
                declared_rows = obj
                    .get("rows")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| refuse("dump end marker has an invalid row count"))?;
                saw_end = true;
            }
            _ => {
                return Err(refuse(format!(
                    "dump line {line_no} has unknown kind {kind:?}"
                )));
            }
        }
    }

    let Some(m) = manifest else {
        return Err(refuse(
            "no manifest: not a two-bot dump, or the file is truncated",
        ));
    };
    // A dump that stops mid-file is the disk-full case. Refuse it rather than
    // restoring a prefix of the data and calling it a success.
    if !saw_end {
        return Err(refuse(
            "dump has no end marker - it is truncated, treat it as lost",
        ));
    }

    let mut read_rows: u64 = 0;
    for table in &m.tables {
        let actual = buffers.get(&table.name).map(Vec::len).unwrap_or(0) as u64;
        if actual != table.count {
            return Err(refuse(format!(
                "{}: manifest declares {} rows, file contains {actual}",
                table.name, table.count
            )));
        }
        read_rows += actual;
    }
    if read_rows != declared_rows {
        return Err(refuse(format!(
            "dump declares {declared_rows} rows, file contains {read_rows}"
        )));
    }

    Ok(DumpContents {
        manifest: m,
        buffers,
        rows: read_rows,
    })
}

/// Read and validate a dump file from disk.
pub fn inspect(path: &Path) -> Result<DumpContents, DumpError> {
    inspect_with_limits(path, INSPECT_LIMITS)
}

fn inspect_with_limits(path: &Path, limits: InspectLimits) -> Result<DumpContents, DumpError> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > limits.compressed {
        return Err(refuse(format!(
            "dump file {} is {len} bytes, past the {}-byte dump cap; refusing rather than buffering it",
            path.display(), limits.compressed
        )));
    }
    // Also bound reads if a file grows after the metadata check.
    inspect_reader(file.take(limits.compressed), limits)
}

fn validate_manifest(obj: &Value) -> Result<DumpManifest, DumpError> {
    if obj.get("createdAt").and_then(Value::as_str).is_none() {
        return Err(refuse("manifest has no createdAt timestamp"));
    }
    if obj
        .get("eventsSequence")
        .and_then(Value::as_i64)
        .is_none_or(|mark| mark < 0)
    {
        return Err(refuse("manifest has an invalid eventsSequence"));
    }
    if !obj
        .get("schemaMigrations")
        .and_then(Value::as_array)
        .is_some_and(|migrations| migrations.iter().all(Value::is_string))
    {
        return Err(refuse("manifest has an invalid schemaMigrations list"));
    }
    // Frozen v3 omits this additive metadata. When present, do not silently
    // ignore malformed marks just because this reader restores only events.
    if let Some(sequences) = obj.get("sequences") {
        let sequences = sequences
            .as_object()
            .ok_or_else(|| refuse("manifest has invalid sequences"))?;
        for (name, mark) in sequences {
            if !is_dump_table(name) {
                return Err(refuse(format!(
                    "manifest sequence {name:?} is not a table this backup format owns"
                )));
            }
            if mark.as_i64().is_none_or(|mark| mark < 0) {
                return Err(refuse(format!(
                    "manifest sequence {name} has an invalid high-water mark"
                )));
            }
        }
    }
    if let Some(marks) = obj.get("sequenceMarks") {
        validate_sequence_marks(marks)?;
    }
    let tables = obj
        .get("tables")
        .and_then(Value::as_array)
        .ok_or_else(|| refuse("manifest has no table list"))?;
    let mut names = std::collections::BTreeSet::new();
    for table in tables {
        let row = table
            .as_object()
            .ok_or_else(|| refuse("manifest table is not an object"))?;
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| refuse("manifest table has no name"))?;
        if !is_dump_table(name) {
            return Err(refuse(format!(
                "manifest table: {name:?} is not a table this backup format owns \
                 (expected one of {})",
                DUMP_TABLES.join(", ")
            )));
        }
        if !names.insert(name.to_owned()) {
            return Err(refuse(format!("manifest table {name} is duplicated")));
        }
        let columns = row
            .get("columns")
            .and_then(Value::as_array)
            .ok_or_else(|| refuse(format!("manifest table {name} has an invalid column list")))?;
        if columns.iter().any(|c| !c.is_string()) {
            return Err(refuse(format!(
                "manifest table {name} has an invalid column list"
            )));
        }
        let count = row
            .get("count")
            .and_then(Value::as_u64)
            .ok_or_else(|| refuse(format!("manifest table {name} has an invalid row count")))?;
        let _ = count;
    }
    // The first 22 entries are frozen v3 coverage, pinned by the checked-in
    // legacy fixture. V3 may omit later additions; v4 must declare them all.
    let required = if obj.get("version").and_then(Value::as_u64) == Some(3) {
        22
    } else {
        DUMP_TABLES.len()
    };
    let missing: Vec<&str> = DUMP_TABLES
        .iter()
        .take(required)
        .filter(|name| required == 22 || !OPTIONAL_LEGACY_TABLES.contains(name))
        .filter(|name| !names.contains(**name))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(refuse(format!(
            "manifest is missing tables: {}",
            missing.join(", ")
        )));
    }
    serde_json::from_value(obj.clone()).map_err(|e| refuse(format!("manifest is malformed: {e}")))
}

/// Restore raises allocators to these marks, so a malformed one must refuse
/// rather than silently restore without the high-water it was meant to carry.
fn validate_sequence_marks(marks: &Value) -> Result<(), DumpError> {
    let marks = marks
        .as_array()
        .ok_or_else(|| refuse("manifest has invalid sequenceMarks"))?;
    let mut keys = std::collections::BTreeSet::new();
    for mark in marks {
        let mark = mark
            .as_object()
            .ok_or_else(|| refuse("manifest sequence mark is not an object"))?;
        let table = mark
            .get("table")
            .and_then(Value::as_str)
            .filter(|table| is_dump_table(table))
            .ok_or_else(|| refuse("manifest sequence mark names no backup table"))?;
        let column = mark
            .get("column")
            .and_then(Value::as_str)
            .filter(|column| !column.is_empty())
            .ok_or_else(|| refuse(format!("manifest sequence mark on {table} has no column")))?;
        if mark.get("lastValue").and_then(Value::as_i64).is_none()
            || mark.get("isCalled").and_then(Value::as_bool).is_none()
            || mark
                .get("increment")
                .and_then(Value::as_i64)
                .is_none_or(|increment| increment == 0)
        {
            return Err(refuse(format!(
                "manifest sequence mark {table}.{column} has an invalid position"
            )));
        }
        if !keys.insert((table, column)) {
            return Err(refuse(format!(
                "manifest sequence mark {table}.{column} is duplicated"
            )));
        }
    }
    Ok(())
}

/// Convert a validated cell to bound PostgreSQL input, never coercing an
/// unsupported native value into SQL NULL. Legacy's driver used native scalar
/// JSON; this port marks its PostgreSQL text-output encoding with column types.
#[cfg(any(feature = "db", test))]
pub(crate) fn cell_input(
    value: &Value,
    target_type: &str,
    text_encoded: bool,
) -> Result<Option<String>, DumpError> {
    if value.is_null() {
        return Ok(None);
    }
    if !text_encoded && matches!(target_type, "json" | "jsonb") {
        return serde_json::to_string(value)
            .map(Some)
            .map_err(|e| refuse(format!("cannot encode native JSON cell: {e}")));
    }
    match value {
        Value::String(s) => Ok(Some(s.clone())),
        Value::Number(_) | Value::Bool(_) if !text_encoded => Ok(Some(value.to_string())),
        _ => Err(refuse(format!(
            "cannot encode native cell for PostgreSQL {target_type}; refusing rather than restoring NULL"
        ))),
    }
}

/// Fresh gzip encoder at the legacy compression level (9).
#[must_use]
pub fn new_encoder() -> GzEncoder<Vec<u8>> {
    GzEncoder::new(Vec::new(), Compression::new(9))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder as WEnc;

    fn complete_tables(overrides: &BTreeMap<&str, (Vec<&str>, u64)>) -> Vec<Value> {
        DUMP_TABLES
            .iter()
            .map(|name| {
                let (cols, count) = overrides.get(name).cloned().unwrap_or_default();
                serde_json::json!({
                    "name": name,
                    "columns": cols,
                    "column_types": cols.iter().map(|_| "text").collect::<Vec<_>>(),
                    "count": count,
                })
            })
            .collect()
    }

    fn manifest(tables: Vec<Value>) -> Value {
        serde_json::json!({
            "kind": "manifest",
            "version": DUMP_VERSION,
            "createdAt": "2026-08-25T00:00:00.000Z",
            "tables": tables,
            "eventsSequence": 1,
            "schemaMigrations": ["001"],
        })
    }

    fn gzip_lines(objs: &[Value]) -> Vec<u8> {
        let mut enc = WEnc::new(Vec::new(), Compression::new(9));
        for obj in objs {
            let mut line = serde_json::to_vec(obj).unwrap();
            line.push(b'\n');
            enc.write_all(&line).unwrap();
        }
        enc.finish().unwrap()
    }

    fn good_dump() -> Vec<Value> {
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id", "guild_id"], 1));
        vec![
            manifest(complete_tables(&over)),
            serde_json::json!({"kind":"row","table":"events","data":{"id":"1","guild_id":"g"}}),
            serde_json::json!({"kind":"end","rows":1}),
        ]
    }

    fn empty_dump() -> Vec<Value> {
        vec![
            manifest(complete_tables(&BTreeMap::new())),
            serde_json::json!({"kind":"end","rows":0}),
        ]
    }

    #[test]
    fn accepts_empty_archives_with_zero_sequence_and_empty_migrations() {
        let mut objs = empty_dump();
        objs[0]["eventsSequence"] = serde_json::json!(0);
        objs[0]["schemaMigrations"] = serde_json::json!([]);
        // Legacy v3 does not emit column_types or sequences.
        for table in objs[0]["tables"].as_array_mut().unwrap() {
            table.as_object_mut().unwrap().shift_remove("column_types");
        }
        let contents = inspect_bytes(&gzip_lines(&objs)).unwrap();
        assert_eq!(contents.rows, 0);
        assert!(contents.buffers.is_empty());
        assert_eq!(contents.manifest.events_sequence, 0);
        assert!(contents.manifest.schema_migrations.is_empty());
        // Rust retains BIGINT marks exactly, without JavaScript's safe-integer
        // limitation. Present additive metadata must still be well-formed.
        objs[0]["eventsSequence"] = serde_json::json!(i64::MAX);
        objs[0]["sequences"] = serde_json::json!({"events": i64::MAX});
        assert_eq!(
            inspect_bytes(&gzip_lines(&objs))
                .unwrap()
                .manifest
                .events_sequence,
            i64::MAX
        );
    }

    #[test]
    fn refuses_malformed_end_counts_even_in_empty_archives() {
        let mut missing = empty_dump();
        missing[1].as_object_mut().unwrap().shift_remove("rows");
        let err = inspect_bytes(&gzip_lines(&missing)).unwrap_err();
        assert!(err.to_string().contains("invalid row count"), "{err}");
        for rows in [
            Value::Null,
            serde_json::json!("0"),
            serde_json::json!(false),
            serde_json::json!(-1),
            serde_json::json!(0.0),
            serde_json::json!(0.5),
            serde_json::json!(1e20),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            let mut objs = empty_dump();
            objs[1]["rows"] = rows.clone();
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(
                err.to_string().contains("invalid row count"),
                "rows={rows}: {err}"
            );
        }
        let mut mismatch = empty_dump();
        mismatch[1]["rows"] = serde_json::json!(u64::MAX);
        let err = inspect_bytes(&gzip_lines(&mismatch)).unwrap_err();
        assert!(err.to_string().contains("file contains 0"), "{err}");
    }

    #[test]
    fn refuses_non_object_records_and_malformed_kinds() {
        for record in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!([{"kind":"end", "rows":0}]),
            serde_json::json!(42),
            serde_json::json!("end"),
            serde_json::json!(true),
        ] {
            for index in [0, 1] {
                let mut objs = empty_dump();
                objs[index] = record.clone();
                let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
                assert!(err.to_string().contains("not an object"), "{err}");
            }
        }
        for record in [
            serde_json::json!({"rows":0}),
            serde_json::json!({"kind":null,"rows":0}),
            serde_json::json!({"kind":[],"rows":0}),
            serde_json::json!({"kind":"checkpoint","rows":0}),
        ] {
            let mut objs = empty_dump();
            objs[1] = record;
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(err.to_string().contains("unknown kind"), "{err}");
        }
    }

    #[test]
    fn refuses_missing_or_mistyped_required_manifest_metadata() {
        for field in [
            "version",
            "createdAt",
            "eventsSequence",
            "schemaMigrations",
            "tables",
        ] {
            let mut objs = empty_dump();
            objs[0].as_object_mut().unwrap().shift_remove(field);
            assert!(
                inspect_bytes(&gzip_lines(&objs)).is_err(),
                "missing {field}"
            );
            for value in [Value::Null, serde_json::json!(true), serde_json::json!({})] {
                let mut objs = empty_dump();
                objs[0][field] = value.clone();
                assert!(
                    inspect_bytes(&gzip_lines(&objs)).is_err(),
                    "{field}={value}"
                );
            }
        }
        for (field, values) in [
            (
                "version",
                vec![
                    serde_json::json!("3"),
                    serde_json::json!(3.0),
                    serde_json::json!(-1),
                ],
            ),
            (
                "createdAt",
                vec![serde_json::json!(42), serde_json::json!([])],
            ),
            (
                "eventsSequence",
                vec![
                    serde_json::json!("0"),
                    serde_json::json!(-1),
                    serde_json::json!(0.0),
                    serde_json::json!(0.5),
                    serde_json::json!(i64::MAX as u64 + 1),
                ],
            ),
            (
                "schemaMigrations",
                vec![
                    serde_json::json!("001"),
                    serde_json::json!([null]),
                    serde_json::json!(["001", 2]),
                    serde_json::json!([[]]),
                ],
            ),
        ] {
            for value in values {
                let mut objs = empty_dump();
                objs[0][field] = value.clone();
                let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
                assert!(err.to_string().contains(field), "{field}={value}: {err}");
            }
        }
    }

    #[test]
    fn refuses_malformed_present_sequence_metadata() {
        for sequences in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!("events"),
            serde_json::json!(0),
            serde_json::json!(true),
            serde_json::json!({"website_users": 1}),
            serde_json::json!({"events": null}),
            serde_json::json!({"events": "0"}),
            serde_json::json!({"events": -1}),
            serde_json::json!({"events": 0.0}),
            serde_json::json!({"events": 0.5}),
            serde_json::json!({"events": i64::MAX as u64 + 1}),
            serde_json::json!({"events": []}),
            serde_json::json!({"events": {}}),
        ] {
            let mut objs = empty_dump();
            objs[0]["sequences"] = sequences.clone();
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(err.to_string().contains("sequence"), "{sequences}: {err}");
        }
    }

    #[test]
    fn sequence_marks_parse_and_report_the_next_allocation() {
        let mut objs = empty_dump();
        objs[0]["sequenceMarks"] = serde_json::json!([
            {"table": "events", "column": "id", "lastValue": 41, "isCalled": true, "increment": 1},
            {"table": "members", "column": "backup_identity", "lastValue": 17, "isCalled": false, "increment": 3},
            {"table": "guild_settings", "column": "cas_version", "lastValue": -9, "isCalled": true, "increment": -1},
            {"table": "xp_awards", "column": "id", "lastValue": i64::MAX, "isCalled": true, "increment": 1},
        ]);
        let marks = inspect_bytes(&gzip_lines(&objs))
            .unwrap()
            .manifest
            .sequence_marks;
        let next: Vec<_> = marks.iter().map(SequenceMark::next).collect();
        assert_eq!(next, vec![Some(42), Some(17), Some(-10), None]);
        // Absent marks (v3, older v4) default to none and are not re-emitted.
        let manifest = inspect_bytes(&gzip_lines(&empty_dump())).unwrap().manifest;
        assert!(manifest.sequence_marks.is_empty());
        assert!(serde_json::to_value(&manifest)
            .unwrap()
            .get("sequenceMarks")
            .is_none());
    }

    #[test]
    fn refuses_malformed_sequence_marks() {
        let good = serde_json::json!({"table": "events", "column": "id", "lastValue": 1, "isCalled": true, "increment": 1});
        let with = |field: &str, value: Value| {
            let mut mark = good.clone();
            mark[field] = value;
            serde_json::json!([mark])
        };
        let mut cases = vec![
            Value::Null,
            serde_json::json!({}),
            serde_json::json!("events"),
            serde_json::json!([1]),
            serde_json::json!([good.clone(), good.clone()]),
            with("table", serde_json::json!("website_users")),
            with("table", Value::Null),
            with("column", serde_json::json!("")),
            with("column", serde_json::json!(1)),
            with("lastValue", serde_json::json!("1")),
            with("lastValue", serde_json::json!(0.5)),
            with("lastValue", serde_json::json!(i64::MAX as u64 + 1)),
            with("isCalled", serde_json::json!("true")),
            with("increment", serde_json::json!(0)),
            with("increment", Value::Null),
        ];
        let mut missing = good.clone();
        missing.as_object_mut().unwrap().shift_remove("isCalled");
        cases.push(serde_json::json!([missing]));
        for marks in cases {
            let mut objs = empty_dump();
            objs[0]["sequenceMarks"] = marks.clone();
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(err.to_string().contains("sequence"), "{marks}: {err}");
        }
    }

    #[test]
    fn refuses_malformed_table_counts_and_typed_column_metadata() {
        for count in [
            Value::Null,
            serde_json::json!("0"),
            serde_json::json!(-1),
            serde_json::json!(0.0),
            serde_json::json!(1e20),
            serde_json::json!([]),
        ] {
            let mut objs = empty_dump();
            objs[0]["tables"][0]["count"] = count;
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(err.to_string().contains("invalid row count"), "{err}");
        }
        for field in ["name", "columns", "count"] {
            let mut objs = empty_dump();
            objs[0]["tables"][0]
                .as_object_mut()
                .unwrap()
                .shift_remove(field);
            assert!(
                inspect_bytes(&gzip_lines(&objs)).is_err(),
                "missing table {field}"
            );
        }
        for types in [
            Value::Null,
            serde_json::json!("text"),
            serde_json::json!([null]),
            serde_json::json!([1]),
        ] {
            let mut objs = empty_dump();
            objs[0]["tables"][0]["column_types"] = types;
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(err.to_string().contains("malformed"), "{err}");
        }
    }

    fn publication_dir(label: &str) -> std::path::PathBuf {
        let scratch = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = scratch.join(format!(
            "dump-publication-{label}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn write_archive(path: &Path, objs: &[Value], limits: InspectLimits) -> Result<(), DumpError> {
        let mut writer = DumpWriter::with_limits(path, limits)?;
        for obj in objs {
            writer.write_line(obj)?;
        }
        writer.publish()
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn live_writer_reader_boundaries_for_every_budget() {
        let mut objs = good_dump();
        objs[1]["data"]["guild_id"] = Value::String("x".repeat(4096));
        objs[0]["tables"][0]["count"] = serde_json::json!(2);
        objs.insert(2, objs[1].clone());
        objs[3]["rows"] = serde_json::json!(2);
        let lines: Vec<Vec<u8>> = objs
            .iter()
            .map(|obj| {
                let mut line = serde_json::to_vec(obj).unwrap();
                line.push(b'\n');
                line
            })
            .collect();
        let exact = InspectLimits {
            compressed: gzip_lines(&objs).len() as u64,
            decoded: lines.iter().map(|l| l.len() as u64).sum(),
            line: lines.iter().map(|l| l.len() as u64).max().unwrap(),
            retained: lines[..lines.len() - 1]
                .iter()
                .map(|l| retained_weight(&serde_json::from_slice(l).unwrap()))
                .sum(),
        };
        let dir = publication_dir("budgets");
        let good = dir.join("two-funnel-good.ndjson.gz");
        write_archive(&good, &objs, exact).expect("at every cap is readable and publishable");
        inspect_with_limits(&good, exact).unwrap();
        inspect(&good).unwrap();
        let saved = std::fs::read(&good).unwrap();
        for (label, limits, message) in [
            (
                "compressed",
                InspectLimits {
                    compressed: exact.compressed - 1,
                    ..INSPECT_LIMITS
                },
                "compressed content",
            ),
            (
                "decoded",
                InspectLimits {
                    decoded: exact.decoded - 1,
                    ..INSPECT_LIMITS
                },
                "decoded content",
            ),
            (
                "line",
                InspectLimits {
                    line: exact.line - 1,
                    ..INSPECT_LIMITS
                },
                "decoded line",
            ),
            (
                "retained",
                InspectLimits {
                    retained: exact.retained - 1,
                    ..INSPECT_LIMITS
                },
                "retained data",
            ),
        ] {
            inspect_with_limits(&good, limits)
                .expect_err("reader refuses the same one-byte budget excess");
            // A budget error must neither create a new backup nor overwrite an
            // existing valid destination. Every temporary is cleaned on refusal.
            let absent = dir.join(format!("two-funnel-{label}.ndjson.gz"));
            for destination in [&absent, &good] {
                let err = write_archive(destination, &objs, limits).unwrap_err();
                assert!(err.to_string().contains(message), "{label}: {err}");
                assert!(!absent.exists());
                assert_eq!(std::fs::read(&good).unwrap(), saved);
                assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn live_writer_validates_counts_before_publication_and_never_clobbers() {
        let dir = publication_dir("validation");
        let path = dir.join("two-funnel-good.ndjson.gz");
        let mut invalid = good_dump();
        invalid[2]["rows"] = serde_json::json!(2);
        let err = write_archive(&path, &invalid, INSPECT_LIMITS).unwrap_err();
        assert!(err.to_string().contains("dump declares 2 rows"), "{err}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        write_archive(&path, &good_dump(), INSPECT_LIMITS).unwrap();
        let saved = std::fs::read(&path).unwrap();
        let err = write_archive(&path, &good_dump(), INSPECT_LIMITS).unwrap_err();
        assert!(
            matches!(err, DumpError::Io(ref e) if e.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        inspect(&path).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn live_writer_refuses_malformed_empty_metadata_before_publication() {
        let dir = publication_dir("empty-metadata");
        let prior = dir.join("two-funnel-prior.ndjson.gz");
        write_archive(&prior, &empty_dump(), INSPECT_LIMITS).unwrap();
        let saved = std::fs::read(&prior).unwrap();
        let next = dir.join("two-funnel-next.ndjson.gz");
        for field in ["end", "manifest"] {
            let mut invalid = empty_dump();
            if field == "end" {
                invalid[1].as_object_mut().unwrap().shift_remove("rows");
            } else {
                invalid[0]["eventsSequence"] = serde_json::json!(-1);
            }
            for path in [&prior, &next] {
                write_archive(path, &invalid, INSPECT_LIMITS).unwrap_err();
                assert!(!next.exists());
                assert_eq!(std::fs::read(&prior).unwrap(), saved);
                assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn abandoned_live_writer_is_not_a_backup_and_cleans_up() {
        let dir = publication_dir("abandoned");
        let path = dir.join("two-funnel-abandoned.ndjson.gz");
        {
            let mut writer = DumpWriter::new(&path).unwrap();
            writer.write_line(&good_dump()[0]).unwrap();
            let temporary = std::fs::read_dir(&dir)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name();
            let name = temporary.to_string_lossy();
            assert!(!name.starts_with("two-funnel-"));
            assert!(!name.ends_with(".ndjson.gz"));
            assert!(!path.exists());
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn capped_output_retries_short_writes_and_propagates_failure() {
        struct ShortWriter(Vec<u8>);
        impl Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0.len() == 3 {
                    return Err(std::io::Error::other("injected disk failure"));
                }
                self.0.push(bytes[0]);
                Ok(1)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut out = CappedWrite {
            inner: ShortWriter(Vec::new()),
            written: 0,
            limit: 10,
            what: "test",
        };
        let err = out.write_all(b"abcdef").unwrap_err();
        assert!(err.to_string().contains("injected disk failure"));
        assert_eq!(out.written, 3);
        assert_eq!(out.inner.0, b"abc");
    }

    #[test]
    fn accepts_a_well_formed_dump() {
        let contents = inspect_bytes(&gzip_lines(&good_dump())).expect("good dump");
        assert_eq!(contents.rows, 1);
        assert_eq!(contents.buffers["events"].len(), 1);
        assert_eq!(contents.manifest.version, DUMP_VERSION);
    }

    #[test]
    fn refuses_a_foreign_table_on_the_read_path() {
        // A backup file is not a trusted input; restore() interpolates table
        // names into SQL, so the gate belongs on the read path.
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id"], 0));
        let bytes = gzip_lines(&[
            manifest(complete_tables(&over)),
            serde_json::json!({"kind":"row","table":"website_users","data":{}}),
            serde_json::json!({"kind":"end","rows":0}),
        ]);
        let err = inspect_bytes(&bytes).expect_err("foreign table must be refused");
        assert!(err.to_string().contains("website_users"), "{err}");
    }

    #[test]
    fn refuses_a_truncated_dump_without_end_marker() {
        let objs = good_dump();
        let err = inspect_bytes(&gzip_lines(&objs[..2])).expect_err("truncated");
        assert!(err.to_string().contains("no end marker"), "{err}");
    }

    #[test]
    fn refuses_a_version_mismatch() {
        let mut m = manifest(complete_tables(&BTreeMap::new()));
        m["version"] = serde_json::json!(DUMP_VERSION + 1);
        let bytes = gzip_lines(&[m, serde_json::json!({"kind":"end","rows":0})]);
        let err = inspect_bytes(&bytes).expect_err("version must be refused");
        assert!(err.to_string().contains("dump version"), "{err}");
    }

    #[test]
    fn refuses_data_after_the_end_marker() {
        let mut objs = good_dump();
        objs.push(serde_json::json!({"kind":"row","table":"events","data":{}}));
        let err = inspect_bytes(&gzip_lines(&objs)).expect_err("trailing data");
        assert!(err.to_string().contains("after its end marker"), "{err}");
    }

    #[test]
    fn refuses_a_row_count_mismatch() {
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id"], 2));
        let bytes = gzip_lines(&[
            manifest(complete_tables(&over)),
            serde_json::json!({"kind":"row","table":"events","data":{"id":"1"}}),
            serde_json::json!({"kind":"end","rows":1}),
        ]);
        let err = inspect_bytes(&bytes).expect_err("count mismatch");
        assert!(
            err.to_string().contains("manifest declares 2 rows"),
            "{err}"
        );
    }

    #[test]
    fn refuses_a_manifest_missing_tables() {
        let tables =
            vec![serde_json::json!({"name":"events","columns":[],"column_types":[],"count":0})];
        let bytes = gzip_lines(&[manifest(tables), serde_json::json!({"kind":"end","rows":0})]);
        let err = inspect_bytes(&bytes).expect_err("missing tables");
        assert!(err.to_string().contains("missing tables"), "{err}");
    }

    #[test]
    fn inspects_fixture_emitted_by_the_frozen_legacy_writer() {
        let mut enc = new_encoder();
        enc.write_all(include_bytes!(
            "../../tests/fixtures/legacy-v3-native.ndjson"
        ))
        .unwrap();
        let contents = inspect_bytes(&finish_gzip(enc).unwrap()).unwrap();
        assert_eq!(contents.rows, 2);
        assert_eq!(contents.manifest.version, 3);
        let legacy_names: std::collections::BTreeSet<_> = contents
            .manifest
            .tables
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(legacy_names, DUMP_TABLES.iter().take(22).copied().collect());
        assert_eq!(
            contents.manifest.missing_tables().len(),
            DUMP_TABLES.len() - 22
        );
        assert!(contents
            .manifest
            .tables
            .iter()
            .all(|t| t.column_types.is_empty()));
        assert_eq!(contents.buffers["events"][0]["id"], serde_json::json!(1));
        assert_eq!(
            contents.buffers["join_risk_flags"][0]["score"],
            serde_json::json!(3)
        );
        assert_eq!(
            contents.buffers["join_risk_flags"][0]["flagged"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn v3_requires_its_original_tables_but_v4_requires_complete_coverage() {
        let legacy: Value = serde_json::from_str(
            include_str!("../../tests/fixtures/legacy-v3-native.ndjson")
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        let mut missing_v3 = legacy.clone();
        missing_v3["tables"].as_array_mut().unwrap().remove(0);
        assert!(validate_manifest(&missing_v3)
            .unwrap_err()
            .to_string()
            .contains("missing tables"));
        let mut incomplete_v4 = legacy;
        incomplete_v4["version"] = serde_json::json!(4);
        assert!(validate_manifest(&incomplete_v4)
            .unwrap_err()
            .to_string()
            .contains("missing tables"));
    }

    #[test]
    fn refuses_a_native_cell_in_a_text_encoded_dump() {
        // Native cells are valid in legacy dumps, not in this port's
        // explicitly marked PostgreSQL text-output encoding.
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id"], 1));
        let bytes = gzip_lines(&[
            manifest(complete_tables(&over)),
            serde_json::json!({"kind":"row","table":"events","data":{"id": 1}}),
            serde_json::json!({"kind":"end","rows":1}),
        ]);
        let err = inspect_bytes(&bytes).expect_err("native cell must be refused");
        assert!(err.to_string().contains("not a string or null"), "{err}");
    }

    #[test]
    fn legacy_cells_decode_using_target_types_without_null_coercion() {
        for (value, ty, expected) in [
            (serde_json::json!(42), "bigint", "42"),
            (serde_json::json!(false), "boolean", "false"),
            (serde_json::json!(true), "boolean", "true"),
            (serde_json::json!("{\"k\":1}"), "text", "{\"k\":1}"),
            (serde_json::json!("hello"), "jsonb", "\"hello\""),
            (serde_json::json!({"k":1}), "jsonb", "{\"k\":1}"),
            (serde_json::json!([true, 2]), "json", "[true,2]"),
        ] {
            assert_eq!(
                cell_input(&value, ty, false).unwrap().as_deref(),
                Some(expected)
            );
        }
        assert_eq!(cell_input(&Value::Null, "text", false).unwrap(), None);
        assert_eq!(
            cell_input(&serde_json::json!("{\"k\":1}"), "jsonb", true)
                .unwrap()
                .as_deref(),
            Some("{\"k\":1}")
        );
        assert!(cell_input(&serde_json::json!({"k":1}), "text", false).is_err());
        assert!(cell_input(&serde_json::json!(42), "bigint", true).is_err());
    }

    #[test]
    fn caps_a_highly_compressible_line_before_unbounded_allocation() {
        let mut enc = new_encoder();
        enc.write_all(&vec![b' '; 32 * 1024]).unwrap();
        let bytes = finish_gzip(enc).unwrap();
        assert!(bytes.len() < 1024);
        let limits = InspectLimits {
            line: 1024,
            ..INSPECT_LIMITS
        };
        let err = inspect_reader(bytes.as_slice(), limits).unwrap_err();
        assert!(err.to_string().contains("decoded line exceeds"), "{err}");
    }

    #[test]
    fn caps_cumulative_decoded_bytes_even_when_each_line_is_small() {
        let mut enc = new_encoder();
        enc.write_all(&vec![b'\n'; 32 * 1024]).unwrap();
        let bytes = finish_gzip(enc).unwrap();
        assert!(bytes.len() < 1024);
        let limits = InspectLimits {
            decoded: 1024,
            ..INSPECT_LIMITS
        };
        let err = inspect_reader(bytes.as_slice(), limits).unwrap_err();
        assert!(err.to_string().contains("decoded content exceeds"), "{err}");
    }

    #[test]
    fn caps_cumulative_retained_rows_including_value_overhead() {
        let mut objs = good_dump();
        objs[0]["tables"][0]["count"] = serde_json::json!(2);
        objs.insert(2, objs[1].clone());
        objs[3]["rows"] = serde_json::json!(2);
        let weight = retained_weight(&objs[0]) + 2 * retained_weight(&objs[1]);
        let bytes = gzip_lines(&objs);
        inspect_reader(
            bytes.as_slice(),
            InspectLimits {
                retained: weight,
                ..INSPECT_LIMITS
            },
        )
        .unwrap();
        let err = inspect_reader(
            bytes.as_slice(),
            InspectLimits {
                retained: weight - 1,
                ..INSPECT_LIMITS
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("retained data exceeds"), "{err}");
    }

    #[test]
    fn size_cap_refuses_past_the_limit_and_accepts_at_it() {
        ensure_within_size_cap(MAX_DUMP_BYTES, "dump").expect("at cap is fine");
        let err = ensure_within_size_cap(MAX_DUMP_BYTES + 1, "dump").expect_err("past cap refused");
        assert!(err.to_string().contains("past the"), "{err}");
    }

    #[test]
    fn inspect_refuses_an_oversize_file_from_metadata_without_reading_it() {
        // Sparse file: the length is metadata, no bytes are allocated, and
        // `inspect` refuses from the metadata before `fs::read` runs.
        let dir = publication_dir("reader-metadata");
        let path = dir.join("oversize.ndjson.gz");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_DUMP_BYTES + 1)
            .unwrap();
        let err = inspect(&path).expect_err("oversize must be refused");
        assert!(err.to_string().contains("past the"), "{err}");
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn refuses_a_row_before_the_manifest() {
        let bytes = gzip_lines(&[
            serde_json::json!({"kind":"row","table":"events","data":{}}),
            manifest(complete_tables(&BTreeMap::new())),
            serde_json::json!({"kind":"end","rows":0}),
        ]);
        let err = inspect_bytes(&bytes).expect_err("row before manifest");
        assert!(err.to_string().contains("before the manifest"), "{err}");
    }
}
