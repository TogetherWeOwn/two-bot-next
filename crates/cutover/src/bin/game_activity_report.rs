//! On-demand read-only aggregate game-activity operator report.
//!
//! Prints one guild's per-game aggregates (game, total play ms, in-voice
//! ms, session counts) as one JSON object on stdout: the object shape
//! always prints, an empty guild adds a hint, and an unscoped invocation
//! (no `--guild`) prints the hint and exits 0 without opening a database.
//! Read-only: the only statement is the guild-scoped aggregate SELECT, the
//! pool opens with migrations off, and there is no scheduling, Discord
//! call, or write. Aggregates carry no per-person column, so neither does
//! this report.
//!
//! `game-activity-report --guild <ID> [--allow-live-guild]`
//! Env: `TWO_DATABASE_URL` (required when `--guild` is given).

use two_bot_core::game_activity::{render_game_activity_report, render_game_activity_unscoped};
use two_bot_core::game_activity_store::read_guild_aggregates;
use two_bot_cutover::cli::{open_db, require_guild, Args};

const USAGE: &str = "Usage: game-activity-report --guild <ID> [--allow-live-guild]\nOn-demand read-only game-activity aggregate report (never scheduled): per-game totals for one guild as JSON on stdout. TWO_DATABASE_URL required. Without --guild: empty report plus hint, exit 0.\n";

// On-demand operator report: the JSON goes to stdout by contract (written
// through an explicit handle, not the print macros the workspace denies);
// diagnostics and usage errors use stderr. A stdout write failure is an
// error like any other: a truncated or missing report must not exit 0.
fn emit_to(out: &mut impl std::io::Write, text: &str) -> std::io::Result<()> {
    out.write_all(text.as_bytes())?;
    out.flush()
}

/// Write a resolved report to `out`; resolution errors pass through and
/// write failures become errors, so the caller exits non-zero either way.
fn deliver(output: Result<String, String>, out: &mut impl std::io::Write) -> Result<(), String> {
    emit_to(out, &output?).map_err(|e| format!("cannot write report to stdout: {e}"))
}

#[tokio::main]
async fn main() {
    let args = Args::parse(&std::env::args().skip(1).collect::<Vec<_>>());
    let output = execute(&args).await;
    let mut out = std::io::stdout().lock();
    if let Err(e) = deliver(output, &mut out) {
        eprintln!("game-activity-report: {e}");
        std::process::exit(2);
    }
}

/// Resolve one invocation to its stdout text. Usage errors and query
/// failures return `Err` (the caller prints them and exits 2); the help and
/// unscoped paths resolve without opening a database.
async fn execute(args: &Args) -> Result<String, String> {
    if args.has("help") {
        return Ok(USAGE.to_owned());
    }
    if args.values.keys().any(|k| k != "guild")
        || args.flags.iter().any(|k| k != "allow-live-guild")
        || !args.positionals.is_empty()
    {
        return Err("unknown or malformed argument".to_owned());
    }
    // Unscoped: exit 0 with the empty-state hint; no database is opened.
    if !args.values.contains_key("guild") {
        return Ok(render_game_activity_unscoped());
    }
    let guild = require_guild(args, "guild");
    let db = open_db(args, true).await;
    let rows = read_guild_aggregates(db.pool(), &guild)
        .await
        .map_err(|_| {
            "query failed; check authorized database/schema (no migrations are applied)"
        })?;
    let output = render_game_activity_report(&guild, &rows);
    db.close().await;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(argv: &[&str]) -> Args {
        Args::parse(&argv.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
    }

    #[tokio::test]
    async fn help_resolves_to_usage_without_a_database() {
        assert_eq!(execute(&args(&["--help"])).await, Ok(USAGE.to_owned()));
    }

    #[tokio::test]
    async fn unscoped_resolves_to_the_hint_report_without_a_database() {
        assert_eq!(
            execute(&args(&[])).await,
            Ok(render_game_activity_unscoped())
        );
    }

    #[tokio::test]
    async fn unknown_value_flag_is_refused() {
        assert_eq!(
            execute(&args(&["--bogus", "x"])).await,
            Err("unknown or malformed argument".to_owned())
        );
    }

    #[tokio::test]
    async fn unknown_bare_flag_is_refused() {
        assert_eq!(
            execute(&args(&["--bogus"])).await,
            Err("unknown or malformed argument".to_owned())
        );
    }

    #[tokio::test]
    async fn positional_argument_is_refused() {
        assert_eq!(
            execute(&args(&["hello"])).await,
            Err("unknown or malformed argument".to_owned())
        );
    }

    #[test]
    fn emit_to_writes_exact_bytes() {
        let mut buf: Vec<u8> = Vec::new();
        let written = emit_to(&mut buf, "x\n");
        assert!(written.is_ok());
        assert_eq!(buf, b"x\n".to_vec());
    }

    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn deliver_writes_resolved_report_bytes() {
        let mut buf: Vec<u8> = Vec::new();
        assert_eq!(deliver(Ok("x\n".to_owned()), &mut buf), Ok(()));
        assert_eq!(buf, b"x\n".to_vec());
    }

    #[test]
    fn deliver_propagates_stdout_failure_as_an_error() {
        assert!(deliver(Ok("x".to_owned()), &mut FailingWriter)
            .is_err_and(|e| e.starts_with("cannot write report to stdout: ")));
    }

    #[test]
    fn deliver_passes_resolution_errors_through() {
        let mut buf: Vec<u8> = Vec::new();
        assert_eq!(
            deliver(Err("nope".to_owned()), &mut buf),
            Err("nope".to_owned())
        );
        assert!(buf.is_empty());
    }
}
