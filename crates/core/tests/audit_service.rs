//! TOG-10345 mirror-service acceptance against explicit Postgres plus a
//! scripted in-memory `AuditMirror`. Fault injection covers accepted-post
//! ack loss, restart, timeout, stale claims, nonce reuse, history ambiguity,
//! privacy/permission refusal and the halt window between claim and POST.
//! cargo test -p two-bot-core --features db --test audit_service --locked -- --ignored
//!
//! DB access is only the fixed approved test services (agent-testdb /
//! CI Postgres), empty test password, isolated schema per test. Auth or
//! ownership failures propagate; no alternate credentials are tried. The
//! mirror is in-process — no Discord endpoint is contacted.
#![cfg(feature = "db")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, QueryBuilder};
use two_bot_core::audit::{
    delivery_nonce, format_audit_event, AuditChannelIds, AuditEvent, AuditKind, KillSwitchLog,
};
use two_bot_core::audit_mirror::{AuditMirror, MirrorChannel, MirrorError, MirrorMessage};
use two_bot_core::audit_service::{
    AuditMirrorService, DeliverOutcome, MirrorConfig, RecordOutcome,
};
use two_bot_core::audit_store::{AuditStore, DeliveryState, QuarantineReason};

type TestResult = Result<(), Box<dyn std::error::Error>>;
const MIGRATION: &str = include_str!("../../cutover/migrations/0340_operational_audit.sql");
static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

const GUILD: &str = "18446744073709551615";
const AUDIT_CH: &str = "1111";
const VOICE_CH: &str = "2222";
const MOD_CH: &str = "3333";
const BOT: &str = "9999";

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
            "audit_svc_{}_{}_{}",
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

    /// A second pool on the same schema — the "other worker"/operator side.
    async fn peer(&self) -> Result<PgPool, sqlx::Error> {
        Self::connect(&self.options, &self.schema).await
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

/// One recorded post attempt (channel, content, nonce).
#[derive(Debug)]
struct Post {
    channel_id: String,
    content: String,
    nonce: String,
}

/// In-memory mirror double: scripted per-call response queues plus recorded
/// posts and a history-read counter. Emptied queues fall back to a private
/// in-guild channel, an empty history page, and an accepted post. Cheaply
/// cloneable so the service and the test share the same script.
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
}

fn private_document(guild_id: &str) -> MirrorChannel {
    MirrorChannel {
        guild_id: guild_id.to_owned(),
        everyone: Some(two_bot_core::audit_mirror::MirrorOverwrite {
            allow: "0".to_owned(),
            // VIEW_CHANNEL denied to @everyone: the private-mirror gate.
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

// ----------------------------------------------------------------- tests --

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn prepared_owner_losing_lease_or_replaced_by_quarantine_never_posts() -> TestResult {
    for replace in [false, true] {
        let db = TestDb::new().await?;
        let (store, service, mirror) = service_for(&db.pool);
        let peer = db.peer().await?;
        let gate_pool = peer.clone();
        let replacement_mirror = mirror.clone();
        let service = service.with_pre_send_gate(move || {
            let pool = gate_pool.clone();
            let mirror = replacement_mirror.clone();
            Box::pin(async move {
                sqlx::query("UPDATE operational_audit_log SET delivery_lease_until = clock_timestamp() - interval '1 second' WHERE entry_id = 'expired-prepared'")
                    .execute(&pool).await.unwrap();
                if replace {
                    let recovery = AuditMirrorService::new(AuditStore::new(&pool), mirror, config());
                    assert_eq!(recovery.deliver_entry("expired-prepared").await.unwrap(),
                        DeliverOutcome::Quarantined(QuarantineReason::MarkerMissing));
                }
            })
        });
        service.record(&event("expired-prepared")).await?;
        assert_eq!(
            service.deliver_entry("expired-prepared").await?,
            DeliverOutcome::Unclaimed
        );
        assert_eq!(mirror.post_count(), 0, "expired prepared owner cannot POST");
        let row = store.get("expired-prepared").await?.unwrap();
        assert_eq!(
            row.state,
            if replace {
                DeliveryState::Quarantined
            } else {
                DeliveryState::Delivering
            }
        );
        assert!(row.mirror_message_id.is_none());
        assert_eq!(row.search_before.as_deref(), Some("0"));
        peer.close().await;
        db.finish().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn interrupted_dedup_adoption_recovers_the_known_mirror() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    let ev = event("adoption-crash");
    service.record(&ev).await?;
    mirror
        .history
        .lock()
        .unwrap()
        .push_back(Ok(vec![marked("640", &ev)]));
    // Reject only the acceptance write, after prepare_send has committed.
    sqlx::raw_sql("CREATE FUNCTION fail_acceptance() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.mirror_message_id IS NOT NULL THEN RAISE EXCEPTION 'injected ack loss'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER fail_acceptance BEFORE UPDATE ON operational_audit_log FOR EACH ROW EXECUTE FUNCTION fail_acceptance();")
        .execute(&db.pool).await?;
    assert!(service.deliver_entry("adoption-crash").await.is_err());
    let row = store.get("adoption-crash").await?.unwrap();
    assert_eq!(row.search_before.as_deref(), Some("640"));
    assert!(row.mirror_message_id.is_none());
    sqlx::query("DROP TRIGGER fail_acceptance ON operational_audit_log")
        .execute(&db.pool)
        .await?;
    db.expire("adoption-crash").await?;
    mirror
        .history
        .lock()
        .unwrap()
        .push_back(Ok(vec![marked("640", &ev)]));
    let restarted = AuditMirrorService::new(AuditStore::new(&db.pool), mirror.clone(), config());
    assert_eq!(
        restarted.deliver_entry("adoption-crash").await?,
        DeliverOutcome::Reconciled {
            message_id: "640".to_owned()
        }
    );
    assert_eq!(mirror.post_count(), 0);
    assert_eq!(
        store.get("adoption-crash").await?.unwrap().state,
        DeliveryState::Delivered
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn unreadable_history_defers_then_preserves_recovery_until_healthy() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    let ev = event("unreadable-history");
    service.record(&ev).await?;
    mirror
        .history
        .lock()
        .unwrap()
        .push_back(Err(MirrorError::Uncertain(
            "malformed successful history page".to_owned(),
        )));
    assert_eq!(
        service.deliver_entry(&ev.entry_id).await?,
        DeliverOutcome::Deferred
    );
    assert_eq!(mirror.post_count(), 0);
    assert!(store
        .get(&ev.entry_id)
        .await?
        .unwrap()
        .search_before
        .is_none());
    sqlx::query("UPDATE operational_audit_log SET delivery_deferred_until = clock_timestamp() - interval '1 second' WHERE entry_id = $1")
        .bind(&ev.entry_id).execute(&db.pool).await?;
    mirror
        .post_script
        .lock()
        .unwrap()
        .push_back(Err(MirrorError::Uncertain(
            "accepted but timed out".to_owned(),
        )));
    assert_eq!(
        service.deliver_entry(&ev.entry_id).await?,
        DeliverOutcome::Ambiguous
    );
    db.expire(&ev.entry_id).await?;
    mirror
        .history
        .lock()
        .unwrap()
        .push_back(Err(MirrorError::Uncertain(
            "malformed successful history page".to_owned(),
        )));
    assert_eq!(
        service.deliver_entry(&ev.entry_id).await?,
        DeliverOutcome::Ambiguous
    );
    let row = store.get(&ev.entry_id).await?.unwrap();
    assert_eq!(row.state, DeliveryState::Pending);
    assert_eq!(row.search_before.as_deref(), Some("0"));
    db.expire(&ev.entry_id).await?;
    mirror
        .history
        .lock()
        .unwrap()
        .push_back(Ok(vec![marked("640", &ev)]));
    assert_eq!(
        service.deliver_entry(&ev.entry_id).await?,
        DeliverOutcome::Reconciled {
            message_id: "640".to_owned()
        }
    );
    assert_eq!(
        mirror.post_count(),
        1,
        "healthy evidence recovers without a second POST"
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn queued_row_posts_once_with_stored_nonce_and_marker() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    let ev = event("happy");
    assert_eq!(
        service.record(&ev).await?,
        RecordOutcome::Queued {
            mirror_channel_id: AUDIT_CH.to_owned()
        }
    );
    assert_eq!(
        service.deliver_entry("happy").await?,
        DeliverOutcome::Delivered {
            message_id: "900000".to_owned()
        }
    );
    let (channel, nonce, content) = {
        let posts = mirror.posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        (
            posts[0].channel_id.clone(),
            posts[0].nonce.clone(),
            posts[0].content.clone(),
        )
    };
    assert_eq!(channel, AUDIT_CH);
    assert_eq!(
        nonce,
        delivery_nonce("happy"),
        "the stored deterministic nonce is enforced"
    );
    assert!(
        content.starts_with(&ev.identity()),
        "post carries the audit identity marker"
    );
    let row = store.get("happy").await?.unwrap();
    assert_eq!(row.state, DeliveryState::Delivered);
    assert_eq!(row.mirror_message_id.as_deref(), Some("900000"));
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn record_routes_loop_store_only_and_wrong_guild_without_mirror() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);

    // Source IS a mirror channel: tamper-loop suppression.
    let mut looped = event("loop");
    looped.source_channel_id = Some(AUDIT_CH.to_owned());
    assert_eq!(
        service.record(&looped).await?,
        RecordOutcome::TamperLoop { stored: true }
    );
    // Replay of the same row is still a loop, never re-queued.
    assert_eq!(
        service.record(&looped).await?,
        RecordOutcome::TamperLoop { stored: false }
    );

    // No destination for the family: store-only. Voice channel cleared.
    let no_voice = MirrorConfig {
        channels: AuditChannelIds {
            audit: None,
            voice: None,
            moderation: None,
        },
        ..config()
    };
    let bare =
        AuditMirrorService::new(AuditStore::new(&db.pool), ScriptMirror::default(), no_voice);
    assert_eq!(
        bare.record(&event("no-dest")).await?,
        RecordOutcome::StoreOnly
    );

    // Destination outside the event's guild: never mirrored.
    let foreign = MirrorConfig {
        mirror_guild_id: Some("00000000000000000099".to_owned()),
        ..config()
    };
    let fenced =
        AuditMirrorService::new(AuditStore::new(&db.pool), ScriptMirror::default(), foreign);
    assert_eq!(
        fenced.record(&event("foreign")).await?,
        RecordOutcome::WrongGuild {
            requested_channel_id: AUDIT_CH.to_owned()
        }
    );

    for entry in ["loop", "no-dest", "foreign"] {
        let row = store.get(entry).await?.unwrap();
        assert_eq!(row.state, DeliveryState::None, "{entry} must be store-only");
        assert!(row.mirror_channel_id.is_none() || row.mirror_channel_id.as_deref() == Some(""));
        assert!(service.deliver_entry(entry).await? == DeliverOutcome::Unclaimed);
    }
    assert_eq!(mirror.post_count(), 0);
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn failed_record_never_reaches_the_mirror() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    let mut invalid = event("bad");
    invalid.metadata_json = "[]".to_owned();
    assert!(service.record(&invalid).await.is_err());
    assert_eq!(mirror.post_count(), 0);
    assert!(store.get("bad").await?.is_none());
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn uncertain_post_reconciles_found_mirror_without_resend() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    let ev = event("ack-loss");
    service.record(&ev).await?;
    // Send-time history: newest row "500" seeds boundary "501"; the POST
    // outcome is ambiguous (accepted-then-timeout shape).
    mirror
        .history
        .lock()
        .unwrap()
        .push_back(Ok(vec![MirrorMessage {
            id: "500".to_owned(),
            author_id: "5555".to_owned(),
            content: "unrelated".to_owned(),
        }]));
    mirror
        .post_script
        .lock()
        .unwrap()
        .push_back(Err(MirrorError::Uncertain("timeout".to_owned())));
    assert_eq!(
        service.deliver_entry("ack-loss").await?,
        DeliverOutcome::Ambiguous
    );
    let row = store.get("ack-loss").await?.unwrap();
    assert_eq!(row.search_before.as_deref(), Some("501"));
    assert_eq!(row.state, DeliveryState::Pending);

    // Restart (lease expiry + fresh claim): reconciliation must find the
    // marked mirror at/after the boundary — a below-boundary twin does not
    // count — and must NOT post again.
    db.expire("ack-loss").await?;
    mirror.history.lock().unwrap().push_back(Ok(vec![
        marked("777", &ev),
        marked("400", &ev), // older marker below the boundary
    ]));
    assert_eq!(
        service.deliver_entry("ack-loss").await?,
        DeliverOutcome::Reconciled {
            message_id: "777".to_owned()
        }
    );
    assert_eq!(mirror.post_count(), 1, "no blind resend after ambiguity");
    let row = store.get("ack-loss").await?.unwrap();
    assert_eq!(row.state, DeliveryState::Delivered);
    assert_eq!(row.mirror_message_id.as_deref(), Some("777"));
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn reconcile_exhaustion_and_uncertain_reads_stay_bounded() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    let ev = event("lost-marker");
    service.record(&ev).await?;
    mirror
        .post_script
        .lock()
        .unwrap()
        .push_back(Err(MirrorError::Uncertain("reset".to_owned())));
    assert_eq!(
        service.deliver_entry("lost-marker").await?,
        DeliverOutcome::Ambiguous
    );
    db.expire("lost-marker").await?;

    // Ambiguous history read: stays fenced, never quarantined off an error.
    mirror
        .history
        .lock()
        .unwrap()
        .push_back(Err(MirrorError::Uncertain("pool reset".to_owned())));
    assert_eq!(
        service.deliver_entry("lost-marker").await?,
        DeliverOutcome::Ambiguous
    );
    let row = store.get("lost-marker").await?.unwrap();
    assert_eq!(row.state, DeliveryState::Pending);
    db.expire("lost-marker").await?;

    // History proves absence: the page is empty and the durable boundary
    // ("0", seeded on an empty channel) admits everything — a missing marker
    // quarantines rather than resending.
    let row = store.get("lost-marker").await?.unwrap();
    assert_eq!(row.search_before.as_deref(), Some("0"));
    mirror.history.lock().unwrap().push_back(Ok(vec![]));
    assert_eq!(
        service.deliver_entry("lost-marker").await?,
        DeliverOutcome::Quarantined(QuarantineReason::MarkerMissing)
    );
    assert_eq!(mirror.post_count(), 1);
    assert_eq!(
        store.get("lost-marker").await?.unwrap().state,
        DeliveryState::Quarantined
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn definite_rejection_retries_and_uncertain_restart_keeps_evidence() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    service.record(&event("retry")).await?;
    mirror
        .post_script
        .lock()
        .unwrap()
        .push_back(Err(MirrorError::Rejected("missing access".to_owned())));
    assert_eq!(
        service.deliver_entry("retry").await?,
        DeliverOutcome::Rejected
    );
    let row = store.get("retry").await?.unwrap();
    assert_eq!(row.state, DeliveryState::Pending);
    assert_eq!(row.attempts, 1);
    assert!(
        row.search_before.is_none(),
        "definite rejection clears the boundary"
    );

    // Next claim re-sends; the stored nonce survives.
    assert_eq!(
        service.deliver_entry("retry").await?,
        DeliverOutcome::Delivered {
            message_id: "900000".to_owned()
        }
    );
    {
        let posts = mirror.posts.lock().unwrap();
        assert_eq!(posts.len(), 2);
        assert!(posts.iter().all(|p| p.nonce == delivery_nonce("retry")));
    }
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn recorded_acceptance_finishes_without_history_on_restart() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    service.record(&event("accepted-crash")).await?;
    // Drive the protocol manually up to accepted-but-uncompleted: the crash
    // window between note_accepted and complete.
    let claim = store.claim("accepted-crash").await?.unwrap();
    assert_eq!(
        store.prepare_send(&claim, "501").await?,
        two_bot_core::audit_store::PrepareSend::Prepared
    );
    assert!(store.note_accepted(&claim, "777").await?);
    db.expire("accepted-crash").await?;

    let recovery = store.claim("accepted-crash").await?.unwrap();
    assert_eq!(
        service.deliver(&recovery).await?,
        DeliverOutcome::Reconciled {
            message_id: "777".to_owned()
        }
    );
    assert_eq!(
        mirror.history_reads.load(Ordering::Relaxed),
        0,
        "a recorded accepted id completes with no Discord reads"
    );
    assert_eq!(mirror.post_count(), 0);
    assert_eq!(
        store.get("accepted-crash").await?.unwrap().state,
        DeliveryState::Delivered
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn halt_before_send_releases_without_post_and_recovers() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    let operator_pool = db.peer().await?;
    let operator = AuditStore::new(&operator_pool);
    service.record(&event("held")).await?;
    let claim = store.claim("held").await?.unwrap();
    operator.engage_halt("18446744073709551611").await?;
    assert_eq!(service.deliver(&claim).await?, DeliverOutcome::Held);
    let row = store.get("held").await?.unwrap();
    assert_eq!(row.state, DeliveryState::Pending);
    assert_eq!(row.attempts, 0, "a pre-prepare halt never counts a POST");
    assert!(row.search_before.is_none());
    assert_eq!(mirror.post_count(), 0);
    assert!(
        service.transitions().contains(&KillSwitchLog::Engaged),
        "halt engagement is logged"
    );
    // While halted the row is not claimable again.
    assert!(store.claim("held").await?.is_none());
    operator.disengage_halt().await?;
    assert_eq!(
        service.deliver_entry("held").await?,
        DeliverOutcome::Delivered {
            message_id: "900000".to_owned()
        }
    );
    assert_eq!(mirror.post_count(), 1);
    operator_pool.close().await;
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn halt_between_prepare_and_post_records_non_acceptance() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    let operator_pool = db.peer().await?;
    let gate_pool = operator_pool.clone();
    let gate_fired = Arc::new(AtomicBool::new(false));
    let service = service.with_pre_send_gate(move || {
        let pool = gate_pool.clone();
        let fired = gate_fired.clone();
        Box::pin(async move {
            // The operator switch lands once, inside the prepare→POST window.
            if !fired.swap(true, Ordering::SeqCst) {
                let _ = AuditStore::new(&pool)
                    .engage_halt("18446744073709551611")
                    .await;
            }
        })
    });
    service.record(&event("mid-halt")).await?;
    assert_eq!(
        service.deliver_entry("mid-halt").await?,
        DeliverOutcome::Held
    );
    assert_eq!(mirror.post_count(), 0, "the pre-POST check caught the halt");
    let row = store.get("mid-halt").await?.unwrap();
    assert_eq!(row.state, DeliveryState::Pending);
    assert_eq!(
        row.attempts, 1,
        "preparation counted, then definite non-acceptance"
    );
    // Definite non-acceptance clears the boundary; the row is retained.
    assert!(row.search_before.is_none());
    // While halted it cannot be reclaimed.
    assert!(store.claim("mid-halt").await?.is_none());
    AuditStore::new(&operator_pool).disengage_halt().await?;
    assert_eq!(
        service.deliver_entry("mid-halt").await?,
        DeliverOutcome::Delivered {
            message_id: "900000".to_owned()
        }
    );
    operator_pool.close().await;
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn privacy_and_guild_preflight_failures_quarantine() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);

    // Destination became public: @everyone can view again.
    service.record(&event("public")).await?;
    mirror
        .documents
        .lock()
        .unwrap()
        .push_back(Ok(MirrorChannel {
            guild_id: GUILD.to_owned(),
            everyone: Some(two_bot_core::audit_mirror::MirrorOverwrite {
                allow: "0".to_owned(),
                deny: "0".to_owned(),
            }),
        }));
    assert_eq!(
        service.deliver_entry("public").await?,
        DeliverOutcome::Quarantined(QuarantineReason::PermissionRevoked)
    );

    // Channel document reports another guild entirely.
    service.record(&event("mismatch")).await?;
    mirror
        .documents
        .lock()
        .unwrap()
        .push_back(Ok(MirrorChannel {
            guild_id: "00000000000000000077".to_owned(),
            everyone: Some(two_bot_core::audit_mirror::MirrorOverwrite {
                allow: "0".to_owned(),
                deny: "1024".to_owned(),
            }),
        }));
    assert_eq!(
        service.deliver_entry("mismatch").await?,
        DeliverOutcome::Quarantined(QuarantineReason::EvidenceConflict)
    );

    // Permission vanished outright (404/403 on the document read).
    service.record(&event("gone")).await?;
    mirror
        .documents
        .lock()
        .unwrap()
        .push_back(Err(MirrorError::Rejected("unknown channel".to_owned())));
    assert_eq!(
        service.deliver_entry("gone").await?,
        DeliverOutcome::Quarantined(QuarantineReason::PermissionRevoked)
    );

    assert_eq!(mirror.post_count(), 0);
    for entry in ["public", "mismatch", "gone"] {
        assert_eq!(
            store.get(entry).await?.unwrap().state,
            DeliveryState::Quarantined,
            "{entry} must be quarantined"
        );
    }
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn transient_preflight_defers_without_attempt_or_post() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    service.record(&event("deferred")).await?;
    mirror
        .documents
        .lock()
        .unwrap()
        .push_back(Err(MirrorError::Uncertain("socket reset".to_owned())));
    assert_eq!(
        service.deliver_entry("deferred").await?,
        DeliverOutcome::Deferred
    );
    let row = store.get("deferred").await?.unwrap();
    assert_eq!(row.attempts, 0, "preflight deferral is not a POST attempt");
    assert_eq!(mirror.post_count(), 0);
    assert!(
        !store.pending_ids().await?.contains(&"deferred".to_owned()),
        "deferred row hides from queue discovery during backoff"
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn dedup_scan_adopts_existing_mirror_without_posting() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    let ev = event("dup");
    service.record(&ev).await?;
    // The mirror already carries this event's marker (a pre-restart send or
    // a peer worker's post): dedup must adopt it, never double-post.
    mirror
        .history
        .lock()
        .unwrap()
        .push_back(Ok(vec![marked("640", &ev)]));
    assert_eq!(
        service.deliver_entry("dup").await?,
        DeliverOutcome::Reconciled {
            message_id: "640".to_owned()
        }
    );
    assert_eq!(mirror.post_count(), 0);
    let row = store.get("dup").await?.unwrap();
    assert_eq!(row.state, DeliveryState::Delivered);
    assert_eq!(row.mirror_message_id.as_deref(), Some("640"));
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn stale_claim_is_fenced_and_sends_nothing() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    service.record(&event("stale")).await?;
    let old = store.claim("stale").await?.unwrap();
    db.expire("stale").await?;
    let current = store.claim("stale").await?.unwrap();
    // The expired owner's claim fences out at preparation: no POST, no writes.
    assert_eq!(service.deliver(&old).await?, DeliverOutcome::Unclaimed);
    assert_eq!(mirror.post_count(), 0);
    let row = store.get("stale").await?.unwrap();
    assert_eq!(row.attempts, 0);
    assert_eq!(row.state, DeliveryState::Delivering);
    // The live claim still owns the row and delivers normally.
    assert_eq!(
        service.deliver(&current).await?,
        DeliverOutcome::Delivered {
            message_id: "900000".to_owned()
        }
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn drain_pending_delivers_the_bounded_batch() -> TestResult {
    let db = TestDb::new().await?;
    let (store, service, mirror) = service_for(&db.pool);
    for i in 0..3 {
        service.record(&event(&format!("drain-{i}"))).await?;
    }
    let report = service.drain_pending().await?;
    assert!(report.failed.is_empty());
    assert_eq!(report.deliveries.len(), 3);
    assert!(report
        .deliveries
        .iter()
        .all(|(_, o)| matches!(o, DeliverOutcome::Delivered { .. })));
    assert_eq!(mirror.post_count(), 3);
    for i in 0..3 {
        assert_eq!(
            store.get(&format!("drain-{i}")).await?.unwrap().state,
            DeliveryState::Delivered
        );
    }
    db.finish().await
}
