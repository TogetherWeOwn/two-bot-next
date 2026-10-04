//! Shared CLI prelude for the seven cutover binaries.
//!
//! Every CLI refuses the live guild without `--allow-live-guild` BEFORE it
//! opens any database or file (legacy contract pinned by
//! `test/e2e.leveling-scripts.test.ts`: exit 2, "Refusing live guild", no
//! connection attempt). Dry run is the default everywhere that writes.

use crate::{is_snowflake, CutoverDb, DB_POOL_MAX_DEFAULT, LIVE_GUILD_ID};

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
        Self::try_parse(argv).unwrap_or_else(|error| {
            eprintln!("input error: {error}");
            std::process::exit(2);
        })
    }

    /// Safety opt-ins may appear only once, bare and without a value.
    /// Validate while parsing so refusal precedes every consumer's file/DB access.
    fn try_parse(argv: &[String]) -> Result<Self, String> {
        let mut out = Self::default();
        let mut i = 0;
        while i < argv.len() {
            let a = &argv[i];
            if let Some(eq) = a.find('=') {
                let (k, v) = a.split_at(eq);
                if let Some(key) = k.strip_prefix("--") {
                    if is_safety_opt_in(key) {
                        return Err(format!(
                            "--{key} is a bare opt-in and does not accept a value"
                        ));
                    }
                    out.values.insert(key.to_owned(), v[1..].to_owned());
                    i += 1;
                    continue;
                }
            }
            if let Some(stripped) = a.strip_prefix("--") {
                let name = stripped.to_owned();
                if is_safety_opt_in(&name) {
                    if out.has(&name) {
                        return Err(format!("--{name} must not be repeated"));
                    }
                    if i + 1 < argv.len() && !argv[i + 1].starts_with("--") {
                        return Err(format!(
                            "--{name} is a bare opt-in and does not accept a value"
                        ));
                    }
                }
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
        if out.has("apply") && out.has("dry-run") {
            return Err("--apply conflicts with --dry-run".to_owned());
        }
        Ok(out)
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

fn is_safety_opt_in(name: &str) -> bool {
    matches!(name, "apply" | "allow-lower" | "allow-live-guild")
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

#[cfg(test)]
mod tests {
    use super::Args;

    fn parse(argv: &[&str]) -> Result<Args, String> {
        Args::try_parse(&argv.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn safety_opt_ins_reject_all_equals_and_space_values_in_any_order() {
        for flag in ["apply", "allow-lower", "allow-live-guild"] {
            for value in ["false", "true", "0", "1", "no", "", "fixture.json"] {
                let equals = format!("--{flag}={value}");
                let bare = format!("--{flag}");
                for invalid in [vec![equals.as_str()], vec![bare.as_str(), value]] {
                    for before in [false, true] {
                        let mut argv = vec!["--guild-id", "111111111111111111"];
                        if before {
                            argv.splice(0..0, invalid.clone());
                        } else {
                            argv.extend_from_slice(&invalid);
                        }
                        let error = parse(&argv).unwrap_err();
                        assert!(error.contains(&bare), "{argv:?}: {error}");
                        assert!(error.contains("does not accept a value"));
                    }
                }
            }
        }
    }

    #[test]
    fn safety_opt_ins_reject_duplicates_and_conflicting_forms() {
        for flag in ["apply", "allow-lower", "allow-live-guild"] {
            let bare = format!("--{flag}");
            let false_value = format!("--{flag}=false");
            for argv in [
                vec![bare.as_str(), bare.as_str()],
                vec![
                    bare.as_str(),
                    "--guild-id=111111111111111111",
                    bare.as_str(),
                ],
                vec![bare.as_str(), false_value.as_str()],
                vec![false_value.as_str(), bare.as_str()],
                vec![bare.as_str(), "false", bare.as_str()],
                vec![bare.as_str(), bare.as_str(), "false"],
            ] {
                assert!(parse(&argv).is_err(), "accepted {argv:?}");
            }
        }
    }

    #[test]
    fn apply_and_dry_run_are_conflicting_in_either_order() {
        for argv in [
            vec!["--apply", "--dry-run"],
            vec!["--dry-run", "--apply"],
            vec!["--apply", "--dry-run=false"],
            vec!["--dry-run", "false", "--apply"],
        ] {
            assert!(parse(&argv).unwrap_err().contains("conflicts"));
        }
        assert!(parse(&["--dry-run"]).is_ok());
    }

    #[test]
    fn bare_opt_ins_preserve_values_positionals_and_option_order() {
        for argv in [
            vec![
                "inventory",
                "--apply",
                "--guild-id",
                "111111111111111111",
                "--allow-lower",
                "--input=fixture.json",
                "--allow-live-guild",
            ],
            vec![
                "inventory",
                "--allow-live-guild",
                "--input",
                "fixture.json",
                "--guild-id=111111111111111111",
                "--allow-lower",
                "--apply",
            ],
        ] {
            let args = parse(&argv).unwrap();
            assert_eq!(args.positionals, ["inventory"]);
            for flag in ["apply", "allow-lower", "allow-live-guild"] {
                assert!(args.has(flag));
                assert!(args.flags.contains(flag));
                assert!(!args.values.contains_key(flag));
                assert_eq!(args.get(flag), Some(""));
            }
            assert_eq!(args.get("guild-id"), Some("111111111111111111"));
            assert_eq!(args.get("input"), Some("fixture.json"));
        }
        let args = parse(&["--input=first", "--input", "second", "--json"]).unwrap();
        assert_eq!(args.get("input"), Some("second"));
        assert!(args.has("json"));
        assert!(!args.has("apply"));
    }
}
