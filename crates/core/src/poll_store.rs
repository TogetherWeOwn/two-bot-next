//! Durable poll state. Every voter/closer takes the same parent row lock.

use crate::polls::{PollInput, MAX_OPEN_AUTHOR, MAX_OPEN_GUILD, MAX_VOTERS};
use sqlx::{PgPool, Postgres, Row, Transaction};

pub type PollTransaction = Transaction<'static, Postgres>;

/// Never derive Debug: this row contains member-supplied text and identity.
pub struct Poll {
    pub id: String,
    pub question: String,
    pub options: Vec<String>,
    pub channel: String,
    pub message: Option<String>,
    pub author: String,
    pub open: bool,
    pub expired: bool,
    pub cap: i64,
    pub posted: bool,
    pub started: bool,
    pub counts: Option<Vec<i64>>,
    pub display_dirty: bool,
}

/// A guild lock serializes both quotas; the interaction id is also a replay key.
/// Reservation commits BEFORE Discord: a lost creation receipt is not replayed.
pub async fn reserve(
    pool: &PgPool,
    guild: &str,
    id: &str,
    channel: &str,
    author: &str,
    input: &PollInput,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('poll-create:' || $1, 0))")
        .bind(guild)
        .execute(&mut *tx)
        .await?;
    let (guild_count, author_count): (i64, i64) = sqlx::query_as("SELECT count(*), count(*) FILTER (WHERE created_by = $2) FROM polls WHERE guild_id = $1 AND status = 'open'")
        .bind(guild).bind(author).fetch_one(&mut *tx).await?;
    if guild_count >= MAX_OPEN_GUILD || author_count >= MAX_OPEN_AUTHOR {
        return Ok(false);
    }
    let added = sqlx::query("INSERT INTO polls (guild_id, id, channel_id, created_by, question, options_json, ends_at) VALUES ($1,$2,$3,$4,$5,$6,now() + $7 * interval '1 minute') ON CONFLICT (guild_id,id) DO NOTHING")
        .bind(guild).bind(id).bind(channel).bind(author).bind(&input.question)
        .bind(serde_json::json!(input.options)).bind(input.minutes as f64)
        .execute(&mut *tx).await?.rows_affected() == 1;
    tx.commit().await?;
    Ok(added)
}

pub async fn lock(
    pool: &PgPool,
    guild: &str,
    id: &str,
) -> Result<(PollTransaction, Option<Poll>), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT id,question,options_json,channel_id,message_id,created_by,status,ends_at <= clock_timestamp() AS expired,coalesce(max_voters,1000) AS cap,results_posted_at IS NOT NULL AS posted,results_started_at IS NOT NULL AS started,counts_json,display_dirty FROM polls WHERE guild_id=$1 AND id=$2 FOR UPDATE")
        .bind(guild).bind(id).fetch_optional(&mut *tx).await?;
    let poll = row
        .map(|r| -> Result<Poll, sqlx::Error> {
            let options: serde_json::Value = r.try_get("options_json")?;
            let counts: Option<serde_json::Value> = r.try_get("counts_json")?;
            Ok(Poll {
                id: r.try_get("id")?,
                question: r.try_get("question")?,
                options: serde_json::from_value(options)
                    .map_err(|_| sqlx::Error::Protocol("invalid poll options".into()))?,
                channel: r.try_get("channel_id")?,
                message: r.try_get("message_id")?,
                author: r.try_get("created_by")?,
                open: r.try_get::<String, _>("status")? == "open",
                expired: r.try_get("expired")?,
                cap: r.try_get::<i64, _>("cap")?.min(MAX_VOTERS),
                posted: r.try_get("posted")?,
                started: r.try_get("started")?,
                display_dirty: r.try_get("display_dirty")?,
                counts: counts
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|_| sqlx::Error::Protocol("invalid poll counts".into()))?,
            })
        })
        .transpose()?;
    Ok((tx, poll))
}

pub async fn counts(
    tx: &mut PollTransaction,
    guild: &str,
    poll: &Poll,
) -> Result<Vec<i64>, sqlx::Error> {
    if let Some(counts) = &poll.counts {
        return Ok(counts.clone());
    }
    let mut counts = vec![0; poll.options.len()];
    let rows: Vec<(i64,i64)> = sqlx::query_as("SELECT option_index,count(*) FROM poll_votes WHERE guild_id=$1 AND poll_id=$2 GROUP BY option_index")
        .bind(guild).bind(&poll.id).fetch_all(&mut **tx).await?;
    for (index, count) in rows {
        if let Some(slot) = usize::try_from(index).ok().and_then(|i| counts.get_mut(i)) {
            *slot = count;
        }
    }
    Ok(counts)
}

/// Caller holds the parent lock. Existing voters can change at capacity.
pub async fn vote(
    tx: &mut PollTransaction,
    guild: &str,
    poll: &Poll,
    user: &str,
    index: usize,
) -> Result<bool, sqlx::Error> {
    if !poll.open || poll.expired || index >= poll.options.len() || poll.message.is_none() {
        return Ok(false);
    }
    let (size, seated): (i64,bool) = sqlx::query_as("SELECT count(*), coalesce(bool_or(user_id=$3),false) FROM poll_votes WHERE guild_id=$1 AND poll_id=$2")
        .bind(guild).bind(&poll.id).bind(user).fetch_one(&mut **tx).await?;
    if !seated && size >= poll.cap {
        return Ok(false);
    }
    sqlx::query("INSERT INTO poll_votes (guild_id,poll_id,user_id,option_index) VALUES ($1,$2,$3,$4) ON CONFLICT (guild_id,poll_id,user_id) DO UPDATE SET option_index=excluded.option_index,voted_at=now()")
        .bind(guild).bind(&poll.id).bind(user).bind(index as i64).execute(&mut **tx).await?;
    sqlx::query("UPDATE polls SET display_dirty=true,updated_at=now() WHERE guild_id=$1 AND id=$2")
        .bind(guild)
        .bind(&poll.id)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

/// First maximum wins a tie; no winner for an empty ballot.
pub fn winner(counts: &[i64]) -> Option<usize> {
    let max = counts.iter().copied().max().unwrap_or(0);
    (max > 0)
        .then(|| counts.iter().position(|n| *n == max))
        .flatten()
}

/// Freeze counts and arm the non-replayable send fence before any network IO.
/// An already-closed unposted row is recovered without changing closed_at.
pub async fn claim_results(
    tx: &mut PollTransaction,
    guild: &str,
    poll: &Poll,
    counts: &[i64],
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE polls SET status='closed',closed_at=coalesce(closed_at,now()),winner_index=$3,counts_json=$4,results_started_at=now(),display_dirty=false,updated_at=now() WHERE guild_id=$1 AND id=$2")
        .bind(guild).bind(&poll.id).bind(winner(counts).map(|i| i as i64)).bind(serde_json::json!(counts)).execute(&mut **tx).await?;
    Ok(())
}

pub async fn settle_results(
    pool: &PgPool,
    guild: &str,
    id: &str,
    message: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE polls SET results_posted_at=coalesce(results_posted_at,now()),results_message_id=$3,results_started_at=NULL,updated_at=now() WHERE guild_id=$1 AND id=$2 AND status='closed'")
        .bind(guild).bind(id).bind(message).execute(pool).await?;
    Ok(())
}

/// Bounded and guild-fenced. Uncertain sends are not automatically replayed.
pub async fn due(pool: &PgPool, guild: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM polls WHERE guild_id=$1 AND ((status='open' AND (ends_at <= now() OR display_dirty)) OR (status='closed' AND results_posted_at IS NULL AND results_started_at IS NULL)) ORDER BY ends_at,id LIMIT 10")
        .bind(guild).fetch_all(pool).await
}
