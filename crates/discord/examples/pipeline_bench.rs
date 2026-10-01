//! Bounded synthetic pipeline + dispatch-store measurement; disposable DB only.
//! See docs/b1-baseline.md for boundaries and reproduction, not a full-bot soak.

#[allow(dead_code)]
#[path = "../tests/common/mod.rs"]
mod common;

use anyhow::{bail, ensure, Context, Result};
use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing::{Event as TraceEvent, Subscriber};
use tracing_subscriber::{layer::Context as TraceContext, prelude::*, Layer};
use twilight_gateway::{Event, EventTypeFlags};
use twilight_model::id::Id;
use two_bot_core::gateway_funnel::GatewayFunnelBuffer;
use two_bot_core::gateway_session::{DispatchAction, GatewaySession};
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_discord::{ActionExecutor, NoClassification, NoInvites, Pipeline};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 2222;
const EPOCH_MS: i64 = 1_790_812_800_000;

#[derive(Clone, Debug, PartialEq)]
struct Config {
    members: u64,
    channels: u64,
    duration_secs: u64,
    messages_per_sec: u64,
    voice_per_sec: u64,
    rest_every: u64,
}

impl Config {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut config = Self {
            members: 107,
            channels: 10,
            duration_secs: 30,
            messages_per_sec: 20,
            voice_per_sec: 10,
            rest_every: 30,
        };
        let mut args = args;
        while let Some(flag) = args.next() {
            let value: u64 = args
                .next()
                .context("flags require integer values")?
                .parse()?;
            match flag.as_str() {
                "--members" => config.members = value,
                "--channels" => config.channels = value,
                "--duration-secs" => config.duration_secs = value,
                "--messages-per-sec" => config.messages_per_sec = value,
                "--voice-per-sec" => config.voice_per_sec = value,
                "--rest-every" => config.rest_every = value,
                _ => bail!("unknown benchmark flag"),
            }
        }
        ensure!(
            (1..=10_000).contains(&config.members),
            "members must be 1..10000"
        );
        ensure!(
            (2..=1_000).contains(&config.channels),
            "channels must be 2..1000"
        );
        ensure!(
            (1..=120).contains(&config.duration_secs),
            "duration must be 1..120 seconds"
        );
        ensure!(
            config.messages_per_sec <= 1_000 && config.voice_per_sec <= 1_000,
            "rates must be <=1000/s"
        );
        ensure!(
            config.messages_per_sec + config.voice_per_sec > 0,
            "at least one event rate is required"
        );
        ensure!(config.events() <= 100_000, "at most 100000 measured events");
        ensure!(
            config.rest_every > 0
                && config.rest_every <= config.events()
                && config.events() / config.rest_every <= 1_000,
            "REST sampling must be 1..1000 calls"
        );
        Ok(config)
    }

    fn events(&self) -> u64 {
        self.duration_secs * (self.messages_per_sec + self.voice_per_sec)
    }
}

// SQLx emits one diagnostic per completed query, including COMMIT but not
// BEGIN (sqlx-postgres 0.9 queues BEGIN directly). No SQL text is retained.
#[derive(Clone, Default)]
struct Queries {
    statements: Arc<AtomicU64>,
    commits: Arc<AtomicU64>,
}

#[derive(Default)]
struct Summary(bool);

impl Visit for Summary {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "summary" && format!("{value:?}").contains("COMMIT") {
            self.0 = true;
        }
    }
}

impl<S: Subscriber> Layer<S> for Queries {
    fn on_event(&self, event: &TraceEvent<'_>, _: TraceContext<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            self.statements.fetch_add(1, Ordering::Relaxed);
            let mut summary = Summary::default();
            event.record(&mut summary);
            if summary.0 {
                self.commits.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn dispatch(sequence: u64, kind: &str, data: Value) -> Result<Event> {
    let packet = json!({"op":0,"s":sequence,"t":kind,"d":data}).to_string();
    let parsed = twilight_gateway::parse(packet, EventTypeFlags::all())?
        .context("synthetic event not recognized")?;
    Ok(Event::from(parsed))
}

fn user(id: u64) -> Value {
    json!({"id":id.to_string(),"username":"bench-member","discriminator":"0","bot":false})
}

fn gateway_timestamp(at_ms: i64) -> String {
    // Twilight requires Discord's explicit UTC offset, not the core's Z suffix.
    two_bot_core::format_iso_millis(at_ms).replace('Z', "+00:00")
}

fn member(id: u64) -> Value {
    json!({"guild_id":GUILD.to_string(),"user":user(id),"roles":[],
        "joined_at":gateway_timestamp(EPOCH_MS),
        "pending":false,"deaf":false,"mute":false,"flags":0})
}

fn message(id: u64, channel: u64, sequence: u64, stamp: &str) -> Value {
    json!({"guild_id":GUILD.to_string(),"id":(1_000_000+sequence).to_string(),
        "channel_id":channel.to_string(),"author":user(id),"timestamp":stamp,
        "type":0,"content":"synthetic benchmark message","attachments":[],"embeds":[],
        "mentions":[],"mention_roles":[],"mention_everyone":false,"pinned":false,"tts":false})
}

fn voice(id: u64, channel: Option<u64>) -> Value {
    json!({"guild_id":GUILD.to_string(),"user_id":id.to_string(),
        "channel_id":channel.map(|c| c.to_string()),"session_id":"bench-voice",
        "deaf":false,"mute":false,"self_deaf":false,"self_mute":false,
        "self_video":false,"suppress":false,"member":member(id)})
}

async fn commit(
    pipeline: &Pipeline<GatewayFunnelBuffer>,
    store: &GatewaySessionStore,
    event: Event,
    sequence: u64,
    at_ms: i64,
) -> Result<()> {
    pipeline.handle_at(&event, &two_bot_core::format_iso_millis(at_ms));
    let checkpoint = GatewaySession {
        session_id: "pipeline-bench".into(),
        sequence,
        resume_url: "ws://127.0.0.1:1".into(),
        updated_at_ms: at_ms,
    };
    ensure!(
        store
            .commit_dispatch(&checkpoint, pipeline.handlers().store().take_batch())
            .await?
            == DispatchAction::Apply,
        "unexpected duplicate dispatch"
    );
    Ok(())
}

fn percentiles(mut samples: Vec<f64>) -> Value {
    samples.sort_by(f64::total_cmp);
    let pick = |percent: usize| samples[(samples.len() * percent).div_ceil(100) - 1];
    json!({"p50":pick(50),"p99":pick(99)})
}

fn peak_rss_mib() -> Result<f64> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let line = status
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .context("Linux VmHWM measurement required")?;
    let kib: f64 = line
        .split_whitespace()
        .nth(1)
        .context("VmHWM missing value")?
        .parse()?;
    ensure!(kib > 0.0, "RSS measurement unavailable");
    Ok(kib / 1024.0)
}

async fn measure(config: &Config, db: &TestDatabase, queries: &Queries) -> Result<Value> {
    let pipeline: Pipeline<GatewayFunnelBuffer> = Pipeline::new(
        GatewayFunnelBuffer::default(),
        None,
        None,
        NoInvites,
        NoClassification,
    );
    let store = GatewaySessionStore::new(db.pool().clone(), GUILD.to_string(), 0);
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"9999"}))).await;
    let executor = ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin()))
        .map_err(anyhow::Error::msg)?;
    let mut sequence = 0;
    for index in 0..config.channels {
        sequence += 1;
        let event = dispatch(
            sequence,
            "CHANNEL_CREATE",
            json!({"guild_id":GUILD.to_string(),
            "id":(10_000+index).to_string(),"name":"bench-channel","type":if index % 2 == 0 {0} else {2}}),
        )?;
        pipeline.cache().update(&event);
    }
    for index in 0..config.members {
        sequence += 1;
        commit(
            &pipeline,
            &store,
            dispatch(sequence, "GUILD_MEMBER_ADD", member(100 + index))?,
            sequence,
            EPOCH_MS,
        )
        .await?;
    }
    ensure!(
        pipeline.cache().stats().guild_members(Id::new(GUILD)) == Some(config.members as usize),
        "member cache did not populate"
    );
    ensure!(
        pipeline.cache().stats().channels() == config.channels as usize,
        "channel cache did not populate"
    );

    let statements_before = queries.statements.load(Ordering::Relaxed);
    let commits_before = queries.commits.load(Ordering::Relaxed);
    let mut latency = Vec::with_capacity(config.events() as usize);
    let mut rest_latency = Vec::new();
    let replay_started = Instant::now();
    let mut messages = 0;
    let mut voices = 0;
    let mut voice_connected = vec![false; config.members as usize];
    for second in 0..config.duration_secs {
        let rate = config.messages_per_sec + config.voice_per_sec;
        for slot in 0..rate {
            let index = second * rate + slot;
            let deadline =
                Duration::from_secs(second) + Duration::from_secs_f64(slot as f64 / rate as f64);
            tokio::time::sleep_until((replay_started + deadline).into()).await;
            sequence += 1;
            let at_ms = EPOCH_MS + 1_000 + deadline.as_millis() as i64;
            let stamp = gateway_timestamp(at_ms);
            let (kind, data) = if slot < config.messages_per_sec {
                let id = 100 + messages % config.members;
                let channel = 10_000 + 2 * (messages % config.channels.div_ceil(2));
                messages += 1;
                ("MESSAGE_CREATE", message(id, channel, sequence, &stamp))
            } else {
                let member_index = (voices % config.members) as usize;
                let connected = &mut voice_connected[member_index];
                *connected = !*connected;
                voices += 1;
                (
                    "VOICE_STATE_UPDATE",
                    voice(100 + member_index as u64, connected.then_some(10_001)),
                )
            };
            // Event construction/parsing and rate pacing are outside handler latency.
            let event = dispatch(sequence, kind, data)?;
            let started = Instant::now();
            commit(&pipeline, &store, event, sequence, at_ms).await?;
            latency.push(started.elapsed().as_secs_f64() * 1_000_000.0);
            if (index + 1).is_multiple_of(config.rest_every) {
                let started = Instant::now();
                executor
                    .post_message("10000", "synthetic benchmark reply", Some(sequence))
                    .await?;
                rest_latency.push(started.elapsed().as_secs_f64() * 1_000_000.0);
            }
        }
    }
    let replay_secs = replay_started.elapsed().as_secs_f64();
    let statements = queries.statements.load(Ordering::Relaxed) - statements_before;
    let commits = queries.commits.load(Ordering::Relaxed) - commits_before;
    ensure!(
        commits == config.events(),
        "SQLx COMMIT diagnostics missing; invalid query measurement"
    );
    ensure!(
        statements >= config.events() * 4,
        "SQLx query diagnostics missing"
    );
    let rest_calls = executor.requests();
    ensure!(
        rest_calls == config.events() / config.rest_every,
        "unexpected REST request count"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(db.pool())
        .await?;
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM members")
        .fetch_one(db.pool())
        .await?;
    ensure!(
        rows >= (config.members * 2) as i64 && members == config.members as i64,
        "durable effects missing"
    );
    let checkpoint = store.load().await?.context("checkpoint missing")?;
    ensure!(
        checkpoint.sequence == sequence,
        "checkpoint did not reach final dispatch"
    );
    let peak = peak_rss_mib()?;
    mock.shutdown().await;
    Ok(json!({
        "schema_version":1,
        "workload":{
            "members":config.members,"channels":config.channels,"duration_secs":config.duration_secs,
            "messages_per_sec":config.messages_per_sec,"voice_per_sec":config.voice_per_sec,
            "rest_every":config.rest_every,"profile":"debug","handler":"twilight-funnel+dispatch-commit",
            "db_accounting":"sqlx-completed-statements+unlogged-BEGIN"
        },
        "metrics":{
            "peak_rss_mib":peak,"handler_latency_us":percentiles(latency),
            "db_round_trips_per_event":(statements + config.events()) as f64 / config.events() as f64,
            "mock_rest_latency_us":if rest_latency.is_empty() {Value::Null} else {percentiles(rest_latency)},
            "replay_secs":replay_secs
        },
        "evidence":{
            "measured_events":config.events(),"messages":messages,"voice_updates":voices,
            "sqlx_statements":statements,"begin_exchanges":config.events(),"commits":commits,
            "mock_rest_calls":rest_calls,"durable_events":rows,"durable_members":members,
            "cached_members":pipeline.cache().stats().guild_members(Id::new(GUILD)),
            "cached_channels":pipeline.cache().stats().channels(),"checkpoint_sequence":sequence
        }
    }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let process_started = Instant::now();
    let config = Config::parse(std::env::args().skip(1))?;
    let queries = Queries::default();
    tracing_subscriber::registry()
        .with(queries.clone())
        .try_init()?;
    let raw = std::env::var("TWO_TEST_DATABASE_URL")
        .context("explicit TWO_TEST_DATABASE_URL required; no fallback")?;
    let db = TestDatabase::create(&raw, &sqlx::migrate!("../cutover/migrations")).await?;
    let measured =
        tokio::time::timeout(Duration::from_secs(180), measure(&config, &db, &queries)).await;
    db.close()
        .await
        .context("verify disposable database teardown")?;
    let mut report = measured.context("benchmark exceeded 180 seconds")??;
    let elapsed = process_started.elapsed().as_secs_f64();
    ensure!(
        elapsed < 240.0,
        "benchmark exceeded 240 second command budget"
    );
    report["metrics"]["command_secs"] = json!(elapsed);
    report["evidence"]["disposable_database_dropped"] = json!(true);
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_bounded_profiles() {
        let default = Config::parse(std::iter::empty()).unwrap();
        assert_eq!(default.events(), 900);
        for args in [
            vec!["--members", "0"],
            vec!["--channels", "1"],
            vec!["--duration-secs", "121"],
            vec!["--messages-per-sec", "1001"],
            vec!["--rest-every", "0"],
            vec!["--unknown", "1"],
            vec!["--members"],
        ] {
            assert!(Config::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn exact_nearest_rank_percentiles() {
        let result = percentiles((1..=100).rev().map(f64::from).collect());
        assert_eq!(result, json!({"p50":50.0,"p99":99.0}));
    }

    #[test]
    fn synthetic_dispatches_parse_and_populate_cache() {
        let pipeline: Pipeline<GatewayFunnelBuffer> = Pipeline::new(
            GatewayFunnelBuffer::default(),
            None,
            None,
            NoInvites,
            NoClassification,
        );
        pipeline.handle_at(
            &dispatch(1, "GUILD_MEMBER_ADD", member(100)).unwrap(),
            &two_bot_core::format_iso_millis(EPOCH_MS),
        );
        pipeline.handle_at(
            &dispatch(
                2,
                "MESSAGE_CREATE",
                message(100, 10000, 2, &gateway_timestamp(EPOCH_MS)),
            )
            .unwrap(),
            &two_bot_core::format_iso_millis(EPOCH_MS),
        );
        pipeline.handle_at(
            &dispatch(3, "VOICE_STATE_UPDATE", voice(100, Some(10001))).unwrap(),
            &two_bot_core::format_iso_millis(EPOCH_MS),
        );
        assert_eq!(
            pipeline.cache().stats().guild_members(Id::new(GUILD)),
            Some(1)
        );
        assert!(!pipeline.handlers().store().take_batch().events.is_empty());
    }
}
