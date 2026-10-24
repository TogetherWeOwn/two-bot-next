//! Giveaway administration, member entry, timed draw and reroll.
//!
//! Single implementation behind both dispatch paths: the bot `CommandRuntime`
//! (live production) delegates here, and the shared `InteractionRuntime`
//! serves the timed-draw seam. Slice 2 of the giveaways design: the
//! draw-ticker claim lease, draw-time eligibility re-validation, the separate
//! retryable announce phase, end/pause/resume/extend/reroll, and restart
//! recovery (die before claim, holding a claim, or mid-announce).
//!
//! Concurrency: one process-wide effects lock serializes every command, entry
//! and tick in the single always-on container, so two workers never compute
//! the same round. The database claim lease and the draw-row primary key are
//! the backstop across restarts: a second draw of one round always fails
//! closed (23505 or a lost claim), and a re-announce replays the stored row.
//!
//! Mentions stay suppressed on every outbound message (house policy); the
//! winners post carries `<@id>` text, the draw seed and the revealed nonce so
//! any member can replay the draw.

use std::{collections::HashMap, sync::Mutex, time::Instant};

use sqlx::PgPool;
use time::OffsetDateTime;
use twilight_model::application::interaction::{
    application_command::{CommandDataOption, CommandOptionValue},
    Interaction, InteractionData,
};
use two_bot_core::{
    giveaway::{self, MemberStanding, NewGiveawayInput},
    giveaway_store::{
        self as store, EntryAdmission, GiveawayDrawRow, GiveawayEntryRow, GiveawayRow, NewDraw,
        NewGiveaway,
    },
};

use crate::ActionExecutor;

const FAILURE: &str = "Giveaway unavailable; try again later.";
const INVALID: &str = "Invalid giveaway options. Use a prize up to 200 characters, 1–43200 minutes and 1–100 winners.";
const WRITE_BUDGET: u32 = 10;
const BUDGET_KEYS: usize = 4096;
/// Draw-ticker claim lease: a claim older than this is reclaimable by the
/// next tick, which bounds recovery after a crash holding a claim.
pub const CLAIM_LEASE_SECS: i64 = 600;
/// Rows one tick touches per phase; the per-guild open quota bounds due rows
/// far below this, so a tick always terminates.
const TICK_BATCH: i64 = 25;
/// Winners-post packing budget in bytes. Byte length always meets or exceeds
/// UTF-16 units, so posts packed under this ceiling never trip Discord's
/// 2000-unit rejection.
const POST_BUDGET: usize = 1800;

/// One tick's work: fresh draws committed plus announcements delivered (fresh
/// draws, re-announces and skip-to-announce recoveries all count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GiveawayTickSummary {
    pub drawn: usize,
    pub announced: usize,
}

/// Tick/refresh failure: the database, Discord reads, or an announcement that
/// did not deliver. Callers map this onto their job error class; remaining
/// work retries on the next tick.
#[derive(Debug, thiserror::Error)]
pub enum GiveawayJobError {
    #[error("giveaway job database failure")]
    Database(#[from] sqlx::Error),
    #[error("giveaway job Discord read failure")]
    Rest(String),
    #[error("giveaway job announcement failure")]
    Announce,
}

/// What one claimed row produced: a fresh draw, a lost race, or a skip to
/// announce because a draw row already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawKind {
    Drawn,
    Raced,
    Skipped,
}

/// One claimed row's report: what happened, whether an announcement went out,
/// and whether anything failed and must fail the tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClaimReport {
    kind: DrawKind,
    announced: bool,
    failed: bool,
}

/// Draw-phase failure, before any commit. Every exit releases the claim, so a
/// transient failure retries on the next tick instead of stalling to lease
/// expiry. Corrupt stored data fails the row (loudly, every tick) instead of
/// voiding the giveaway or drawing from guessed inputs.
#[derive(Debug, thiserror::Error)]
enum DrawError {
    #[error("giveaway draw database failure")]
    Database(#[from] sqlx::Error),
    #[error("giveaway draw member-read failure")]
    Rest(String),
    #[error("giveaway draw stored-data failure")]
    Data,
}

// Commands and the supervised ticker share one edit order in the single
// always-on container.
static EFFECTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug)]
pub struct GiveawayRuntime {
    pool: PgPool,
    executor: ActionExecutor,
    // Serialize count edits with entry/cancel, including the refresh supervisor.
    // No spawned waiters: the existing command admission lane bounds callers.
    budgets: Mutex<HashMap<String, (Instant, u32)>>,
}

impl GiveawayRuntime {
    pub fn new(pool: PgPool, executor: ActionExecutor) -> Self {
        Self {
            pool,
            executor,
            budgets: Mutex::new(HashMap::new()),
        }
    }

    /// Called only after the shared router authorized and deferred the command.
    pub async fn command(&self, interaction: &Interaction) {
        let _effects = EFFECTS.lock().await;
        let result = self.command_inner(interaction).await;
        self.finish(interaction, result.unwrap_or_else(|_| FAILURE.to_owned()))
            .await;
    }

    async fn command_inner(&self, interaction: &Interaction) -> Result<String, sqlx::Error> {
        let Some(guild) = interaction.guild_id else {
            return Ok(INVALID.into());
        };
        let guild = guild.get().to_string();
        let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
            return Ok(INVALID.into());
        };
        let [sub] = data.options.as_slice() else {
            return Ok(INVALID.into());
        };
        let CommandOptionValue::SubCommand(options) = &sub.value else {
            return Ok(INVALID.into());
        };
        match sub.name.as_str() {
            "create" => self.create(interaction, &guild, options).await,
            "list" => {
                let rows = store::list_active(&self.pool, &guild).await?;
                if rows.is_empty() {
                    return Ok("No open giveaways.".into());
                }
                let mut text = String::from("Open giveaways:\n");
                // A bounded preview keeps 10 maximum-length prizes below Discord's ceiling.
                for row in rows {
                    let prize: String = row.prize.chars().take(32).collect();
                    let line = format!("`{}` — {} ({})\n", row.id, prize, row.status);
                    if text.len() + line.len() > 1800 {
                        break;
                    }
                    text.push_str(&line);
                }
                Ok(text)
            }
            "cancel" => {
                let Some(id) = string_option(options, "id").filter(|id| valid_id(id)) else {
                    return Ok(INVALID.into());
                };
                let Some(row) =
                    store::cancel_audited(&self.pool, &guild, id, &actor_id(interaction)).await?
                else {
                    return Ok("Giveaway is unknown or already closed.".into());
                };
                if row.message_id != "pending" && self.update(&row).await.is_err() {
                    return Ok("Giveaway cancelled; its message could not be refreshed.".into());
                }
                Ok("Giveaway cancelled.".into())
            }
            "end" => {
                let Some(id) = string_option(options, "id").filter(|id| valid_id(id)) else {
                    return Ok(INVALID.into());
                };
                self.end(&guild, id, &actor_id(interaction)).await
            }
            "pause" => {
                let Some(id) = string_option(options, "id").filter(|id| valid_id(id)) else {
                    return Ok(INVALID.into());
                };
                if !store::pause_giveaway(&self.pool, &guild, id, OffsetDateTime::now_utc()).await?
                {
                    return Ok("Giveaway is unknown, already closed, or mid-draw.".into());
                }
                if let Ok(Some(row)) = store::get_giveaway(&self.pool, &guild, id).await {
                    if self.update(&row).await.is_err() {
                        return Ok("Giveaway paused; its message could not be refreshed.".into());
                    }
                }
                Ok("Giveaway paused.".into())
            }
            "resume" => {
                let Some(id) = string_option(options, "id").filter(|id| valid_id(id)) else {
                    return Ok(INVALID.into());
                };
                if store::resume_giveaway(&self.pool, &guild, id, OffsetDateTime::now_utc())
                    .await?
                    .is_none()
                {
                    return Ok("Giveaway is unknown or not paused.".into());
                }
                if let Ok(Some(row)) = store::get_giveaway(&self.pool, &guild, id).await {
                    if self.update(&row).await.is_err() {
                        return Ok("Giveaway resumed; its message could not be refreshed.".into());
                    }
                }
                Ok("Giveaway resumed.".into())
            }
            "extend" => {
                let Some(id) = string_option(options, "id").filter(|id| valid_id(id)) else {
                    return Ok(INVALID.into());
                };
                let Some(minutes) = minutes_option(options) else {
                    return Ok(INVALID.into());
                };
                let Some(seconds) = minutes.checked_mul(60) else {
                    return Ok(INVALID.into());
                };
                if giveaway::extend_deadline_ms(0, seconds).is_err() {
                    return Ok(INVALID.into());
                }
                if store::extend_giveaway(
                    &self.pool,
                    &guild,
                    id,
                    seconds,
                    OffsetDateTime::now_utc(),
                )
                .await?
                .is_none()
                {
                    return Ok("Giveaway is unknown or already closed.".into());
                }
                if let Ok(Some(row)) = store::get_giveaway(&self.pool, &guild, id).await {
                    if self.update(&row).await.is_err() {
                        return Ok("Giveaway extended; its message could not be refreshed.".into());
                    }
                }
                Ok("Giveaway extended.".into())
            }
            "reroll" => {
                let Some(id) = string_option(options, "id").filter(|id| valid_id(id)) else {
                    return Ok(INVALID.into());
                };
                let mut target = None;
                for option in options {
                    match (&*option.name, &option.value) {
                        ("id", CommandOptionValue::String(_)) => {}
                        ("user", CommandOptionValue::User(id)) => {
                            target = Some(id.get().to_string());
                        }
                        _ => return Ok(INVALID.into()),
                    }
                }
                self.reroll(&guild, id, target, &actor_id(interaction))
                    .await
            }
            _ => Ok(INVALID.into()),
        }
    }

    async fn create(
        &self,
        interaction: &Interaction,
        guild: &str,
        options: &[CommandDataOption],
    ) -> Result<String, sqlx::Error> {
        let Some(prize) = string_option(options, "prize") else {
            return Ok(INVALID.into());
        };
        let Some(seconds) = minutes_option(options).and_then(|n| n.checked_mul(60)) else {
            return Ok(INVALID.into());
        };
        let Some(winners) = integer_option(options, "winners") else {
            return Ok(INVALID.into());
        };
        let Ok(validated) = giveaway::validate_new_giveaway(&NewGiveawayInput {
            prize: prize.to_owned(),
            winners_count: winners,
            duration_seconds: seconds,
        }) else {
            return Ok(INVALID.into());
        };
        let mut roles = Vec::new();
        let mut account_days = 0;
        let mut server_days = 0;
        for option in options {
            match (option.name.as_str(), &option.value) {
                ("prize", CommandOptionValue::String(_))
                | ("duration" | "winners" | "duration-minutes", CommandOptionValue::Integer(_)) => {
                }
                ("required-role", CommandOptionValue::Role(id)) => roles.push(id.get().to_string()),
                ("min-account-age-days", CommandOptionValue::Integer(n))
                    if (0..=3650).contains(n) =>
                {
                    account_days = *n
                }
                ("min-server-age-days", CommandOptionValue::Integer(n))
                    if (0..=3650).contains(n) =>
                {
                    server_days = *n
                }
                // Accepted for forward compatibility: creation always posts in
                // the invocation channel, so a channel argument never steers.
                ("channel", CommandOptionValue::Channel(_)) => {}
                _ => return Ok(INVALID.into()),
            }
        }
        let Some(channel) = interaction_channel(interaction) else {
            return Ok(INVALID.into());
        };
        let actor = actor_id(interaction);
        if actor.is_empty() {
            return Ok(INVALID.into());
        }
        let now = OffsetDateTime::now_utc();
        // The interaction id is an opaque correlation id, not an authorization
        // capability. It makes a retried create fail closed before Discord I/O.
        let id = interaction.id.get().to_string();
        let input = NewGiveaway {
            guild_id: guild.into(),
            id: id.clone(),
            prize: validated.prize,
            channel_id: channel,
            message_id: "pending".into(),
            created_by: actor,
            winners_count: validated.winners_count as i32,
            ends_at: now + time::Duration::seconds(validated.duration_seconds),
            requirements_json: serde_json::json!({"roles": roles, "min_account_age_days": account_days, "min_guild_age_days": server_days}),
            max_entries: store::MAX_GIVEAWAY_ENTRIES,
            draw_nonce: giveaway::mint_draw_nonce(),
            created_at: now,
        };
        let Some(row) = store::create_admitted(&self.pool, &input).await? else {
            return Ok("Giveaway already recorded or the server has 10 active giveaways.".into());
        };
        let nonce = format!("gw{id}");
        match self.executor.post_message_with_components(&row.channel_id, &render(&row, 0), &components(&row), &nonce).await {
            Ok(message) => {
                if matches!(store::attach_message(&self.pool, guild, &id, &message).await, Ok(true)) {
                    return Ok(format!("Giveaway `{id}` created."));
                }
                // A confirmed POST with a failed attach must not leave an
                // enabled orphan. Govern exactly one DELETE of the known id;
                // redelivery resolves exists-before-POST, so this never rePOSTs.
                if self
                    .executor
                    .delete_message(
                        &row.channel_id,
                        &message,
                        "giveaway attach failed; neutralizing orphan entry message",
                    )
                    .await
                    .is_ok()
                {
                    return Ok("Giveaway recorded but its entry message could not be attached; the orphan was neutralized. Do not recreate it blindly.".into());
                }
                Ok(format!("Giveaway entry message {message} could not be cleaned up after a failed attach; inspect the channel and remove it manually. Do not recreate it blindly."))
            }
            Err(_) => Ok(format!("Giveaway `{id}` recorded but message delivery is unconfirmed. Inspect the channel before cancelling or recreating it.")),
        }
    }

    /// Draw one giveaway now under a fresh claim: the explicit `/giveaway end`.
    /// Shares the tick's draw engine, so end-now and deadline draws commit,
    /// audit and announce identically.
    async fn end(&self, guild: &str, id: &str, actor: &str) -> Result<String, sqlx::Error> {
        let now = OffsetDateTime::now_utc();
        let claim = giveaway::mint_draw_nonce();
        let Some(row) = store::claim_specific(
            &self.pool,
            guild,
            id,
            now,
            now - time::Duration::seconds(CLAIM_LEASE_SECS),
            &claim,
        )
        .await?
        else {
            return Ok("Giveaway is unknown, already closed, or mid-draw.".into());
        };
        match self.draw_under_claim(&row, &claim, Some(actor)).await {
            Ok(report) if report.kind == DrawKind::Raced => Ok("Giveaway drawn.".into()),
            Ok(report) if report.announced => Ok("Giveaway drawn.".into()),
            Ok(_) => {
                // Either the announcement failed (the tick retries) or the row
                // moved on while claimed (a cancel won the race): re-read so
                // the reply names the actual state.
                let status = store::get_giveaway(&self.pool, guild, id)
                    .await
                    .ok()
                    .flatten()
                    .map(|row| row.status);
                if status.as_deref() == Some("cancelled") {
                    Ok("Giveaway is unknown or already closed.".into())
                } else {
                    Ok("Giveaway drawn; its announcement will retry shortly.".into())
                }
            }
            Err(DrawError::Database(error)) => Err(error),
            Err(DrawError::Rest(_)) => {
                Ok("Giveaway draw could not verify members; try again shortly.".into())
            }
            Err(DrawError::Data) => {
                Ok("Giveaway draw data is damaged; an operator must inspect it.".into())
            }
        }
    }

    /// Append one reroll round: a full redraw, or the replacement of one named
    /// winner. History is immutable: the new row carries `draw_no + 1` and the
    /// old rows are never touched. A failed audit or announcement reopens the
    /// announce marker, so the tick heals both from the stored row.
    async fn reroll(
        &self,
        guild: &str,
        id: &str,
        target: Option<String>,
        actor: &str,
    ) -> Result<String, sqlx::Error> {
        let row = match store::get_giveaway(&self.pool, guild, id).await? {
            Some(row) => row,
            None => return Ok("Giveaway is unknown or already closed.".into()),
        };
        let draws = store::get_draws(&self.pool, guild, id).await?;
        let Some(latest) = draws.last() else {
            return Ok("Giveaway has not been drawn yet.".into());
        };
        let latest_winners: Vec<String> = serde_json::from_value(latest.winner_ids.clone())
            .map_err(|_| sqlx::Error::Protocol("invalid giveaway winners".into()))?;
        if let Some(target) = &target {
            if !latest_winners.contains(target) {
                return Ok("That member is not a winner of this giveaway.".into());
            }
        }
        let now = OffsetDateTime::now_utc();
        let requirements = match store_requirements(&row) {
            Ok(requirements) => requirements,
            Err(()) => {
                return Ok("Giveaway draw data is damaged; an operator must inspect it.".into());
            }
        };
        let entries = store::list_entries(&self.pool, guild, id).await?;
        let eligible = match self
            .eligible_entrants(guild, &requirements, &entries, now)
            .await
        {
            Ok(eligible) => eligible,
            Err(_) => {
                return Ok("Giveaway reroll could not verify members; try again shortly.".into());
            }
        };
        let draw_no = latest.draw_no + 1;
        let Ok(seed) = giveaway::draw_seed(&row.draw_nonce, i64::from(draw_no)) else {
            return Ok("Giveaway draw data is damaged; an operator must inspect it.".into());
        };
        let (winners, reroll_kind) = match &target {
            None => (
                giveaway::draw(&eligible, &seed, row.winners_count.max(0) as usize),
                giveaway::REROLL_KIND_FULL.to_owned(),
            ),
            Some(target) => {
                let keep = giveaway::single_reroll_keep(&eligible, &latest_winners, target);
                let pool = giveaway::single_reroll_pool(&eligible, &keep);
                (
                    giveaway::draw(&pool, &seed, 1),
                    giveaway::reroll_kind_single(target),
                )
            }
        };
        let input = NewDraw {
            guild_id: guild.to_owned(),
            giveaway_id: id.to_owned(),
            draw_no,
            drawn_at: now,
            eligible_count: eligible.len() as i32,
            eligible_hash: giveaway::eligible_hash(&eligible),
            seed,
            winner_ids: serde_json::to_value(&winners)
                .map_err(|_| sqlx::Error::Protocol("invalid giveaway winners".into()))?,
            reroll_kind: Some(reroll_kind),
        };
        let draw = match store::insert_draw(&self.pool, &input).await {
            Ok(draw) => draw,
            Err(error) if is_unique_violation(&error) => {
                return Ok("Giveaway reroll collided; try again.".into());
            }
            Err(error) => return Err(error),
        };
        if let Err(error) =
            store::audit_reroll(&self.pool, guild, id, draw_no, Some(actor), now).await
        {
            tracing::warn!(
                guild_id = guild,
                giveaway_id = id,
                "giveaway reroll audit failed; details withheld"
            );
            let _ = store::unmark_announced(&self.pool, guild, id, now).await;
            return Err(error);
        }
        if self.announce(&row, &draw).await {
            Ok("Giveaway rerolled.".into())
        } else {
            let _ = store::unmark_announced(&self.pool, guild, id, now).await;
            Ok("Giveaway rerolled; its announcement will retry shortly.".into())
        }
    }

    /// Called only after guild/gate routing and a successful ephemeral defer.
    pub async fn enter(&self, interaction: &Interaction, custom_id: &str) {
        let actor = actor_id(interaction);
        if actor.is_empty() || !self.take_budget(&actor) {
            self.finish(
                interaction,
                "Giveaway entry rate limit reached; try again in a minute.",
            )
            .await;
            return;
        }
        let _effects = EFFECTS.lock().await;
        let reply = self
            .enter_inner(interaction, custom_id, &actor)
            .await
            .unwrap_or_else(|_| FAILURE.into());
        self.finish(interaction, reply).await;
    }

    async fn enter_inner(
        &self,
        interaction: &Interaction,
        custom_id: &str,
        actor: &str,
    ) -> Result<String, sqlx::Error> {
        let Some(id) = custom_id.strip_prefix("gw:").filter(|id| valid_id(id)) else {
            return Ok("Invalid giveaway button.".into());
        };
        let Some(guild) = interaction.guild_id else {
            return Ok("Giveaway entry requires a server.".into());
        };
        let guild = guild.get().to_string();
        let now = OffsetDateTime::now_utc();
        let Some(member) = interaction.member.as_ref() else {
            return Ok("Giveaway membership could not be verified.".into());
        };
        let Some(user) = member.user.as_ref().filter(|user| !user.bot) else {
            return Ok("Giveaway entry is for server members.".into());
        };
        let created_ms = (user.id.get() >> 22) + 1_420_070_400_000;
        let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as u64;
        if created_ms > now_ms {
            return Ok("Giveaway account age could not be verified.".into());
        }
        // Missing or future membership evidence counts as zero days: a set age
        // gate then refuses via adjudication, while an open gate passes.
        let guild_age_days = member
            .joined_at
            .filter(|at| at.as_secs() <= now.unix_timestamp())
            .map(|at| ((now.unix_timestamp() - at.as_secs()) / 86_400) as u32)
            .unwrap_or(0);
        let standing = MemberStanding {
            role_ids: member.roles.iter().map(|id| id.get().to_string()).collect(),
            account_age_days: ((now_ms - created_ms) / 86_400_000) as u32,
            guild_age_days,
        };
        let admission = store::admit_entry(&self.pool, &guild, id, actor, &standing, now).await?;
        Ok(match admission {
            EntryAdmission::Entered => {
                let row = store::get_giveaway(&self.pool, &guild, id).await?;
                if let Some(row) = row {
                    if self.update(&row).await.is_err() {
                        return Ok(
                            "You entered the giveaway; its count display will refresh later."
                                .into(),
                        );
                    }
                }
                "You entered the giveaway.".into()
            }
            EntryAdmission::AlreadyEntered => "You already entered this giveaway.".into(),
            EntryAdmission::Closed => "Giveaway is closed or unavailable.".into(),
            EntryAdmission::Full => "Giveaway entry capacity reached.".into(),
            EntryAdmission::Ineligible => {
                "You do not meet this giveaway's role or age requirements.".into()
            }
            EntryAdmission::RateLimited => {
                "Giveaway entry rate limit reached; try again in a minute.".into()
            }
        })
    }

    fn take_budget(&self, actor: &str) -> bool {
        let Ok(mut budgets) = self.budgets.lock() else {
            return false;
        };
        let now = Instant::now();
        budgets.retain(|_, (at, _)| now.duration_since(*at).as_secs() < 60);
        if !budgets.contains_key(actor) && budgets.len() >= BUDGET_KEYS {
            return false;
        }
        let (_, count) = budgets.entry(actor.into()).or_insert((now, 0));
        if *count >= WRITE_BUDGET {
            return false;
        }
        *count += 1;
        true
    }

    async fn update(&self, row: &GiveawayRow) -> Result<(), GiveawayJobError> {
        let count = store::entry_count(&self.pool, &row.guild_id, &row.id).await?;
        self.executor
            .edit_message_with_components(
                &row.channel_id,
                &row.message_id,
                &render(row, count),
                &components(row),
            )
            .await
            .map_err(|error| GiveawayJobError::Rest(error.to_string()))
    }

    /// Countdown text itself is rendered by Discord. At most one edit per
    /// durable 1h/10m/1m/30s bucket, never one edit per supervisor tick.
    pub async fn refresh(&self, guild: &str) -> Result<(), GiveawayJobError> {
        let _effects = EFFECTS.lock().await;
        let rows = store::list_active(&self.pool, guild).await?;
        let now = OffsetDateTime::now_utc();
        for row in rows {
            let remaining = (row.ends_at - now).whole_seconds();
            if row.status != "open" || row.message_id == "pending" || remaining <= 0 {
                continue;
            }
            let threshold = [30, 60, 600, 3600]
                .into_iter()
                .find(|threshold| remaining <= *threshold);
            if let Some(threshold) = threshold {
                if store::claim_refresh(&self.pool, &row, threshold).await? {
                    self.update(&row).await?;
                }
            }
        }
        Ok(())
    }

    /// One due-scan tick: claim every due giveaway and draw under the claim,
    /// then re-announce every drawn-but-unannounced row from its stored draw.
    /// A row that fails is released for the next tick; the tick reports
    /// failure when any announcement did not deliver.
    pub async fn tick(&self, guild: &str) -> Result<GiveawayTickSummary, GiveawayJobError> {
        let _effects = EFFECTS.lock().await;
        let mut summary = GiveawayTickSummary {
            drawn: 0,
            announced: 0,
        };
        let mut failed = false;
        let now = OffsetDateTime::now_utc();
        let reclaim_before = now - time::Duration::seconds(CLAIM_LEASE_SECS);
        for _ in 0..TICK_BATCH {
            let claim = giveaway::mint_draw_nonce();
            let claimed =
                store::claim_due_giveaway(&self.pool, guild, now, reclaim_before, &claim).await?;
            let Some(row) = claimed else {
                break;
            };
            match self.draw_under_claim(&row, &claim, None).await {
                Ok(report) => {
                    if report.kind == DrawKind::Drawn {
                        summary.drawn += 1;
                    }
                    if report.announced {
                        summary.announced += 1;
                    }
                    failed |= report.failed;
                }
                Err(DrawError::Database(error)) => return Err(GiveawayJobError::Database(error)),
                Err(DrawError::Rest(cause)) => return Err(GiveawayJobError::Rest(cause)),
                Err(DrawError::Data) => failed = true,
            }
        }
        for row in store::list_unannounced(&self.pool, guild, TICK_BATCH).await? {
            let draws = store::get_draws(&self.pool, guild, &row.id).await?;
            let Some(latest) = draws.last() else {
                tracing::warn!(
                    guild_id = guild,
                    giveaway_id = row.id.as_str(),
                    "drawn giveaway has no draw row; leaving it for an operator"
                );
                continue;
            };
            // Idempotent heal: a crash between draw and audit left the row
            // without its audit, and the deterministic id makes the rewrite
            // a no-op instead of a conflict.
            heal_draw_audit(&self.pool, guild, &row.id, latest, now).await;
            if self.announce(&row, latest).await {
                summary.announced += 1;
            } else {
                failed = true;
            }
        }
        if failed {
            Err(GiveawayJobError::Announce)
        } else {
            Ok(summary)
        }
    }

    /// Draw under a held claim: re-validate eligibility from fresh membership
    /// reads, run the pure draw, commit the draw row and the flip atomically,
    /// audit, then announce. A row that already holds a draw row skips to
    /// announce and never re-draws. Every error exit releases the claim.
    async fn draw_under_claim(
        &self,
        row: &GiveawayRow,
        claim: &str,
        actor: Option<&str>,
    ) -> Result<ClaimReport, DrawError> {
        let settled = |kind, announced, failed| ClaimReport {
            kind,
            announced,
            failed,
        };
        let release = || async {
            let _ = store::release_claim(
                &self.pool,
                &row.guild_id,
                &row.id,
                claim,
                OffsetDateTime::now_utc(),
            )
            .await;
        };
        let draws = store::get_draws(&self.pool, &row.guild_id, &row.id).await?;
        if let Some(latest) = draws.last() {
            // Crash recovery: the draw row exists but the flip or the announce
            // never landed. Complete the flip, heal the audit, announce.
            if !store::mark_drawn(
                &self.pool,
                &row.guild_id,
                &row.id,
                claim,
                OffsetDateTime::now_utc(),
            )
            .await?
            {
                // The row moved on (cancelled, or a racing worker finished):
                // stay silent instead of announcing a stale draw.
                return Ok(settled(DrawKind::Skipped, false, false));
            }
            let now = OffsetDateTime::now_utc();
            heal_draw_audit(&self.pool, &row.guild_id, &row.id, latest, now).await;
            let announced = self.announce(row, latest).await;
            return Ok(settled(DrawKind::Skipped, announced, !announced));
        }
        let now = OffsetDateTime::now_utc();
        let requirements = match store_requirements(row) {
            Ok(requirements) => requirements,
            Err(()) => {
                tracing::warn!(
                    guild_id = row.guild_id.as_str(),
                    giveaway_id = row.id.as_str(),
                    "giveaway requirements are damaged; leaving the row for an operator"
                );
                release().await;
                return Err(DrawError::Data);
            }
        };
        let entries = store::list_entries(&self.pool, &row.guild_id, &row.id).await?;
        let eligible = match self
            .eligible_entrants(&row.guild_id, &requirements, &entries, now)
            .await
        {
            Ok(eligible) => eligible,
            Err(cause) => {
                release().await;
                return Err(DrawError::Rest(cause));
            }
        };
        let Ok(seed) = giveaway::draw_seed(&row.draw_nonce, giveaway::FIRST_DRAW_NO) else {
            tracing::warn!(
                guild_id = row.guild_id.as_str(),
                giveaway_id = row.id.as_str(),
                "giveaway draw nonce is damaged; leaving the row for an operator"
            );
            release.await;
            return Err(DrawError::Data);
        };
        let winners = giveaway::draw(&eligible, &seed, row.winners_count.max(0) as usize);
        let winner_ids = match serde_json::to_value(&winners) {
            Ok(ids) => ids,
            Err(_) => {
                release().await;
                return Err(DrawError::Data);
            }
        };
        let input = NewDraw {
            guild_id: row.guild_id.clone(),
            giveaway_id: row.id.clone(),
            draw_no: giveaway::FIRST_DRAW_NO as i32,
            drawn_at: now,
            eligible_count: eligible.len() as i32,
            eligible_hash: giveaway::eligible_hash(&eligible),
            seed,
            winner_ids,
            reroll_kind: None,
        };
        let draw = match store::commit_draw_from(&self.pool, &input, claim, now, &row.status).await
        {
            Ok(Some(draw)) => draw,
            Ok(None) => {
                release().await;
                return Ok(settled(DrawKind::Raced, false, false));
            }
            Err(error) if is_unique_violation(&error) => {
                release().await;
                return Ok(settled(DrawKind::Raced, false, false));
            }
            Err(error) => {
                release().await;
                return Err(DrawError::Database(error));
            }
        };
        // Audit before announce: a crash here leaves announced_at NULL, so the
        // next tick heals the audit and announces from the stored row.
        if let Err(error) =
            store::audit_draw(&self.pool, &row.guild_id, &row.id, draw.draw_no, actor, now).await
        {
            tracing::warn!(
                guild_id = row.guild_id.as_str(),
                giveaway_id = row.id.as_str(),
                "giveaway draw audit failed; details withheld"
            );
            return Err(DrawError::Database(error));
        }
        let announced = self.announce(row, &draw).await;
        Ok(settled(DrawKind::Drawn, announced, !announced))
    }

    /// Re-validate every entrant against fresh membership reads: still in the
    /// guild, still holding the required roles, still passing the age gates.
    /// Absent members are excluded; a transport failure aborts the draw so a
    /// Discord outage never silently shrinks the eligible set.
    async fn eligible_entrants(
        &self,
        guild: &str,
        requirements: &giveaway::GiveawayRequirements,
        entries: &[GiveawayEntryRow],
        now: OffsetDateTime,
    ) -> Result<Vec<String>, String> {
        let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as u64;
        let mut eligible = Vec::with_capacity(entries.len());
        for entry in entries {
            let Ok(snowflake) = entry.user_id.parse::<u64>() else {
                continue;
            };
            let created_ms = (snowflake >> 22) + 1_420_070_400_000;
            if created_ms > now_ms {
                continue;
            }
            let account_age_days = ((now_ms - created_ms) / 86_400_000) as u32;
            let evidence = self.member_evidence(guild, &entry.user_id, now).await?;
            let Some(evidence) = evidence else {
                continue;
            };
            let standing = MemberStanding {
                role_ids: evidence.roles,
                account_age_days,
                guild_age_days: evidence.guild_age_days,
            };
            if giveaway::adjudicate_eligibility(requirements, &standing)
                == giveaway::Eligibility::Eligible
            {
                eligible.push(entry.user_id.clone());
            }
        }
        Ok(eligible)
    }

    /// One membership read: roles plus guild age in days. `None` means absent
    /// or unproven (403/404, or an unreadable body): excluded, fail closed.
    async fn member_evidence(
        &self,
        guild: &str,
        user_id: &str,
        now: OffsetDateTime,
    ) -> Result<Option<MemberEvidence>, String> {
        let member = self
            .executor
            .get_json(&format!("/guilds/{guild}/members/{user_id}"))
            .await?
            .filter(|value| value.is_object());
        let Some(member) = member else {
            return Ok(None);
        };
        let roles = member
            .get("roles")
            .and_then(serde_json::Value::as_array)
            .map(|roles| {
                roles
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        // Missing or unreadable membership timing counts as zero days: a set
        // age gate then excludes via adjudication, while an open gate passes.
        let guild_age_days = member
            .get("joined_at")
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| {
                time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
                    .ok()
            })
            .filter(|joined| joined.unix_timestamp() <= now.unix_timestamp())
            .map(|joined| ((now.unix_timestamp() - joined.unix_timestamp()) / 86_400) as u32)
            .unwrap_or(0);
        Ok(Some(MemberEvidence {
            roles,
            guild_age_days,
        }))
    }

    /// Announce from a stored draw row: edit the giveaway message first, then
    /// post the winners (mentions, seed, revealed nonce), then mark. `true`
    /// means delivered and marked (or marked by a racing worker); `false`
    /// leaves `announced_at` NULL for the next tick. Never draws.
    async fn announce(&self, row: &GiveawayRow, draw: &GiveawayDrawRow) -> bool {
        let count = match store::entry_count(&self.pool, &row.guild_id, &row.id).await {
            Ok(count) => count,
            Err(_) => {
                tracing::warn!(
                    guild_id = row.guild_id.as_str(),
                    giveaway_id = row.id.as_str(),
                    "giveaway announce count read failed; details withheld"
                );
                return false;
            }
        };
        let mut shown = row.clone();
        shown.status = "drawn".to_owned();
        if self
            .executor
            .edit_message_with_components(
                &row.channel_id,
                &row.message_id,
                &render(&shown, count),
                &components(&shown),
            )
            .await
            .is_err()
        {
            tracing::warn!(
                guild_id = row.guild_id.as_str(),
                giveaway_id = row.id.as_str(),
                "giveaway announce edit failed; details withheld"
            );
            return false;
        }
        let winners: Vec<String> = match serde_json::from_value(draw.winner_ids.clone()) {
            Ok(winners) => winners,
            Err(_) => {
                tracing::warn!(
                    guild_id = row.guild_id.as_str(),
                    giveaway_id = row.id.as_str(),
                    "giveaway draw winners are damaged; announcing without mentions"
                );
                Vec::new()
            }
        };
        for post in winners_posts(
            &row.prize,
            &winners,
            &draw.seed,
            &row.draw_nonce,
            i64::from(draw.draw_no),
            draw.eligible_count,
            &draw.eligible_hash,
        ) {
            if self
                .executor
                .post_message(&row.channel_id, &post, None)
                .await
                .is_err()
            {
                tracing::warn!(
                    guild_id = row.guild_id.as_str(),
                    giveaway_id = row.id.as_str(),
                    "giveaway announce post failed; details withheld"
                );
                return false;
            }
        }
        match store::mark_announced(
            &self.pool,
            &row.guild_id,
            &row.id,
            OffsetDateTime::now_utc(),
        )
        .await
        {
            Ok(_) => true,
            Err(_) => {
                tracing::warn!(
                    guild_id = row.guild_id.as_str(),
                    giveaway_id = row.id.as_str(),
                    "giveaway announce mark failed; details withheld"
                );
                false
            }
        }
    }

    async fn finish(&self, interaction: &Interaction, text: impl AsRef<str>) {
        if self
            .executor
            .edit_interaction_response_with_blocked_retry(
                interaction.application_id.get(),
                &interaction.token,
                text.as_ref(),
            )
            .await
            .is_err()
        {
            tracing::warn!(interaction_id = %interaction.id, "giveaway reply failed; details withheld");
        }
    }
}

/// Fresh membership evidence for one entrant at draw time.
struct MemberEvidence {
    roles: Vec<String>,
    guild_age_days: u32,
}

/// Parse the stored requirements of a giveaway row. `Err` means the stored
/// JSON is damaged; callers fail the row instead of guessing the gates.
fn store_requirements(row: &GiveawayRow) -> Result<giveaway::GiveawayRequirements, ()> {
    serde_json::to_string(&row.requirements_json)
        .ok()
        .and_then(|raw| giveaway::parse_requirements(&raw).ok())
        .ok_or(())
}

/// Rewrite one draw/reroll audit idempotently: the deterministic round id
/// makes the rewrite a no-op when the original write already landed.
async fn heal_draw_audit(
    pool: &PgPool,
    guild: &str,
    giveaway_id: &str,
    draw: &GiveawayDrawRow,
    now: OffsetDateTime,
) {
    let healed = if draw.reroll_kind.is_none() {
        store::audit_draw(pool, guild, giveaway_id, draw.draw_no, None, now).await
    } else {
        store::audit_reroll(pool, guild, giveaway_id, draw.draw_no, None, now).await
    };
    if healed.is_err() {
        tracing::warn!(
            guild_id = guild,
            giveaway_id = giveaway_id,
            "giveaway audit heal failed; details withheld"
        );
    }
}

/// True for a PostgreSQL unique violation (23505): the lost side of a raced
/// draw insert, never a bug to propagate.
fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

fn actor_id(interaction: &Interaction) -> String {
    interaction
        .member
        .as_ref()
        .and_then(|member| member.user.as_ref())
        .or(interaction.user.as_ref())
        .map(|user| user.id.get().to_string())
        .unwrap_or_default()
}

fn interaction_channel(interaction: &Interaction) -> Option<String> {
    interaction
        .channel
        .as_ref()
        .map(|channel| channel.id.to_string())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn string_option<'a>(options: &'a [CommandDataOption], name: &str) -> Option<&'a str> {
    options
        .iter()
        .find_map(|option| match (&*option.name, &option.value) {
            (key, CommandOptionValue::String(value)) if key == name => Some(value.as_str()),
            _ => None,
        })
}

fn integer_option(options: &[CommandDataOption], name: &str) -> Option<i64> {
    options
        .iter()
        .find_map(|option| match (&*option.name, &option.value) {
            (key, CommandOptionValue::Integer(value)) if key == name => Some(*value),
            _ => None,
        })
}

/// Duration in minutes under either accepted name: `duration-minutes` (the
/// timed-draw contract) or `duration` (the create/entry registry pin).
fn minutes_option(options: &[CommandDataOption]) -> Option<i64> {
    integer_option(options, "duration-minutes").or_else(|| integer_option(options, "duration"))
}

/// Winners announcement, chunked so every post stays under the packing
/// budget: the first post carries the replay inputs (seed, revealed nonce,
/// eligible hash), later posts carry overflow mentions only.
fn winners_posts(
    prize: &str,
    winners: &[String],
    seed: &str,
    nonce: &str,
    draw_no: i64,
    eligible_count: i32,
    eligible_hash: &str,
) -> Vec<String> {
    let header = format!("Giveaway winners for {prize}");
    let footer = format!(
        "Seed: {seed}\nNonce: {nonce}\nEligible: {eligible_count} (hash {eligible_hash})\nReplay: sha256(\"{nonce}:{draw_no}\")"
    );
    if winners.is_empty() {
        return vec![format!("{header}\nNo eligible entrants.\n{footer}")];
    }
    let first_budget = POST_BUDGET.saturating_sub(footer.len() + 1);
    let mut posts = vec![header];
    for winner in winners {
        let mention = format!("<@{winner}>");
        let budget = if posts.len() == 1 {
            first_budget
        } else {
            POST_BUDGET
        };
        let fits = posts
            .last()
            .is_some_and(|last: &String| last.len() + 1 + mention.len() <= budget);
        if fits {
            if let Some(last) = posts.last_mut() {
                last.push(' ');
                last.push_str(&mention);
            }
        } else {
            posts.push(mention);
        }
    }
    if let Some(first) = posts.first_mut() {
        first.push('\n');
        first.push_str(&footer);
    }
    posts
}

fn render(row: &GiveawayRow, count: i64) -> String {
    let ending = if row.status == "open"
        && row.ends_at.unix_timestamp() - OffsetDateTime::now_utc().unix_timestamp() <= 30
    {
        " — ending soon!"
    } else {
        ""
    };
    format!(
        "Giveaway: {}\n{} entrants · {} winners\nEnds <t:{}:R> · {}{}\nID: `{}`",
        row.prize,
        count,
        row.winners_count,
        row.ends_at.unix_timestamp(),
        row.status,
        ending,
        row.id
    )
}

fn components(row: &GiveawayRow) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"type": 1, "components": [{"type": 2, "style": 1, "label": "Enter giveaway", "custom_id": format!("gw:{}", row.id), "disabled": row.status != "open"}]}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_budgets_allow_ten_writes_per_minute_per_member() {
        let pool = PgPool::connect_lazy("postgres://localhost:1/budget-unit").expect("lazy pool");
        let executor = ActionExecutor::with_proxy("mock-token-not-a-credential".to_owned(), None)
            .expect("proxy");
        let runtime = GiveawayRuntime::new(pool, executor);
        for _ in 0..WRITE_BUDGET {
            assert!(runtime.take_budget("member-budget-unit"));
        }
        assert!(
            !runtime.take_budget("member-budget-unit"),
            "the 11th write in a minute is refused"
        );
        assert!(
            runtime.take_budget("other-member-budget-unit"),
            "budgets are per member"
        );
    }

    #[test]
    fn winners_posts_carry_replay_inputs_and_chunk_overflow() {
        let posts = winners_posts(
            "Test Prize",
            &["1".to_owned()],
            "seed",
            "nonce",
            0,
            1,
            "hash",
        );
        assert_eq!(posts.len(), 1);
        assert!(posts[0].contains("Test Prize"));
        assert!(posts[0].contains("<@1>"));
        assert!(posts[0].contains("seed"));
        assert!(posts[0].contains("nonce"));
        assert!(posts[0].contains("hash"));
        assert!(posts[0].len() <= POST_BUDGET);
        let empty: Vec<String> = Vec::new();
        let posts = winners_posts("Test Prize", &empty, "seed", "nonce", 0, 0, "hash");
        assert_eq!(posts.len(), 1);
        assert!(posts[0].contains("seed"));
        let hundred: Vec<String> = (0..100).map(|n| format!("155010203040506{n:04}")).collect();
        let posts = winners_posts("Test Prize", &hundred, "seed", "nonce", 0, 100, "hash");
        assert!(posts.len() > 1, "100 winners do not fit one post");
        for post in &posts {
            assert!(
                post.len() <= POST_BUDGET,
                "every post stays packed: {}",
                post.len()
            );
        }
        let joined = posts.join("\n");
        for winner in &hundred {
            assert!(joined.contains(&format!("<@{winner}>")));
        }
        assert!(posts[0].contains("seed"));
        assert!(posts[0].contains("nonce"));
    }
}
