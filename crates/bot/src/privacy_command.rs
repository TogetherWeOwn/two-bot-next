//! Member privacy self-serve (`/privacy opt-out` / `opt-in` / `status`;
//! parity §1 row 32).
//!
//! Thin wiring over the reviewed `two_bot_cutover::privacy` service: decode
//! the subcommand → confirm the guild plus the invoker identity → run the
//! service on the runtime pool → ephemeral completion. Routing (the `Always`
//! gate, the guild fence and the open-to-everyone permission row) happens in
//! the shared router before this runs, so the handler assumes an authorized
//! guild invocation and still refuses a guild-less interaction itself.
//!
//! The subject is always the invoker: the published definition carries no
//! target option, so one member can never purge another member's rows and no
//! caller-supplied ID ever reaches the service. The audit actor is the fixed
//! `member:self-serve` label — never a subject ID, count, content or secret
//! (the scope columns already name the pair, like every other receipt).
//!
//! Repeat taps never retake the purge's write-blocking table locks: under
//! the shared pair lock, an already-suppressed pair gets the already-opted-out
//! reply, and a pair with a purge receipt younger than the cooldown gets the
//! cooldown reply. Either early return writes no new receipt; only a fresh
//! purge takes the table locks and deletes.

use sqlx::{Pool, Postgres};
use twilight_model::application::interaction::{
    application_command::CommandOptionValue, Interaction,
};
use two_bot_cutover::{
    db::CutoverDb,
    privacy::{
        analytics_settings, is_opted_out, opt_back_in, opt_out_member_with_cooldown, OptOutOutcome,
    },
};
use two_bot_discord::ActionExecutor;

use crate::command_runtime::actor_id;
use crate::db_log::{db_class, discord_class};

/// Fixed audit actor for member self-serve: role attribution, never a
/// subject ID (see `docs/privacy.md`: the subject lives only in the scope
/// columns, never in the actor label).
const MEMBER_SELF_SERVE_ACTOR: &str = "member:self-serve";

/// Safe reply when the store fails — never leak sqlx internals to Discord.
const STORE_FAILURE_REPLY: &str = "Privacy command failed; try again.";

const GUILD_ONLY_REPLY: &str = "This command only works in a server.";

const UNKNOWN_MEMBER_REPLY: &str = "Could not identify you for this server; try again.";

const USAGE_REPLY: &str = "Use `/privacy opt-out`, `/privacy opt-in` or `/privacy status`.";

/// Purge cooldown for member self-serve: a pair with a purge receipt younger
/// than this window gets the cooldown reply instead of a fresh locking
/// purge, so alternating opt-in/opt-out cannot loop the write-blocking
/// locks. Assisted operator calls go through the service directly and are
/// unaffected.
const OPT_OUT_COOLDOWN_MINUTES: i32 = 10;

const ALREADY_OPTED_OUT_REPLY: &str = "You are already opted out of analytics for this server. \
     Your stored activity here was deleted and future activity will not be stored. \
     Use `/privacy opt-in` to resume.";

const OPT_OUT_COOLDOWN_REPLY: &str = "You recently changed your privacy setting; \
     try `/privacy opt-out` again in a few minutes.";

fn opt_out_reply(outcome: &OptOutOutcome) -> &'static str {
    match outcome {
        OptOutOutcome::OptedOut(_) => {
            "You are opted out of analytics for this server. \
             Your stored activity here was deleted and future activity will not be stored. \
             Moderation, safety and delivery records are unchanged. \
             Use `/privacy opt-in` to resume."
        }
        OptOutOutcome::AlreadyOptedOut => ALREADY_OPTED_OUT_REPLY,
        OptOutOutcome::Cooldown => OPT_OUT_COOLDOWN_REPLY,
    }
}

/// The `/privacy` subcommand the interaction invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrivacyAction {
    OptOut,
    OptIn,
    Status,
    Invalid,
}

/// Decode the invoked subcommand: the first option names it
/// (`opt-out` / `opt-in` / `status`). Anything else — missing options, a
/// non-subcommand value, an unknown name — is `Invalid` and refuses before
/// any database access.
pub(crate) fn privacy_action(interaction: &Interaction) -> PrivacyAction {
    let Interaction {
        data:
            Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)),
        ..
    } = interaction
    else {
        return PrivacyAction::Invalid;
    };
    let Some(first) = data.options.first() else {
        return PrivacyAction::Invalid;
    };
    if !matches!(first.value, CommandOptionValue::SubCommand(_)) {
        return PrivacyAction::Invalid;
    }
    match first.name.as_str() {
        "opt-out" => PrivacyAction::OptOut,
        "opt-in" => PrivacyAction::OptIn,
        "status" => PrivacyAction::Status,
        _ => PrivacyAction::Invalid,
    }
}

/// Complete the original ephemeral defer with `content`.
async fn finish(executor: &ActionExecutor, interaction: &Interaction, content: impl AsRef<str>) {
    if let Err(err) = executor
        .edit_interaction_response_with_blocked_retry(
            interaction.application_id.get(),
            &interaction.token,
            content.as_ref(),
        )
        .await
    {
        tracing::warn!(interaction_id = %interaction.id.get(), reason = discord_class(&err), "privacy reply edit failed");
    }
}

fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "on"
    } else {
        "off"
    }
}

/// `/privacy`: run the invoked subcommand for the invoker in this guild.
/// The caller defers first; every path below completes the defer with one
/// ephemeral edit and writes nothing for anyone but the invoker.
pub(crate) async fn privacy_command(
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    interaction: &Interaction,
) {
    let Some(guild_id) = interaction.guild_id.map(|id| id.get().to_string()) else {
        finish(executor, interaction, GUILD_ONLY_REPLY).await;
        return;
    };
    let member = actor_id(interaction);
    if member.is_empty() {
        finish(executor, interaction, UNKNOWN_MEMBER_REPLY).await;
        return;
    }
    // Borrow the runtime pool for one service call: no new connections, no
    // migrations, no fallback credential — exactly the pool the gateway runs.
    let db = CutoverDb::from_pool(pool.clone());
    match privacy_action(interaction) {
        PrivacyAction::OptOut => {
            // The service checks suppression and cooldown under the same
            // pair lock as opt-in, before taking any purge table locks.
            match opt_out_member_with_cooldown(
                &db,
                &guild_id,
                &member,
                MEMBER_SELF_SERVE_ACTOR,
                OPT_OUT_COOLDOWN_MINUTES,
            )
            .await
            {
                Ok(outcome) => finish(executor, interaction, opt_out_reply(&outcome)).await,
                Err(err) => {
                    tracing::warn!(reason = db_class(&err), "privacy opt-out failed");
                    finish(executor, interaction, STORE_FAILURE_REPLY).await;
                }
            }
        }
        PrivacyAction::OptIn => match opt_back_in(&db, &guild_id, &member).await {
            Ok(true) => {
                finish(
                    executor,
                    interaction,
                    "Analytics persistence is back on for this server. \
                     Rows deleted while you were opted out are not restored.",
                )
                .await;
            }
            Ok(false) => {
                finish(
                    executor,
                    interaction,
                    "You were not opted out; analytics persistence was already on for this server.",
                )
                .await;
            }
            Err(err) => {
                tracing::warn!(reason = db_class(&err), "privacy opt-in failed");
                finish(executor, interaction, STORE_FAILURE_REPLY).await;
            }
        },
        PrivacyAction::Status => {
            // Both reads are independent, so run them concurrently.
            let (suppressed, switches) = tokio::join!(
                is_opted_out(&db, &guild_id, &member),
                analytics_settings(&db, &guild_id)
            );
            match (suppressed, switches) {
                (Ok(suppressed), Ok((presence, activity, voice))) => {
                    finish(
                        executor,
                        interaction,
                        format!(
                            "Analytics for you in this server: {}. \
                             Server switches: presence {}, activity {}, voice {}.",
                            if suppressed { "opted out" } else { "on" },
                            on_off(presence),
                            on_off(activity),
                            on_off(voice),
                        ),
                    )
                    .await;
                }
                (Err(err), _) | (_, Err(err)) => {
                    tracing::warn!(reason = db_class(&err), "privacy status failed");
                    finish(executor, interaction, STORE_FAILURE_REPLY).await;
                }
            }
        }
        PrivacyAction::Invalid => {
            finish(executor, interaction, USAGE_REPLY).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use twilight_model::id::Id;

    #[tokio::test]
    async fn concurrent_opt_out_replies_once_then_already_and_preserves_cooldown() {
        let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
            eprintln!("SKIP privacy reply race: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let fixture = two_bot_testsupport::TestDatabase::create(
            &url,
            &sqlx::migrate!("../cutover/migrations"),
        )
        .await
        .expect("disposable privacy reply database");
        let (base, _) = url.rsplit_once('/').expect("guarded bootstrap URL");
        let fixture_url = format!("{base}/{}", fixture.name());
        // Wider than the herd so pool admission cannot serialize the race.
        let db = two_bot_cutover::connect_with_tls(
            &fixture_url,
            16,
            true,
            two_bot_core::database_tls::TlsPolicy::LocalOnly,
        )
        .await
        .expect("connect to owned disposable database");
        const GUILD: u64 = 100000000000000431;
        const MEMBER: u64 = 100000000000000432;
        let guild = GUILD.to_string();
        let member = MEMBER.to_string();

        let replies = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = Arc::clone(&replies);
        let app = axum::Router::new().fallback(axum::routing::patch(
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let recorded = Arc::clone(&recorded);
                async move {
                    recorded
                        .lock()
                        .expect("record reply")
                        .push(body["content"].as_str().expect("reply text").to_owned());
                    axum::Json(serde_json::json!({}))
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock REST listen");
        let origin = format!("http://{}", listener.local_addr().expect("mock address"));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock REST serve");
        });
        let executor = ActionExecutor::with_proxy("mock-only-token".into(), Some(origin))
            .expect("mock executor");
        let interaction = |tap: u64, action: &str| {
            let mut invocation = crate::command_runtime_tests::slash(
                "privacy",
                None,
                vec![twilight_model::application::interaction::application_command::CommandDataOption {
                    name: action.to_owned(),
                    value: CommandOptionValue::SubCommand(Vec::new()),
                }],
            );
            invocation.guild_id = Some(Id::new(GUILD));
            invocation.id = Id::new(tap + 1);
            invocation.token = format!("privacy-tap-{tap}");
            let invoker = invocation.member.as_mut().expect("member");
            invoker.user.as_mut().expect("member user").id = Id::new(MEMBER);
            invoker.permissions = Some(twilight_model::guild::Permissions::empty());
            invocation
        };
        // Direct handler calls bypass the interaction lane so this proves the
        // service boundary, not accidental serialization by gateway routing.
        // The account is empty: its winning tap must still get the fresh reply.
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let mut taps = Vec::new();
        for tap in 0..8 {
            let db = db.clone();
            let executor = executor.clone();
            let barrier = Arc::clone(&barrier);
            let invocation = interaction(tap, "opt-out");
            taps.push(tokio::spawn(async move {
                barrier.wait().await;
                privacy_command(db.pool(), &executor, &invocation).await;
            }));
        }
        for tap in taps {
            tap.await.expect("privacy tap");
        }
        {
            let replies = replies.lock().expect("read replies");
            assert_eq!(replies.len(), 8);
            assert_eq!(
                replies
                    .iter()
                    .filter(|text| text.starts_with("You are opted out"))
                    .count(),
                1
            );
            assert_eq!(
                replies
                    .iter()
                    .filter(|text| text.as_str() == ALREADY_OPTED_OUT_REPLY)
                    .count(),
                7
            );
        }
        let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM privacy_opt_out_audit")
            .fetch_one(db.pool())
            .await
            .expect("receipts");
        assert_eq!(receipts, 1);
        assert!(is_opted_out(&db, &guild, &member)
            .await
            .expect("suppressed"));

        privacy_command(db.pool(), &executor, &interaction(8, "opt-in")).await;
        privacy_command(db.pool(), &executor, &interaction(9, "opt-out")).await;
        assert!(!is_opted_out(&db, &guild, &member)
            .await
            .expect("re-enabled"));
        {
            let replies = replies.lock().expect("read replies");
            assert!(replies[8].starts_with("Analytics persistence is back on"));
            assert_eq!(replies[9], OPT_OUT_COOLDOWN_REPLY);
        }
        // The member cooldown does not apply to assisted operator calls.
        two_bot_cutover::privacy::opt_out_member(&db, &guild, &member, "operator:fixture")
            .await
            .expect("operator opt-out ignores member cooldown");
        let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM privacy_opt_out_audit")
            .fetch_one(db.pool())
            .await
            .expect("receipts");
        assert_eq!(receipts, 2);
        server.abort();
        db.close().await;
        fixture.close().await.expect("drop disposable database");
    }

    #[test]
    fn opt_out_reply_distinguishes_empty_purge_already_and_cooldown() {
        use two_bot_cutover::privacy::OptOutOutcome;

        // A first opt-out with no stored rows is still a real transition.
        let fresh = opt_out_reply(&OptOutOutcome::OptedOut(Vec::new()));
        assert!(fresh.starts_with("You are opted out of analytics"));
        assert_eq!(
            opt_out_reply(&OptOutOutcome::AlreadyOptedOut),
            ALREADY_OPTED_OUT_REPLY
        );
        assert_eq!(
            opt_out_reply(&OptOutOutcome::Cooldown),
            OPT_OUT_COOLDOWN_REPLY
        );
        assert_ne!(fresh, ALREADY_OPTED_OUT_REPLY);
        assert_ne!(fresh, OPT_OUT_COOLDOWN_REPLY);
    }
}
