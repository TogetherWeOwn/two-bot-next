//! Shared CLI prelude for the seven cutover binaries.
//!
//! Every CLI refuses the live guild without `--allow-live-guild` BEFORE it
//! opens any database or file (legacy contract pinned by
//! `test/e2e.leveling-scripts.test.ts`: exit 2, "Refusing live guild", no
//! connection attempt). Dry run is the default everywhere that writes.

use crate::{is_snowflake, CutoverDb, ScanCompletion, DB_POOL_MAX_DEFAULT, LIVE_GUILD_ID};

/// Shared completion accounting for both history CLIs. Probes are recorded only
/// when interrupted; their intentional one-page cap is not a backfill outcome.
#[derive(Debug, Default)]
pub struct ScanReport {
    counts: std::collections::BTreeMap<ScanCompletion, usize>,
    interrupted: Vec<(String, ScanCompletion)>,
}

impl ScanReport {
    pub fn record(&mut self, channel: &str, completion: ScanCompletion) {
        *self.counts.entry(completion).or_default() += 1;
        if completion.interrupted() {
            self.interrupted.push((channel.to_owned(), completion));
        }
    }

    #[must_use]
    pub fn has_incomplete_history(&self) -> bool {
        self.counts
            .keys()
            .any(|reason| *reason != ScanCompletion::EndOfHistory)
    }

    #[must_use]
    pub fn render(&self) -> String {
        let counts: Vec<_> = self
            .counts
            .iter()
            .map(|(reason, count)| format!("{reason}={count}"))
            .collect();
        let mut report = format!("  scan completion       {}\n", counts.join("   "));
        for (channel, reason) in &self.interrupted {
            report.push_str(&format!(
                "  INCOMPLETE: {reason} on {channel}. Partial history retained; this is not a page cap. Check access/service health before re-running.\n"
            ));
        }
        report
    }
}

/// Parsed `--name value` / `--name=value` / bare `--flag` arguments.
#[derive(Debug, Default)]
pub struct Args {
    pub positionals: Vec<String>,
    pub flags: std::collections::HashSet<String>,
    pub values: std::collections::HashMap<String, String>,
}

impl Args {
    #[must_use]
    pub fn parse(argv: &[String]) -> Self {
        let mut out = Self::default();
        let mut i = 0;
        while i < argv.len() {
            let a = &argv[i];
            if let Some(eq) = a.find('=') {
                let (k, v) = a.split_at(eq);
                if let Some(key) = k.strip_prefix("--") {
                    out.values.insert(key.to_owned(), v[1..].to_owned());
                    i += 1;
                    continue;
                }
            }
            if let Some(stripped) = a.strip_prefix("--") {
                let name = stripped.to_owned();
                if i + 1 < argv.len() && !argv[i + 1].starts_with("--") {
                    out.values.insert(name, argv[i + 1].clone());
                    i += 2;
                } else {
                    out.flags.insert(name);
                    i += 1;
                }
            } else {
                out.positionals.push(a.clone());
                i += 1;
            }
        }
        out
    }

    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.flags.contains(name) || self.values.contains_key(name)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        if let Some(v) = self.values.get(name) {
            return Some(v.as_str());
        }
        if self.flags.contains(name) {
            return Some("");
        }
        None
    }
}

/// Refuse the live guild unless `--allow-live-guild` is present. Returns the
/// validated guild id, or exits 2 (legacy exit code for refusal/usage).
pub fn require_guild(args: &Args, flag: &str) -> String {
    let Some(guild) = args.values.get(flag) else {
        eprintln!("missing --{flag} <snowflake>");
        std::process::exit(2);
    };
    if !is_snowflake(guild) {
        eprintln!("--{flag} must be a Discord snowflake");
        std::process::exit(2);
    }
    if guild == LIVE_GUILD_ID && !args.has("allow-live-guild") {
        eprintln!(
            "Refusing live guild {LIVE_GUILD_ID}. Use --allow-live-guild only for an owner-approved rollout."
        );
        std::process::exit(2);
    }
    guild.clone()
}

/// Read-only inventory exemption: `inventory` may read the live guild
/// without `--allow-live-guild` (taking stock of live `member_levels`
/// before an import is what it exists for). Still validates the snowflake.
pub fn require_guild_read(args: &Args, flag: &str) -> String {
    let Some(guild) = args.values.get(flag) else {
        eprintln!("missing --{flag} <snowflake>");
        std::process::exit(2);
    };
    if !is_snowflake(guild) {
        eprintln!("--{flag} must be a Discord snowflake");
        std::process::exit(2);
    }
    guild.clone()
}

/// Open the database (`TWO_DATABASE_URL` required, pool max from
/// `TWO_DB_POOL_MAX ?? 5`). `migrations_off` mirrors the probe's
/// `skipMigrations` (a read-only tool must not build schema by accident).
pub async fn open_db(_args: &Args, migrations_off: bool) -> CutoverDb {
    let url = std::env::var("TWO_DATABASE_URL").unwrap_or_default();
    if url.trim().is_empty() {
        eprintln!("TWO_DATABASE_URL is required.");
        std::process::exit(2);
    }
    let pool_max: u32 = std::env::var("TWO_DB_POOL_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DB_POOL_MAX_DEFAULT);
    match crate::connect(&url, pool_max, migrations_off).await {
        Ok(db) => db,
        Err(_) => {
            eprintln!("cannot open database; connection details redacted");
            std::process::exit(1);
        }
    }
}

/// Current time as ISO-8601 UTC millis.
#[must_use]
pub fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis() as i64;
    // Format manually: time crate's Rfc3339 needs an OffsetDateTime.
    let dt =
        time::OffsetDateTime::from_unix_timestamp(secs).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let date = dt.date();
    let time_part = dt.time();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        date.year(),
        u8::from(date.month()),
        date.day(),
        time_part.hour(),
        time_part.minute(),
        time_part.second(),
        millis
    )
}
