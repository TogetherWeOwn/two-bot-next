//! Reconcile-bound acceptance: the boundary scan stops at a page cap.
//!
//! RED now (behavioral): `deliver_reconcile` pages `channel_history` until the
//! marker is proven, the floor drops, or history runs out, so the deep
//! fixture below is paged past any bound (49 reads) and ends quarantined as
//! `MarkerMissing` — a merely-unscanned row is terminally held. Uses only the
//! current public API, so this file compiles on the unmodified tree; the RED
//! is assertion failures, not a build failure.
//!
//! DB access is only the fixed approved test services (agent-testdb / CI
//! Postgres), empty test password, isolated schema per test. The mirror is a
//! scripted in-memory double — no Discord endpoint is contacted.
//! Run: `cargo test -p two-bot-core --features db --test reconcile_bound_acceptance --locked -- --ignored`
//!
//! Builder contract: give `deliver_reconcile` a documented
//! `RECONCILE_MAX_PAGES` cap of at most 16 and stop with the fixed
//! `DeliverOutcome::ScanBoundReached` marker when the cap is reached, leaving
//! the row reconcile-eligible for the next owner (boundary kept, no
//! quarantine, no second POST). Marker identity, floor and empty-history
//! outcomes within the bound are unchanged. Fetch-then-count or
//! count-then-fetch both satisfy the `CAP_SLACK` bound below. If the cap, the
//! marker, or the eligibility needs to change, renegotiate on the card —
//! never edit this file to loosen it.
//!
//! NOTE (builder): this target needs the same CI step as the other ignored
//! core suites (check.yml's audit step runs `--test audit_store --test
//! audit_service ... -- --ignored`): add `--test reconcile_bound_acceptance`
//! there in the implementation PR. This file must stay untouched.
#![cfg(feature = "db")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        Ok(Self {
            admin,
            pool,
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

#[allow(dead_code)]
#[derive(Debug)]
struct Post {
    channel_id: String,
    content: String,
    nonce: String,
}

#[derive(Clone, Default)]
struct ScriptMirror {
    posts: Arc<Mutex<Vec<Post>>>,
    post_script: Arc<Mutex<VecDeque<PostResult>>>,
    documents: Arc<Mutex<VecDeque<DocResult>>>,
    history: Arc<Mutex<VecDeque<PageResult>>>,
    history_reads: Arc<AtomicU64>,
}

impl ScriptMirror {
    fn post_count(&self) -> usize {
        self.posts.lock().unwrap().len()
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
        self.posts.lock().unwrap().push(Post {
            channel_id: channel_id.to_owned(),
            content: content.to_owned(),
            nonce: nonce.to_owned(),
        });
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
    mirror
        .history
        .lock()
        .unwrap()
        .push_back(Ok(vec![marked("640", &ev)]));
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
