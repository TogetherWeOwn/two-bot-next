//! Operator-only member erasure. No gateway, Discord calls, or migrations.

// Operator command intentionally emits its dry-run/execute report to stdout.
#![allow(clippy::print_stdout)]

use two_bot_cutover::member_erasure::{erase_member, ErasureMode};

pub(crate) const USAGE: &str = "\
  two-bot erase-member --guild <id> --user <id> [--execute]
      Dry run by default: print matching row counts, write nothing.
      --execute deletes in one transaction; requires TWO_ERASURE_ACTOR.
      Env: TWO_DATABASE_URL (required). See docs/privacy.md.
";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    guild: String,
    user: String,
    execute: bool,
}

fn snowflake(value: &str) -> bool {
    two_bot_cutover::is_snowflake(value) && value.parse::<u64>().is_ok_and(|number| number > 0)
}

fn parse(args: &[String]) -> Result<Args, ()> {
    let mut guild = None;
    let mut user = None;
    let mut execute = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--guild" if guild.is_none() => guild = Some(args.next().ok_or(())?.clone()),
            "--user" if user.is_none() => user = Some(args.next().ok_or(())?.clone()),
            "--execute" if !execute => execute = true,
            _ => return Err(()),
        }
    }
    let guild = guild.filter(|id| snowflake(id)).ok_or(())?;
    let user = user.filter(|id| snowflake(id)).ok_or(())?;
    Ok(Args {
        guild,
        user,
        execute,
    })
}

pub(crate) async fn dispatch(args: &[String]) -> i32 {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        print!("{USAGE}");
        return 0;
    }
    let Ok(args) = parse(args) else {
        eprintln!("erase-member: invalid arguments.\n{USAGE}");
        return 2;
    };
    let actor = std::env::var("TWO_ERASURE_ACTOR").ok();
    let mode = if args.execute {
        let Some(actor) = actor.as_deref().filter(|actor| {
            !actor.trim().is_empty() && actor.len() <= 128 && !actor.chars().any(char::is_control)
        }) else {
            eprintln!("erase-member: --execute requires a nonempty TWO_ERASURE_ACTOR (max 128 characters, no control characters)");
            return 2;
        };
        ErasureMode::Execute { actor }
    } else {
        ErasureMode::DryRun
    };
    let Ok(url) = std::env::var("TWO_DATABASE_URL") else {
        eprintln!("erase-member: TWO_DATABASE_URL is required");
        return 1;
    };
    // Shared parser refuses unknown query keys before driver diagnostics can
    // disclose secrets. Never print connection/SQL errors or the target IDs.
    let db = match two_bot_cutover::connect(&url, 1, true).await {
        Ok(db) => db,
        Err(_) => {
            eprintln!("erase-member: connection failed (details redacted)");
            return 1;
        }
    };
    let result = erase_member(db.pool(), &args.guild, &args.user, mode).await;
    db.close().await;
    match result {
        Ok(report) => {
            for row in report {
                println!("{}\t{}", row.table, row.rows);
            }
            println!(
                "{}",
                if args.execute {
                    "ERASURE COMMITTED"
                } else {
                    "DRY RUN: no changes"
                }
            );
            0
        }
        Err(_) => {
            eprintln!("erase-member: transaction not confirmed; inspect a fresh dry run before retrying (details redacted)");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn erasure_cli_defaults_to_dry_run_and_requires_explicit_execute() {
        let base = [
            "--guild",
            "1545644954272137297",
            "--user",
            "123456789012345678",
        ];
        let dry = parse(&args(&base)).unwrap();
        assert!(!dry.execute);
        let mut execute = args(&base);
        execute.push("--execute".into());
        assert!(parse(&execute).unwrap().execute);
        for bad in [
            vec![],
            vec!["--execute"],
            vec!["--guild", "1545644954272137297"],
            vec!["--guild", "1545644954272137297", "--user", "0"],
            vec![
                "--guild",
                "1545644954272137297",
                "--user",
                "18446744073709551616",
            ],
            vec![
                "--guild",
                "1545644954272137297",
                "--user",
                "123456789012345678",
                "--force",
            ],
            vec![
                "--guild",
                "1545644954272137297",
                "--user",
                "123456789012345678",
                "--execute",
                "--execute",
            ],
            vec![
                "--guild",
                "1545644954272137297",
                "--user",
                "123456789012345678",
                "--user",
                "123456789012345679",
            ],
        ] {
            assert!(parse(&args(&bad)).is_err(), "{bad:?}");
        }
    }
}
