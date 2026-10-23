//! Timed polls over the shared router, governed REST lane and durable store.
//! Parent row locks order votes and final edits. Results use a durable send
//! fence, not Discord's short-lived nonce cache, as their replay authority.

use crate::{
    interactions::{deferred_response, RoutedInteraction},
    ActionExecutor, DiscordError,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use twilight_model::application::interaction::{
    application_command::{CommandDataOption, CommandOptionValue},
    Interaction, InteractionData,
};
use two_bot_core::{
    poll_store::{self, Poll},
    polls::PollInput,
    ComponentHandler, ComponentOutcome, HandlerId, SlashOutcome,
};

fn db_error(_: sqlx::Error) -> DiscordError {
    DiscordError::Unavailable("poll storage unavailable".into())
}

pub(crate) fn owns(routed: &RoutedInteraction) -> bool {
    matches!(
        routed,
        RoutedInteraction::Slash {
            outcome: SlashOutcome::Handled {
                handler: HandlerId::Poll
            },
            ..
        } | RoutedInteraction::Component {
            outcome: ComponentOutcome::Handled {
                handler: ComponentHandler::PollVote
            },
            ..
        }
    )
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())
}

fn string<'a>(args: &'a [CommandDataOption], name: &str) -> Option<&'a str> {
    match &args.iter().find(|o| o.name == name)?.value {
        CommandOptionValue::String(s) => Some(s),
        _ => None,
    }
}

/// Reject unknown/duplicate fields and sparse options rather than silently
/// taking the first of attacker-controlled arguments.
fn create_input(args: &[CommandDataOption]) -> Option<PollInput> {
    let mut seen = std::collections::HashSet::new();
    for arg in args {
        if !seen.insert(arg.name.as_str()) {
            return None;
        }
        if arg.name == "question" || arg.name == "duration-minutes" {
            continue;
        }
        let i = arg.name.strip_prefix("option")?.parse::<usize>().ok()?;
        if !(1..=10).contains(&i) || arg.name != format!("option{i}") {
            return None;
        }
    }
    let minutes = match args.iter().find(|o| o.name == "duration-minutes")?.value {
        CommandOptionValue::Integer(n) => n,
        _ => return None,
    };
    let mut options = Vec::new();
    for i in 1..=10 {
        let key = format!("option{i}");
        if let Some(s) = string(args, &key) {
            options.push(s.to_owned());
        } else if args.iter().any(|o| o.name == key)
            || i <= 2
            || (i + 1..=10).any(|j| args.iter().any(|o| o.name == format!("option{j}")))
        {
            return None;
        } else {
            break;
        }
    }
    PollInput::parse(string(args, "question")?, &options, minutes)
}

struct PollFailure {
    error: DiscordError,
    vote_saved: bool,
}

impl From<DiscordError> for PollFailure {
    fn from(error: DiscordError) -> Self {
        Self {
            error,
            vote_saved: false,
        }
    }
}

pub(crate) async fn handle(
    pool: &PgPool,
    rest: &ActionExecutor,
    interaction: &Interaction,
) -> Result<bool, DiscordError> {
    rest.answer_interaction_with_blocked_retry(
        interaction.id.get(),
        &interaction.token,
        &deferred_response(true),
    )
    .await?;
    let reply = execute(pool, rest, interaction).await;
    let content = match &reply {
        Ok(s) => s.as_str(),
        Err(failure) if failure.vote_saved =>
            "Vote saved, but the poll display could not be confirmed. Check the poll message later.",
        Err(_) => "Poll operation could not be confirmed. Do not retry creation; check the poll message first.",
    };
    rest.edit_interaction_response_with_blocked_retry(
        interaction.application_id.get(),
        &interaction.token,
        content,
    )
    .await?;
    reply.map(|_| true).map_err(|failure| failure.error)
}

async fn execute(
    pool: &PgPool,
    rest: &ActionExecutor,
    interaction: &Interaction,
) -> Result<String, PollFailure> {
    let refused = || {
        "Invalid poll request. Use /poll create with 2–10 choices and 1–10080 minutes, or /poll close id.".to_owned()
    };
    let Some(guild) = interaction.guild_id.map(|id| id.to_string()) else {
        return Ok(refused());
    };
    let Some(user) = interaction
        .member
        .as_ref()
        .and_then(|m| m.user.as_ref())
        .filter(|u| !u.bot)
    else {
        return Ok("Only server members, not bots, may use polls.".into());
    };
    match interaction.data.as_ref() {
        Some(InteractionData::ApplicationCommand(data)) => {
            if data.options.len() != 1 {
                return Ok(refused());
            }
            let sub = &data.options[0];
            let CommandOptionValue::SubCommand(args) = &sub.value else {
                return Ok(refused());
            };
            match sub.name.as_str() {
                "create" => {
                    let Some(input) = create_input(args) else {
                        return Ok(refused());
                    };
                    let Some(channel) = interaction.channel.as_ref().map(|c| c.id.to_string())
                    else {
                        return Ok(refused());
                    };
                    let id = interaction.id.to_string();
                    if !poll_store::reserve(
                        pool,
                        &guild,
                        &id,
                        &channel,
                        &user.id.to_string(),
                        &input,
                    )
                    .await
                    .map_err(db_error)?
                    {
                        return Ok("Poll limit reached (10 open per server, 3 per author), or request already recorded.".into());
                    }
                    let (tx, poll) = poll_store::lock(pool, &guild, &id)
                        .await
                        .map_err(db_error)?;
                    let poll = poll.ok_or_else(|| {
                        DiscordError::Unavailable("poll reservation missing".into())
                    })?;
                    // The reservation is durable, and this path is never replayed.
                    let counts = vec![0; poll.options.len()];
                    let message = rest
                        .write_poll_message(&channel, None, render(&poll, &counts, false, false))
                        .await?;
                    let mut tx = tx;
                    sqlx::query("UPDATE polls SET message_id=$3,updated_at=now() WHERE guild_id=$1 AND id=$2")
                        .bind(&guild).bind(&id).bind(message).execute(&mut *tx).await.map_err(db_error)?;
                    tx.commit().await.map_err(db_error)?;
                    Ok(format!("Poll {id} created. Vote using its buttons; /poll close id:{id} closes early."))
                }
                "close" => {
                    let Some(id) = string(args, "id").filter(|id| valid_id(id)) else {
                        return Ok(refused());
                    };
                    if args.len() != 1 {
                        return Ok(refused());
                    }
                    close(pool, rest, &guild, id, Some(&user.id.to_string()))
                        .await
                        .map_err(Into::into)
                }
                _ => Ok(refused()),
            }
        }
        Some(InteractionData::MessageComponent(data)) => {
            let parts: Vec<&str> = data.custom_id.split(':').collect();
            if parts.len() != 3
                || parts[0] != "poll"
                || !valid_id(parts[1])
                || parts[2].len() > 2
                || !parts[2].bytes().all(|b| b.is_ascii_digit())
            {
                return Ok(refused());
            }
            let Ok(index) = parts[2].parse::<usize>() else {
                return Ok(refused());
            };
            let (mut tx, poll) = poll_store::lock(pool, &guild, parts[1])
                .await
                .map_err(db_error)?;
            let Some(poll) = poll else {
                return Ok("That poll does not exist.".into());
            };
            if !poll_store::vote(&mut tx, &guild, &poll, &user.id.to_string(), index)
                .await
                .map_err(db_error)?
            {
                return Ok(
                    "Vote refused: poll closed/expired, invalid choice, or voter limit reached."
                        .into(),
                );
            }
            // A displayed vote must already be durable. A failed or cancelled
            // PATCH leaves the dirty marker committed for the next tick.
            tx.commit().await.map_err(db_error)?;
            refresh_display(pool, rest, &guild, &poll.id)
                .await
                .map_err(|error| PollFailure {
                    error,
                    vote_saved: true,
                })?;
            Ok("Vote saved. Click another choice to change it.".into())
        }
        _ => Ok(refused()),
    }
}

/// Re-read under the same lock used by closers: never apply an older open
/// snapshot after a close. Retrying this PATCH cannot create another message.
async fn refresh_display(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    id: &str,
) -> Result<(), DiscordError> {
    let (mut tx, poll) = poll_store::lock(pool, guild, id).await.map_err(db_error)?;
    let Some(poll) = poll.filter(|p| p.open && p.display_dirty) else {
        return Ok(());
    };
    let Some(message) = poll.message.as_deref() else {
        return Ok(());
    };
    let counts = poll_store::counts(&mut tx, guild, &poll)
        .await
        .map_err(db_error)?;
    let rejected = match rest
        .write_poll_message(
            &poll.channel,
            Some(message),
            render(&poll, &counts, false, false),
        )
        .await
    {
        Ok(_) => None,
        // Stop retrying a definitively rejected display; retain the ballot.
        // Guards, rate limits and uncertain edits remain dirty for recovery.
        Err(error @ DiscordError::Rejected(_)) => Some(error),
        Err(error) => return Err(error),
    };
    sqlx::query(
        "UPDATE polls SET display_dirty=false,updated_at=now() WHERE guild_id=$1 AND id=$2",
    )
    .bind(guild)
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    rejected.map_or(Ok(()), Err)
}

/// Shared manual/scheduled close. The durable send fence is committed before
/// editing/posting. Competing closers skip it; no process-local mutex required.
async fn close(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    id: &str,
    author: Option<&str>,
) -> Result<String, DiscordError> {
    let (mut tx, poll) = poll_store::lock(pool, guild, id).await.map_err(db_error)?;
    let Some(poll) = poll else {
        return Ok("That poll does not exist.".into());
    };
    if author.is_some_and(|a| a != poll.author) {
        return Ok("Only the poll author may close it.".into());
    }
    if author.is_none() && poll.open && !poll.expired {
        tx.rollback().await.map_err(db_error)?;
        refresh_display(pool, rest, guild, id).await?;
        return Ok("Poll display reconciled.".into());
    }
    if poll.posted {
        return Ok("Poll already closed; results were posted.".into());
    }
    if poll.started {
        return Ok(
            "Poll closed; results delivery is pending confirmation. It will not be replayed."
                .into(),
        );
    }
    let counts = poll_store::counts(&mut tx, guild, &poll)
        .await
        .map_err(db_error)?;
    poll_store::claim_results(&mut tx, guild, &poll, &counts)
        .await
        .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    // Expired invisible reservations release quota without inventing a
    // receipt or replaying a possibly accepted creation/results send.
    let Some(message) = poll.message.as_deref() else {
        return Ok("Poll closed; creation receipt still requires confirmation.".into());
    };
    if let Err(error) = rest
        .write_poll_message(
            &poll.channel,
            Some(message),
            render(&poll, &counts, true, false),
        )
        .await
    {
        // A confirmed rejection of the display edit cannot duplicate results.
        // Every other failure retains the existing no-replay fence.
        if !matches!(error, DiscordError::Rejected(_)) {
            return Err(error);
        }
    }
    let receipt = rest
        .write_poll_message(&poll.channel, None, render(&poll, &counts, true, true))
        .await?;
    poll_store::settle_results(pool, guild, id, &receipt)
        .await
        .map_err(db_error)?;
    Ok("Poll closed. Results posted.".into())
}

pub(crate) async fn tick(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
) -> Result<(), DiscordError> {
    let mut first_error = None;
    for id in poll_store::due(pool, guild).await.map_err(db_error)? {
        if let Err(error) = close(pool, rest, guild, &id, None).await {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn inert(text: &str) -> String {
    text.replace('@', "@\u{200b}")
}

fn label(text: &str) -> String {
    let mut units = 0;
    inert(text)
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= 80
        })
        .collect()
}

fn render(poll: &Poll, counts: &[i64], closed: bool, results: bool) -> Value {
    let mut description = inert(&poll.question);
    for (index, option) in poll.options.iter().enumerate() {
        description.push_str(&format!(
            "\n{}. {} — {}",
            index + 1,
            inert(option),
            counts.get(index).copied().unwrap_or(0)
        ));
    }
    if results {
        if let Some(index) = poll_store::winner(counts).filter(|i| *i < poll.options.len()) {
            description.push_str(&format!("\nWinner: {}", inert(&poll.options[index])));
        } else {
            description.push_str("\nNo votes cast; no winner.");
        }
    }
    let buttons: Vec<Value> = poll.options.iter().enumerate().map(|(i,option)| json!({
        "type":2,"style":1,"label":label(option),"custom_id":format!("poll:{}:{i}",poll.id),"disabled":closed
    })).collect();
    let components: Vec<Value> = if results {
        Vec::new()
    } else {
        buttons
            .chunks(5)
            .map(|chunk| json!({"type":1,"components":chunk}))
            .collect()
    };
    json!({
        "embeds":[{"title": if results {"Poll results"} else if closed {"Poll closed"} else {"Poll open — click to vote or change"}, "description":description,
            "footer":{"text":format!("Poll {} • {}",poll.id,if closed {"Closed"} else {"One vote per member; author may /poll close"})}}],
        "components":components,"allowed_mentions":{"parse":[]},
        "nonce":format!("{}-{}",if results {"r"} else {"p"},poll.id),"enforce_nonce":true
    })
}
