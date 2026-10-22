#![cfg(test)]
//! Protected acceptance: `event_voice_channels` table + presence spans.
//!
//! PROTECTED FILE: the paired Builder implements the migration, the claim
//! path, the presence writer and the reconcile rules without editing this
//! file. The only wiring outside it is the `#[path] mod
//! event_voice_channels_acceptance;` line in `voice_rooms_tests.rs`.
//!
//! Contract under test (spec: event voice channels, card 1 + R9/R19/R20):
//!
//! - Double `INSERT` on `(event_id, instance_starts_at)` yields a single
//!   `creating` winner via `ON CONFLICT DO NOTHING`; the loser never calls
//!   Discord (claim before any Discord call).
//! - Join/move-in through the real voice sink writes a presence row
//!   immediately (no flush wait); leave sets `left_at`; rejoins add spans
//!   that sum per instance.
//! - Stale `creating` rows (>5 min, no channel) reconcile via a Discord GET:
//!   bind the found channel, else clear the lease for retry.
//! - A channel id that 404s is marked `orphaned` with exactly one retry, and
//!   the card-1 reconcile never deletes (an occupied room is kept, never
//!   deleted).
//! - `starts_at`/`ends_at` (and the span stamps) are `timestamptz` (R19);
//!   instances key on `(event_id, instance_starts_at)` (R20).
//!
//! Shape of the pins: schema, constraint, type and nullability pins are
//! plain SQL against the real cutover migrator, so they fail on the
//! baseline with "relation does not exist" and pass once the Builder's
//! migration lands. The presence flow runs through the real
//! `VoiceRuntime` sink with `PgRoomStore`, so only the Builder's writer
//! hook can turn it green. The claim/reconcile/orphan procedures are
//! executable protocol pins: the helpers below encode the REQUIRED
//! decision order (claim-then-Discord, GET-then-bind-or-clear,
//! 404-then-orphan-plus-one-retry, occupied-then-keep) against scripted
//! Discord doubles, and the Builder's production functions must implement
//! the same procedures with their own unit tests.
//!
//! Discovery, not dictation, where the cards are silent: the lifecycle
//! column may be named `status` or `state`, and the row-age column may be
//! any of `created_at`/`claimed_at`/`leased_at`/`updated_at` (repo
//! convention is `created_at`). Values (`creating`, `orphaned`), key
//! membership, types and nullability are pinned exactly.

use super::*;
use serde_json::json;
use sqlx::PgPool;
use sqlx::Row;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use twilight_model::gateway::payload::incoming::{GuildCreate, Ready, VoiceStateUpdate};
use two_bot_cutover::voice_rooms::PgRoomStore;
use two_bot_discord::MemPipeline;
use two_bot_testsupport::TestDatabase;

const EVENT_ID: &str = "9001";
const EVENT_ID_B: &str = "9002";
const INSTANCE: &str = "2026-11-01T20:00:00+00:00";
const INSTANCE_B: &str = "2026-11-08T20:00:00+00:00";
const ENDS_AT: &str = "2026-11-01T22:00:00+00:00";
const EVENT_CHANNEL: u64 = 9501;
const OTHER_CHANNEL: u64 = 9502;
const MEMBER_A: u64 = 3001;
const MEMBER_B: u64 = 3002;

/// Discovered schema facts: the cards pin values and key membership, while
/// the lifecycle/age column names accept the documented alternatives.
struct EvSchema {
    lifecycle: String,
    age_columns: Vec<String>,
}

fn ev_database_url() -> Option<String> {
    std::env::var("TWO_TEST_DATABASE_URL").ok()
}

async fn ev_db() -> Option<TestDatabase> {
    let url = ev_database_url()?;
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("disposable fixture database with the cutover migration set"),
    )
}

async fn ev_exec(pool: &PgPool, sql: String) {
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await
        .expect("acceptance SQL executes");
}

async fn ev_count(pool: &PgPool, sql: String) -> i64 {
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_one(pool)
        .await
        .expect("count query returns one row")
        .try_get::<i64, _>(0)
        .expect("count decodes as i64")
}

async fn ev_optional_text(pool: &PgPool, sql: String) -> Option<String> {
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_optional(pool)
        .await
        .expect("optional text query runs")
        .and_then(|row| row.try_get::<Option<String>, _>(0).expect("text decodes"))
}

async fn ev_sum_seconds(pool: &PgPool, sql: String) -> f64 {
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_one(pool)
        .await
        .expect("sum query returns one row")
        .try_get::<f64, _>(0)
        .expect("sum decodes as float8")
}

async fn ev_columns(pool: &PgPool, table: &str) -> HashMap<String, (String, String)> {
    // `table` is always one of this file's own constants, so inline
    // interpolation is safe and sidesteps text-vs-name bind typing.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT column_name, data_type, is_nullable FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = '{table}' ORDER BY 1"
    )))
    .fetch_all(pool)
    .await
    .expect("information_schema is readable");
    rows.into_iter()
        .map(|row| {
            let name: String = row.try_get(0).expect("column name decodes");
            let data_type: String = row.try_get(1).expect("data type decodes");
            let nullable: String = row.try_get(2).expect("nullability decodes");
            (name, (data_type, nullable))
        })
        .collect()
}

/// Fail fast with the Builder-migration reason when the card-1 tables are
/// absent, so every behavioral test below reports the same clear RED.
async fn ev_assert_schema(pool: &PgPool) -> EvSchema {
    let channels = ev_columns(pool, "event_voice_channels").await;
    for column in [
        "event_id",
        "instance_starts_at",
        "starts_at",
        "ends_at",
        "owner_tick_lease",
        "voice_channel_id",
    ] {
        assert!(
            channels.contains_key(column),
            "event_voice_channels.{column} is missing; the Builder migration has not landed"
        );
    }
    let lifecycle = if channels.contains_key("status") {
        "status".to_owned()
    } else {
        assert!(
            channels.contains_key("state"),
            "event_voice_channels needs a lifecycle column named status or state"
        );
        "state".to_owned()
    };
    let age_columns: Vec<String> = ["created_at", "claimed_at", "leased_at", "updated_at"]
        .into_iter()
        .filter(|name| channels.contains_key(*name))
        .map(str::to_owned)
        .collect();
    assert!(
        !age_columns.is_empty(),
        "event_voice_channels needs a row-age timestamp (created_at, claimed_at, \
         leased_at or updated_at) so stale leases are selectable"
    );
    let presence = ev_columns(pool, "event_voice_presence").await;
    for column in ["event_id", "instance", "user_id", "joined_at", "left_at"] {
        assert!(
            presence.contains_key(column),
            "event_voice_presence.{column} is missing; the Builder migration has not landed"
        );
    }
    EvSchema {
        lifecycle,
        age_columns,
    }
}

/// Column/value lists assigning every discovered age column, so seeds work
/// whether or not the Builder gave them defaults.
fn ev_age_columns(schema: &EvSchema) -> String {
    schema.age_columns.join(", ")
}

fn ev_age_values(schema: &EvSchema, value_sql: &str) -> String {
    schema
        .age_columns
        .iter()
        .map(|_| value_sql.to_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Seed parameters for one `event_voice_channels` row.
struct EvSeed<'a> {
    event_id: &'a str,
    instance: &'a str,
    lifecycle_value: &'a str,
    lease: Option<&'a str>,
    channel: Option<u64>,
    age_sql: &'a str,
}

async fn ev_seed_channel_row(pool: &PgPool, schema: &EvSchema, seed: &EvSeed<'_>) {
    let lease_sql = seed
        .lease
        .map_or_else(|| "NULL".to_owned(), |value| format!("'{value}'"));
    let channel_sql = seed
        .channel
        .map_or_else(|| "NULL".to_owned(), |id| format!("'{id}'"));
    let age_columns = ev_age_columns(schema);
    let age_values = ev_age_values(schema, seed.age_sql);
    let lifecycle = schema.lifecycle.as_str();
    let event_id = seed.event_id;
    let instance = seed.instance;
    let lifecycle_value = seed.lifecycle_value;
    ev_exec(
        pool,
        format!(
            "INSERT INTO event_voice_channels (event_id, instance_starts_at, starts_at, \
             ends_at, {lifecycle}, owner_tick_lease, voice_channel_id, {age_columns}) \
             VALUES ('{event_id}', '{instance}', '{instance}', '{ENDS_AT}', \
             '{lifecycle_value}', {lease_sql}, {channel_sql}, {age_values})",
        ),
    )
    .await;
}

/// The REQUIRED claim procedure: `INSERT ... ON CONFLICT DO NOTHING` first,
/// and only the insert winner touches Discord. Returns true for the winner.
/// The Builder's create path must follow this exact order.
async fn ev_claim(
    pool: &PgPool,
    schema: &EvSchema,
    discord: &FakeDiscord,
    event_id: &str,
    instance: &str,
    lease: &str,
) -> bool {
    let age_columns = ev_age_columns(schema);
    let age_values = ev_age_values(schema, "NOW()");
    let lifecycle = schema.lifecycle.as_str();
    let won = sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO event_voice_channels (event_id, instance_starts_at, starts_at, \
         ends_at, {lifecycle}, owner_tick_lease, voice_channel_id, {age_columns}) \
         VALUES ('{event_id}', '{instance}', '{instance}', '{ENDS_AT}', 'creating', \
         '{lease}', NULL, {age_values}) \
         ON CONFLICT DO NOTHING RETURNING owner_tick_lease",
    )))
    .fetch_optional(pool)
    .await
    .expect("claim insert runs")
    .is_some();
    if won {
        let channel = discord.create_channel(event_id);
        ev_exec(
            pool,
            format!(
                "UPDATE event_voice_channels SET voice_channel_id = '{channel}' \
                 WHERE event_id = '{event_id}' AND instance_starts_at = '{instance}'"
            ),
        )
        .await;
    }
    won
}

/// Scripted Discord channel-create double: records every call.
struct FakeDiscord {
    calls: Mutex<Vec<String>>,
    next_channel: AtomicU64,
}

impl FakeDiscord {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            next_channel: AtomicU64::new(9601),
        }
    }

    fn create_channel(&self, event_id: &str) -> u64 {
        self.calls
            .lock()
            .expect("fake discord lock")
            .push(format!("create {event_id}"));
        self.next_channel.fetch_add(1, Ordering::SeqCst)
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("fake discord lock").clone()
    }
}

/// Outcome of the Discord GET that reconciles one stale `creating` row.
#[derive(Clone, Copy)]
enum ProbeOutcome {
    FoundChannel(u64),
    NoChannel,
}

/// Scripted Discord GET double for the stale-lease reconcile.
struct FakeProbe {
    script: HashMap<String, ProbeOutcome>,
    gets: Mutex<Vec<String>>,
}

impl FakeProbe {
    fn new(script: &[(&str, ProbeOutcome)]) -> Self {
        Self {
            script: script
                .iter()
                .map(|(key, outcome)| ((*key).to_owned(), *outcome))
                .collect(),
            gets: Mutex::new(Vec::new()),
        }
    }

    fn get_event_channel(&self, event_id: &str) -> ProbeOutcome {
        self.gets
            .lock()
            .expect("fake probe lock")
            .push(event_id.to_owned());
        *self
            .script
            .get(event_id)
            .expect("reconcile only probes scripted events")
    }

    fn gets(&self) -> Vec<String> {
        self.gets.lock().expect("fake probe lock").clone()
    }
}

/// The REQUIRED stale-lease procedure: rows in `creating` older than 5
/// minutes with no channel are verified via exactly one Discord GET each;
/// a found channel binds, a missing one clears the lease for retry. Fresh
/// rows are never probed. The Builder's reconcile must do the same.
async fn ev_reconcile_stale(pool: &PgPool, schema: &EvSchema, probe: &FakeProbe) {
    let age = schema.age_columns[0].as_str();
    let lifecycle = schema.lifecycle.as_str();
    let stale: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT event_id::TEXT FROM event_voice_channels \
         WHERE {lifecycle} = 'creating' AND voice_channel_id IS NULL \
         AND {age} < NOW() - INTERVAL '5 minutes' ORDER BY 1",
    )))
    .fetch_all(pool)
    .await
    .expect("stale select runs")
    .into_iter()
    .map(|row| row.try_get::<String, _>(0).expect("event id decodes"))
    .collect();
    for event_id in stale {
        match probe.get_event_channel(&event_id) {
            ProbeOutcome::FoundChannel(channel) => {
                ev_exec(
                    pool,
                    format!(
                        "UPDATE event_voice_channels SET voice_channel_id = '{channel}' \
                         WHERE event_id = '{event_id}'"
                    ),
                )
                .await;
            }
            ProbeOutcome::NoChannel => {
                ev_exec(
                    pool,
                    format!(
                        "UPDATE event_voice_channels SET owner_tick_lease = NULL \
                         WHERE event_id = '{event_id}'"
                    ),
                )
                .await;
            }
        }
    }
}

/// Outcome of a Discord channel GET in the orphan reconcile.
#[derive(Clone, Copy)]
enum ChannelOutcome {
    Exists { occupied: bool },
    Missing404,
}

/// Scripted Discord double for the orphan reconcile: records GETs and the
/// single retry GET per 404. It deliberately offers no delete: the card-1
/// reconcile binds, clears and marks only, never deletes.
struct FakeChannel {
    script: HashMap<u64, ChannelOutcome>,
    gets: Mutex<Vec<u64>>,
    retries: Mutex<Vec<u64>>,
}

impl FakeChannel {
    fn new(script: &[(u64, ChannelOutcome)]) -> Self {
        Self {
            script: script.iter().copied().collect(),
            gets: Mutex::new(Vec::new()),
            retries: Mutex::new(Vec::new()),
        }
    }

    fn get_channel(&self, channel: u64) -> ChannelOutcome {
        self.gets.lock().expect("fake channel lock").push(channel);
        *self
            .script
            .get(&channel)
            .expect("reconcile only probes scripted channels")
    }

    fn retry_get(&self, channel: u64) {
        self.retries
            .lock()
            .expect("fake channel lock")
            .push(channel);
    }

    fn gets(&self) -> Vec<u64> {
        self.gets.lock().expect("fake channel lock").clone()
    }

    fn retries(&self) -> Vec<u64> {
        self.retries.lock().expect("fake channel lock").clone()
    }
}

/// The REQUIRED orphan procedure: a channel id that 404s is marked
/// `orphaned` with exactly one retry GET; live channels are kept untouched
/// (an occupied room is never deleted). The procedure contains no DELETE
/// statement at all, Discord or SQL: the Builder's reconcile must keep
/// that bound durably.
async fn ev_reconcile_channels(pool: &PgPool, schema: &EvSchema, discord: &FakeChannel) {
    let lifecycle = schema.lifecycle.as_str();
    let rows: Vec<(String, u64)> = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT event_id::TEXT, voice_channel_id::TEXT::BIGINT \
         FROM event_voice_channels \
         WHERE voice_channel_id IS NOT NULL AND {lifecycle} <> 'orphaned' ORDER BY 2",
    )))
    .fetch_all(pool)
    .await
    .expect("channel-status select runs")
    .into_iter()
    .map(|row| {
        (
            row.try_get::<String, _>(0).expect("event id decodes"),
            row.try_get::<i64, _>(1).expect("channel id decodes") as u64,
        )
    })
    .collect();
    for (event_id, channel) in rows {
        match discord.get_channel(channel) {
            ChannelOutcome::Missing404 => {
                ev_exec(
                    pool,
                    format!(
                        "UPDATE event_voice_channels SET {lifecycle} = 'orphaned' \
                         WHERE event_id = '{event_id}'",
                    ),
                )
                .await;
                discord.retry_get(channel);
            }
            ChannelOutcome::Exists { occupied } => {
                // Card-1 reconcile never deletes: occupied rooms are kept, and
                // this procedure contains no DELETE for empty rooms either.
                let _ = occupied;
            }
        }
    }
}

/// `RoomWrites` fake that records Discord mutating calls without touching
/// the network; voice-state observation must stay read-only.
#[derive(Clone)]
struct RecordHttp {
    calls: Trace,
    next_id: std::sync::Arc<AtomicU64>,
}

impl RecordHttp {
    fn new() -> Self {
        Self {
            calls: Trace::default(),
            next_id: std::sync::Arc::new(AtomicU64::new(6000)),
        }
    }

    fn record(&self, entry: String) {
        self.calls.lock().expect("record lock").push(entry);
    }
}

impl RoomWrites for RecordHttp {
    async fn create(
        &self,
        guild: u64,
        name: &str,
        _attributes: &RoomChannelAttributes,
        _guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        self.record(format!("create {guild} {name}"));
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        Ok(channel(id, 2, None))
    }

    async fn move_member(
        &self,
        guild: u64,
        member: u64,
        channel: u64,
        _guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.record(format!("move {guild} {member} {channel}"));
        Ok(())
    }

    async fn disconnect(
        &self,
        guild: u64,
        member: u64,
        _guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.record(format!("disconnect {guild} {member}"));
        Ok(())
    }

    async fn deny_connect(
        &self,
        channel: u64,
        member: u64,
        _guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.record(format!("deny {channel} {member}"));
        Ok(())
    }

    async fn delete(&self, channel: u64, _guard: WriteGuard) -> Result<(), RoomHttpError> {
        self.record(format!("delete {channel}"));
        Ok(())
    }

    async fn rename(&self, channel: u64, name: &str) -> Result<(), RoomHttpError> {
        self.record(format!("rename {channel} {name}"));
        Ok(())
    }

    async fn set_user_limit(
        &self,
        channel: u64,
        user_limit: u32,
        _guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.record(format!("limit {channel} {user_limit}"));
        Ok(())
    }

    async fn download_attachment(
        &self,
        url: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, RoomHttpError> {
        self.record(format!("download {url} {max_bytes}"));
        Ok(Vec::new())
    }

    async fn create_companion(
        &self,
        _plan: &TextChannelPlan,
        bot_id: u64,
        _guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        self.record(format!("companion {bot_id}"));
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        Ok(channel(id, 0, None))
    }

    async fn grant_companion_view(
        &self,
        text_channel_id: u64,
        member_id: u64,
        _guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.record(format!("grant {text_channel_id} {member_id}"));
        Ok(())
    }

    async fn revoke_companion_view(
        &self,
        text_channel_id: u64,
        member_id: u64,
        _guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.record(format!("revoke {text_channel_id} {member_id}"));
        Ok(())
    }
}

type EvRuntime = VoiceRuntime<PgRoomStore, RecordHttp>;

fn ev_ready_packet() -> Event {
    Event::Ready(serde_json::from_value::<Ready>(json!({
        "v": 10, "user": {"id": "999", "username": "mock-bot", "discriminator": "0", "bot": true, "mfa_enabled": false},
        "session_id": "session", "resume_gateway_url": "wss://gateway.discord.gg",
        "guilds": [{"id": "100", "unavailable": true}], "application": {"id": "1111", "flags": 0}
    })).expect("ready fixture parses"))
}

fn ev_guild_packet(extra: &[u64]) -> Event {
    let mut guild: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/voice_guild.json"))
            .expect("guild fixture parses");
    guild["roles"][0]["permissions"] = json!(permissions().bits().to_string());
    for id in extra {
        guild["channels"]
            .as_array_mut()
            .expect("guild fixture has channels")
            .push(
                serde_json::to_value(channel(*id, 2, Some(CATEGORY))).expect("channel serializes"),
            );
    }
    Event::GuildCreate(Box::new(GuildCreate::Available(
        serde_json::from_value(guild).expect("guild fixture types parse"),
    )))
}

fn ev_voice_packet(member: u64, channel: Option<u64>) -> Event {
    Event::VoiceStateUpdate(Box::new(
        serde_json::from_value::<VoiceStateUpdate>(json!({
            "guild_id": GUILD.to_string(), "user_id": member.to_string(),
            "channel_id": channel.map(|id| id.to_string()), "session_id": "voice",
            "deaf": false, "mute": false, "self_deaf": false, "self_mute": false,
            "self_video": false, "suppress": false,
            "member": {"user": {"id": member.to_string(), "username": "human", "discriminator": "0"},
                "roles": [], "deaf": false, "mute": false, "flags": 0, "joined_at": NOW}
        }))
        .expect("voice-state fixture parses"),
    ))
}

fn ev_feed(runtime: &EvRuntime, pipeline: &MemPipeline, event: Event) {
    pipeline.handle(&event);
    runtime.handle(&event, pipeline.cache());
}

fn ev_bootstrap(runtime: &EvRuntime, pipeline: &MemPipeline, extra: &[u64]) {
    ev_feed(runtime, pipeline, ev_ready_packet());
    ev_feed(runtime, pipeline, ev_guild_packet(extra));
}

fn ev_presence_sql(user_id: u64) -> String {
    format!(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = '{EVENT_ID}' AND user_id = '{user_id}'"
    )
}

fn ev_open_sql(user_id: u64) -> String {
    format!(
        "SELECT COUNT(*) FROM event_voice_presence \
         WHERE event_id = '{EVENT_ID}' AND user_id = '{user_id}' \
         AND left_at IS NULL"
    )
}

async fn ev_presence_count(pool: &PgPool, user_id: u64) -> i64 {
    ev_count(pool, ev_presence_sql(user_id)).await
}

async fn ev_open_count(pool: &PgPool, user_id: u64) -> i64 {
    ev_count(pool, ev_open_sql(user_id)).await
}

/// Poll a count query until it reads `want` or the deadline passes; the
/// writer runs on the guild actor, so one poll loop covers any Builder
/// hook point. Never calls any flush: rows must appear on the voice
/// packets alone (R9).
async fn ev_poll_count(pool: &PgPool, label: &str, sql: &str, want: i64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if ev_count(pool, sql.to_owned()).await == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{label} did not reach {want} within 10s of the voice packet (rows must \
             be written on join/move/leave, never after a flush wait)"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn event_voice_double_insert_yields_single_creating_winner() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let schema = ev_assert_schema(pool).await;

    let age_columns = ev_age_columns(&schema);
    let age_values = ev_age_values(&schema, "NOW()");
    let lifecycle = schema.lifecycle.as_str();
    let claim_sql = |lease: &str| {
        format!(
            "INSERT INTO event_voice_channels (event_id, instance_starts_at, starts_at, \
             ends_at, {lifecycle}, owner_tick_lease, voice_channel_id, {age_columns}) \
             VALUES ('{EVENT_ID}', '{INSTANCE}', '{INSTANCE}', '{ENDS_AT}', 'creating', \
             '{lease}', NULL, {age_values}) \
             ON CONFLICT DO NOTHING RETURNING owner_tick_lease",
        )
    };
    let attempt = |pool: PgPool, sql: String| async move {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_optional(&pool)
            .await
            .expect("claim insert runs")
            .is_some()
    };
    let pool_a = PgPool::clone(pool);
    let pool_b = PgPool::clone(pool);
    let (won_a, won_b) = tokio::join!(
        attempt(pool_a, claim_sql("tick-a")),
        attempt(pool_b, claim_sql("tick-b")),
    );
    assert!(
        won_a ^ won_b,
        "exactly one of the two concurrent claims must win, got tick-a={won_a} tick-b={won_b}"
    );
    let rows = ev_count(
        pool,
        format!(
            "SELECT COUNT(*) FROM event_voice_channels \
             WHERE event_id = '{EVENT_ID}' AND instance_starts_at = '{INSTANCE}'"
        ),
    )
    .await;
    assert_eq!(rows, 1, "double insert must leave a single row");
    let winner = if won_a { "tick-a" } else { "tick-b" };
    let lease = ev_optional_text(
        pool,
        format!(
            "SELECT owner_tick_lease FROM event_voice_channels \
             WHERE event_id = '{EVENT_ID}' AND instance_starts_at = '{INSTANCE}'"
        ),
    )
    .await;
    assert_eq!(lease.as_deref(), Some(winner));
    let uniques = ev_count(
        pool,
        "SELECT COUNT(*) FROM pg_constraint WHERE conrelid = \
         'event_voice_channels'::regclass AND contype = 'u' \
         AND pg_get_constraintdef(oid) LIKE '%event_id%' \
         AND pg_get_constraintdef(oid) LIKE '%instance_starts_at%'"
            .to_owned(),
    )
    .await;
    assert!(
        uniques >= 1,
        "the (event_id, instance_starts_at) unique key is the dedup mechanism"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn event_voice_claim_loser_makes_no_discord_call() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let schema = ev_assert_schema(pool).await;
    let discord = FakeDiscord::new();

    assert!(
        ev_claim(pool, &schema, &discord, EVENT_ID, INSTANCE, "tick-a").await,
        "the first claim wins"
    );
    assert_eq!(discord.calls(), vec!["create 9001".to_owned()]);
    assert!(
        !ev_claim(pool, &schema, &discord, EVENT_ID, INSTANCE, "tick-b").await,
        "the second claim loses"
    );
    assert_eq!(
        discord.calls(),
        vec!["create 9001".to_owned()],
        "the losing claim must not call Discord"
    );
    let rows = ev_count(
        pool,
        format!(
            "SELECT COUNT(*) FROM event_voice_channels \
             WHERE event_id = '{EVENT_ID}' AND instance_starts_at = '{INSTANCE}'"
        ),
    )
    .await;
    assert_eq!(rows, 1);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn event_voice_presence_join_leave_rejoin_flows_through_sink() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let schema = ev_assert_schema(pool).await;
    ev_seed_channel_row(
        pool,
        &schema,
        &EvSeed {
            event_id: EVENT_ID,
            instance: INSTANCE,
            lifecycle_value: "creating",
            lease: Some("seed-1"),
            channel: Some(EVENT_CHANNEL),
            age_sql: "NOW()",
        },
    )
    .await;

    let store = PgRoomStore::new(PgPool::clone(pool));
    let http = RecordHttp::new();
    let runtime = VoiceRuntime::new(
        move || (store.clone(), http.clone()),
        Duration::from_millis(10),
        true,
    )
    .with_empty_grace(Duration::ZERO);
    let pipeline = MemPipeline::for_replay();
    ev_bootstrap(&runtime, &pipeline, &[EVENT_CHANNEL, OTHER_CHANNEL]);

    ev_feed(
        &runtime,
        &pipeline,
        ev_voice_packet(MEMBER_A, Some(EVENT_CHANNEL)),
    );
    ev_poll_count(pool, "join row", &ev_presence_sql(MEMBER_A), 1).await;
    assert_eq!(
        ev_open_count(pool, MEMBER_A).await,
        1,
        "the join row stays open until the leave"
    );

    ev_feed(
        &runtime,
        &pipeline,
        ev_voice_packet(MEMBER_B, Some(OTHER_CHANNEL)),
    );
    ev_feed(
        &runtime,
        &pipeline,
        ev_voice_packet(MEMBER_B, Some(EVENT_CHANNEL)),
    );
    ev_poll_count(pool, "move-in row", &ev_presence_sql(MEMBER_B), 1).await;

    ev_feed(&runtime, &pipeline, ev_voice_packet(MEMBER_A, None));
    ev_poll_count(pool, "leave stamp", &ev_open_sql(MEMBER_A), 0).await;
    assert_eq!(
        ev_presence_count(pool, MEMBER_A).await,
        1,
        "leave stamps left_at on the same row instead of adding one"
    );

    ev_feed(
        &runtime,
        &pipeline,
        ev_voice_packet(MEMBER_A, Some(EVENT_CHANNEL)),
    );
    ev_poll_count(pool, "rejoin row", &ev_presence_sql(MEMBER_A), 2).await;
    assert_eq!(
        ev_open_count(pool, MEMBER_A).await,
        1,
        "rejoin opens a second span while the first stays closed"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn event_voice_presence_rejoin_sums_spans_per_instance() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    ev_assert_schema(pool).await;

    for (joined, left) in [
        ("2026-11-01T20:00:00+00:00", "2026-11-01T20:05:00+00:00"),
        ("2026-11-01T20:10:00+00:00", "2026-11-01T20:25:00+00:00"),
    ] {
        ev_exec(
            pool,
            format!(
                "INSERT INTO event_voice_presence (event_id, instance, user_id, joined_at, \
                 left_at) VALUES ('{EVENT_ID}', '{INSTANCE}', '3009', '{joined}', '{left}')"
            ),
        )
        .await;
    }
    ev_exec(
        pool,
        format!(
            "INSERT INTO event_voice_presence (event_id, instance, user_id, joined_at, \
             left_at) VALUES ('{EVENT_ID}', '{INSTANCE_B}', '3009', \
             '2026-11-08T20:00:00+00:00', '2026-11-08T21:00:00+00:00')"
        ),
    )
    .await;
    let total = ev_sum_seconds(
        pool,
        format!(
            "SELECT COALESCE(SUM(EXTRACT(EPOCH FROM (left_at - joined_at))::float8), 0) \
             FROM event_voice_presence \
             WHERE event_id = '{EVENT_ID}' AND instance = '{INSTANCE}' \
             AND user_id = '3009' AND left_at IS NOT NULL"
        ),
    )
    .await;
    assert!(
        (total - 1200.0).abs() < 0.001,
        "rejoined spans sum per instance (300s + 900s), got {total}"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn event_voice_stale_creating_reconciles_via_get() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let schema = ev_assert_schema(pool).await;
    ev_seed_channel_row(
        pool,
        &schema,
        &EvSeed {
            event_id: EVENT_ID,
            instance: INSTANCE,
            lifecycle_value: "creating",
            lease: Some("tick-stale"),
            channel: None,
            age_sql: "NOW() - INTERVAL '6 minutes'",
        },
    )
    .await;
    ev_seed_channel_row(
        pool,
        &schema,
        &EvSeed {
            event_id: EVENT_ID_B,
            instance: INSTANCE,
            lifecycle_value: "creating",
            lease: Some("tick-stale-b"),
            channel: None,
            age_sql: "NOW() - INTERVAL '6 minutes'",
        },
    )
    .await;
    ev_seed_channel_row(
        pool,
        &schema,
        &EvSeed {
            event_id: "9003",
            instance: INSTANCE,
            lifecycle_value: "creating",
            lease: Some("tick-fresh"),
            channel: None,
            age_sql: "NOW()",
        },
    )
    .await;

    let probe = FakeProbe::new(&[
        (EVENT_ID, ProbeOutcome::FoundChannel(9601)),
        (EVENT_ID_B, ProbeOutcome::NoChannel),
    ]);
    ev_reconcile_stale(pool, &schema, &probe).await;

    let bound = ev_optional_text(
        pool,
        format!(
            "SELECT voice_channel_id::TEXT FROM event_voice_channels WHERE event_id = '{EVENT_ID}'"
        ),
    )
    .await;
    assert_eq!(
        bound.as_deref(),
        Some("9601"),
        "a channel that exists binds to the stale row"
    );
    let surviving = ev_count(
        pool,
        format!("SELECT COUNT(*) FROM event_voice_channels WHERE event_id = '{EVENT_ID_B}'"),
    )
    .await;
    assert_eq!(
        surviving, 1,
        "lease clearing keeps the row for the next tick"
    );
    let cleared = ev_optional_text(
        pool,
        format!(
            "SELECT owner_tick_lease FROM event_voice_channels WHERE event_id = '{EVENT_ID_B}'"
        ),
    )
    .await;
    assert_eq!(
        cleared, None,
        "a missing channel clears the lease so the next tick retries"
    );
    let fresh = ev_optional_text(
        pool,
        "SELECT owner_tick_lease FROM event_voice_channels WHERE event_id = '9003'".to_owned(),
    )
    .await;
    assert_eq!(
        fresh.as_deref(),
        Some("tick-fresh"),
        "a fresh creating row is never probed"
    );
    assert_eq!(
        probe.gets(),
        vec![EVENT_ID.to_owned(), EVENT_ID_B.to_owned()]
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn event_voice_missing_channel_marks_orphaned_with_single_retry_and_never_deletes_occupied() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    let schema = ev_assert_schema(pool).await;
    ev_seed_channel_row(
        pool,
        &schema,
        &EvSeed {
            event_id: EVENT_ID,
            instance: INSTANCE,
            lifecycle_value: "creating",
            lease: Some("tick-a"),
            channel: Some(9701),
            age_sql: "NOW()",
        },
    )
    .await;
    ev_seed_channel_row(
        pool,
        &schema,
        &EvSeed {
            event_id: EVENT_ID_B,
            instance: INSTANCE,
            lifecycle_value: "creating",
            lease: Some("tick-b"),
            channel: Some(9702),
            age_sql: "NOW()",
        },
    )
    .await;
    ev_seed_channel_row(
        pool,
        &schema,
        &EvSeed {
            event_id: "9003",
            instance: INSTANCE,
            lifecycle_value: "creating",
            lease: Some("tick-c"),
            channel: Some(9703),
            age_sql: "NOW()",
        },
    )
    .await;

    let discord = FakeChannel::new(&[
        (9701, ChannelOutcome::Missing404),
        (9702, ChannelOutcome::Exists { occupied: true }),
        (9703, ChannelOutcome::Exists { occupied: false }),
    ]);
    ev_reconcile_channels(pool, &schema, &discord).await;

    let lifecycle = schema.lifecycle.as_str();
    let status = ev_optional_text(
        pool,
        format!("SELECT {lifecycle} FROM event_voice_channels WHERE event_id = '{EVENT_ID}'"),
    )
    .await;
    assert_eq!(
        status.as_deref(),
        Some("orphaned"),
        "a 404 channel id marks the row orphaned"
    );
    assert_eq!(
        discord.retries(),
        vec![9701],
        "the 404 row gets exactly one retry"
    );
    let kept = ev_optional_text(
        pool,
        format!("SELECT {lifecycle} FROM event_voice_channels WHERE event_id = '{EVENT_ID_B}'"),
    )
    .await;
    assert_eq!(
        kept.as_deref(),
        Some("creating"),
        "an occupied room keeps its lifecycle row"
    );
    let kept_lease = ev_optional_text(
        pool,
        format!(
            "SELECT owner_tick_lease FROM event_voice_channels WHERE event_id = '{EVENT_ID_B}'"
        ),
    )
    .await;
    assert_eq!(
        kept_lease.as_deref(),
        Some("tick-b"),
        "an occupied room keeps its lease: reconcile never deletes it"
    );
    let kept_channel = ev_optional_text(
        pool,
        format!(
            "SELECT voice_channel_id::TEXT FROM event_voice_channels \
             WHERE event_id = '{EVENT_ID_B}'"
        ),
    )
    .await;
    assert_eq!(
        kept_channel.as_deref(),
        Some("9702"),
        "an occupied room keeps its channel binding"
    );
    assert_eq!(discord.gets(), vec![9701, 9702, 9703]);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn event_voice_lifecycle_columns_are_timestamptz_and_nullable() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();
    ev_assert_schema(pool).await;

    let channels = ev_columns(pool, "event_voice_channels").await;
    for column in ["starts_at", "ends_at"] {
        let (data_type, _) = channels
            .get(column)
            .unwrap_or_else(|| panic!("event_voice_channels.{column} is missing"));
        assert_eq!(
            data_type, "timestamp with time zone",
            "{column} must be timestamptz (R19)"
        );
    }
    for column in ["owner_tick_lease", "voice_channel_id"] {
        let (_, nullable) = channels
            .get(column)
            .unwrap_or_else(|| panic!("event_voice_channels.{column} is missing"));
        assert_eq!(
            nullable, "YES",
            "{column} must accept NULL (lease clearing / unbound rows)"
        );
    }
    let presence = ev_columns(pool, "event_voice_presence").await;
    for column in ["joined_at", "left_at"] {
        let (data_type, _) = presence
            .get(column)
            .unwrap_or_else(|| panic!("event_voice_presence.{column} is missing"));
        assert_eq!(
            data_type, "timestamp with time zone",
            "{column} must be timestamptz"
        );
    }
    let (_, nullable) = presence
        .get("left_at")
        .expect("event_voice_presence.left_at is missing");
    assert_eq!(nullable, "YES", "left_at stays NULL while a span is open");
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn event_voice_control_migrations_apply_cleanly() {
    let Some(db) = ev_db().await else { return };
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn event_voice_control_non_event_join_writes_no_span() {
    let Some(db) = ev_db().await else { return };
    let pool = db.pool();

    let store = PgRoomStore::new(PgPool::clone(pool));
    let http = RecordHttp::new();
    let runtime = VoiceRuntime::new(
        move || (store.clone(), http.clone()),
        Duration::from_millis(10),
        true,
    )
    .with_empty_grace(Duration::ZERO);
    let pipeline = MemPipeline::for_replay();
    ev_bootstrap(&runtime, &pipeline, &[EVENT_CHANNEL]);
    ev_feed(
        &runtime,
        &pipeline,
        ev_voice_packet(MEMBER, Some(EVENT_CHANNEL)),
    );
    tokio::time::sleep(Duration::from_secs(2)).await;

    let rows = match sqlx::query("SELECT COUNT(*) FROM event_voice_presence WHERE user_id = '300'")
        .fetch_one(pool)
        .await
    {
        Ok(row) => row.try_get::<i64, _>(0).expect("count decodes"),
        Err(error) if error.to_string().contains("does not exist") => 0,
        Err(error) => panic!("presence count failed unexpectedly: {error}"),
    };
    assert_eq!(
        rows, 0,
        "a join with no event mapping must not write a presence span"
    );
    db.close().await.expect("fixture teardown");
}
