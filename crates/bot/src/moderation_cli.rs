//! `two-bot moderation preflight` operator check.
//!
//! Read-only guard for disabling moderation/automation: reports the releases
//! still owed in Postgres (pending tempban unbans, active lockdowns, enabled
//! scheduled messages) and refuses — exit 1, naming outstanding ids — while
//! any are owed. Unbans left `running` by a stopped worker are tagged apart
//! from the rest: they never drain on their own and need the documented
//! recovery steps. Exit 0 is CLEAR (or an explicit override, logged loudly);
//! exit 2 means the state could not be read. State comes from the database,
//! never Discord; nothing here writes, migrates, or starts the gateway.

// Operator CLI intentionally emits reports and help to stdout.
#![allow(clippy::print_stdout)]

use std::collections::HashMap;

use two_bot_core::disable_preflight::{
    self, DisableGates, OwedReleases, OVERRIDE_ENV, OVERRIDE_FLAG,
};

pub const USAGE: &str = "\
  two-bot moderation preflight [--json] [--allow-owed]
      Read-only disable guard: refuse (exit 1) while Postgres still holds
      pending tempban unbans, active lockdowns or enabled scheduled messages.
      A `running` unban claim (left by a stopped worker) is tagged [running]
      and listed in --json as running_unbans, a subset of pending_unbans.
      Exit 0 is CLEAR, or an explicit override (--allow-owed, logged loudly).
      Exit 2 means the state could not be read.
      Env: TWO_DATABASE_URL (or DATABASE_URL when unset), TWO_MODERATION,
      TWO_AUTOMATIONS, TWO_ALLOW_OWED_RELEASES, TWO_DATABASE_TLS.
";

fn database_url(vars: &HashMap<String, String>) -> Result<String, &'static str> {
    match vars.get("TWO_DATABASE_URL") {
        Some(url) if !url.trim().is_empty() => Ok(url.clone()),
        Some(_) => Err("TWO_DATABASE_URL is set but empty"),
        None => match vars.get("DATABASE_URL") {
            Some(url) if !url.trim().is_empty() => Ok(url.clone()),
            _ => Err("set TWO_DATABASE_URL before the disable-guard check"),
        },
    }
}

fn render_json(owed: &OwedReleases, overridden: bool) {
    println!(
        "{}",
        serde_json::json!({
            "schema_version": 1,
            "clear": owed.is_clear(),
            "overridden": overridden,
            "pending_unbans": owed.unbans,
            "running_unbans": owed.running_unbans,
            "active_lockdowns": owed.lockdowns,
            "enabled_scheduled": owed.scheduled,
        })
    );
}

fn render_text(owed: &OwedReleases, overridden: bool, gates: &DisableGates) {
    if owed.is_clear() {
        println!(
            "moderation disable preflight: CLEAR (moderation={}, automations={})",
            if gates.moderation { "on" } else { "off" },
            if gates.automations { "on" } else { "off" },
        );
        return;
    }
    if overridden {
        println!("moderation disable preflight: OVERRIDE ({OVERRIDE_ENV}=1 or {OVERRIDE_FLAG}) — proceeding with releases still owed. Members may stay banned and channels locked.");
    }
    println!("moderation disable preflight: {}", owed.report());
    if !overridden {
        println!(
            "Complete or cancel these releases, then retry — or re-run with {OVERRIDE_FLAG} (the override is logged)."
        );
    }
}

pub async fn dispatch(args: &[String]) -> i32 {
    if args.first().is_some_and(|arg| arg == "release-channel") {
        return crate::moderation_release_cli::dispatch(&args[1..]).await;
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}{}", crate::moderation_release_cli::USAGE);
        return 0;
    }
    let mut json = false;
    let mut flag_override = false;
    let mut verb_seen = false;
    for arg in args {
        match arg.as_str() {
            "preflight" if !verb_seen => verb_seen = true,
            "--json" => json = true,
            _ if arg == OVERRIDE_FLAG => flag_override = true,
            _ => {
                eprintln!("unknown moderation option {arg:?}\n{USAGE}");
                return 2;
            }
        }
    }
    if !verb_seen {
        eprintln!("{USAGE}");
        return 2;
    }
    let vars: HashMap<String, String> = std::env::vars().collect();
    let url = match database_url(&vars) {
        Ok(url) => url,
        Err(error) => {
            eprintln!("moderation preflight: {error}");
            return 2;
        }
    };
    let gates = DisableGates::from_map(&vars);
    let mut effective_args = Vec::new();
    if flag_override {
        effective_args.push(OVERRIDE_FLAG.to_owned());
    }
    let overridden = disable_preflight::override_active(&vars, &effective_args);
    // Read-only by construction: skip migrations (no DDL), two connections max.
    let pool = match two_bot_cutover::connect(&url, 2, true).await {
        Ok(db) => db.pool().clone(),
        Err(_) => {
            eprintln!("moderation preflight: UNKNOWN — database unreachable (details redacted)");
            return 2;
        }
    };
    let owed = match disable_preflight::outstanding(&pool).await {
        Ok(owed) => owed,
        Err(_) => {
            eprintln!("moderation preflight: UNKNOWN — owed-release state unreadable");
            return 2;
        }
    };
    if json {
        render_json(&owed, overridden);
    } else {
        render_text(&owed, overridden, &gates);
    }
    if owed.is_clear() || overridden {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn database_url_prefers_primary_and_rejects_empties() {
        assert_eq!(
            database_url(&vars(&[("TWO_DATABASE_URL", "postgres://db/a")])),
            Ok("postgres://db/a".to_owned())
        );
        assert_eq!(
            database_url(&vars(&[
                ("TWO_DATABASE_URL", "postgres://db/a"),
                ("DATABASE_URL", "postgres://db/b"),
            ])),
            Ok("postgres://db/a".to_owned())
        );
        assert_eq!(
            database_url(&vars(&[("DATABASE_URL", "postgres://db/b")])),
            Ok("postgres://db/b".to_owned())
        );
        // A present-but-empty primary is a configuration error, never
        // permission to fall back to the alias.
        assert!(database_url(&vars(&[("TWO_DATABASE_URL", "")])).is_err());
        assert!(database_url(&vars(&[
            ("TWO_DATABASE_URL", ""),
            ("DATABASE_URL", "postgres://db/b"),
        ]))
        .is_err());
        assert!(database_url(&vars(&[])).is_err());
    }
}
