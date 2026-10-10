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
    TerminalResponse,
};
use two_bot_core::internal_actions::{is_snowflake, valid_event_key};

pub(crate) const USAGE: &str = "\
  two-bot reconcile-event --guild <id> --list
      List unknown event.upsert/event.cancel intents oldest first.
      Read-only. Env: TWO_DATABASE_URL (required). Staging guild only.

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
      without touching mappings. Retrying then needs a new Idempotency-Key.
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
    List,
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
            "--execute" if !execute => execute = true,
            _ => return Err(()),
        }
    }
    let guild = guild.filter(|id| is_snowflake(id)).ok_or(())?;
    let command = match mode.ok_or(())? {
        "--list" => Command::List,
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
            },
            ReconciliationEvidence::DiscordConfirmedEffect,
            Some((event_key.as_str(), event_id.as_str())),
        ),
        ResolveMode::Cancelled { event_id } => (
            TerminalResponse::Success {
                resource_id: Some(DiscordId::new(event_id).expect("event id validated")),
                affected: 1,
                outcome: Some(EventOutcome::Cancelled),
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

async fn run(store: &InternalActionStore, guild: &str, command: &Command, execute: bool) -> i32 {
    match command {
        Command::List => match store.list_unknown_event_intents(guild).await {
            Ok(rows) => {
                if rows.is_empty() {
                    println!("no unknown event intents");
                }
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
            let expected_action = match mode {
                ResolveMode::Cancelled { .. } => "event.cancel",
                _ => "event.upsert",
            };
            match store
                .resolve_event_intent(
                    *intent,
                    expected_action,
                    guild,
                    mapping,
                    &response,
                    evidence,
                )
                .await
            {
                Ok(()) => {
                    println!("resolved unknown event intent: {description}");
                    0
                }
                Err(two_bot_core::internal_action_store::InternalStoreError::TransitionRefused) => {
                    eprintln!(
                        "reconcile-event: intent {intent} is not an unknown {expected_action} \
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
        assert!(matches!(parsed.command, Command::List));
        assert!(!parsed.execute);
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
}
