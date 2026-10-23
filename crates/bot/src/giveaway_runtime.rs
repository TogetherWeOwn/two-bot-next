//! Giveaway administration and member entry over the shared router/executor.

use std::{collections::HashMap, sync::Mutex, time::Instant};

use sqlx::PgPool;
use time::OffsetDateTime;
use twilight_model::application::interaction::{
    application_command::{CommandDataOption, CommandOptionValue},
    Interaction, InteractionData,
};
use two_bot_core::{
    giveaway::{self, MemberStanding, NewGiveawayInput},
    giveaway_store::{self as store, EntryAdmission, GiveawayRow, NewGiveaway},
};
use two_bot_discord::ActionExecutor;

use crate::{
    command_runtime::{actor_id, interaction_channel},
    jobs::ErrorClass,
};

const FAILURE: &str = "Giveaway unavailable; try again later.";
const INVALID: &str = "Invalid giveaway options. Use a prize up to 200 characters, 1–43200 minutes and 1–100 winners.";
const WRITE_BUDGET: u32 = 10;
const BUDGET_KEYS: usize = 4096;
pub(crate) const REFRESH_JOB: &str = "giveaway_countdown";
// Commands and the supervised ticker use separately composed pool handles,
// but share one edit order in the single always-on container.
static EFFECTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) struct GiveawayRuntime {
    pool: PgPool,
    executor: ActionExecutor,
    // Serialize count edits with entry/cancel, including the refresh supervisor.
    // No spawned waiters: the existing command admission lane bounds callers.
    budgets: Mutex<HashMap<String, (Instant, u32)>>,
}

impl GiveawayRuntime {
    pub(crate) fn new(pool: PgPool, executor: ActionExecutor) -> Self {
        Self {
            pool,
            executor,
            budgets: Mutex::new(HashMap::new()),
        }
    }

    /// Called only after the shared router authorized and deferred the command.
    pub(crate) async fn command(&self, interaction: &Interaction) {
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
                // A bounded preview keeps 25 maximum-length prizes below Discord's ceiling.
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
        let Some(seconds) = integer_option(options, "duration").and_then(|n| n.checked_mul(60))
        else {
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
                | ("duration" | "winners", CommandOptionValue::Integer(_)) => {}
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
            return Ok("Giveaway already recorded or the server has 25 active giveaways.".into());
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

    /// Called only after guild/gate routing and a successful ephemeral defer.
    pub(crate) async fn enter(&self, interaction: &Interaction, custom_id: &str) {
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
        // Missing/future membership evidence fails closed, even for an age-zero gate.
        let Some(joined) = member
            .joined_at
            .filter(|at| at.as_secs() <= now.unix_timestamp())
        else {
            return Ok("Giveaway membership could not be verified.".into());
        };
        let created_ms = (user.id.get() >> 22) + 1_420_070_400_000;
        let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as u64;
        if created_ms > now_ms {
            return Ok("Giveaway account age could not be verified.".into());
        }
        let standing = MemberStanding {
            role_ids: member.roles.iter().map(|id| id.get().to_string()).collect(),
            account_age_days: ((now_ms - created_ms) / 86_400_000) as u32,
            guild_age_days: ((now.unix_timestamp() - joined.as_secs()) / 86_400) as u32,
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

    async fn update(&self, row: &GiveawayRow) -> Result<(), ErrorClass> {
        let count = store::entry_count(&self.pool, &row.guild_id, &row.id)
            .await
            .map_err(|_| ErrorClass::Database)?;
        self.executor
            .edit_message_with_components(
                &row.channel_id,
                &row.message_id,
                &render(row, count),
                &components(row),
            )
            .await
            .map_err(|_| ErrorClass::Rest)
    }

    /// Countdown text itself is rendered by Discord. At most one edit per
    /// durable 1h/10m/1m/30s bucket, never one edit per supervisor tick.
    pub(crate) async fn refresh(&self, guild: &str) -> Result<(), ErrorClass> {
        let _effects = EFFECTS.lock().await;
        let rows = store::list_active(&self.pool, guild)
            .await
            .map_err(|_| ErrorClass::Database)?;
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
                if store::claim_refresh(&self.pool, &row, threshold)
                    .await
                    .map_err(|_| ErrorClass::Database)?
                {
                    self.update(&row).await?;
                }
            }
        }
        Ok(())
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

/// Reuse the existing job supervisor, identity fence, pool and REST admission.
pub(crate) fn register(
    context: std::sync::Arc<crate::website_jobs::Context>,
    activation: &crate::activation::BootActivation,
) -> Option<crate::jobs::Job> {
    let gates = two_bot_core::FeatureGates::from_env().ok()?;
    if !activation.constrain_features(gates).automations {
        return None;
    }
    let cadence = std::time::Duration::from_secs(15);
    Some(crate::jobs::Job {
        name: REFRESH_JOB,
        cadence,
        cadence_source: None,
        startup_jitter: crate::jobs::startup_jitter(cadence, rand::random()),
        timeout: std::time::Duration::from_secs(120),
        action: std::sync::Arc::new(move || {
            let context = context.clone();
            Box::pin(async move {
                GiveawayRuntime::new(context.pool().await?.clone(), context.rest.clone())
                    .refresh(&context.guild)
                    .await
            })
        }),
    })
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
