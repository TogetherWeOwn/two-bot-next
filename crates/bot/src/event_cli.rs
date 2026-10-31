//! Operator-only reconciliation for unknown website event intents. No gateway,
//! no Discord calls, no migrations.
//!
//! An `event.upsert` create whose Discord call times out, gets a 5xx or an
//! unreadable reply, or whose key mapping or mirror write fails, leaves an
//! `unknown` claim with no key mapping behind: Discord may hold an event the
//! website cannot name. Retrying under a new `Idempotency-Key` would then make
//! a second event, so the receiver refuses re-submits until an operator
//! reconciles the Discord event by hand first. Listing is read-only and runs
//! any time; every resolution needs `--execute` and closes only the exact
//! intent named, never by age or current remote state. Before resolving, open
//! the guild's scheduled events in Discord and confirm the event is (or is
//! not) there; the evidence you assert here is what replay serves forever.

// Operator command intentionally emits its dry-run/execute report to stdout.
#![allow(clippy::print_stdout)]

use two_bot_core::internal_action_store::{
    DiscordId, EventOutcome, InternalActionStore, ReconciliationEvidence, TerminalFailure,
    TerminalResponse, OPERATOR_LIST_DEFAULT_LIMIT, OPERATOR_LIST_MAX_LIMIT,
};
use two_bot_core::internal_actions::{is_snowflake, valid_event_key};

pub(crate) const USAGE: &str = "\
  two-bot reconcile-event --guild <id> --list [--limit <n>]
      List unknown event.upsert/event.cancel intents oldest first.
      Read-only. Shows 25 rows by default, at most 100; a capped listing
      ends with `showing <shown> of <total> rows (truncated; rerun with --limit <total>)`.
      Past 100 rows no rerun converges: resolve shown rows and re-list.
      Env: TWO_DATABASE_URL (required). Staging guild only.

  two-bot reconcile-event --guild <id> --intent <n> --created --event-key <k> --event-id <id> [--execute]
      The create landed in Discord: register the key mapping and record a
      terminal created receipt. Verify the event in Discord first.

  two-bot reconcile-event --guild <id> --intent <n> --updated --event-key <k> --event-id <id> [--execute]
      The update landed in Discord: re-point the key mapping and record a
      terminal updated receipt. Verify the event in Discord first.

  two-bot reconcile-event --guild <id> --intent <n> --cancelled --event-id <id> [--execute]
      The cancellation landed in Discord: record a terminal cancelled
      receipt. The key mapping stays retained, never rewritten here.

  two-bot reconcile-event --guild <id> --intent <n> --no-effect [--execute]
      Discord holds no such event: record a terminal no-effect failure
      without touching mappings, for an upsert or a cancel intent alike.
      Retrying then needs a new Idempotency-Key.
      Without --execute every resolution above prints DRY RUN and writes nothing.
";

#[derive(Debug, PartialEq, Eq)]
enum ResolveMode {
    Created { event_key: String, event_id: String },
    Updated { event_key: String, event_id: String },
    Cancelled { event_id: String },
    NoEffect,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    List { limit: i64 },
    Resolve { intent: i64, mode: ResolveMode },
}

struct Args {
    guild: String,
    command: Command,
    execute: bool,
}

fn parse(args: &[String]) -> Result<Args, ()> {
    let mut guild = None;
    let mut intent = None;
    let mut event_key = None;
    let mut event_id = None;
    let mut mode: Option<&str> = None;
    let mut limit = None;
    let mut execute = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--guild" if guild.is_none() => guild = Some(args.next().ok_or(())?.clone()),
            "--intent" if intent.is_none() => {
                let value: i64 = args.next().ok_or(())?.parse().map_err(|_| ())?;
                if value <= 0 {
                    return Err(());
                }
                intent = Some(value);
            }
            "--event-key" if event_key.is_none() => {
                let value = args.next().ok_or(())?.clone();
                if !valid_event_key(&value) {
                    return Err(());
                }
                event_key = Some(value);
            }
            "--event-id" if event_id.is_none() => {
                let value = args.next().ok_or(())?.clone();
                if !is_snowflake(&value) {
                    return Err(());
                }
                event_id = Some(value);
            }
            "--list" | "--created" | "--updated" | "--cancelled" | "--no-effect"
                if mode.is_none() =>
            {
                mode = Some(arg.as_str());
            }
            "--limit" if limit.is_none() => {
                let value: i64 = args.next().ok_or(())?.parse().map_err(|_| ())?;
                if value <= 0 {
                    return Err(());
                }
                limit = Some(value);
            }
            "--execute" if !execute => execute = true,
            _ => return Err(()),
        }
    }
    let guild = guild.filter(|id| is_snowflake(id)).ok_or(())?;
    // The row cap belongs to listings only: silently ignoring it on a
    // resolution would mislead the operator about what ran.
    if !matches!(mode, Some("--list")) && limit.is_some() {
        return Err(());
    }
    let command = match mode.ok_or(())? {
        "--list" => Command::List {
            limit: limit
                .unwrap_or(OPERATOR_LIST_DEFAULT_LIMIT)
                .min(OPERATOR_LIST_MAX_LIMIT),
        },
        "--created" => Command::Resolve {
            intent: intent.ok_or(())?,
            mode: ResolveMode::Created {
                event_key: event_key.clone().ok_or(())?,
                event_id: event_id.clone().ok_or(())?,
            },
        },
        "--updated" => Command::Resolve {
            intent: intent.ok_or(())?,
            mode: ResolveMode::Updated {
                event_key: event_key.clone().ok_or(())?,
                event_id: event_id.clone().ok_or(())?,
            },
        },
        "--cancelled" => {
            if event_key.is_some() {
                return Err(());
            }
            Command::Resolve {
                intent: intent.ok_or(())?,
                mode: ResolveMode::Cancelled {
                    event_id: event_id.clone().ok_or(())?,
                },
            }
        }
        "--no-effect" => {
            if event_key.is_some() || event_id.is_some() {
                return Err(());
            }
            Command::Resolve {
                intent: intent.ok_or(())?,
                mode: ResolveMode::NoEffect,
            }
        }
        _ => return Err(()),
    };
    // Listing and no-effect resolutions take no identifiers beyond the intent;
    // effect resolutions always carry both sides of the mapping.
    if matches!(mode, Some("--list"))
        && (intent.is_some() || event_key.is_some() || event_id.is_some())
    {
        return Err(());
    }
    Ok(Args {
        guild,
        command,
        execute,
    })
}

pub(crate) async fn dispatch(args: &[String]) -> i32 {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        print!("{USAGE}");
        return 0;
    }
    let Ok(args) = parse(args) else {
        eprintln!("reconcile-event: invalid arguments.\n{USAGE}");
        return 2;
    };
    if !crate::self_role_handlers::staging_allowlist().contains(&args.guild) {
        eprintln!("reconcile-event: staging guild only");
        return 2;
    }
    let Ok(url) = std::env::var("TWO_DATABASE_URL") else {
        eprintln!("reconcile-event: TWO_DATABASE_URL is required");
        return 1;
    };
    // Shared parser refuses unknown query keys before driver diagnostics can
    // disclose secrets. Never print connection/SQL errors or identifiers.
    let db = match two_bot_cutover::connect(&url, 1, true).await {
        Ok(db) => db,
        Err(_) => {
            eprintln!("reconcile-event: connection failed (details redacted)");
            return 1;
        }
    };
    let store = InternalActionStore::new(db.pool().clone());
    let code = run(&store, &args.guild, &args.command, args.execute).await;
    db.close().await;
    code
}

fn resolution_of(
    mode: &ResolveMode,
) -> (
    TerminalResponse,
    ReconciliationEvidence,
    Option<(&str, &str)>,
) {
    match mode {
        ResolveMode::Created {
            event_key,
            event_id,
        } => (
            TerminalResponse::Success {
                resource_id: Some(DiscordId::new(event_id).expect("event id validated")),
                affected: 1,
                outcome: Some(EventOutcome::Created),
                stop_reason: None,
            },
            ReconciliationEvidence::DiscordConfirmedEffect,
            Some((event_key.as_str(), event_id.as_str())),
        ),
        ResolveMode::Updated {
            event_key,
            event_id,
        } => (
            TerminalResponse::Success {
                resource_id: Some(DiscordId::new(event_id).expect("event id validated")),
                affected: 1,
                outcome: Some(EventOutcome::Updated),
                stop_reason: None,
            },
            ReconciliationEvidence::DiscordConfirmedEffect,
            Some((event_key.as_str(), event_id.as_str())),
        ),
        ResolveMode::Cancelled { event_id } => (
            TerminalResponse::Success {
                resource_id: Some(DiscordId::new(event_id).expect("event id validated")),
                affected: 1,
                outcome: Some(EventOutcome::Cancelled),
                stop_reason: None,
            },
            ReconciliationEvidence::DiscordConfirmedEffect,
            None,
        ),
        ResolveMode::NoEffect => (
            TerminalResponse::Failure(TerminalFailure::NoEffect),
            ReconciliationEvidence::DiscordConfirmedNoEffect,
            None,
        ),
    }
}

fn describe(intent: i64, mode: &ResolveMode) -> String {
    match mode {
        ResolveMode::Created {
            event_key,
            event_id,
        } => {
            format!("record intent {intent} as created for key {event_key:?} at event {event_id}")
        }
        ResolveMode::Updated {
            event_key,
            event_id,
        } => {
            format!("record intent {intent} as updated for key {event_key:?} at event {event_id}")
        }
        ResolveMode::Cancelled { event_id } => {
            format!("record intent {intent} as cancelled at event {event_id}")
        }
        ResolveMode::NoEffect => format!("record intent {intent} as no-effect"),
    }
}

/// The row action a resolution must match. Effect modes pin their own verb;
/// a proven no-effect closes whatever the fenced row holds, so a cancel
/// intent clears without recording a cancellation that never happened.
fn expected_action<'a>(mode: &ResolveMode, unknown_action: Option<&'a str>) -> Option<&'a str> {
    match mode {
        ResolveMode::Created { .. } | ResolveMode::Updated { .. } => Some("event.upsert"),
        ResolveMode::Cancelled { .. } => Some("event.cancel"),
        ResolveMode::NoEffect => unknown_action,
    }
}

/// The truncation receipt for a capped page, if capped. A total within the
/// ceiling names the rerun that shows it all; past the ceiling no `--limit`
/// rerun converges (values clamp), so the receipt names the ceiling and the
/// way to advance instead of prescribing a looping rerun.
fn truncation_receipt(shown: i64, total: i64) -> Option<String> {
    if total <= shown {
        return None;
    }
    if total <= OPERATOR_LIST_MAX_LIMIT {
        return Some(format!(
            "showing {shown} of {total} rows (truncated; rerun with --limit {total})"
        ));
    }
    Some(format!(
        "showing {shown} of {total} rows (truncated to the {}-row ceiling; resolve shown rows and re-list to advance)",
        OPERATOR_LIST_MAX_LIMIT
    ))
}

async fn run(store: &InternalActionStore, guild: &str, command: &Command, execute: bool) -> i32 {
    match command {
        Command::List { limit } => match store
            .list_unknown_event_intents_limited(guild, *limit)
            .await
        {
            Ok((rows, total)) => {
                if rows.is_empty() {
                    println!("no unknown event intents");
                }
                let shown = rows.len() as i64;
                for row in rows {
                    // Event keys stay hashed in the database, so the listing
                    // names only the exact intent id the operator resolves.
                    // codeql[rust/cleartext-logging]: FP - intent ids, action
                    // words and UTC timestamps are non-secret operator
                    // references to stdout; hashes, keys and tokens never
                    // print; connection/SQL errors are redacted; output is
                    // staging-guild fenced; listing fenced intents is the
                    // tool purpose.
                    println!("{}\t{}\t{}", row.intent_id, row.action, row.created_at_utc);
                }
                // The store pages before counting, so a listing that ends
                // exactly at the cap prints no receipt; only a capped one
                // names its total and the way to see the rest.
                if let Some(receipt) = truncation_receipt(shown, total) {
                    println!("{receipt}");
                }
                0
            }
            Err(_) => {
                eprintln!("reconcile-event: surface read failed (details redacted)");
                1
            }
        },
        Command::Resolve { intent, mode } => {
            let description = describe(*intent, mode);
            if !execute {
                println!("DRY RUN: {description}");
                return 0;
            }
            let (response, evidence, mapping) = resolution_of(mode);
            // A no-effect closes the row's own verb: the listing names only
            // fenced unknown intents, and `resolve_event_intent` re-checks
            // the row under lock before writing, so a row that moved on is
            // still refused below.
            let looked_up;
            let unknown_action = if matches!(mode, ResolveMode::NoEffect) {
                match store.list_unknown_event_intents(guild).await {
                    Ok(rows) => {
                        looked_up = rows.into_iter().find(|row| row.intent_id == *intent);
                        looked_up.as_ref().map(|row| row.action.as_str())
                    }
                    Err(_) => {
                        eprintln!("reconcile-event: surface read failed (details redacted)");
                        return 1;
                    }
                }
            } else {
                None
            };
            let Some(action) = expected_action(mode, unknown_action) else {
                eprintln!(
                    "reconcile-event: intent {intent} is not an unknown event intent \
                     in this guild (details redacted); row stays fenced"
                );
                return 1;
            };
            match store
                .resolve_event_intent(*intent, action, guild, mapping, &response, evidence)
                .await
            {
                Ok(()) => {
                    println!("resolved unknown event intent: {description}");
                    0
                }
                Err(two_bot_core::internal_action_store::InternalStoreError::TransitionRefused) => {
                    eprintln!(
                        "reconcile-event: intent {intent} is not an unknown {action} \
                         intent in this guild (details redacted); row stays fenced"
                    );
                    1
                }
                Err(_) => {
                    eprintln!(
                        "reconcile-event: resolution failed (details redacted); row stays fenced"
                    );
                    1
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn list_parses() {
        let parsed = parse(&args(&["--guild", "222222222222222222", "--list"])).expect("list");
        assert_eq!(parsed.guild, "222222222222222222");
        assert!(matches!(
            parsed.command,
            Command::List { limit } if limit == OPERATOR_LIST_DEFAULT_LIMIT
        ));
        assert!(!parsed.execute);
    }

    #[test]
    fn list_limit_override_parses_and_clamps() {
        let parsed = parse(&args(&[
            "--guild",
            "222222222222222222",
            "--list",
            "--limit",
            "5",
        ]))
        .expect("override");
        assert!(matches!(parsed.command, Command::List { limit: 5 }));
        let parsed = parse(&args(&[
            "--guild",
            "222222222222222222",
            "--list",
            "--limit",
            "500",
        ]))
        .expect("clamped");
        assert!(matches!(
            parsed.command,
            Command::List { limit } if limit == OPERATOR_LIST_MAX_LIMIT
        ));
    }

    #[test]
    fn list_limit_rejects_non_positive_non_numeric_and_missing() {
        for value in ["0", "-5", "abc"] {
            assert!(
                parse(&args(&[
                    "--guild",
                    "222222222222222222",
                    "--list",
                    "--limit",
                    value
                ]))
                .is_err(),
                "--limit {value} is invalid"
            );
        }
        assert!(parse(&args(&[
            "--guild",
            "222222222222222222",
            "--list",
            "--limit"
        ]))
        .is_err());
    }

    #[test]
    fn truncation_receipt_names_rerun_within_ceiling_and_ceiling_past_it() {
        assert_eq!(truncation_receipt(0, 0), None);
        assert_eq!(truncation_receipt(25, 25), None);
        assert_eq!(truncation_receipt(30, 30), None);
        assert_eq!(
            truncation_receipt(5, 30).as_deref(),
            Some("showing 5 of 30 rows (truncated; rerun with --limit 30)")
        );
        assert_eq!(
            truncation_receipt(25, 150).as_deref(),
            Some(
                "showing 25 of 150 rows (truncated to the 100-row ceiling; resolve shown rows and re-list to advance)"
            )
        );
        assert_eq!(
            truncation_receipt(100, 150).as_deref(),
            Some(
                "showing 100 of 150 rows (truncated to the 100-row ceiling; resolve shown rows and re-list to advance)"
            )
        );
    }

    #[test]
    fn limit_is_refused_without_list() {
        assert!(parse(&args(&[
            "--guild",
            "222222222222222222",
            "--intent",
            "7",
            "--no-effect",
            "--limit",
            "5",
        ]))
        .is_err());
    }

    #[test]
    fn list_refuses_stray_identifiers() {
        assert!(parse(&args(&[
            "--guild",
            "222222222222222222",
            "--list",
            "--intent",
            "7"
        ]))
        .is_err());
    }

    #[test]
    fn created_needs_intent_key_and_event() {
        assert!(parse(&args(&[
            "--guild",
            "222222222222222222",
            "--intent",
            "7",
            "--created",
            "--event-key",
            "launch"
        ]))
        .is_err());
        let parsed = parse(&args(&[
            "--guild",
            "222222222222222222",
            "--intent",
            "7",
            "--created",
            "--event-key",
            "launch",
            "--event-id",
            "333333333333333333",
            "--execute",
        ]))
        .expect("created");
        assert!(matches!(
            parsed.command,
            Command::Resolve {
                intent: 7,
                mode: ResolveMode::Created { .. }
            }
        ));
        assert!(parsed.execute);
    }

    #[test]
    fn cancelled_forbids_event_key() {
        assert!(parse(&args(&[
            "--guild",
            "222222222222222222",
            "--intent",
            "7",
            "--cancelled",
            "--event-key",
            "launch",
            "--event-id",
            "333333333333333333",
        ]))
        .is_err());
        let parsed = parse(&args(&[
            "--guild",
            "222222222222222222",
            "--intent",
            "9",
            "--cancelled",
            "--event-id",
            "333333333333333333",
        ]))
        .expect("cancelled");
        assert!(matches!(
            parsed.command,
            Command::Resolve {
                intent: 9,
                mode: ResolveMode::Cancelled { .. }
            }
        ));
        assert!(!parsed.execute);
    }

    #[test]
    fn no_effect_takes_no_identifiers() {
        assert!(parse(&args(&[
            "--guild",
            "222222222222222222",
            "--intent",
            "7",
            "--no-effect",
            "--event-id",
            "333333333333333333",
        ]))
        .is_err());
        let parsed = parse(&args(&[
            "--guild",
            "222222222222222222",
            "--intent",
            "7",
            "--no-effect",
            "--execute",
        ]))
        .expect("no-effect");
        assert!(matches!(
            parsed.command,
            Command::Resolve {
                intent: 7,
                mode: ResolveMode::NoEffect
            }
        ));
    }

    #[test]
    fn resolutions_build_matching_receipts() {
        let created = ResolveMode::Created {
            event_key: "launch".to_owned(),
            event_id: "333333333333333333".to_owned(),
        };
        let (response, evidence, mapping) = resolution_of(&created);
        assert_eq!(
            response,
            TerminalResponse::Success {
                resource_id: Some(DiscordId::new("333333333333333333").unwrap()),
                affected: 1,
                outcome: Some(EventOutcome::Created),
                stop_reason: None,
            }
        );
        assert_eq!(evidence, ReconciliationEvidence::DiscordConfirmedEffect);
        assert_eq!(mapping, Some(("launch", "333333333333333333")));
        let (response, evidence, mapping) = resolution_of(&ResolveMode::NoEffect);
        assert_eq!(
            response,
            TerminalResponse::Failure(TerminalFailure::NoEffect)
        );
        assert_eq!(evidence, ReconciliationEvidence::DiscordConfirmedNoEffect);
        assert_eq!(mapping, None);
    }

    #[test]
    fn expected_action_pins_effects_and_passes_through_no_effect() {
        let created = ResolveMode::Created {
            event_key: "launch".to_owned(),
            event_id: "333333333333333333".to_owned(),
        };
        assert_eq!(expected_action(&created, None), Some("event.upsert"));
        let updated = ResolveMode::Updated {
            event_key: "launch".to_owned(),
            event_id: "333333333333333333".to_owned(),
        };
        assert_eq!(expected_action(&updated, None), Some("event.upsert"));
        let cancelled = ResolveMode::Cancelled {
            event_id: "333333333333333333".to_owned(),
        };
        assert_eq!(expected_action(&cancelled, None), Some("event.cancel"));
        // A proven no-effect closes the row's own verb, so a cancel intent
        // clears without recording a cancellation that never happened; an
        // intent with no fenced row stays refused.
        assert_eq!(
            expected_action(&ResolveMode::NoEffect, Some("event.cancel")),
            Some("event.cancel")
        );
        assert_eq!(
            expected_action(&ResolveMode::NoEffect, Some("event.upsert")),
            Some("event.upsert")
        );
        assert_eq!(expected_action(&ResolveMode::NoEffect, None), None);
    }
}
