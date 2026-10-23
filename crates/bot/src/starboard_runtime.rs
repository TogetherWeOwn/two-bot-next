//! Config-gated highlights. Pending work contains IDs only; content is fetched
//! after the source channel passes the privacy gate and is never persisted.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{json, Value};
use sqlx::PgPool;
use twilight_model::gateway::event::Event;
use two_bot_core::events::{CoreEvent, ReactionDelivery, ReactionEmoji, ReactionEvent};
use two_bot_discord::{ActionExecutor, DiscordError};
use two_bot_store::starboard::{get_starboard_config, StarboardPost};

type Key = (i64, i64);
const COALESCE: Duration = Duration::from_secs(2);
const MAX_MESSAGES: usize = 8;
const MAX_STARRERS: usize = 1024;

struct Change {
    reaction: ReactionEvent,
    remove: bool,
    arrival: u64,
}

struct Pending {
    channel: i64,
    changes: BTreeMap<(u64, String, bool, String), Change>,
    deleted: bool,
}

pub(crate) struct StarboardRuntime {
    pool: PgPool,
    executor: ActionExecutor,
    guild: u64,
    pending: Mutex<HashMap<Key, Pending>>,
    arrival: AtomicU64,
}

/// Owned by the command runtime's cancellation scope. Even a refused spawn or
/// an aborted worker releases its in-memory slot, never its durable send fence.
pub(crate) struct Work {
    runtime: Arc<StarboardRuntime>,
    key: Key,
    finished: bool,
}

impl Drop for Work {
    fn drop(&mut self) {
        if !self.finished {
            self.runtime
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&self.key);
        }
    }
}

impl StarboardRuntime {
    pub(crate) fn new(pool: PgPool, executor: ActionExecutor, guild: u64) -> Arc<Self> {
        Arc::new(Self {
            pool,
            executor,
            guild,
            pending: Mutex::new(HashMap::new()),
            arrival: AtomicU64::new(0),
        })
    }

    /// Collapse a burst to the last add/remove for each user. No tasks or SQL
    /// waiters are allocated for coalesced events; both maps have hard bounds.
    pub(crate) fn enqueue(
        self: &Arc<Self>,
        event: &Event,
        delivery: Option<&ReactionDelivery>,
    ) -> Result<Option<Work>, ()> {
        let core = match delivery {
            Some(delivery) => {
                two_bot_discord::adapter::event_to_core_with_delivery(event, delivery)
            }
            None => two_bot_discord::adapter::event_to_core(event),
        };
        let (guild, channel, message, change, deleted) = match core {
            Some(CoreEvent::ReactionAdded(reaction))
            | Some(CoreEvent::ReactionRemoved(reaction)) => {
                let remove = matches!(event, Event::ReactionRemove(_));
                (
                    reaction.guild_id,
                    reaction.channel_id,
                    reaction.message_id,
                    Some(Change {
                        reaction,
                        remove,
                        arrival: self.arrival.fetch_add(1, Ordering::Relaxed),
                    }),
                    false,
                )
            }
            Some(CoreEvent::MessageDeleted {
                guild_id,
                channel_id,
                message_id,
            }) => (guild_id, channel_id, message_id, None, true),
            _ => return Ok(None),
        };
        if guild != Some(self.guild) {
            return Ok(None);
        }
        let (Ok(guild), Ok(channel), Ok(message)) = (
            i64::try_from(self.guild),
            i64::try_from(channel),
            i64::try_from(message),
        ) else {
            return Ok(None);
        };
        let key = (guild, message);
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let fresh = !pending.contains_key(&key);
        if fresh && pending.len() >= MAX_MESSAGES {
            return Err(());
        }
        let entry = pending.entry(key).or_insert_with(|| Pending {
            channel,
            changes: BTreeMap::new(),
            deleted: false,
        });
        if entry.channel != channel {
            return Ok(None);
        }
        entry.deleted |= deleted;
        if let Some(change) = change {
            let emoji = match &change.reaction.emoji {
                ReactionEmoji::Unicode(name) => format!("unicode:{name}"),
                ReactionEmoji::Custom(id) => format!("custom:{id}"),
            };
            let order = &change.reaction.delivery.event_order;
            let session = order
                .rsplit_once(':')
                .map_or(order.as_str(), |(session, _)| session)
                .to_owned();
            let user = (
                change.reaction.user_id,
                emoji,
                change.reaction.burst,
                session,
            );
            if !entry.changes.contains_key(&user) && entry.changes.len() >= MAX_STARRERS {
                return Err(());
            }
            if entry.changes.get(&user).is_none_or(|old| {
                change.reaction.delivery.event_order > old.reaction.delivery.event_order
            }) {
                entry.changes.insert(user, change);
            }
        }
        Ok(fresh.then(|| Work {
            runtime: Arc::clone(self),
            key,
            finished: false,
        }))
    }

    async fn apply(&self, key: Key, batch: &Pending) -> Result<bool, sqlx::Error> {
        let Some(config) = get_starboard_config(&self.pool, key.0).await? else {
            return Ok(true);
        };
        let Some(board) = config.board_channel_id else {
            return Ok(true);
        };
        if batch.channel == board || config.excluded_channel_ids.contains(&batch.channel) {
            return Ok(true);
        }
        // An unreadable channel is not proof that its content is safe to copy.
        if !self.safe_channel(batch.channel).await {
            return Ok(true);
        }
        let source = self
            .executor
            .get_json(&format!("/channels/{}/messages/{}", batch.channel, key.1))
            .await
            .ok()
            .flatten();
        let fallback_author = batch
            .changes
            .values()
            .find_map(|c| c.reaction.message_author_id);
        let author = source
            .as_ref()
            .and_then(|s| s["author"]["id"].as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .or(fallback_author);
        // Missing message content is not permission to bypass the bot-author
        // filter. A public user lookup can prove the author without copying text.
        let human_author = if let Some(source) = source.as_ref() {
            source["author"]["bot"].as_bool() != Some(true)
        } else if let Some(author) = author {
            matches!(self.executor.get_json(&format!("/users/{author}")).await,
                Ok(Some(user)) if user["id"].as_str().and_then(|id| id.parse::<u64>().ok()) == Some(author)
                    && user["bot"].as_bool() != Some(true))
        } else {
            false
        };
        if !human_author && !batch.deleted {
            return Ok(true);
        }
        let mut tx = self.pool.begin().await?;
        // Serialize updates for this source across runtime instances as well as
        // within the bounded process worker. The lock is transaction-scoped.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("starboard:{}:{}", key.0, key.1))
            .execute(&mut *tx)
            .await?;
        let mut changed = false;
        let mut changes: Vec<_> = batch.changes.values().collect();
        changes.sort_by_key(|change| change.arrival);
        for change in changes {
            let reaction = &change.reaction;
            if !emoji_matches(&reaction.emoji, &config.emoji)
                || reaction.burst
                || reaction.member_is_bot == Some(true)
                || author.or(reaction.message_author_id) == Some(reaction.user_id)
                || author.or(reaction.message_author_id).is_none()
            {
                continue;
            }
            let Ok(user) = i64::try_from(reaction.user_id) else {
                continue;
            };
            let order = &reaction.delivery.event_order;
            let session = order
                .rsplit_once(':')
                .map_or(order.as_str(), |(session, _)| session);
            // The remove watermark survives deletion of the active starrer.
            // Session hashes have no chronological ordering across IDENTIFYs.
            let admitted = sqlx::query("INSERT INTO starboard_reaction_cursors (guild_id,source_message_id,user_id,session_key,event_order) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (guild_id,source_message_id,user_id,session_key) DO UPDATE SET event_order=EXCLUDED.event_order, updated_at=clock_timestamp() WHERE starboard_reaction_cursors.event_order < EXCLUDED.event_order")
                .bind(key.0).bind(key.1).bind(user).bind(session).bind(order).execute(&mut *tx).await?.rows_affected() == 1;
            if !admitted {
                continue;
            }
            let result = if change.remove {
                sqlx::query("DELETE FROM starboard_starrers WHERE guild_id=$1 AND source_message_id=$2 AND user_id=$3")
                    .bind(key.0).bind(key.1).bind(user).execute(&mut *tx).await?
            } else {
                sqlx::query("INSERT INTO starboard_starrers (guild_id, source_message_id, user_id, source_channel_id) VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING")
                    .bind(key.0).bind(key.1).bind(user).bind(batch.channel).execute(&mut *tx).await?
            };
            changed |= result.rows_affected() != 0;
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM starboard_starrers WHERE guild_id=$1 AND source_message_id=$2",
        )
        .bind(key.0)
        .bind(key.1)
        .fetch_one(&mut *tx)
        .await?;
        let winner = if changed && !batch.deleted && count >= i64::from(config.threshold) {
            sqlx::query("INSERT INTO starboard_posts (guild_id, source_message_id, source_channel_id, board_channel_id, star_count) VALUES ($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
                .bind(key.0).bind(key.1).bind(batch.channel).bind(board).bind(count)
                .execute(&mut *tx).await?.rows_affected() == 1
        } else {
            false
        };
        if batch.deleted {
            sqlx::query("UPDATE starboard_posts SET state='source_deleted', updated_at=clock_timestamp() WHERE guild_id=$1 AND source_message_id=$2")
                .bind(key.0).bind(key.1).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        // A replay may have already committed the starrer transaction while
        // its subsequent edit failed. Reconciliation must not depend on delta.
        self.sync(
            key,
            winner,
            source.as_ref(),
            fallback_author,
            config.threshold,
            &config.emoji,
        )
        .await
    }

    async fn safe_channel(&self, channel: i64) -> bool {
        let Ok(Some(document)) = self
            .executor
            .get_json(&format!("/channels/{channel}"))
            .await
        else {
            return false;
        };
        if document["id"]
            .as_str()
            .and_then(|id| id.parse::<i64>().ok())
            != Some(channel)
            || document["guild_id"]
                .as_str()
                .and_then(|id| id.parse::<u64>().ok())
                != Some(self.guild)
            || document["nsfw"].as_bool() == Some(true)
        {
            return false;
        }
        match document["type"].as_u64() {
            Some(0 | 5 | 15 | 16) => document["nsfw"].as_bool() == Some(false),
            Some(10..=12) => {
                let Some(parent) = document["parent_id"]
                    .as_str()
                    .and_then(|id| id.parse::<i64>().ok())
                else {
                    return false;
                };
                let Ok(Some(parent)) = self.executor.get_json(&format!("/channels/{parent}")).await
                else {
                    return false;
                };
                parent["nsfw"].as_bool() == Some(false)
                    && parent["guild_id"]
                        .as_str()
                        .and_then(|id| id.parse::<u64>().ok())
                        == Some(self.guild)
            }
            _ => false,
        }
    }

    async fn sync(
        &self,
        key: Key,
        winner: bool,
        source: Option<&Value>,
        author: Option<u64>,
        threshold: i16,
        emoji: &str,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let post: Option<StarboardPost> = sqlx::query_as("SELECT guild_id, source_channel_id, source_message_id, board_channel_id, board_message_id, star_count, state FROM starboard_posts WHERE guild_id=$1 AND source_message_id=$2 FOR UPDATE")
            .bind(key.0).bind(key.1).fetch_optional(&mut *tx).await?;
        let Some(mut post) = post else {
            return Ok(true);
        };
        // A pending row is an enduring send fence, not a lease. Recover only
        // positive acceptance evidence from this bot; absence never permits POST.
        if post.board_message_id.is_none() && (!winner || post.state == "source_deleted") {
            let recovered = match self.executor.current_bot_user_id().await {
                Ok(bot) => self
                    .executor
                    .recover_message_by_nonce(
                        &post.board_channel_id.to_string(),
                        &format!("sb:{}", key.1),
                        bot,
                    )
                    .await
                    .ok()
                    .flatten(),
                Err(_) => None,
            };
            post.board_message_id = recovered
                .and_then(|id| id.parse::<i64>().ok())
                .filter(|id| *id > 0);
            if post.board_message_id.is_none() {
                return Ok(true);
            }
            sqlx::query("UPDATE starboard_posts SET board_message_id=$3, updated_at=clock_timestamp() WHERE guild_id=$1 AND source_message_id=$2")
                .bind(key.0).bind(key.1).bind(post.board_message_id).execute(&mut *tx).await?;
        }
        let mut base = source.cloned();
        if post.state == "source_deleted" {
            if let Some(id) = post.board_message_id {
                if let Ok(Some(board)) = self
                    .executor
                    .get_json(&format!(
                        "/channels/{}/messages/{id}",
                        post.board_channel_id
                    ))
                    .await
                {
                    base = board["embeds"]
                        .as_array()
                        .and_then(|embeds| embeds.first())
                        .cloned()
                        .or(base);
                }
            }
            // Replacing an unreadable board embed with "Unknown author" would
            // destroy the only surviving attribution. Keep it for a later sync;
            // the durable source_deleted state is already committed by apply.
            if base
                .as_ref()
                .is_none_or(|document| !document["author"].is_object())
            {
                tx.commit().await?;
                return Ok(true);
            }
        }
        let mut complete = true;
        for attempt in 0..3 {
            // Recount after every 429/backoff, not from the previous attempt.
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM starboard_starrers WHERE guild_id=$1 AND source_message_id=$2")
                .bind(key.0).bind(key.1).fetch_one(&mut *tx).await?;
            let state = if post.state == "source_deleted" {
                "source_deleted"
            } else if count < i64::from(threshold) {
                "below_threshold"
            } else {
                "posted"
            };
            if post.board_message_id.is_some()
                && count == post.star_count
                && state == post.state
                && state != "source_deleted"
            {
                break;
            }
            let embed = embed(&post, base.as_ref(), author, count, state, emoji);
            let result = match post.board_message_id {
                Some(id) => self
                    .executor
                    .edit_highlight(&post.board_channel_id.to_string(), &id.to_string(), &embed)
                    .await
                    .map(|()| id),
                None => self
                    .executor
                    .post_highlight(&post.board_channel_id.to_string(), &embed, key.1 as u64)
                    .await
                    .and_then(|id| {
                        id.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| {
                            DiscordError::Unavailable("invalid highlight receipt".into())
                        })
                    }),
            };
            match result {
                Ok(id) => {
                    sqlx::query("UPDATE starboard_posts SET board_message_id=$3, star_count=$4, state=$5, updated_at=clock_timestamp() WHERE guild_id=$1 AND source_message_id=$2")
                        .bind(key.0).bind(key.1).bind(id).bind(count).bind(state).execute(&mut *tx).await?;
                    break;
                }
                Err(DiscordError::RateLimited) if attempt < 2 => tokio::time::sleep(COALESCE).await,
                Err(error) => {
                    tracing::debug!(
                        cause = error.cause(),
                        "highlight sync failed; pending sends remain fenced"
                    );
                    complete = false;
                    break;
                }
            }
        }
        tx.commit().await?;
        Ok(complete)
    }
}

impl Work {
    pub(crate) async fn run(mut self) {
        // Check admission before coalescing: enabling a board during the wait
        // must not resurrect reactions received while it was disabled.
        if !matches!(get_starboard_config(&self.runtime.pool, self.key.0).await,
            Ok(Some(config)) if config.board_channel_id.is_some())
        {
            return;
        }
        loop {
            tokio::time::sleep(COALESCE).await;
            let batch = {
                let mut pending = self
                    .runtime
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let Some(entry) = pending.get_mut(&self.key) else {
                    return;
                };
                Pending {
                    channel: entry.channel,
                    changes: std::mem::take(&mut entry.changes),
                    deleted: std::mem::take(&mut entry.deleted),
                }
            };
            // Keep the drained batch until its transaction and edit settle.
            // New arrivals remain coalesced in the bounded pending slot; they
            // cannot overtake an older batch that is retrying after rollback.
            let mut backoff = COALESCE;
            loop {
                match self.runtime.apply(self.key, &batch).await {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(_) => tracing::debug!("highlight sync database failure"),
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            let mut pending = self
                .runtime
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if pending
                .get(&self.key)
                .is_none_or(|entry| entry.changes.is_empty() && !entry.deleted)
            {
                pending.remove(&self.key);
                self.finished = true;
                return;
            }
        }
    }
}

fn emoji_matches(emoji: &ReactionEmoji, configured: &str) -> bool {
    match emoji {
        ReactionEmoji::Unicode(name) => name == configured,
        ReactionEmoji::Custom(id) => {
            configured.parse::<u64>().ok() == Some(*id)
                || configured
                    .trim_end_matches('>')
                    .rsplit(':')
                    .next()
                    .and_then(|id| id.parse::<u64>().ok())
                    == Some(*id)
        }
    }
}

fn embed(
    post: &StarboardPost,
    source: Option<&Value>,
    fallback_author: Option<u64>,
    count: i64,
    state: &str,
    emoji: &str,
) -> Value {
    let deleted = state == "source_deleted";
    let mut embed = json!({"footer": {"text": format!("{emoji} {count} · {state}")}});
    if let Some(source) = source {
        let author = &source["author"];
        let name = author["global_name"]
            .as_str()
            .or_else(|| author["username"].as_str())
            .or_else(|| author["name"].as_str())
            .unwrap_or("Unknown author");
        embed["author"] = json!({"name": truncate(name, 256)});
        if let (Some(id), Some(avatar)) = (author["id"].as_str(), author["avatar"].as_str()) {
            embed["author"]["icon_url"] = json!(format!(
                "https://cdn.discordapp.com/avatars/{id}/{avatar}.png"
            ));
        } else if let Some(icon) = author["icon_url"].as_str() {
            embed["author"]["icon_url"] = json!(icon);
        }
        if let Some(timestamp) = source["timestamp"].as_str() {
            embed["timestamp"] = json!(timestamp);
        }
        if !deleted {
            if let Some(text) = source["content"].as_str() {
                embed["description"] = json!(truncate(text, 4096));
            }
            if let Some(image) = source["attachments"].as_array().and_then(|items| {
                items.iter().find(|a| {
                    a["content_type"]
                        .as_str()
                        .is_some_and(|kind| kind.starts_with("image/"))
                })
            }) {
                if let Some(url) = image["url"].as_str() {
                    embed["image"] = json!({"url": url});
                }
            }
        }
    } else {
        embed["author"] = json!({"name": fallback_author.map_or_else(|| "Unknown author".to_owned(), |id| format!("User {id}"))});
    }
    if deleted {
        embed["description"] = json!("Source message deleted.");
    } else {
        embed["fields"] = json!([{"name":"Source", "value":format!("[Jump to Message](https://discord.com/channels/{}/{}/{})", post.guild_id, post.source_channel_id, post.source_message_id)}]);
    }
    embed
}

fn truncate(text: &str, max_units: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max_units
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use twilight_model::gateway::{payload::incoming::ReactionAdd, GatewayReaction};
    use twilight_model::{channel::message::EmojiReactionType, id::Id};

    use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
    use two_bot_testsupport::TestDatabase;

    fn added() -> Event {
        Event::ReactionAdd(Box::new(ReactionAdd(GatewayReaction {
            burst: false,
            burst_colors: Vec::new(),
            channel_id: Id::new(2),
            emoji: EmojiReactionType::Unicode { name: "⭐".into() },
            guild_id: Some(Id::new(1)),
            member: None,
            message_author_id: Some(Id::new(4)),
            message_id: Id::new(3),
            user_id: Id::new(5),
        })))
    }

    fn response(request: &RestRequest) -> ScriptedResponse {
        let body = match request.path.as_str().trim_start_matches("/api/v10") {
            "/channels/2" => json!({"id":"2","guild_id":"1","type":0,"nsfw":false}),
            "/channels/2/messages/3" => {
                json!({"id":"3","author":{"id":"4","username":"member","bot":false},"content":"highlight"})
            }
            "/users/@me" => json!({"id":"99","bot":true}),
            _ => json!({"id":"10"}),
        };
        ScriptedResponse::json(200, body)
    }

    async fn fixture() -> Option<TestDatabase> {
        let url = std::env::var("TWO_TEST_DATABASE_URL").ok()?;
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("starboard fixture");
        sqlx::query(
            "INSERT INTO starboard_config (guild_id,board_channel_id,threshold) VALUES (1,9,1)",
        )
        .execute(db.pool())
        .await
        .expect("config");
        Some(db)
    }

    fn with_mock(db: &TestDatabase, mock: &MockRest) -> Arc<StarboardRuntime> {
        let executor =
            ActionExecutor::with_proxy("test".into(), Some(mock.origin())).expect("executor");
        StarboardRuntime::new(db.pool().clone(), executor, 1)
    }

    #[tokio::test]
    async fn starboard_worker_retries_a_rolled_back_batch() {
        let Some(db) = fixture().await else { return };
        sqlx::raw_sql("CREATE SEQUENCE fail_star_once; CREATE FUNCTION fail_star_once() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF nextval('fail_star_once') = 1 THEN RAISE EXCEPTION 'synthetic write failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_star_once BEFORE INSERT ON starboard_starrers FOR EACH ROW EXECUTE FUNCTION fail_star_once()")
            .execute(db.pool()).await.expect("failure injection");
        let mock = MockRest::with_responder(response).await;
        let runtime = with_mock(&db, &mock);
        tokio::time::timeout(
            Duration::from_secs(15),
            runtime
                .enqueue(&added(), None)
                .expect("admit")
                .expect("work")
                .run(),
        )
        .await
        .expect("bounded retry");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM starboard_starrers")
            .fetch_one(db.pool())
            .await
            .expect("count");
        let writes = mock
            .requests()
            .iter()
            .filter(|r| r.method == "POST")
            .count();
        db.close().await.expect("close fixture");
        assert_eq!(count, 1, "failed SQL must not discard the drained reaction");
        assert_eq!(writes, 1, "recovered batch must post exactly once");
    }

    #[tokio::test]
    async fn starboard_degraded_fetch_requires_human_author_proof() {
        let Some(db) = fixture().await else { return };
        use std::sync::atomic::{AtomicBool, Ordering};
        let bot = Arc::new(AtomicBool::new(true));
        let author_bot = Arc::clone(&bot);
        let mock = MockRest::with_responder(move |request| {
            if request.path.ends_with("/messages/3") {
                return ScriptedResponse::json(404, json!({}));
            }
            if request.path.ends_with("/users/4") {
                return ScriptedResponse::json(
                    200,
                    json!({"id":"4","bot":author_bot.load(Ordering::SeqCst)}),
                );
            }
            response(request)
        })
        .await;
        let runtime = with_mock(&db, &mock);
        runtime
            .enqueue(&added(), None)
            .expect("admit")
            .expect("work")
            .run()
            .await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM starboard_starrers")
            .fetch_one(db.pool())
            .await
            .expect("count");
        let writes = mock
            .requests()
            .iter()
            .filter(|r| r.method == "POST")
            .count();
        bot.store(false, Ordering::SeqCst);
        runtime
            .enqueue(&added(), None)
            .expect("admit human")
            .expect("work")
            .run()
            .await;
        let human_count: i64 = sqlx::query_scalar("SELECT count(*) FROM starboard_starrers")
            .fetch_one(db.pool())
            .await
            .expect("human count");
        db.close().await.expect("close fixture");
        assert_eq!(count, 0, "an author ID alone does not prove a human author");
        assert_eq!(writes, 0, "bot-authored messages must not be highlighted");
        assert_eq!(
            human_count, 1,
            "the same degraded path admits a verified human author"
        );
    }

    #[tokio::test]
    async fn starboard_pending_send_recovers_by_nonce_without_reposting() {
        let Some(db) = fixture().await else { return };
        sqlx::query("INSERT INTO starboard_posts (guild_id,source_message_id,source_channel_id,board_channel_id) VALUES (1,3,2,9)")
            .execute(db.pool()).await.expect("uncertain reservation");
        let mock = MockRest::with_responder(|request| {
            if request.path.contains("/channels/9/messages?") {
                return ScriptedResponse::json(
                    200,
                    json!([{"id":"10","channel_id":"9","author":{"id":"99"},"nonce":"sb:3"}]),
                );
            }
            response(request)
        })
        .await;
        let runtime = with_mock(&db, &mock);
        runtime
            .enqueue(&added(), None)
            .expect("admit")
            .expect("work")
            .run()
            .await;
        let id: Option<i64> = sqlx::query_scalar(
            "SELECT board_message_id FROM starboard_posts WHERE guild_id=1 AND source_message_id=3",
        )
        .fetch_one(db.pool())
        .await
        .expect("receipt");
        let writes = mock
            .requests()
            .iter()
            .filter(|r| r.method == "POST")
            .count();
        db.close().await.expect("close fixture");
        assert_eq!(
            id,
            Some(10),
            "accepted send must be reconciled after a lost receipt"
        );
        assert_eq!(
            writes, 0,
            "an enduring reservation never authorizes another send"
        );
    }

    #[tokio::test]
    async fn starboard_failed_edit_retries_after_starrer_commit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let Some(db) = fixture().await else { return };
        sqlx::query("INSERT INTO starboard_posts (guild_id,source_message_id,source_channel_id,board_channel_id,board_message_id) VALUES (1,3,2,9,10)")
            .execute(db.pool()).await.expect("existing post");
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let mock = MockRest::with_responder(move |request| {
            if request.method == "PATCH" && counter.fetch_add(1, Ordering::SeqCst) == 0 {
                return ScriptedResponse::json(500, json!({}));
            }
            response(request)
        })
        .await;
        let runtime = with_mock(&db, &mock);
        tokio::time::timeout(
            Duration::from_secs(15),
            runtime
                .enqueue(&added(), None)
                .expect("admit")
                .expect("work")
                .run(),
        )
        .await
        .expect("bounded recovery");
        let count: i64 = sqlx::query_scalar(
            "SELECT star_count FROM starboard_posts WHERE guild_id=1 AND source_message_id=3",
        )
        .fetch_one(db.pool())
        .await
        .expect("count");
        db.close().await.expect("close fixture");
        assert_eq!(
            count, 1,
            "a committed reaction must still retry its failed edit"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn starboard_unreadable_deletion_never_erases_existing_attribution() {
        let Some(db) = fixture().await else { return };
        sqlx::query("INSERT INTO starboard_posts (guild_id,source_message_id,source_channel_id,board_channel_id,board_message_id) VALUES (1,3,2,9,10)")
            .execute(db.pool()).await.expect("existing post");
        let mock = MockRest::with_responder(|request| {
            if request.method == "GET" && request.path.contains("/messages/") {
                return ScriptedResponse::json(404, json!({}));
            }
            response(request)
        })
        .await;
        let runtime = with_mock(&db, &mock);
        let batch = Pending {
            channel: 2,
            changes: BTreeMap::new(),
            deleted: true,
        };
        runtime
            .apply((1, 3), &batch)
            .await
            .expect("delete handling");
        let patches = mock
            .requests()
            .iter()
            .filter(|r| r.method == "PATCH")
            .count();
        db.close().await.expect("close fixture");
        assert_eq!(
            patches, 0,
            "without author evidence, retain the attributed board embed for reconciliation"
        );
    }

    #[tokio::test]
    async fn starboard_old_add_cannot_resurrect_a_removed_star_after_restart() {
        use twilight_model::gateway::payload::incoming::ReactionRemove;
        let Some(db) = fixture().await else { return };
        let mock = MockRest::with_responder(response).await;
        let runtime = with_mock(&db, &mock);
        let add = added();
        let Event::ReactionAdd(reaction) = &add else {
            unreachable!()
        };
        let remove = Event::ReactionRemove(Box::new(ReactionRemove(reaction.0.clone())));
        let delivery =
            |sequence| two_bot_discord::adapter::reaction_delivery("replay-session", sequence);
        for (event, sequence) in [(&add, 10), (&remove, 20)] {
            runtime
                .enqueue(event, Some(&delivery(sequence)))
                .expect("admit")
                .expect("work")
                .run()
                .await;
        }
        drop(runtime);
        let restarted = with_mock(&db, &mock);
        for sequence in [10, 15] {
            restarted
                .enqueue(&add, Some(&delivery(sequence)))
                .expect("replay")
                .expect("work")
                .run()
                .await;
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM starboard_starrers")
            .fetch_one(db.pool())
            .await
            .expect("count");
        db.close().await.expect("close fixture");
        assert_eq!(
            count, 0,
            "durable order must reject both duplicate and unseen older Adds after Remove"
        );
    }

    #[tokio::test]
    async fn starboard_coalesced_new_session_is_not_sorted_by_its_hash() {
        use twilight_model::gateway::payload::incoming::ReactionRemove;
        let Some(db) = fixture().await else { return };
        sqlx::query("INSERT INTO starboard_starrers (guild_id,source_message_id,source_channel_id,user_id) VALUES (1,3,2,5)")
            .execute(db.pool()).await.expect("existing star");
        let mock = MockRest::with_responder(response).await;
        let runtime = with_mock(&db, &mock);
        let add = added();
        let Event::ReactionAdd(reaction) = &add else {
            unreachable!()
        };
        let remove = Event::ReactionRemove(Box::new(ReactionRemove(reaction.0.clone())));
        let mut sessions = [
            two_bot_discord::adapter::reaction_delivery("session-one", 1),
            two_bot_discord::adapter::reaction_delivery("session-two", 1),
        ];
        sessions.sort_by(|a, b| a.event_order.cmp(&b.event_order));
        let work = runtime
            .enqueue(&remove, Some(&sessions[1]))
            .expect("remove")
            .expect("worker");
        runtime
            .enqueue(&add, Some(&sessions[0]))
            .expect("new session add");
        work.run().await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM starboard_starrers")
            .fetch_one(db.pool())
            .await
            .expect("count");
        db.close().await.expect("close fixture");
        assert_eq!(
            count, 1,
            "gateway session hashes are not chronological order"
        );
    }

    #[tokio::test]
    async fn unrelated_reaction_cannot_erase_a_pending_star() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@agent-testdb/unused")
            .expect("lazy pool");
        let executor =
            ActionExecutor::with_proxy("test".to_owned(), Some("http://127.0.0.1:1".to_owned()))
                .expect("executor");
        let runtime = StarboardRuntime::new(pool, executor, 1);
        let event = |emoji: &str| {
            Event::ReactionAdd(Box::new(ReactionAdd(GatewayReaction {
                burst: false,
                burst_colors: Vec::new(),
                channel_id: Id::new(2),
                emoji: EmojiReactionType::Unicode {
                    name: emoji.to_owned(),
                },
                guild_id: Some(Id::new(1)),
                member: None,
                message_author_id: Some(Id::new(4)),
                message_id: Id::new(3),
                user_id: Id::new(5),
            })))
        };
        let work = runtime.enqueue(&event("⭐"), None).expect("admit");
        runtime.enqueue(&event("👍"), None).expect("coalesce");
        assert!(runtime.pending.lock().expect("pending")[&(1, 3)]
            .changes
            .values()
            .any(|change| emoji_matches(&change.reaction.emoji, "⭐")));
        drop(work);
    }
}
