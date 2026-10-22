//! Public, read-only invite counts from committed gateway membership evidence.

use std::{collections::HashMap, fmt::Write as _, time::Duration};

use sqlx::PgPool;
use twilight_model::{
    application::interaction::{
        application_command::CommandOptionValue, Interaction, InteractionData,
    },
    channel::message::AllowedMentions,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use two_bot_core::{leveling::sanitize_rank_name, message_safety};
use two_bot_discord::ActionExecutor;

const INVITE_READ_NOTICE: &str = "Invite counts are unavailable. Ask a server admin to check the bot's Manage Server permission and invite-read access, then try again. No counts were assumed.";
const STORE_NOTICE: &str =
    "Invite counts are unavailable right now. Try again later; no counts were assumed.";
const REPLY_BUDGET: Duration = Duration::from_secs(20);

// Latest membership wins, including unknown joins and leaves. The event id
// breaks equal-timestamp ties deterministically. Filtering before DISTINCT ON
// would resurrect an old credited join after a leave or unknown rejoin.
// Aggregate on the server and return at most eleven rows in one snapshot.
const COUNTS_SQL: &str = r#"
WITH latest AS (
    SELECT DISTINCT ON (member_id) member_id, event_type, source, metadata
    FROM events
    WHERE guild_id = $1 AND member_id IS NOT NULL
      AND event_type IN ('member_join', 'member_leave')
    ORDER BY member_id, occurred_at DESC, id DESC
), attributed AS (
    SELECT member_id,
           CASE WHEN metadata IS JSON OBJECT
                THEN metadata::jsonb ->> 'inviterId' END AS inviter
    FROM latest
    WHERE event_type = 'member_join' AND source LIKE 'invite:%'
), valid AS (
    SELECT inviter FROM attributed
    WHERE inviter <> member_id AND
          CASE WHEN inviter ~ '^[1-9][0-9]{0,19}$'
               THEN inviter::numeric <= $3::text::numeric
               ELSE false END
), counts AS (
    SELECT inviter, count(*) AS attributed_count FROM valid GROUP BY inviter
), board AS (
    SELECT inviter, attributed_count FROM counts
    ORDER BY attributed_count DESC, length(inviter), inviter COLLATE "C"
    LIMIT 10
), result AS (
    SELECT false AS is_board, $2::text AS inviter,
           coalesce((SELECT attributed_count FROM counts WHERE inviter = $2), 0) AS attributed_count
    UNION ALL
    SELECT true AS is_board, inviter, attributed_count FROM board
)
SELECT is_board, inviter, attributed_count FROM result
ORDER BY is_board, attributed_count DESC, length(inviter), inviter COLLATE "C"
"#;

/// Guild fencing precedes acknowledgement, database and REST access. The
/// shared router already checks it; keep the execution boundary defensive.
pub(crate) async fn respond(
    pool: &PgPool,
    executor: &ActionExecutor,
    guild_id: u64,
    interaction: &Interaction,
) {
    if interaction.guild_id.map(|id| id.get()) != Some(guild_id) {
        return;
    }
    let Some(target) = target_member(interaction) else {
        return;
    };
    let defer = InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data: Some(InteractionResponseData {
            allowed_mentions: Some(AllowedMentions::default()),
            ..Default::default()
        }),
    };
    if let Err(error) = executor
        .answer_interaction_with_blocked_retry(interaction.id.get(), &interaction.token, &defer)
        .await
    {
        tracing::warn!(reason = error.cause(), "invites acknowledgement failed");
        return;
    }
    let content = tokio::time::timeout(REPLY_BUDGET, reply(pool, executor, guild_id, target))
        .await
        .unwrap_or_else(|_| STORE_NOTICE.to_owned());
    if let Err(error) = executor
        .edit_interaction_response_with_blocked_retry(
            interaction.application_id.get(),
            &interaction.token,
            &content,
        )
        .await
    {
        tracing::warn!(reason = error.cause(), "invites response edit failed");
    }
}

fn target_member(interaction: &Interaction) -> Option<u64> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return None;
    };
    if let Some(option) = data.options.iter().find(|option| option.name == "member") {
        return match option.value {
            CommandOptionValue::User(id) => Some(id.get()),
            _ => None,
        };
    }
    interaction.author_id().map(|id| id.get())
}

async fn reply(pool: &PgPool, executor: &ActionExecutor, guild: u64, target: u64) -> String {
    // A readable empty list proves access, not historical zero attribution.
    // A forbidden, malformed or failed read must never masquerade as zero.
    match executor
        .get_json_strict(&format!("/guilds/{guild}/invites"))
        .await
    {
        Ok(Some(serde_json::Value::Array(_))) => {}
        _ => return INVITE_READ_NOTICE.to_owned(),
    }
    let rows = match counts(pool, guild, target).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                reason = crate::db_log::db_class(&error),
                "invite counts read failed"
            );
            return STORE_NOTICE.to_owned();
        }
    };
    let mut names: HashMap<String, String> = HashMap::new();
    let mut content = String::new();
    let mut board_started = false;
    for (is_board, id, count) in rows {
        let name = match names.get(&id) {
            Some(name) => name.clone(),
            None => {
                let name = member_name(executor, guild, &id).await;
                names.insert(id.clone(), name.clone());
                name
            }
        };
        if is_board {
            if !board_started {
                content.push_str("\n**Top inviters** (net attributed members)\n");
                board_started = true;
            }
            let _ = writeln!(content, "{name}: {count} invites");
        } else {
            let _ = writeln!(content, "{name}: {count} attributed invites (net)");
        }
    }
    if !board_started {
        content.push_str("\nNo attributed inviters yet.");
    }
    content.push_str("\nOnly witnessed joins count; leaving removes credit. Use /invites member to look up someone else.");
    message_safety::content(&content)
}

async fn counts(
    pool: &PgPool,
    guild: u64,
    target: u64,
) -> Result<Vec<(bool, String, i64)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query_as(COUNTS_SQL)
        .bind(guild.to_string())
        .bind(target.to_string())
        .bind(u64::MAX.to_string())
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Latest committed membership only: never recover credit from an older join.
pub(crate) async fn current_inviter(
    pool: &PgPool,
    guild: u64,
    member: u64,
) -> Result<Option<String>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    let inviter = sqlx::query_scalar(
        r#"WITH latest AS (
            SELECT event_type, source, metadata FROM events
            WHERE guild_id = $1 AND member_id = $2
              AND event_type IN ('member_join', 'member_leave')
            ORDER BY occurred_at DESC, id DESC LIMIT 1
        ), attributed AS (
            SELECT CASE WHEN metadata IS JSON OBJECT
                        THEN metadata::jsonb ->> 'inviterId' END AS inviter
            FROM latest WHERE event_type = 'member_join' AND source LIKE 'invite:%'
        )
        SELECT inviter FROM attributed
        WHERE inviter <> $2 AND
              CASE WHEN inviter ~ '^[1-9][0-9]{0,19}$'
                   THEN inviter::numeric <= $3::text::numeric
                   ELSE false END"#,
    )
    .bind(guild.to_string())
    .bind(member.to_string())
    .bind(u64::MAX.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(inviter)
}

pub(crate) async fn member_name(executor: &ActionExecutor, guild: u64, member: &str) -> String {
    let body = executor
        .get_json_strict(&format!("/guilds/{guild}/members/{member}"))
        .await
        .ok()
        .flatten();
    let raw = body.as_ref().and_then(|body| {
        // Do not accept an unrelated or malformed member as this inviter.
        if body["user"]["id"].as_str() != Some(member) {
            return None;
        }
        body["nick"]
            .as_str()
            .or_else(|| body["user"]["global_name"].as_str())
            .or_else(|| body["user"]["username"].as_str())
            .filter(|name| !name.trim().is_empty())
    });
    // Eleven names at most. Bound input before escaping, leaving enough
    // room for every row and the honest counting rule under Discord's cap.
    raw.map(|name| sanitize_rank_name(&message_safety::truncate(name, 48)))
        .unwrap_or_else(|| format!("Member {member}"))
}
