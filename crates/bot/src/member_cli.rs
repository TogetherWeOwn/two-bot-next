//! Operator-only member-moderation reconciliation. No gateway, no Discord
//! calls, no migrations.
//!
//! The unban sweep never reclaims uncertain rows by age: prepared ban
//! intents, running dispatches and quarantined imports stay fenced until an
//! operator resolves them here with authoritative exact-intent evidence.
//! Listing is read-only and runs any time; every mutation needs `--execute`
//! and resolves only the exact attempt named (generation/claim token), never
//! by age or current remote state. Claim tokens never print: fetch a token
//! with a direct database read when a resolution needs one.

// Operator command intentionally emits its dry-run/execute report to stdout.
#![allow(clippy::print_stdout)]

use two_bot_core::{
    funnel::format_iso_millis,
    member_moderation::{
        BanAttempt, HistoricalBanAcceptance, MemberModerationStore, UnbanResolution, UncertainKind,
    },
    member_moderation_store::PgMemberModerationStore,
};

pub(crate) const USAGE: &str = "\
  two-bot reconcile-member --guild <id> --list
      List fenced rows: prepared ban intents, running dispatches and
      quarantined imports. Read-only; claim tokens never print.
      Env: TWO_DATABASE_URL (required). Staging guild only.

  two-bot reconcile-member --guild <id> (--confirm-ban | --reject-ban) --request <req> --user <id> --generation <n> [--execute]
      Confirm (observed Discord acceptance) or reject (definite refusal) one
      exact prepared ban attempt. Generation mismatch resolves nothing.

  two-bot reconcile-member --guild <id> --resolve-unban --request <req> --claim <token> (--completed | --void) [--execute]
      Close one exact uncertain dispatch: --completed proves the DELETE
      landed, --void proves it cannot land. Fetch the claim token with a
      direct database read; it never prints here.

  two-bot reconcile-member --guild <id> --accept-historical --request <req> --user <id> --generation <n> --later-request <req> --later-generation <n> --acceptance <evidence-id> --ordering <evidence-id> --actor <actor> [--execute]
      Accept an older prepared PUT that provably landed before a later
      accepted PUT. Evidence ids are bounded non-secret references; age, the
      generation order and current banned status are never proof.
      Without --execute every mutation above prints DRY RUN and writes nothing.
";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    List,
    ConfirmBan {
        request: String,
        user: String,
        generation: i64,
    },
    RejectBan {
        request: String,
        user: String,
        generation: i64,
    },
    ResolveUnban {
        request: String,
        claim: String,
        completed: bool,
    },
    AcceptHistorical {
        request: String,
        user: String,
        generation: i64,
        later_request: String,
        later_generation: i64,
        acceptance: String,
        ordering: String,
        actor: String,
    },
}

struct Args {
    guild: String,
    command: Command,
    execute: bool,
}

fn snowflake(value: &str) -> bool {
    two_bot_cutover::is_snowflake(value)
        && value.parse::<u64>().ok().is_some_and(|number| number > 0)
}

/// Bounded non-secret reference (evidence ids, actors, request ids, tokens):
/// short, printable, no control characters — never raw responses or secrets.
fn reference(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| !c.is_control() && (c.is_ascii_graphic() || c == ' '))
}

fn parse(args: &[String]) -> Result<Args, ()> {
    let mut guild = None;
    let mut request = None;
    let mut user = None;
    let mut generation = None;
    let mut claim = None;
    let mut later_request = None;
    let mut later_generation = None;
    let mut acceptance = None;
    let mut ordering = None;
    let mut actor = None;
    let mut mode: Option<&str> = None;
    let mut completed = None;
    let mut execute = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--guild" if guild.is_none() => guild = Some(args.next().ok_or(())?.clone()),
            "--request" if request.is_none() => request = Some(args.next().ok_or(())?.clone()),
            "--user" if user.is_none() => user = Some(args.next().ok_or(())?.clone()),
            "--generation" if generation.is_none() => {
                generation = Some(args.next().ok_or(())?.parse::<i64>().map_err(|_| ())?);
            }
            "--claim" if claim.is_none() => claim = Some(args.next().ok_or(())?.clone()),
            "--later-request" if later_request.is_none() => {
                later_request = Some(args.next().ok_or(())?.clone());
            }
            "--later-generation" if later_generation.is_none() => {
                later_generation = Some(args.next().ok_or(())?.parse::<i64>().map_err(|_| ())?);
            }
            "--acceptance" if acceptance.is_none() => {
                acceptance = Some(args.next().ok_or(())?.clone());
            }
            "--ordering" if ordering.is_none() => {
                ordering = Some(args.next().ok_or(())?.clone());
            }
            "--actor" if actor.is_none() => actor = Some(args.next().ok_or(())?.clone()),
            "--list"
            | "--confirm-ban"
            | "--reject-ban"
            | "--resolve-unban"
            | "--accept-historical"
                if mode.is_none() =>
            {
                mode = Some(arg.as_str());
            }
            "--completed" if completed.is_none() => completed = Some(true),
            "--void" if completed.is_none() => completed = Some(false),
            "--execute" if !execute => execute = true,
            _ => return Err(()),
        }
    }
    let guild = guild.filter(|id| snowflake(id)).ok_or(())?;
    let command = match mode.ok_or(())? {
        "--list" => Command::List,
        "--confirm-ban" | "--reject-ban" => {
            let request = request.filter(|id| reference(id)).ok_or(())?;
            let user = user.filter(|id| snowflake(id)).ok_or(())?;
            let generation = generation.ok_or(())?;
            if mode == Some("--confirm-ban") {
                Command::ConfirmBan {
                    request,
                    user,
                    generation,
                }
            } else {
                Command::RejectBan {
                    request,
                    user,
                    generation,
                }
            }
        }
        "--resolve-unban" => Command::ResolveUnban {
            request: request.filter(|id| reference(id)).ok_or(())?,
            claim: claim.filter(|id| reference(id)).ok_or(())?,
            completed: completed.ok_or(())?,
        },
        "--accept-historical" => Command::AcceptHistorical {
            request: request.filter(|id| reference(id)).ok_or(())?,
            user: user.filter(|id| snowflake(id)).ok_or(())?,
            generation: generation.ok_or(())?,
            later_request: later_request.filter(|id| reference(id)).ok_or(())?,
            later_generation: later_generation.ok_or(())?,
            acceptance: acceptance.filter(|id| reference(id)).ok_or(())?,
            ordering: ordering.filter(|id| reference(id)).ok_or(())?,
            actor: actor.filter(|id| reference(id)).ok_or(())?,
        },
        _ => return Err(()),
    };
    Ok(Args {
        guild,
        command,
        execute,
    })
}

fn now_iso() -> String {
    format_iso_millis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64,
    )
}

pub(crate) async fn dispatch(args: &[String]) -> i32 {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        print!("{USAGE}");
        return 0;
    }
    let Ok(args) = parse(args) else {
        eprintln!("reconcile-member: invalid arguments.\n{USAGE}");
        return 2;
    };
    if !crate::self_role_handlers::staging_allowlist().contains(&args.guild) {
        eprintln!("reconcile-member: staging guild only");
        return 2;
    }
    let Ok(url) = std::env::var("TWO_DATABASE_URL") else {
        eprintln!("reconcile-member: TWO_DATABASE_URL is required");
        return 1;
    };
    // Shared parser refuses unknown query keys before driver diagnostics can
    // disclose secrets. Never print connection/SQL errors or identifiers.
    let db = match two_bot_cutover::connect(&url, 1, true).await {
        Ok(db) => db,
        Err(_) => {
            eprintln!("reconcile-member: connection failed (details redacted)");
            return 1;
        }
    };
    let store = PgMemberModerationStore::new(db.pool().clone(), args.guild.clone());
    let code = run(&store, &args.guild, &args.command, args.execute).await;
    db.close().await;
    code
}

async fn run(
    store: &PgMemberModerationStore,
    guild: &str,
    command: &Command,
    execute: bool,
) -> i32 {
    match command {
        Command::List => match store.surface_uncertain(guild).await {
            Ok(rows) => {
                if rows.is_empty() {
                    println!("no fenced member rows");
                }
                for row in rows {
                    let detail = match row.kind {
                        UncertainKind::PreparedBan => format!(
                            "generation={}",
                            row.generation.map(|g| g.to_string()).unwrap_or_default()
                        ),
                        UncertainKind::RunningUnban | UncertainKind::QuarantinedUnban => {
                            format!("execute_at={}", row.execute_at.as_deref().unwrap_or(""))
                        }
                    };
                    println!(
                        "{}\t{}\t{}\t{detail}",
                        row.kind.as_str(),
                        row.request_id,
                        row.user_id
                    );
                }
                0
            }
            Err(_) => {
                eprintln!("reconcile-member: surface read failed (details redacted)");
                1
            }
        },
        Command::ConfirmBan {
            request,
            user,
            generation,
        } => {
            resolve(
                store,
                guild,
                execute,
                &format!("confirm prepared ban {request} generation {generation}"),
                |store, now| async move {
                    store
                        .confirm_ban_attempt(
                            guild,
                            user,
                            request,
                            BanAttempt {
                                generation: *generation,
                            },
                            now,
                        )
                        .await
                },
            )
            .await
        }
        Command::RejectBan {
            request,
            user,
            generation,
        } => {
            resolve(
                store,
                guild,
                execute,
                &format!("reject prepared ban {request} generation {generation}"),
                |store, now| async move {
                    store
                        .reject_ban_attempt(
                            guild,
                            user,
                            request,
                            BanAttempt {
                                generation: *generation,
                            },
                            now,
                        )
                        .await
                },
            )
            .await
        }
        Command::ResolveUnban {
            request,
            claim,
            completed,
        } => {
            let resolution = if *completed {
                UnbanResolution::Completed
            } else {
                UnbanResolution::Void
            };
            let outcome = if *completed { "completed" } else { "void" };
            resolve(
                store,
                guild,
                execute,
                &format!("resolve uncertain unban {request} as {outcome}"),
                |store, _| async move {
                    store
                        .resolve_uncertain_unban(request, claim, resolution)
                        .await
                },
            )
            .await
        }
        Command::AcceptHistorical {
            request,
            user,
            generation,
            later_request,
            later_generation,
            acceptance,
            ordering,
            actor,
        } => {
            let evidence = HistoricalBanAcceptance {
                later_request_id: later_request.clone(),
                later_attempt: BanAttempt {
                    generation: *later_generation,
                },
                acceptance_evidence_id: acceptance.clone(),
                ordering_evidence_id: ordering.clone(),
                actor_id: actor.clone(),
            };
            resolve(
                store,
                guild,
                execute,
                &format!("accept historical ban {request} generation {generation}"),
                |store, now| async move {
                    store
                        .resolve_historical_ban_acceptance(
                            guild,
                            user,
                            request,
                            BanAttempt {
                                generation: *generation,
                            },
                            &evidence,
                            now,
                        )
                        .await
                },
            )
            .await
        }
    }
}

/// Dry-run by default: name the exact resolution, write nothing. With
/// `--execute`, run the attempt-fenced write once; a generation/claim
/// mismatch resolves nothing and reports failure without guessing.
async fn resolve<'a, F, Fut>(
    store: &'a PgMemberModerationStore,
    guild: &str,
    execute: bool,
    intent: &str,
    write: F,
) -> i32
where
    F: FnOnce(&'a PgMemberModerationStore, &'a str) -> Fut + Send,
    Fut: std::future::Future<Output = Result<(), two_bot_core::member_moderation::StoreError>>
        + Send
        + 'a,
{
    let _ = guild;
    if !execute {
        println!("DRY RUN: would {intent}; pass --execute to write");
        return 0;
    }
    let now = now_iso();
    match write(store, &now).await {
        Ok(()) => {
            println!("RECONCILED: {intent}");
            0
        }
        Err(_) => {
            eprintln!("reconcile-member: resolution refused or failed (details redacted); row stays fenced");
            1
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
    fn confirm_needs_exact_attempt() {
        assert!(parse(&args(&[
            "--guild",
            "222222222222222222",
            "--confirm-ban",
            "--request",
            "r"
        ]))
        .is_err());
        let parsed = parse(&args(&[
            "--guild",
            "222222222222222222",
            "--confirm-ban",
            "--request",
            "r",
            "--user",
            "333333333333333333",
            "--generation",
            "7",
            "--execute",
        ]))
        .expect("confirm");
        assert!(matches!(
            parsed.command,
            Command::ConfirmBan { generation: 7, .. }
        ));
        assert!(parsed.execute);
    }

    #[test]
    fn resolve_needs_outcome_and_token() {
        assert!(parse(&args(&[
            "--guild",
            "222222222222222222",
            "--resolve-unban",
            "--request",
            "r",
            "--claim",
            "t",
        ]))
        .is_err());
        let parsed = parse(&args(&[
            "--guild",
            "222222222222222222",
            "--resolve-unban",
            "--request",
            "r",
            "--claim",
            "t",
            "--void",
        ]))
        .expect("resolve");
        assert!(matches!(
            parsed.command,
            Command::ResolveUnban {
                completed: false,
                ..
            }
        ));
    }

    #[test]
    fn historical_needs_evidence_and_actor() {
        let parsed = parse(&args(&[
            "--guild",
            "222222222222222222",
            "--accept-historical",
            "--request",
            "r",
            "--user",
            "333333333333333333",
            "--generation",
            "3",
            "--later-request",
            "s",
            "--later-generation",
            "5",
            "--acceptance",
            "ev-a",
            "--ordering",
            "ev-o",
            "--actor",
            "op-1",
        ]))
        .expect("historical");
        assert!(matches!(parsed.command, Command::AcceptHistorical { .. }));
        assert!(!parsed.execute);
    }
}
