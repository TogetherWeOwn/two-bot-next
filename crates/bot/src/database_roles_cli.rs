//! Database-role commands never enter the gateway or migration path.

use two_bot_core::database_roles;

const USAGE: &str = "two-bot db roles plan|verify\n\
    plan: print SQL only; no connection, apply flag or credential required\n\
    verify: read-only inspection using TWO_DATABASE_URL; exit 1 on drift/error\n";

pub async fn dispatch(args: &[String]) -> i32 {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["--help" | "-h"] | ["roles", "--help" | "-h"] => {
            print!("{USAGE}");
            0
        }
        ["roles", "plan"] => {
            print!("{}", database_roles::plan());
            0
        }
        ["roles", "verify"] => verify().await,
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

async fn verify() -> i32 {
    let Ok(url) = std::env::var("TWO_DATABASE_URL") else {
        eprintln!("db roles verify: TWO_DATABASE_URL is required");
        return 1;
    };
    // skip_migrations=true is essential: this is an audit, never a repair.
    let db = match two_bot_cutover::connect(&url, 1, true).await {
        Ok(db) => db,
        Err(_) => {
            eprintln!(
                "db roles verify: connection failed (details withheld; may contain credentials)"
            );
            return 1;
        }
    };
    let result = database_roles::verify(db.pool()).await;
    db.close().await;
    match result {
        Ok(findings) if findings.is_empty() => {
            println!(
                "DATABASE ROLES VERIFIED (groups only; login bindings require operator audit)"
            );
            0
        }
        Ok(findings) => {
            for finding in &findings {
                eprintln!("drift: {finding}");
            }
            eprintln!("DATABASE ROLE DRIFT: {} finding(s)", findings.len());
            1
        }
        Err(_) => {
            eprintln!(
                "db roles verify: inspection failed (details withheld; may contain credentials)"
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[tokio::test]
    async fn plan_needs_no_database_and_refuses_execution_flags() {
        assert_eq!(dispatch(&args(&["roles", "plan"])).await, 0);
        assert_eq!(dispatch(&args(&["roles", "plan", "--apply"])).await, 2);
        assert_eq!(dispatch(&args(&["roles", "apply"])).await, 2);
        assert_eq!(dispatch(&args(&["roles", "verify", "--apply"])).await, 2);
        assert_eq!(dispatch(&args(&["roles", "--help"])).await, 0);
    }
}
