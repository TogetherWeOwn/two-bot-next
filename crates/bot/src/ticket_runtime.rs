//! Ticket orchestration over the existing guild-scoped store and REST executor.
//! No transcript body is logged, audited, or returned in an interaction reply.

use std::{
    collections::HashSet,
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex as TaskMutex,
    },
    time::Duration,
};

use serde_json::Value;
use sqlx::PgPool;
use tokio::{
    sync::{watch, Mutex},
    task::{JoinHandle, JoinSet},
    time::{Instant, MissedTickBehavior},
};
use twilight_model::{application::interaction::Interaction, guild::Permissions};
use two_bot_core::{
    funnel::{now_millis_for_test, parse_iso_millis},
    tickets::*,
};
use two_bot_cutover::tickets::{OpenResult, StoreError, TicketStore};
use two_bot_discord::{
    executor::{ChannelPresence, TicketChannelRequest, TicketMessage},
    ActionExecutor, DiscordError,
};

use crate::jobs::{ErrorClass, JobAction};

const RECOVERY_CADENCE: Duration = Duration::from_secs(RECOVERY_INTERVAL_SECONDS);
const PURGE_CADENCE: Duration = Duration::from_secs(PURGE_INTERVAL_SECONDS);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(240);
const PURGE_TIMEOUT: Duration = Duration::from_secs(60);

/// The gateway owns this scope. Dropping a cancelled shard also cancels all
/// ticket work; normal shutdown additionally joins it before returning.
pub(crate) struct TicketSupervisor {
    runtime: Arc<TicketRuntime>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl TicketSupervisor {
    pub async fn shutdown(self) {
        self.runtime.stop_tasks();
        self.shutdown.send_replace(true);
        // Await by reference because Drop remains the cancellation fallback.
        let mut this = self;
        let _ = (&mut this.task).await;
        let mut tasks = {
            let mut tasks = this.runtime.tasks.lock().expect("ticket task scope");
            std::mem::take(&mut *tasks)
        };
        tasks.shutdown().await;
    }
}

impl Drop for TicketSupervisor {
    fn drop(&mut self) {
        self.runtime.stop_tasks();
        self.shutdown.send_replace(true);
        self.task.abort();
    }
}

async fn maintenance_loop(
    name: &'static str,
    action: JobAction,
    cadence: Duration,
    timeout: Duration,
    mut ready: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval_at(Instant::now() + cadence, cadence);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut completed_at = None;
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow_and_update() { return; }
                continue;
            }
            changed = ready.changed() => {
                if changed.is_err() { return; }
                ready.borrow_and_update();
            }
            deadline = interval.tick() => {
                // Do not replay ticks that elapsed during a slow action.
                if completed_at.is_some_and(|end| deadline < end) { continue; }
            }
        }
        // Work is inline, not detached, so shutdown/drop cancels its future.
        let result = tokio::select! {
            biased;
            _ = crate::server::shutdown_requested(shutdown.clone()) => return,
            result = tokio::time::timeout(timeout, async { action().await }) => {
                result.unwrap_or(Err(ErrorClass::Timeout))
            }
        };
        completed_at = Some(Instant::now());
        if let Err(class) = result {
            tracing::warn!(job = name, error_class = ?class, "ticket maintenance deferred");
        }
    }
}

#[derive(Clone)]
pub(crate) struct TicketConfig {
    pub guild_id: String,
    pub category_id: String,
    pub panel_channel_id: String,
    pub staff_role_id: String,
    pub cooldown_seconds: u64,
}

impl TicketConfig {
    pub fn from_env(guild_id: u64) -> Option<Self> {
        let setting = |key| std::env::var(key).ok().filter(|v| valid_id(v).is_ok());
        let category_id = setting("DISCORD_TICKET_CATEGORY_ID")?;
        let panel_channel_id = setting("DISCORD_TICKET_PANEL_CHANNEL_ID")?;
        let staff_role_id = setting("DISCORD_TICKET_STAFF_ROLE_ID")?;
        if staff_role_id == guild_id.to_string() || category_id == panel_channel_id {
            tracing::warn!("invalid ticket configuration; tickets disabled");
            return None;
        }
        let cooldown_seconds = match std::env::var("TWO_TICKET_COOLDOWN_SECONDS") {
            Ok(value) => match value.parse() {
                Ok(value) => value,
                Err(_) => {
                    tracing::warn!("invalid ticket cooldown; tickets disabled");
                    return None;
                }
            },
            Err(std::env::VarError::NotPresent) => COOLDOWN_SECONDS,
            Err(_) => return None,
        };
        Some(Self {
            guild_id: guild_id.to_string(),
            category_id,
            panel_channel_id,
            staff_role_id,
            cooldown_seconds,
        })
    }
}

pub(crate) struct TicketRuntime {
    store: TicketStore,
    executor: ActionExecutor,
    config: TicketConfig,
    bot_id: AtomicU64,
    // Ready, buttons and recovery share this lane. Purge has its own lane so
    // slow Discord I/O cannot postpone the privacy ceiling.
    lane: Mutex<()>,
    purge_lane: Mutex<()>,
    ready: watch::Sender<u64>,
    started: AtomicBool,
    stopping: AtomicBool,
    tasks: TaskMutex<JoinSet<()>>,
}

pub(crate) enum Failure {
    Store(StoreError),
    Rest(DiscordError),
    InvalidEvidence,
}

impl From<StoreError> for Failure {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
impl From<DiscordError> for Failure {
    fn from(error: DiscordError) -> Self {
        Self::Rest(error)
    }
}
impl Failure {
    pub fn reply(&self) -> String {
        match self {
            Self::Store(StoreError::Domain(error)) => error.to_string(),
            _ => {
                "Ticket action failed; recovery will retry unfinished work. Try again later.".into()
            }
        }
    }
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Store(_) => ErrorClass::Database,
            Self::Rest(DiscordError::Timeout) => ErrorClass::Timeout,
            Self::Rest(_) | Self::InvalidEvidence => ErrorClass::Rest,
        }
    }
}

type Result<T> = std::result::Result<T, Failure>;

impl TicketRuntime {
    pub fn new(
        pool: PgPool,
        executor: ActionExecutor,
        config: TicketConfig,
    ) -> std::result::Result<Self, StoreError> {
        Ok(Self {
            store: TicketStore::new(pool, config.guild_id.clone())?,
            executor,
            config,
            bot_id: AtomicU64::new(0),
            lane: Mutex::new(()),
            purge_lane: Mutex::new(()),
            ready: watch::channel(0).0,
            started: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            tasks: TaskMutex::new(JoinSet::new()),
        })
    }

    pub fn start(self: &Arc<Self>) -> Option<TicketSupervisor> {
        if self.started.swap(true, Ordering::AcqRel) {
            return None;
        }
        let (shutdown, receiver) = watch::channel(false);
        let recovery = Arc::clone(self);
        let purge = Arc::clone(self);
        let recovery_ready = self.ready.subscribe();
        let purge_ready = self.ready.subscribe();
        let task = tokio::spawn(async move {
            tokio::join!(
                maintenance_loop(
                    "ticket_recovery",
                    Arc::new(move || {
                        let runtime = Arc::clone(&recovery);
                        Box::pin(async move { runtime.recover().await })
                    }),
                    RECOVERY_CADENCE,
                    RECOVERY_TIMEOUT,
                    recovery_ready,
                    receiver.clone(),
                ),
                maintenance_loop(
                    "ticket_purge",
                    Arc::new(move || {
                        let runtime = Arc::clone(&purge);
                        Box::pin(async move { runtime.purge().await })
                    }),
                    PURGE_CADENCE,
                    PURGE_TIMEOUT,
                    purge_ready,
                    receiver,
                ),
            );
        });
        Some(TicketSupervisor {
            runtime: Arc::clone(self),
            shutdown,
            task,
        })
    }

    pub fn on_ready(&self, bot_id: u64) {
        self.set_bot_id(bot_id);
        self.ready
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks.lock().expect("ticket task scope");
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        while let Some(result) = tasks.try_join_next() {
            if result.is_err() {
                tracing::warn!("ticket task stopped; durable state retained");
            }
        }
        tasks.spawn(task);
    }

    fn stop_tasks(&self) {
        let mut tasks = self.tasks.lock().expect("ticket task scope");
        self.stopping.store(true, Ordering::Release);
        tasks.abort_all();
    }

    pub fn set_bot_id(&self, id: u64) {
        self.bot_id.store(id, Ordering::Release);
    }

    fn bot_id(&self) -> Result<String> {
        let id = self.bot_id.load(Ordering::Acquire);
        if id == 0 {
            return Err(Failure::InvalidEvidence);
        }
        Ok(id.to_string())
    }

    pub fn authorize(
        &self,
        interaction: &Interaction,
        action: TicketAction,
    ) -> std::result::Result<(), TicketError> {
        let guild = interaction.guild_id.map(|id| id.get().to_string());
        let roles: Vec<String> = interaction
            .member
            .as_ref()
            .map(|member| {
                member
                    .roles
                    .iter()
                    .map(|role| role.get().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let permissions = interaction
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .map_or(0, |permissions| permissions.bits());
        authorize(
            guild.as_deref(),
            &self.config.guild_id,
            action,
            &roles,
            permissions,
            &self.config.staff_role_id,
        )
    }

    /// Called only after the shared router and successful ephemeral defer.
    pub async fn execute(&self, interaction: &Interaction, action: TicketAction) -> Result<String> {
        self.authorize(interaction, action)
            .map_err(StoreError::Domain)?;
        let _guard = self.lane.lock().await;
        let user = interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
            .ok_or(Failure::InvalidEvidence)?;
        let actor = user.id.get().to_string();
        match action {
            TicketAction::Open => self.open(&actor, &user.name).await,
            TicketAction::Claim | TicketAction::Close => {
                let channel = interaction
                    .channel
                    .as_ref()
                    .map(|channel| channel.id.get().to_string())
                    .ok_or(Failure::InvalidEvidence)?;
                let ticket = self
                    .store
                    .by_channel(&channel)
                    .await?
                    .ok_or(StoreError::NotFound)?;
                match action {
                    TicketAction::Claim => {
                        self.store.claim(&ticket.id, &actor).await?;
                        Ok("Ticket claimed.".into())
                    }
                    TicketAction::Close => {
                        self.close(&ticket).await?;
                        Ok("Ticket closed; transcript saved for up to 90 days.".into())
                    }
                    TicketAction::Open => unreachable!(),
                }
            }
        }
    }

    async fn open(&self, opener: &str, username: &str) -> Result<String> {
        let bot_id = self.bot_id()?;
        let category = self
            .executor
            .fetch_ticket_channel(&self.config.category_id)
            .await?;
        match category {
            ChannelPresence::Present(doc) => {
                self.validate_channel(&doc, &self.config.category_id, 4)?
            }
            ChannelPresence::Absent => return Err(Failure::InvalidEvidence),
        }
        let id = hex::encode(rand::random::<[u8; 16]>());
        let ticket = match self
            .store
            .reserve(
                &id,
                opener,
                now_millis_for_test(),
                self.config.cooldown_seconds,
            )
            .await?
        {
            OpenResult::Created(ticket) => ticket,
            OpenResult::Refused(OpenDecision::Existing { channel_id }) => {
                return Ok(match channel_id {
                    Some(channel) => format!("You already have a ticket: <#{channel}>."),
                    None => "Your ticket is still being created or recovered.".into(),
                })
            }
            OpenResult::Refused(_) => {
                return Ok("Please wait before opening another ticket.".into())
            }
        };
        let channel = match self
            .executor
            .create_ticket_channel(&TicketChannelRequest {
                guild_id: &self.config.guild_id,
                category_id: &self.config.category_id,
                staff_role_id: &self.config.staff_role_id,
                bot_id: &bot_id,
                opener_id: opener,
                username,
                reservation_id: &ticket.id,
            })
            .await
        {
            Ok(channel) => channel,
            Err(error) => {
                // A single-attempt rejection proves no channel was created;
                // timeout/transport/malformed successes keep the reservation.
                if error.is_safe_pre_mutation() {
                    self.store.abandon_creating(&ticket.id).await?;
                }
                return Err(error.into());
            }
        };
        // If this write fails, leave creating: recovery finds the exact topic.
        self.store.record_channel(&ticket.id, &channel).await?;
        self.store.activate(&ticket.id, &channel).await?;
        if let Err(error) = self
            .executor
            .post_ticket_message(&channel, TicketMessage::Controls { opener_id: opener })
            .await
        {
            self.store
                .queue_open_rollback(&ticket.id, now_millis_for_test())
                .await?;
            self.cleanup(&ticket.id, &channel).await?;
            return Err(error.into());
        }
        Ok(format!("Your ticket is ready: <#{channel}>."))
    }

    async fn close(&self, ticket: &Ticket) -> Result<()> {
        let ticket = self
            .store
            .begin_close(&ticket.id, now_millis_for_test())
            .await?;
        let started_at = ticket.closing_started_at.ok_or(Failure::InvalidEvidence)?;
        let channel = ticket
            .channel_id
            .as_deref()
            .ok_or(Failure::InvalidEvidence)?;
        let doc = self.channel_document(channel).await?;
        self.set_writes(&doc, channel, &ticket.opener_id, false)
            .await?;
        // Discord silently returns [] without READ_MESSAGE_HISTORY. Require
        // the bot's explicit ticket overwrite before treating [] as complete.
        self.require_history_access(&doc)?;
        let snapshot = self.capture_history(channel).await?;
        self.store
            .save_transcript(&ticket.id, started_at, now_millis_for_test(), snapshot)
            .await?;
        // The store call above returns only after INSERT + cleanup state COMMIT.
        self.cleanup(&ticket.id, channel).await
    }

    async fn channel_document(&self, channel: &str) -> Result<Value> {
        match self.executor.fetch_ticket_channel(channel).await? {
            ChannelPresence::Present(doc) => {
                self.validate_channel(&doc, channel, 0)?;
                Ok(doc)
            }
            ChannelPresence::Absent => Err(Failure::InvalidEvidence),
        }
    }

    fn validate_channel(&self, doc: &Value, id: &str, kind: u64) -> Result<()> {
        if doc["guild_id"].as_str() != Some(&self.config.guild_id)
            || doc["id"].as_str() != Some(id)
            || doc["type"].as_u64() != Some(kind)
        {
            return Err(Failure::InvalidEvidence);
        }
        Ok(())
    }

    fn require_history_access(&self, doc: &Value) -> Result<()> {
        let (allow, deny) = member_overwrite(doc, &self.bot_id()?)?;
        let required = (Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY).bits();
        if allow & required != required || deny & required != 0 {
            return Err(Failure::InvalidEvidence);
        }
        Ok(())
    }

    async fn set_writes(
        &self,
        doc: &Value,
        channel: &str,
        opener: &str,
        enabled: bool,
    ) -> Result<()> {
        let (allow, deny) = member_overwrite(doc, opener)?;
        self.executor
            .set_ticket_opener_writes(channel, opener, allow, deny, enabled)
            .await?;
        Ok(())
    }

    pub(crate) async fn capture_history(&self, channel: &str) -> Result<TranscriptSnapshot> {
        let mut before: Option<String> = None;
        let mut seen = HashSet::new();
        let mut messages = Vec::new();
        loop {
            let page = self
                .executor
                .fetch_channel_messages(channel, before.as_deref(), 100)
                .await?;
            if page.len() > 100 {
                return Err(Failure::InvalidEvidence);
            }
            let count = page.len();
            let mut floor = before.as_deref().map(valid_id).transpose()?;
            for message in page {
                let id = valid_id(message["id"].as_str().ok_or(Failure::InvalidEvidence)?)?;
                if !seen.insert(id)
                    || before
                        .as_deref()
                        .map(valid_id)
                        .transpose()?
                        .is_some_and(|cursor| id >= cursor)
                {
                    return Err(Failure::InvalidEvidence);
                }
                floor = Some(floor.map_or(id, |cursor| cursor.min(id)));
                let author = message["author"]["username"]
                    .as_str()
                    .ok_or(Failure::InvalidEvidence)?;
                let discriminator = message["author"]
                    .get("discriminator")
                    .and_then(Value::as_str);
                let author_tag = match discriminator {
                    Some(value) if value != "0" => format!("{author}#{value}"),
                    _ => author.to_owned(),
                };
                let attachment_urls = message["attachments"]
                    .as_array()
                    .ok_or(Failure::InvalidEvidence)?
                    .iter()
                    .map(|attachment| {
                        attachment["url"]
                            .as_str()
                            .map(str::to_owned)
                            .ok_or(Failure::InvalidEvidence)
                    })
                    .collect::<Result<Vec<_>>>()?;
                messages.push(TranscriptMessage {
                    created_at: message["timestamp"]
                        .as_str()
                        .and_then(parse_iso_millis)
                        .ok_or(Failure::InvalidEvidence)?,
                    author_tag,
                    content: message["content"]
                        .as_str()
                        .ok_or(Failure::InvalidEvidence)?
                        .to_owned(),
                    attachment_urls,
                });
            }
            if count < 100 {
                break;
            }
            before = Some(floor.ok_or(Failure::InvalidEvidence)?.to_string());
        }
        Ok(format_transcript(messages))
    }

    async fn cleanup(&self, id: &str, channel: &str) -> Result<()> {
        match self.executor.fetch_ticket_channel(channel).await? {
            ChannelPresence::Present(doc) => {
                self.validate_channel(&doc, channel, 0)?;
                self.executor.delete_ticket_channel(channel).await?;
            }
            ChannelPresence::Absent => {}
        }
        self.store.finish_cleanup(id, now_millis_for_test()).await?;
        Ok(())
    }

    pub async fn recover(&self) -> std::result::Result<(), ErrorClass> {
        let Ok(_guard) = self.lane.try_lock() else {
            return Ok(());
        };
        // Before READY there is no trustworthy own-author id. Do no REST work.
        if self.bot_id.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        let tickets = self
            .store
            .recoverable()
            .await
            .map_err(|_| ErrorClass::Database)?;
        let mut failed = None;
        for ticket in tickets {
            if let Err(error) = self.recover_ticket(&ticket).await {
                failed = Some(error.class());
                tracing::warn!(error_class = ?error.class(), "ticket recovery deferred");
            }
        }
        // Panel ensure still runs when one ticket is inaccessible.
        if let Err(error) = self.ensure_panel().await {
            failed = Some(error.class());
        }
        failed.map_or(Ok(()), Err)
    }

    async fn recover_ticket(&self, ticket: &Ticket) -> Result<()> {
        let saved = self.store.transcript_exists(&ticket.id).await?;
        match recovery_action(ticket, now_millis_for_test(), saved) {
            RecoveryAction::None => {}
            RecoveryAction::ReattachOpenControls { channel_id } => {
                self.ensure_controls(ticket, &channel_id).await?
            }
            RecoveryAction::FindCreatingChannel { topic } => {
                let channels = self
                    .executor
                    .fetch_ticket_guild_channels(&self.config.guild_id)
                    .await?;
                let mut matches = Vec::new();
                for doc in channels {
                    // Validate the complete listing before inferring absence.
                    let id = doc["id"].as_str().ok_or(Failure::InvalidEvidence)?;
                    valid_id(id)?;
                    if doc["type"].as_u64().is_none() {
                        return Err(Failure::InvalidEvidence);
                    }
                    if doc
                        .get("guild_id")
                        .is_some_and(|guild| guild.as_str() != Some(&self.config.guild_id))
                    {
                        return Err(Failure::InvalidEvidence);
                    }
                    if doc["topic"].as_str() == Some(&topic) {
                        if doc["type"].as_u64() != Some(0) {
                            return Err(Failure::InvalidEvidence);
                        }
                        matches.push(id.to_owned());
                    }
                }
                match matches.as_slice() {
                    [] => {
                        self.store.abandon_creating(&ticket.id).await?;
                    }
                    [channel] => {
                        self.store.record_channel(&ticket.id, channel).await?;
                        self.store
                            .queue_open_rollback(&ticket.id, now_millis_for_test())
                            .await?;
                        self.cleanup(&ticket.id, channel).await?;
                    }
                    _ => return Err(Failure::InvalidEvidence),
                }
            }
            RecoveryAction::DeleteInterruptedCreate { channel_id } => {
                self.store
                    .queue_open_rollback(&ticket.id, now_millis_for_test())
                    .await?;
                self.cleanup(&ticket.id, &channel_id).await?;
            }
            RecoveryAction::RetryCleanup { channel_id } => {
                self.cleanup(&ticket.id, &channel_id).await?
            }
            RecoveryAction::RecoverSavedClose { started_at } => {
                let ticket = self
                    .store
                    .recover_saved_close(&ticket.id, started_at)
                    .await?;
                self.cleanup(
                    &ticket.id,
                    ticket
                        .channel_id
                        .as_deref()
                        .ok_or(Failure::InvalidEvidence)?,
                )
                .await?;
            }
            RecoveryAction::RestoreOpenerThenReopen {
                channel_id,
                started_at,
            } => match self.executor.fetch_ticket_channel(&channel_id).await? {
                ChannelPresence::Absent => {
                    self.store
                        .abandon_unsaved_close(&ticket.id, started_at, now_millis_for_test())
                        .await?;
                }
                ChannelPresence::Present(doc) => {
                    self.validate_channel(&doc, &channel_id, 0)?;
                    self.set_writes(&doc, &channel_id, &ticket.opener_id, true)
                        .await?;
                    self.store
                        .reopen_interrupted(&ticket.id, started_at)
                        .await?;
                }
            },
        }
        Ok(())
    }

    async fn ensure_controls(&self, ticket: &Ticket, channel: &str) -> Result<()> {
        self.channel_document(channel).await?;
        let messages = self
            .executor
            .fetch_channel_messages(channel, None, 50)
            .await?;
        let panels = panels(messages)?;
        let bot = self.bot_id()?;
        if !panels.iter().any(|message| {
            message.author_id == bot
                && message.custom_ids.iter().any(|id| id == TICKET_CLAIM_ID)
                && message.custom_ids.iter().any(|id| id == TICKET_CLOSE_ID)
        }) {
            self.executor
                .post_ticket_message(
                    channel,
                    TicketMessage::Controls {
                        opener_id: &ticket.opener_id,
                    },
                )
                .await?;
        }
        Ok(())
    }

    async fn ensure_panel(&self) -> Result<()> {
        self.channel_document(&self.config.panel_channel_id).await?;
        let messages = self
            .executor
            .fetch_channel_messages(&self.config.panel_channel_id, None, 50)
            .await?;
        if panel_needed(&self.bot_id()?, &panels(messages)?) {
            self.executor
                .post_ticket_message(&self.config.panel_channel_id, TicketMessage::Panel)
                .await?;
        }
        Ok(())
    }

    pub async fn purge(&self) -> std::result::Result<(), ErrorClass> {
        let Ok(_guard) = self.purge_lane.try_lock() else {
            return Ok(());
        };
        self.store
            .purge_expired(now_millis_for_test())
            .await
            .map(|_| ())
            .map_err(|_| ErrorClass::Database)
    }
}

fn valid_id(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0 && id.to_string() == value)
        .ok_or(Failure::InvalidEvidence)
}

fn member_overwrite(doc: &Value, member: &str) -> Result<(u64, u64)> {
    let rows = doc["permission_overwrites"]
        .as_array()
        .ok_or(Failure::InvalidEvidence)?;
    let row = rows
        .iter()
        .find(|row| row["id"].as_str() == Some(member) && row["type"].as_u64() == Some(1))
        .ok_or(Failure::InvalidEvidence)?;
    let bits = |key| {
        row[key]
            .as_str()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(Failure::InvalidEvidence)
    };
    Ok((bits("allow")?, bits("deny")?))
}

fn panels(messages: Vec<Value>) -> Result<Vec<PanelMessage>> {
    messages
        .into_iter()
        .map(|message| {
            let author_id = message["author"]["id"]
                .as_str()
                .ok_or(Failure::InvalidEvidence)?
                .to_owned();
            let mut custom_ids = Vec::new();
            let rows = match message.get("components") {
                None => &[][..],
                Some(value) => value.as_array().ok_or(Failure::InvalidEvidence)?.as_slice(),
            };
            for row in rows {
                for button in row["components"]
                    .as_array()
                    .ok_or(Failure::InvalidEvidence)?
                {
                    if let Some(id) = button["custom_id"].as_str() {
                        custom_ids.push(id.to_owned());
                    }
                }
            }
            Ok(PanelMessage {
                author_id,
                custom_ids,
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "ticket_runtime_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ticket_timer_tests.rs"]
mod timer_tests;

#[cfg(test)]
#[path = "ticket_acceptance_tests.rs"]
mod acceptance_tests;
