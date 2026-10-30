//! Protected Reconcile acceptance: bounded reads with durable forward progress.
//!
//! Existing page-cap and within-budget controls remain intact. Cursor tests
//! below use a channel keyed by requested `before`, not a FIFO, and existing
//! public APIs plus SQL observations. A missing cursor implementation should
//! fail on behavior, not a reference to a future Rust field or function.
//!
//! Builder contract: at most 16 reads per claim and the existing wall-clock
//! deadline; persist only validated, marker-checked floors under live ownership;
//! resume from the exact saved floor after restart. Forward progress does not
//! spend the consecutive no-progress budget. Reconcile never POSTs.
//!
//! The nullable cursor column is expected-schema fixture setup only, not proof
//! of a shipped migration. Migration-chain verification belongs to the builder.
//! Keep this file and its assertions protected from implementation edits.
//!
//! DB access is only the fixed approved disposable test services, empty test
//! password, isolated schema per test. Doubles never contact Discord.
//! Controller run: `python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test reconcile_bound_acceptance --locked -- --include-ignored --test-threads=1`
//! `--include-ignored` runs both legacy opt-in controls and new unskipped tests.
#![cfg(feature = "db")]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, QueryBuilder};
use two_bot_core::audit::{format_audit_event, AuditChannelIds, AuditEvent, AuditKind};
use two_bot_core::audit_mirror::{AuditMirror, MirrorChannel, MirrorError, MirrorMessage};
use two_bot_core::audit_service::{AuditMirrorService, DeliverOutcome, MirrorConfig};
use two_bot_core::audit_store::{AuditStore, DeliveryState, QuarantineReason};

type TestResult = Result<(), Box<dyn std::error::Error>>;
const MIGRATION: &str = include_str!("../../cutover/migrations/0340_operational_audit.sql");
static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

const GUILD: &str = "18446744073709551615";
const AUDIT_CH: &str = "1111";
const VOICE_CH: &str = "2222";
const MOD_CH: &str = "3333";
const BOT: &str = "9999";

/// Full noise pages served to the Reconcile pass: far past any sane cap.
const DEEP_PAGES: usize = 48;
/// Largest acceptable read count on the deep fixture: cap 16 plus one page
/// of slack for fetch-then-count fix shapes.
const CAP_SLACK: u64 = 17;

// ---------------------------------------------------------------- TestDb --

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    options: PgConnectOptions,
    schema: String,
}

impl TestDb {
    async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let host = if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
        let options = PgConnectOptions::new()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("postgres")
            .ssl_mode(PgSslMode::Disable);
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone())
            .await?;
        let schema = format!(
            "reconcile_bound_{}_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        QueryBuilder::<sqlx::Postgres>::new("CREATE SCHEMA ")
            .push(&schema)
            .build()
            .execute(&admin)
            .await?;
        let options = options.application_name(&schema);
        let pool = Self::connect(&options, &schema).await?;
        sqlx::raw_sql(MIGRATION).execute(&pool).await?;
        // Expected-schema fixture only; this is not shipped-migration proof.
        sqlx::query("ALTER TABLE operational_audit_log ADD COLUMN IF NOT EXISTS delivery_reconcile_before TEXT NULL")
            .execute(&pool).await?;
        Ok(Self {
            admin,
            pool,
            options,
            schema,
        })
    }

    async fn connect(options: &PgConnectOptions, schema: &str) -> Result<PgPool, sqlx::Error> {
        let path = schema.to_owned();
        PgPoolOptions::new()
            .max_connections(3)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options.clone())
            .await
    }

    async fn expire(&self, entry: &str) -> TestResult {
        sqlx::query("UPDATE operational_audit_log SET delivery_lease_until = clock_timestamp() - interval '1 second' WHERE entry_id = $1")
            .bind(entry).execute(&self.pool).await?;
        Ok(())
    }

    async fn finish(self) -> TestResult {
        self.pool.close().await;
        QueryBuilder::<sqlx::Postgres>::new("DROP SCHEMA ")
            .push(&self.schema)
            .push(" CASCADE")
            .build()
            .execute(&self.admin)
            .await?;
        self.admin.close().await;
        Ok(())
    }
}

// ---------------------------------------------------------- ScriptMirror --

type PostResult = Result<String, MirrorError>;
type DocResult = Result<MirrorChannel, MirrorError>;
type PageResult = Result<Vec<MirrorMessage>, MirrorError>;

#[derive(Clone, Default)]
struct ScriptMirror {
    posts: Arc<AtomicU64>,
    post_script: Arc<Mutex<VecDeque<PostResult>>>,
    documents: Arc<Mutex<VecDeque<DocResult>>>,
    history: Arc<Mutex<VecDeque<PageResult>>>,
    history_reads: Arc<AtomicU64>,
}

impl ScriptMirror {
    fn post_count(&self) -> usize {
        self.posts.load(Ordering::Relaxed) as usize
    }

    fn reads(&self) -> u64 {
        self.history_reads.load(Ordering::Relaxed)
    }
}

fn private_document(guild_id: &str) -> MirrorChannel {
    MirrorChannel {
        guild_id: guild_id.to_owned(),
        everyone: Some(two_bot_core::audit_mirror::MirrorOverwrite {
            allow: "0".to_owned(),
            deny: "1024".to_owned(),
        }),
    }
}

fn marked(id: &str, event: &AuditEvent) -> MirrorMessage {
    MirrorMessage {
        id: id.to_owned(),
        author_id: BOT.to_owned(),
        content: format_audit_event(event),
    }
}

/// One full page of non-matching history: 100 messages with strictly
/// descending numeric ids below `before`, authored by a non-bot so the marker
/// identity never matches. `start` is the page's newest id.
fn noise_page(start: u64) -> Vec<MirrorMessage> {
    (0..100u64)
        .map(|offset| MirrorMessage {
            id: (start - offset).to_string(),
            author_id: "1234".to_owned(),
            content: "unrelated channel noise".to_owned(),
        })
        .collect()
}

impl AuditMirror for ScriptMirror {
    async fn post_mirror_checked<Fut, E>(
        &self,
        channel_id: &str,
        content: &str,
        nonce: &str,
        authorize: Fut,
    ) -> Result<Result<String, MirrorError>, E>
    where
        Fut: std::future::Future<Output = Result<(), E>> + Send,
        E: Send,
    {
        authorize.await?;
        let _ = (channel_id, content, nonce);
        self.posts.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .post_script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok("900000".to_owned())))
    }

    async fn channel_document(&self, channel_id: &str) -> Result<MirrorChannel, MirrorError> {
        let _ = channel_id;
        self.documents
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(private_document(GUILD)))
    }

    async fn channel_history(
        &self,
        channel_id: &str,
        _before: Option<&str>,
        _limit: u8,
    ) -> Result<Vec<MirrorMessage>, MirrorError> {
        let _ = channel_id;
        self.history_reads.fetch_add(1, Ordering::Relaxed);
        self.history
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(vec![]))
    }
}

// ---------------------------------------------------------------- shared --

fn event(entry: &str) -> AuditEvent {
    let mut e = AuditEvent::new(
        entry.to_owned(),
        AuditKind::MemberUpdate,
        GUILD.to_owned(),
        "2026-09-30T00:01:02.003Z".to_owned(),
    );
    e.actor_id = Some("18446744073709551614".to_owned());
    e.target_id = Some("18446744073709551613".to_owned());
    e.metadata_json = r#"{"nicknameChanged":true}"#.to_owned();
    e
}

fn config() -> MirrorConfig {
    MirrorConfig {
        channels: AuditChannelIds {
            audit: Some(AUDIT_CH.to_owned()),
            voice: Some(VOICE_CH.to_owned()),
            moderation: Some(MOD_CH.to_owned()),
        },
        configured: vec![AUDIT_CH.to_owned(), VOICE_CH.to_owned(), MOD_CH.to_owned()],
        mirror_guild_id: Some(GUILD.to_owned()),
        bot_user_id: BOT.to_owned(),
    }
}

fn service_for(pool: &PgPool) -> (AuditStore, AuditMirrorService<ScriptMirror>, ScriptMirror) {
    let mirror = ScriptMirror::default();
    (
        AuditStore::new(pool),
        AuditMirrorService::new(AuditStore::new(pool), mirror.clone(), config()),
        mirror,
    )
}

/// Drive one row to `Ambiguous` (reconcile-eligible) through the Send path:
/// scripted dedup history, then an accepted-but-timed-out POST. Returns the
/// history-read count consumed by the Send pass.
async fn drive_to_ambiguous(
    db: &TestDb,
    store: &AuditStore,
    service: &AuditMirrorService<ScriptMirror>,
    mirror: &ScriptMirror,
    entry: &str,
    dedup: Vec<MirrorMessage>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let ev = event(entry);
    service.record(&ev).await?;
    mirror.history.lock().unwrap().push_back(Ok(dedup));
    mirror
        .post_script
        .lock()
        .unwrap()
        .push_back(Err(MirrorError::Uncertain(
            "accepted but timed out".to_owned(),
        )));
    assert_eq!(
        service.deliver_entry(entry).await?,
        DeliverOutcome::Ambiguous
    );
    let row = store.get(entry).await?.unwrap();
    assert_eq!(row.state, DeliveryState::Pending);
    db.expire(entry).await?;
    Ok(mirror.reads())
}

// ----------------------------------------------------------------- tests --

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn deep_scan_stops_at_cap_with_fixed_marker_and_recovers() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    // Empty dedup history seeds boundary "0": every positive floor is at or
    // after it, so only the page cap can stop the scan.
    let send_reads =
        drive_to_ambiguous(&db, &store, &service, &mirror, "deep-scan", vec![]).await?;
    assert_eq!(
        store
            .get("deep-scan")
            .await?
            .unwrap()
            .search_before
            .as_deref(),
        Some("0")
    );
    for page in 0..DEEP_PAGES {
        mirror
            .history
            .lock()
            .unwrap()
            .push_back(Ok(noise_page(900_000 - page as u64 * 100)));
    }
    mirror.history.lock().unwrap().push_back(Ok(vec![]));
    let outcome = service.deliver_entry("deep-scan").await?;
    let reads = mirror.reads() - send_reads;
    assert!(
        reads <= CAP_SLACK,
        "deep scan paged past any bound: {reads} history reads for {DEEP_PAGES} full pages"
    );
    assert_ne!(
        outcome,
        DeliverOutcome::Quarantined(QuarantineReason::MarkerMissing),
        "deep scan quarantined a merely-unscanned row as MarkerMissing"
    );
    assert!(
        format!("{outcome:?}").contains("ScanBoundReached"),
        "deep scan stopped without the fixed marker: {outcome:?}"
    );
    // The bound row stays reconcile-eligible: the next owner adopts a marker
    // within the bound without a second POST.
    db.expire("deep-scan").await?;
    let ev = event("deep-scan");
    {
        let mut history = mirror.history.lock().unwrap();
        history.clear();
        history.push_back(Ok(vec![marked("640", &ev)]));
    }
    assert_eq!(
        service.deliver_entry("deep-scan").await?,
        DeliverOutcome::Reconciled {
            message_id: "640".to_owned()
        }
    );
    assert_eq!(mirror.post_count(), 1, "recovery must not re-POST");
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn marker_within_bound_still_reconciles() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    let send_reads =
        drive_to_ambiguous(&db, &store, &service, &mirror, "shallow-marker", vec![]).await?;
    let ev = event("shallow-marker");
    mirror
        .history
        .lock()
        .unwrap()
        .push_back(Ok(vec![marked("640", &ev)]));
    assert_eq!(
        service.deliver_entry("shallow-marker").await?,
        DeliverOutcome::Reconciled {
            message_id: "640".to_owned()
        }
    );
    assert_eq!(
        mirror.reads() - send_reads,
        1,
        "one page proves the marker; the cap must not add reads"
    );
    assert_eq!(mirror.post_count(), 1);
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn floor_below_boundary_still_quarantines() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    // Non-empty dedup seeds boundary next_snowflake("5000") = "5001".
    let dedup = vec![MirrorMessage {
        id: "5000".to_owned(),
        author_id: "1234".to_owned(),
        content: "unrelated channel noise".to_owned(),
    }];
    let send_reads = drive_to_ambiguous(&db, &store, &service, &mirror, "low-floor", dedup).await?;
    assert_eq!(
        store
            .get("low-floor")
            .await?
            .unwrap()
            .search_before
            .as_deref(),
        Some("5001")
    );
    mirror
        .history
        .lock()
        .unwrap()
        .push_back(Ok(noise_page(4_900)));
    assert_eq!(
        service.deliver_entry("low-floor").await?,
        DeliverOutcome::Quarantined(QuarantineReason::MarkerMissing)
    );
    assert_eq!(
        mirror.reads() - send_reads,
        1,
        "floor below the boundary ends the scan on page one"
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn empty_history_still_quarantines() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    let send_reads =
        drive_to_ambiguous(&db, &store, &service, &mirror, "empty-history", vec![]).await?;
    mirror.history.lock().unwrap().push_back(Ok(vec![]));
    assert_eq!(
        service.deliver_entry("empty-history").await?,
        DeliverOutcome::Quarantined(QuarantineReason::MarkerMissing)
    );
    assert_eq!(
        mirror.reads() - send_reads,
        1,
        "exhausted history ends the scan on page one"
    );
    db.finish().await
}

// Durable-cursor acceptance. These tests use only existing service/store APIs
// and SQL observations, so missing persistence is a behavioral RED, not a
// reference to an unimplemented Rust field. The ALTER above is fixture-local.

#[derive(Clone)]
enum CursorRead {
    Reply(PageResult),
    ExpireDeadline(Duration),
    Pause(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>),
}

/// Pause only while the double owns a history read. Cancellation drops this
/// guard before the service performs any checkpoint SQL; real SQLx I/O never
/// runs under paused-clock auto-advance.
struct HistoryClock;

impl HistoryClock {
    fn pause() -> Self {
        tokio::time::pause();
        Self
    }
}

impl Drop for HistoryClock {
    fn drop(&mut self) {
        tokio::time::resume();
    }
}

#[derive(Default)]
struct CursorHistory {
    messages: Vec<MirrorMessage>,
    requests: Vec<Option<String>>,
    overrides: HashMap<Option<String>, CursorRead>,
}

/// A channel, not a FIFO: resetting `before` really re-reads the head.
#[derive(Clone, Default)]
struct CursorMirror {
    history: Arc<Mutex<CursorHistory>>,
    posts: Arc<AtomicU64>,
}

impl CursorMirror {
    fn install(&self, pages: usize, marker: Option<&AuditEvent>) {
        let mut history = self.history.lock().unwrap();
        history.messages = (0..pages)
            .flat_map(|page| noise_page(900_000 - page as u64 * 100))
            .collect();
        if let Some(event) = marker {
            history.messages.push(marked("895100", event));
        }
    }

    fn newer_traffic(&self, start: u64) {
        self.history
            .lock()
            .unwrap()
            .messages
            .splice(0..0, noise_page(start));
    }

    fn requests(&self) -> Vec<Option<String>> {
        self.history.lock().unwrap().requests.clone()
    }

    fn override_read(&self, before: Option<&str>, read: CursorRead) {
        self.history
            .lock()
            .unwrap()
            .overrides
            .insert(before.map(str::to_owned), read);
    }
}

impl AuditMirror for CursorMirror {
    async fn post_mirror_checked<Fut, E>(
        &self,
        channel: &str,
        content: &str,
        nonce: &str,
        authorize: Fut,
    ) -> Result<Result<String, MirrorError>, E>
    where
        Fut: std::future::Future<Output = Result<(), E>> + Send,
        E: Send,
    {
        authorize.await?;
        assert_eq!(channel, AUDIT_CH);
        assert!(!content.is_empty());
        assert!(!nonce.is_empty());
        self.posts.fetch_add(1, Ordering::Relaxed);
        Ok(Err(MirrorError::Uncertain(
            "accepted but timed out".to_owned(),
        )))
    }

    async fn channel_document(&self, channel: &str) -> Result<MirrorChannel, MirrorError> {
        assert_eq!(channel, AUDIT_CH);
        Ok(private_document(GUILD))
    }

    async fn channel_history(&self, channel: &str, before: Option<&str>, limit: u8) -> PageResult {
        assert_eq!(channel, AUDIT_CH);
        assert_eq!(limit, 100);
        let override_read = {
            let mut history = self.history.lock().unwrap();
            history.requests.push(before.map(str::to_owned));
            history.overrides.get(&before.map(str::to_owned)).cloned()
        };
        match override_read {
            Some(CursorRead::Reply(reply)) => return reply,
            Some(CursorRead::ExpireDeadline(elapsed)) => {
                // A stale std::Instant loop retries the already timed-out page.
                // Make that regression a finite behavioral failure, not a spin
                // until wall time catches up with the controlled Tokio clock.
                self.override_read(
                    before,
                    CursorRead::Reply(Err(MirrorError::Uncertain(
                        "timed-out page was requested again".to_owned(),
                    ))),
                );
                let _clock = HistoryClock::pause();
                tokio::time::advance(elapsed).await;
                return std::future::pending().await;
            }
            Some(CursorRead::Pause(entered, release)) => {
                entered.notify_one();
                release.notified().await;
            }
            None => {}
        }
        let upper = before.map(|id| id.parse::<u64>().unwrap());
        Ok(self
            .history
            .lock()
            .unwrap()
            .messages
            .iter()
            .filter(|message| upper.is_none_or(|upper| message.id.parse::<u64>().unwrap() < upper))
            .take(usize::from(limit))
            .cloned()
            .collect())
    }
}

async fn cursor_seed(db: &TestDb, entry: &str, mirror: &CursorMirror) -> TestResult {
    let service = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config());
    service.record(&event(entry)).await?;
    assert_eq!(
        service.deliver_entry(entry).await?,
        DeliverOutcome::Ambiguous
    );
    let row = AuditStore::new(&db.pool).get(entry).await?.unwrap();
    assert_eq!(row.search_before.as_deref(), Some("0"));
    assert_eq!(row.attempts, 1);
    assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
    db.expire(entry).await
}

async fn cursor(pool: &PgPool, entry: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT delivery_reconcile_before FROM operational_audit_log WHERE entry_id = $1",
    )
    .bind(entry)
    .fetch_one(pool)
    .await
}

/// Seed a prior successful pass for isolated timeout/ownership tests. Forward
/// persistence itself is proved without this helper by the deep-scan test.
async fn seed_floor(db: &TestDb, entry: &str, floor: &str) -> TestResult {
    sqlx::query(
        "UPDATE operational_audit_log SET delivery_reconcile_before = $2 WHERE entry_id = $1",
    )
    .bind(entry)
    .bind(floor)
    .execute(&db.pool)
    .await?;
    Ok(())
}

async fn peer(db: &TestDb) -> Result<PgPool, sqlx::Error> {
    TestDb::connect(&db.options, &db.schema).await
}

async fn assert_released(pool: &PgPool, entry: &str) -> TestResult {
    let released: bool = sqlx::query_scalar("SELECT delivery_state = 'pending' AND delivery_claim_token IS NULL AND delivery_lease_until IS NULL FROM operational_audit_log WHERE entry_id = $1")
        .bind(entry).fetch_one(pool).await?;
    assert!(
        released,
        "bounded pass must atomically persist and release ownership"
    );
    Ok(())
}

#[tokio::test]
async fn durable_floor_crosses_three_budgets_and_restart_without_rescans() -> TestResult {
    let db = TestDb::new().await?;
    let entry = "durable-deep-marker";
    let mirror = CursorMirror::default();
    cursor_seed(&db, entry, &mirror).await?;
    let original = AuditStore::new(&db.pool).get(entry).await?.unwrap();
    mirror.install(40, Some(&event(entry)));
    let mut seen = HashSet::new();
    // Three independent pools, services and adapters. Only the simulated
    // Discord channel and POST counter survive a worker's destruction.
    for (pass, expected_first, expected_floor) in [
        (0, None, Some("898401")),
        (1, Some("898401"), Some("896801")),
        (2, Some("896801"), None),
    ] {
        let pool = peer(&db).await?;
        let restarted_mirror = CursorMirror {
            history: mirror.history.clone(),
            posts: mirror.posts.clone(),
        };
        let service = AuditMirrorService::new(AuditStore::new(&pool), restarted_mirror, config());
        let start = mirror.requests().len();
        let outcome = service.deliver_entry(entry).await?;
        let requests = mirror.requests()[start..].to_vec();
        assert_eq!(
            requests.first().map(|id| id.as_deref()),
            Some(expected_first),
            "pass {pass} must resume exactly at its persisted floor"
        );
        assert!(
            requests.len() <= 16,
            "per-claim cap exceeded: {}",
            requests.len()
        );
        for requested in &requests {
            assert!(
                seen.insert(requested.clone()),
                "rescanned an already proven page: {requested:?}"
            );
        }
        if let Some(floor) = expected_floor {
            assert_eq!(
                outcome,
                DeliverOutcome::ScanBoundReached,
                "strict forward progress must not spend the no-progress budget"
            );
            assert_eq!(requests.len(), 16);
            assert_eq!(
                cursor(&pool, entry).await?.as_deref(),
                Some(floor),
                "save the deepest fully examined page"
            );
            assert_released(&pool, entry).await?;
        } else {
            assert_eq!(
                outcome,
                DeliverOutcome::Reconciled {
                    message_id: "895100".to_owned()
                }
            );
            assert_eq!(requests.len(), 9);
        }
        let row = AuditStore::new(&pool).get(entry).await?.unwrap();
        assert_eq!(row.search_before, original.search_before);
        assert_eq!(row.nonce, original.nonce);
        assert_eq!(row.attempted_at, original.attempted_at);
        assert_eq!(row.attempts, 1);
        assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
        drop(service);
        pool.close().await;
        mirror.newer_traffic(910_000 + pass * 100);
    }
    let final_row = AuditStore::new(&db.pool).get(entry).await?.unwrap();
    assert_eq!(final_row.state, DeliveryState::Delivered);
    assert_eq!(final_row.mirror_message_id.as_deref(), Some("895100"));
    assert!(final_row.accepted_at.is_some());
    assert!(final_row.mirrored_at.is_some());
    db.finish().await
}

#[tokio::test]
async fn time_bound_saves_only_pages_completed_before_deadline() -> TestResult {
    let db = TestDb::new().await?;
    let entry = "deadline-after-progress";
    let mirror = CursorMirror::default();
    cursor_seed(&db, entry, &mirror).await?;
    mirror.install(40, Some(&event(entry)));
    // Reaching page 3 proves pages 1 and 2 were validated and renewed. Only
    // then advance the scan's default 30-second clock beyond its deadline;
    // no short absolute deadline races claim/halt/renew database latency.
    mirror.override_read(
        Some("899801"),
        CursorRead::ExpireDeadline(Duration::from_secs(31)),
    );
    let service = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config());
    let start = mirror.requests().len();
    let result = service.deliver_entry(entry).await?;
    drop(service);
    assert_eq!(result, DeliverOutcome::ScanBoundReached);
    assert_eq!(
        mirror.requests()[start..],
        [None, Some("899901".to_owned()), Some("899801".to_owned())]
    );
    let pool = peer(&db).await?;
    assert_eq!(
        cursor(&pool, entry).await?.as_deref(),
        Some("899801"),
        "timed-out page cannot establish a deeper floor"
    );
    assert_released(&pool, entry).await?;
    mirror.history.lock().unwrap().overrides.clear();
    let restarted = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config());
    let start = mirror.requests().len();
    assert_eq!(
        restarted.deliver_entry(entry).await?,
        DeliverOutcome::ScanBoundReached
    );
    assert_eq!(mirror.requests()[start].as_deref(), Some("899801"));
    assert_eq!(cursor(&pool, entry).await?.as_deref(), Some("898201"));
    assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
    assert_eq!(
        AuditStore::new(&pool).get(entry).await?.unwrap().attempts,
        1
    );
    drop(restarted);
    pool.close().await;
    db.finish().await
}

#[tokio::test]
async fn zero_progress_uncertainty_preserves_floor_and_first_bound_evidence() -> TestResult {
    let db = TestDb::new().await?;
    let entry = "no-progress-with-uncertainty";
    let mirror = CursorMirror::default();
    cursor_seed(&db, entry, &mirror).await?;
    seed_floor(&db, entry, "899901").await?;
    let original = AuditStore::new(&db.pool).get(entry).await?.unwrap();
    let timed = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config())
        .with_reconcile_scan_deadline(Instant::now() - Duration::from_secs(1));
    let reads = mirror.requests().len();
    assert_eq!(
        timed.deliver_entry(entry).await?,
        DeliverOutcome::ScanBoundReached
    );
    assert_eq!(mirror.requests().len(), reads);
    assert_eq!(cursor(&db.pool, entry).await?.as_deref(), Some("899901"));
    assert_released(&db.pool, entry).await?;
    let first = AuditStore::new(&db.pool).get(entry).await?.unwrap();
    assert!(first.last_error.is_some());
    mirror.override_read(
        Some("899901"),
        CursorRead::Reply(Err(MirrorError::Uncertain("read timed out".to_owned()))),
    );
    let pool = peer(&db).await?;
    let uncertain = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config());
    assert_eq!(
        uncertain.deliver_entry(entry).await?,
        DeliverOutcome::Ambiguous
    );
    assert_eq!(mirror.requests()[reads].as_deref(), Some("899901"));
    assert_eq!(cursor(&pool, entry).await?.as_deref(), Some("899901"));
    // The second timeout below is the behavioral proof that uncertainty did
    // not erase the first strike; diagnostics need not encode that state.
    assert_eq!(
        AuditStore::new(&pool).get(entry).await?.unwrap().attempts,
        first.attempts
    );
    db.expire(entry).await?;
    let timed_restart = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config())
        .with_reconcile_scan_deadline(Instant::now() - Duration::from_secs(1));
    let reads = mirror.requests().len();
    assert_eq!(
        timed_restart.deliver_entry(entry).await?,
        DeliverOutcome::Quarantined(QuarantineReason::ScanBoundExceeded)
    );
    assert_eq!(mirror.requests().len(), reads);
    let final_row = AuditStore::new(&pool).get(entry).await?.unwrap();
    assert_eq!(final_row.state, DeliveryState::Quarantined);
    assert_eq!(
        final_row.last_error.as_deref(),
        Some("discord_reconcile_scan_exhausted")
    );
    assert_eq!(cursor(&pool, entry).await?.as_deref(), Some("899901"));
    assert_eq!(final_row.search_before, original.search_before);
    assert_eq!(final_row.nonce, original.nonce);
    assert_eq!(final_row.attempts, 1);
    assert!(AuditStore::new(&pool).claim(entry).await?.is_none());
    assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
    drop((timed, uncertain, timed_restart));
    pool.close().await;
    db.finish().await
}

#[tokio::test]
async fn two_zero_progress_bounds_quarantine_without_rewinding_nullable_floor() -> TestResult {
    for floor in [None, Some("899901")] {
        let db = TestDb::new().await?;
        let entry = "two-empty-budgets";
        let mirror = CursorMirror::default();
        cursor_seed(&db, entry, &mirror).await?;
        if let Some(floor) = floor {
            seed_floor(&db, entry, floor).await?;
        }
        let reads = mirror.requests().len();
        let timed = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config())
            .with_reconcile_scan_deadline(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            timed.deliver_entry(entry).await?,
            DeliverOutcome::ScanBoundReached
        );
        assert_released(&db.pool, entry).await?;
        assert_eq!(cursor(&db.pool, entry).await?.as_deref(), floor);
        let pool = peer(&db).await?;
        let restarted = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config())
            .with_reconcile_scan_deadline(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            restarted.deliver_entry(entry).await?,
            DeliverOutcome::Quarantined(QuarantineReason::ScanBoundExceeded)
        );
        assert_eq!(cursor(&pool, entry).await?.as_deref(), floor);
        assert_eq!(mirror.requests().len(), reads);
        assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
        let row = AuditStore::new(&pool).get(entry).await?.unwrap();
        assert_eq!(row.state, DeliveryState::Quarantined);
        assert_eq!(row.attempts, 1);
        assert!(row.mirror_message_id.is_none());
        drop((timed, restarted));
        pool.close().await;
        db.finish().await?;
    }
    Ok(())
}

#[tokio::test]
async fn progress_resets_a_prior_no_progress_budget() -> TestResult {
    let db = TestDb::new().await?;
    let entry = "progress-after-one-timeout";
    let mirror = CursorMirror::default();
    cursor_seed(&db, entry, &mirror).await?;
    seed_floor(&db, entry, "899901").await?;
    let timed = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config())
        .with_reconcile_scan_deadline(Instant::now() - Duration::from_secs(1));
    assert_eq!(
        timed.deliver_entry(entry).await?,
        DeliverOutcome::ScanBoundReached
    );
    mirror.install(40, Some(&event(entry)));
    let service = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config());
    assert_eq!(
        service.deliver_entry(entry).await?,
        DeliverOutcome::ScanBoundReached,
        "advancing floor is progress, not the second strike"
    );
    assert_eq!(cursor(&db.pool, entry).await?.as_deref(), Some("898301"));
    assert_eq!(
        timed.deliver_entry(entry).await?,
        DeliverOutcome::ScanBoundReached,
        "progress starts a fresh consecutive no-progress budget"
    );
    assert_eq!(
        timed.deliver_entry(entry).await?,
        DeliverOutcome::Quarantined(QuarantineReason::ScanBoundExceeded)
    );
    assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
    db.finish().await
}

#[tokio::test]
async fn cursor_write_cannot_outlive_lease_generation_or_new_acceptance() -> TestResult {
    for fault in ["expired", "replaced", "accepted"] {
        let db = TestDb::new().await?;
        let entry = "cursor-owner-fence";
        let mirror = CursorMirror::default();
        cursor_seed(&db, entry, &mirror).await?;
        seed_floor(&db, entry, "899901").await?;
        mirror.install(40, Some(&event(entry)));
        let pool = peer(&db).await?;
        let store = AuditStore::new(&db.pool);
        let claim = store.claim(entry).await?.unwrap();
        assert_eq!(
            claim.intent(),
            two_bot_core::audit_store::DeliveryIntent::Reconcile
        );
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        // Pause the final page, after the pass has accumulated real progress
        // but before the bound's cursor+release transaction can commit it.
        mirror.override_read(
            Some("898401"),
            CursorRead::Pause(entered.clone(), release.clone()),
        );
        let service = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config());
        let mutate = async {
            entered.notified().await;
            let other = AuditStore::new(&pool);
            let mut replacement = None;
            if fault == "accepted" {
                assert!(other.note_accepted(&claim, "895100").await?);
            } else {
                db.expire(entry).await?;
                if fault == "replaced" {
                    replacement = Some(other.claim(entry).await?.unwrap());
                }
            }
            let lease: Option<String> = sqlx::query_scalar(
                "SELECT delivery_lease_until::text FROM operational_audit_log WHERE entry_id = $1",
            )
            .bind(entry)
            .fetch_one(&pool)
            .await?;
            let snapshot = other.get(entry).await?.unwrap();
            let generation: i64 = sqlx::query_scalar(
                "SELECT delivery_generation FROM operational_audit_log WHERE entry_id = $1",
            )
            .bind(entry)
            .fetch_one(&pool)
            .await?;
            release.notify_one();
            Ok::<_, Box<dyn std::error::Error>>((snapshot, generation, lease, replacement))
        };
        let (result, snapshot) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(service.deliver(&claim), mutate)
        })
        .await?;
        let (snapshot, generation, lease, replacement) = snapshot?;
        assert_eq!(
            result?,
            DeliverOutcome::Unclaimed,
            "{fault} owner cannot save progress"
        );
        assert_eq!(
            store.get(entry).await?.unwrap(),
            snapshot,
            "{fault} cannot release or overwrite authoritative evidence"
        );
        assert_eq!(cursor(&pool, entry).await?.as_deref(), Some("899901"));
        let current_generation: i64 = sqlx::query_scalar(
            "SELECT delivery_generation FROM operational_audit_log WHERE entry_id = $1",
        )
        .bind(entry)
        .fetch_one(&pool)
        .await?;
        assert_eq!(current_generation, generation);
        let current_lease: Option<String> = sqlx::query_scalar(
            "SELECT delivery_lease_until::text FROM operational_audit_log WHERE entry_id = $1",
        )
        .bind(entry)
        .fetch_one(&pool)
        .await?;
        assert_eq!(current_lease, lease, "lost owner must not alter the lease");
        if let Some(replacement) = replacement {
            assert!(
                store.renew(&replacement).await?,
                "new token must still own the row"
            );
        }
        if fault == "accepted" {
            assert!(
                store.complete(&claim).await?,
                "accepted owner retains its completion capability"
            );
        }
        assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
        drop(service);
        pool.close().await;
        db.finish().await?;
    }
    Ok(())
}

/// Removing the failed-renew stop guard must cause an observable extra read,
/// even if the final checkpoint still rejects the stale capability.
#[tokio::test]
async fn lost_owner_on_second_full_page_stops_before_a_third_history_read() -> TestResult {
    for fault in ["expired", "replaced", "accepted"] {
        let db = TestDb::new().await?;
        let entry = "intermediate-owner-fence";
        let mirror = CursorMirror::default();
        cursor_seed(&db, entry, &mirror).await?;
        seed_floor(&db, entry, "899901").await?;
        mirror.install(40, Some(&event(entry)));
        let pool = peer(&db).await?;
        let store = AuditStore::new(&db.pool);
        let claim = store.claim(entry).await?.unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        mirror.override_read(
            Some("899801"),
            CursorRead::Pause(entered.clone(), release.clone()),
        );
        let service = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config());
        let start = mirror.requests().len();
        let mutate = async {
            entered.notified().await;
            let other = AuditStore::new(&pool);
            let mut replacement = None;
            if fault == "accepted" {
                assert!(other.note_accepted(&claim, "895100").await?);
            } else {
                db.expire(entry).await?;
                if fault == "replaced" {
                    replacement = Some(other.claim(entry).await?.unwrap());
                }
            }
            // Includes state, cursor, token, generation, lease, attempt and
            // acceptance evidence; do not print ownership capabilities.
            let snapshot: serde_json::Value = sqlx::query_scalar(
                "SELECT to_jsonb(a) FROM operational_audit_log a WHERE entry_id = $1",
            )
            .bind(entry)
            .fetch_one(&pool)
            .await?;
            release.notify_one();
            Ok::<_, Box<dyn std::error::Error>>((snapshot, replacement))
        };
        let (result, snapshot) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(service.deliver(&claim), mutate)
        })
        .await?;
        let (snapshot, replacement) = snapshot?;
        assert_eq!(result?, DeliverOutcome::Unclaimed, "{fault}");
        assert_eq!(
            mirror.requests()[start..],
            [Some("899901".to_owned()), Some("899801".to_owned())],
            "{fault}: lost ownership must stop at page 2, not just reject the final checkpoint"
        );
        let after: serde_json::Value = sqlx::query_scalar(
            "SELECT to_jsonb(a) FROM operational_audit_log a WHERE entry_id = $1",
        )
        .bind(entry)
        .fetch_one(&pool)
        .await?;
        assert!(after == snapshot, "{fault}: authoritative row was mutated");
        assert_eq!(cursor(&pool, entry).await?.as_deref(), Some("899901"));
        if let Some(replacement) = replacement {
            assert!(
                store.renew(&replacement).await?,
                "replacement keeps ownership"
            );
        }
        if fault == "accepted" {
            assert!(
                store.complete(&claim).await?,
                "accepted owner keeps completion"
            );
        }
        assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
        drop(service);
        pool.close().await;
        db.finish().await?;
    }
    Ok(())
}

/// Lexical comparison would reject 998401 as forward from 1000001. The
/// service must checkpoint numeric progress and another owner must use it.
#[tokio::test]
async fn numeric_floor_crosses_decimal_width_and_restarts_at_exact_checkpoint() -> TestResult {
    let db = TestDb::new().await?;
    let entry = "decimal-width-progress";
    let mirror = CursorMirror::default();
    cursor_seed(&db, entry, &mirror).await?;
    seed_floor(&db, entry, "1000001").await?;
    mirror.history.lock().unwrap().messages = (0..40)
        .flat_map(|page| noise_page(1_000_000 - page * 100))
        .collect();
    let original = AuditStore::new(&db.pool).get(entry).await?.unwrap();
    let service = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config());
    let start = mirror.requests().len();
    assert_eq!(
        service.deliver_entry(entry).await?,
        DeliverOutcome::ScanBoundReached
    );
    assert_eq!(mirror.requests().len() - start, 16);
    assert_eq!(mirror.requests()[start].as_deref(), Some("1000001"));
    assert_eq!(mirror.requests()[start + 15].as_deref(), Some("998501"));
    assert_eq!(cursor(&db.pool, entry).await?.as_deref(), Some("998401"));
    assert_released(&db.pool, entry).await?;
    drop(service);
    let pool = peer(&db).await?;
    let restarted = AuditMirrorService::new(AuditStore::new(&pool), mirror.clone(), config());
    let start = mirror.requests().len();
    assert_eq!(
        restarted.deliver_entry(entry).await?,
        DeliverOutcome::ScanBoundReached
    );
    assert_eq!(mirror.requests().len() - start, 16);
    assert_eq!(mirror.requests()[start].as_deref(), Some("998401"));
    assert_eq!(cursor(&pool, entry).await?.as_deref(), Some("996801"));
    assert_released(&pool, entry).await?;
    let after = AuditStore::new(&pool).get(entry).await?.unwrap();
    assert_eq!(after.search_before, original.search_before);
    assert_eq!(after.nonce, original.nonce);
    assert_eq!(after.attempted_at, original.attempted_at);
    assert_eq!(after.attempts, 1);
    assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
    drop(restarted);
    pool.close().await;
    db.finish().await
}

#[tokio::test]
async fn yielded_capability_cannot_advance_cursor_or_release_replacement() -> TestResult {
    let db = TestDb::new().await?;
    let entry = "reused-cursor-owner";
    let mirror = CursorMirror::default();
    cursor_seed(&db, entry, &mirror).await?;
    seed_floor(&db, entry, "899901").await?;
    mirror.install(40, Some(&event(entry)));
    let store = AuditStore::new(&db.pool);
    let claim = store.claim(entry).await?.unwrap();
    let service = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config());
    assert_eq!(
        service.deliver(&claim).await?,
        DeliverOutcome::ScanBoundReached
    );
    assert_eq!(cursor(&db.pool, entry).await?.as_deref(), Some("898301"));
    assert_released(&db.pool, entry).await?;
    let replacement = store.claim(entry).await?.unwrap();
    let before = store.get(entry).await?.unwrap();
    assert_eq!(service.deliver(&claim).await?, DeliverOutcome::Unclaimed);
    assert_eq!(store.get(entry).await?.unwrap(), before);
    assert_eq!(cursor(&db.pool, entry).await?.as_deref(), Some("898301"));
    assert!(
        store.renew(&replacement).await?,
        "replacement must still own the lease"
    );
    assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
    db.finish().await
}

#[tokio::test]
async fn send_and_accepted_claims_do_not_create_scan_cursor() -> TestResult {
    let db = TestDb::new().await?;
    let entry = "send-does-not-scan";
    let mirror = CursorMirror::default();
    let store = AuditStore::new(&db.pool);
    let service = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config());
    service.record(&event(entry)).await?;
    let send = store.claim(entry).await?.unwrap();
    assert_eq!(
        send.intent(),
        two_bot_core::audit_store::DeliveryIntent::Send
    );
    assert_eq!(service.deliver(&send).await?, DeliverOutcome::Ambiguous);
    assert_eq!(cursor(&db.pool, entry).await?, None);
    db.expire(entry).await?;
    let claim = store.claim(entry).await?.unwrap();
    assert!(store.note_accepted(&claim, "640").await?);
    let accepted = store.get(entry).await?.unwrap();
    let timed = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config())
        .with_reconcile_scan_deadline(Instant::now() - Duration::from_secs(1));
    assert_eq!(timed.deliver(&claim).await?, DeliverOutcome::Unclaimed);
    assert_eq!(store.get(entry).await?.unwrap(), accepted);
    assert_eq!(cursor(&db.pool, entry).await?, None);
    assert!(store.complete(&claim).await?);
    db.expire(entry).await?;
    assert!(store.claim(entry).await?.is_none());
    assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
    db.finish().await
}

#[tokio::test]
async fn marker_on_last_budget_page_is_checked_before_saving_its_floor() -> TestResult {
    let db = TestDb::new().await?;
    let entry = "marker-at-page-sixteen";
    let mirror = CursorMirror::default();
    cursor_seed(&db, entry, &mirror).await?;
    mirror.install(16, None);
    // A valid marker is the oldest item of the final full page. Saving that
    // floor before marker matching would permanently skip the only evidence.
    *mirror.history.lock().unwrap().messages.last_mut().unwrap() = marked("898401", &event(entry));
    let start = mirror.requests().len();
    let service = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config());
    assert_eq!(
        service.deliver_entry(entry).await?,
        DeliverOutcome::Reconciled {
            message_id: "898401".to_owned()
        }
    );
    assert_eq!(mirror.requests().len() - start, 16);
    let row = AuditStore::new(&db.pool).get(entry).await?.unwrap();
    assert_eq!(row.state, DeliveryState::Delivered);
    assert_eq!(row.attempts, 1);
    assert!(row.accepted_at.is_some());
    assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
    db.finish().await
}

#[tokio::test]
async fn resumed_terminal_and_invalid_history_keep_the_original_fences() -> TestResult {
    for case in [
        "empty",
        "below-boundary",
        "permission",
        "invalid-author",
        "non-decreasing",
        "foreign-marker",
    ] {
        let db = TestDb::new().await?;
        let entry = "resumed-history-fence";
        let mirror = CursorMirror::default();
        cursor_seed(&db, entry, &mirror).await?;
        seed_floor(&db, entry, "899901").await?;
        if case == "below-boundary" {
            sqlx::query("UPDATE operational_audit_log SET delivery_search_before = '899850' WHERE entry_id = $1")
                .bind(entry).execute(&db.pool).await?;
        }
        let original = AuditStore::new(&db.pool).get(entry).await?.unwrap();
        let response = match case {
            "empty" => Ok(vec![]),
            "below-boundary" => Ok(noise_page(899_800)),
            "permission" => Err(MirrorError::Rejected(
                "synthetic permission rejection".to_owned(),
            )),
            "invalid-author" => {
                let mut page = noise_page(899_900);
                page[0] = marked("899900", &event(entry));
                page[99].author_id = "invalid".to_owned();
                Ok(page)
            }
            "non-decreasing" => Ok(noise_page(899_901)),
            "foreign-marker" => Ok(vec![marked("899900", &event("another-entry"))]),
            _ => unreachable!(),
        };
        mirror.override_read(Some("899901"), CursorRead::Reply(response));
        let service = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config());
        let start = mirror.requests().len();
        let outcome = service.deliver_entry(entry).await?;
        let expected = match case {
            "permission" => DeliverOutcome::Quarantined(QuarantineReason::PermissionRevoked),
            "invalid-author" | "non-decreasing" => DeliverOutcome::Ambiguous,
            _ => DeliverOutcome::Quarantined(QuarantineReason::MarkerMissing),
        };
        assert_eq!(outcome, expected, "{case}");
        assert_eq!(
            mirror.requests()[start..],
            [Some("899901".to_owned())],
            "{case} must inspect only the requested resumed page"
        );
        let row = AuditStore::new(&db.pool).get(entry).await?.unwrap();
        assert_eq!(
            cursor(&db.pool, entry).await?.as_deref(),
            Some("899901"),
            "{case} cannot establish an unverified floor"
        );
        assert_eq!(row.search_before, original.search_before);
        assert_eq!(row.nonce, original.nonce);
        assert_eq!(row.attempts, 1);
        assert!(
            row.mirror_message_id.is_none(),
            "{case} cannot adopt invalid/foreign evidence"
        );
        assert!(row.accepted_at.is_none());
        assert_eq!(mirror.posts.load(Ordering::Relaxed), 1);
        db.finish().await?;
    }
    Ok(())
}
