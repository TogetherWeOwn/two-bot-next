//! Protected acceptance for mirror-history evidence at the real service/store
//! boundary. Removing whole-page validation or backwards-progress fencing must
//! fail the negative matrices; rejecting healthy evidence must fail controls.
//! No live Discord. Database fixtures use only the shared disposable DB guard.
#![cfg(feature = "db")]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::PgPool;
use two_bot_core::audit::{
    delivery_nonce, format_audit_event, AuditChannelIds, AuditEvent, AuditKind,
};
use two_bot_core::audit_mirror::{
    AuditMirror, MirrorChannel, MirrorError, MirrorMessage, MirrorOverwrite,
};
use two_bot_core::audit_service::{AuditMirrorService, DeliverOutcome, MirrorConfig};
use two_bot_core::audit_store::{
    AuditStore, DeliveryIntent, DeliveryState, PrepareSend, QuarantineReason,
};
use two_bot_testsupport::TestDatabase;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type PageResult = Result<Vec<MirrorMessage>, MirrorError>;
type HistoryRead = (Option<String>, u8);
const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const BOT: &str = "9999";

#[derive(Clone, Default)]
struct Mirror {
    pages: Arc<Mutex<VecDeque<PageResult>>>,
    reads: Arc<Mutex<Vec<HistoryRead>>>,
    posts: Arc<Mutex<Vec<String>>>,
    uncertain_post: bool,
    refused: bool,
}

impl Mirror {
    fn script(&self, pages: Vec<Vec<MirrorMessage>>) {
        *self.pages.lock().unwrap() = pages.into_iter().map(Ok).collect();
        self.reads.lock().unwrap().clear();
    }

    fn cursors(&self) -> Vec<Option<String>> {
        self.reads
            .lock()
            .unwrap()
            .iter()
            .map(|(cursor, _)| cursor.clone())
            .collect()
    }
}

impl AuditMirror for Mirror {
    async fn channel_document(&self, _: &str) -> Result<MirrorChannel, MirrorError> {
        if self.refused {
            return Err(MirrorError::Rejected("permission denied".to_owned()));
        }
        Ok(MirrorChannel {
            guild_id: GUILD.to_owned(),
            everyone: Some(MirrorOverwrite {
                allow: "0".to_owned(),
                deny: "1024".to_owned(),
            }),
        })
    }

    async fn channel_history(
        &self,
        channel: &str,
        before: Option<&str>,
        limit: u8,
    ) -> Result<Vec<MirrorMessage>, MirrorError> {
        assert_eq!(channel, CHANNEL);
        self.reads
            .lock()
            .unwrap()
            .push((before.map(str::to_owned), limit));
        if self.refused {
            return Err(MirrorError::Rejected("permission denied".to_owned()));
        }
        self.pages.lock().unwrap().pop_front().unwrap_or_else(|| {
            // A finite script bounds a broken renewal loop without hiding it:
            // acceptance asserts it stops BEFORE this sentinel read.
            Err(MirrorError::Uncertain(
                "fixture read budget exhausted".to_owned(),
            ))
        })
    }

    async fn post_mirror_checked<Fut, E>(
        &self,
        channel: &str,
        _: &str,
        nonce: &str,
        authorize: Fut,
    ) -> Result<Result<String, MirrorError>, E>
    where
        Fut: std::future::Future<Output = Result<(), E>> + Send,
        E: Send,
    {
        authorize.await?;
        assert_eq!(channel, CHANNEL);
        self.posts.lock().unwrap().push(nonce.to_owned());
        Ok(if self.uncertain_post {
            Err(MirrorError::Uncertain(
                "accepted then disconnected".to_owned(),
            ))
        } else {
            Ok("900000".to_owned())
        })
    }
}

async fn database() -> Result<Option<TestDatabase>, Box<dyn std::error::Error>> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        eprintln!("audit history service acceptance not executed: TWO_TEST_DATABASE_URL absent");
        return Ok(None);
    };
    Ok(Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations")).await?,
    ))
}

fn event(id: &str) -> AuditEvent {
    AuditEvent::new(
        id.to_owned(),
        AuditKind::MemberUpdate,
        GUILD.to_owned(),
        "2026-10-19T00:00:00Z".to_owned(),
    )
}

fn service(pool: &PgPool, mirror: Mirror) -> AuditMirrorService<Mirror> {
    AuditMirrorService::new(
        AuditStore::new(pool),
        mirror,
        MirrorConfig {
            channels: AuditChannelIds {
                audit: Some(CHANNEL.to_owned()),
                voice: None,
                moderation: None,
            },
            configured: vec![CHANNEL.to_owned()],
            mirror_guild_id: Some(GUILD.to_owned()),
            bot_user_id: BOT.to_owned(),
        },
    )
}

fn row(id: &str) -> MirrorMessage {
    MirrorMessage {
        id: id.to_owned(),
        author_id: BOT.to_owned(),
        content: "unrelated history-content-sentinel".to_owned(),
    }
}

fn marked(id: &str, ev: &AuditEvent) -> MirrorMessage {
    MirrorMessage {
        content: format_audit_event(ev),
        ..row(id)
    }
}

fn full_page() -> Vec<MirrorMessage> {
    (701..=800).rev().map(|id| row(&id.to_string())).collect()
}

// A fresh unattempted row and a recovery owner must reject the SAME complete
// page, including a valid-looking marker before or after the corrupt row.
fn invalid_pages(ev: &AuditEvent) -> Vec<(&'static str, Vec<Vec<MirrorMessage>>)> {
    let mut cases = vec![
        ("duplicate", vec![vec![row("700"), row("700")]]),
        ("reversed", vec![vec![row("600"), row("700")]]),
        (
            "interior-disorder",
            vec![vec![row("700"), row("600"), row("650"), row("550")]],
        ),
        ("zero", vec![vec![row("0")]]),
        ("leading-zero", vec![vec![row("0700")]]),
        ("signed", vec![vec![row("+700")]]),
        ("whitespace", vec![vec![row("700 ")]]),
        ("overflow", vec![vec![row("18446744073709551616")]]),
        ("empty", vec![vec![row("")]]),
        (
            "oversized",
            vec![(600..=700).rev().map(|id| row(&id.to_string())).collect()],
        ),
        (
            "cursor-equal",
            vec![full_page(), vec![marked("701", ev), row("650")]],
        ),
        (
            "cursor-newer",
            vec![full_page(), vec![marked("702", ev), row("650")]],
        ),
        ("repeated-full", vec![full_page(), full_page(), full_page()]),
    ];
    // An unordered full page must not claim it has crossed boundary 501.
    let mut false_floor = full_page();
    false_floor[50] = row("850");
    false_floor[99] = row("500");
    cases.push(("false-floor", vec![false_floor]));
    for (name, author) in [
        ("zero-author", "0"),
        ("noncanonical-author", "09999"),
        ("overflow-author", "18446744073709551616"),
        ("empty-author", ""),
        ("invalid-author", "invalid-identity-sentinel"),
    ] {
        let bad = MirrorMessage {
            author_id: author.to_owned(),
            ..row("600")
        };
        cases.push((name, vec![vec![marked("700", ev), bad]]));
    }
    cases.push((
        "marker-before-corruption",
        vec![vec![marked("700", ev), row("0")]],
    ));
    cases.push((
        "marker-after-corruption",
        vec![vec![row("0700"), marked("650", ev)]],
    ));
    cases
}

async fn retry_now(pool: &PgPool, id: &str) -> TestResult {
    sqlx::query("UPDATE operational_audit_log SET delivery_lease_until = clock_timestamp() - interval '1 second', delivery_deferred_until = clock_timestamp() - interval '1 second' WHERE entry_id = $1")
        .bind(id).execute(pool).await?;
    Ok(())
}

async fn deliver_bounded(
    svc: &AuditMirrorService<Mirror>,
    id: &str,
) -> Result<DeliverOutcome, Box<dyn std::error::Error>> {
    Ok(tokio::time::timeout(Duration::from_secs(10), svc.deliver_entry(id)).await??)
}

#[tokio::test]
async fn invalid_preflight_cannot_post_adopt_or_prepare_a_boundary() -> TestResult {
    let Some(db) = database().await? else {
        return Ok(());
    };
    let store = AuditStore::new(db.pool());
    let mut failures = Vec::new();
    for (index, (name, _)) in invalid_pages(&event("placeholder")).into_iter().enumerate() {
        let ev = event(&format!("preflight-{index}"));
        let (_, pages) = invalid_pages(&ev).remove(index);
        let max_reads = if pages.len() > 1 { 2 } else { 1 };
        let mirror = Mirror::default();
        mirror.script(pages);
        let svc = service(db.pool(), mirror.clone());
        svc.record(&ev).await?;
        let outcome = deliver_bounded(&svc, &ev.entry_id).await?;
        let saved = store.get(&ev.entry_id).await?.expect("recorded row");
        let reads = mirror.cursors();
        if outcome != DeliverOutcome::Deferred
            || !mirror.posts.lock().unwrap().is_empty()
            || saved.state != DeliveryState::Pending
            || saved.attempts != 0
            || saved.search_before.is_some()
            || saved.mirror_message_id.is_some()
            || saved.accepted_at.is_some()
            || reads.len() > max_reads
        {
            failures.push(format!("{name}: outcome={outcome:?}, posts={}, state={:?}, attempts={}, boundary={:?}, receipt={:?}, reads={reads:?}", mirror.posts.lock().unwrap().len(), saved.state, saved.attempts, saved.search_before, saved.mirror_message_id));
        }
        if saved.state == DeliveryState::Pending && saved.search_before.is_none() {
            retry_now(db.pool(), &ev.entry_id).await?;
            mirror.script(vec![vec![]]);
            assert_eq!(
                deliver_bounded(&svc, &ev.entry_id).await?,
                DeliverOutcome::Delivered {
                    message_id: "900000".to_owned()
                },
                "healthy preflight retry for {name}"
            );
            assert_eq!(
                mirror.posts.lock().unwrap().as_slice(),
                &[delivery_nonce(&ev.entry_id)]
            );
        }
        for sentinel in ["history-content-sentinel", "invalid-identity-sentinel"] {
            assert!(
                !saved.last_error.as_deref().unwrap_or("").contains(sentinel),
                "diagnostics must be metadata only"
            );
        }
    }
    db.close().await?;
    assert!(
        failures.is_empty(),
        "invalid preflight evidence was used:\n{}",
        failures.join("\n")
    );
    Ok(())
}

#[tokio::test]
async fn invalid_recovery_preserves_ambiguity_then_healthy_owner_reconciles_without_resend(
) -> TestResult {
    let Some(db) = database().await? else {
        return Ok(());
    };
    let store = AuditStore::new(db.pool());
    let mut failures = Vec::new();
    for (index, (name, _)) in invalid_pages(&event("placeholder")).into_iter().enumerate() {
        let ev = event(&format!("recovery-{index}"));
        let mirror = Mirror {
            uncertain_post: true,
            ..Mirror::default()
        };
        let svc = service(db.pool(), mirror.clone());
        svc.record(&ev).await?;
        mirror.script(vec![vec![row("500")]]);
        assert_eq!(
            deliver_bounded(&svc, &ev.entry_id).await?,
            DeliverOutcome::Ambiguous
        );
        let before = store.get(&ev.entry_id).await?.expect("prepared ambiguity");
        assert_eq!(before.search_before.as_deref(), Some("501"));
        assert_eq!(
            before.nonce.as_deref(),
            Some(delivery_nonce(&ev.entry_id).as_str())
        );
        retry_now(db.pool(), &ev.entry_id).await?;
        let stale = store
            .claim(&ev.entry_id)
            .await?
            .expect("old recovery owner");
        assert_eq!(stale.intent(), DeliveryIntent::Reconcile);
        retry_now(db.pool(), &ev.entry_id).await?;
        let (_, pages) = invalid_pages(&ev).remove(index);
        let max_reads = if pages.len() > 1 { 2 } else { 1 };
        mirror.script(pages);
        let outcome = deliver_bounded(&svc, &ev.entry_id).await?;
        let after = store.get(&ev.entry_id).await?.expect("ambiguity retained");
        let reads = mirror.cursors();
        let preserved = after.state == DeliveryState::Pending
            && after.search_before == before.search_before
            && after.nonce == before.nonce
            && after.attempts == before.attempts
            && after.attempted_at == before.attempted_at
            && after.mirror_message_id == before.mirror_message_id
            && after.accepted_at == before.accepted_at;
        if outcome != DeliverOutcome::Ambiguous || !preserved || reads.len() > max_reads {
            failures.push(format!("{name}: outcome={outcome:?}, state={:?}, boundary={:?}, receipt={:?}, preserved={preserved}, reads={reads:?}", after.state, after.search_before, after.mirror_message_id));
        }
        assert_eq!(
            mirror.posts.lock().unwrap().len(),
            1,
            "reconciliation never POSTs"
        );
        assert!(
            !store.note_accepted(&stale, "777").await?,
            "expired owner cannot replace evidence"
        );
        assert!(
            !store.complete(&stale).await?,
            "expired owner cannot complete"
        );
        if preserved {
            retry_now(db.pool(), &ev.entry_id).await?;
            mirror.script(vec![vec![marked("650", &ev), row("500")]]);
            // Fresh pool models process/owner replacement, not in-memory reset.
            let peer = db.independent_pool().await?;
            let restarted = service(&peer, mirror.clone());
            let recovered = deliver_bounded(&restarted, &ev.entry_id).await?;
            assert_eq!(
                recovered,
                DeliverOutcome::Reconciled {
                    message_id: "650".to_owned()
                },
                "healthy retry for {name}"
            );
            let delivered = store.get(&ev.entry_id).await?.unwrap();
            assert_eq!(delivered.state, DeliveryState::Delivered);
            assert_eq!(delivered.search_before, before.search_before);
            assert_eq!(delivered.nonce, before.nonce);
            assert_eq!(delivered.mirror_message_id.as_deref(), Some("650"));
            assert_eq!(
                mirror.posts.lock().unwrap().as_slice(),
                &[delivery_nonce(&ev.entry_id)]
            );
            peer.close().await;
        }
    }
    db.close().await?;
    assert!(
        failures.is_empty(),
        "invalid recovery evidence cleared ambiguity or failed to stop:\n{}",
        failures.join("\n")
    );
    Ok(())
}

#[tokio::test]
async fn valid_pagination_empty_exhaustion_and_marker_author_controls() -> TestResult {
    let Some(db) = database().await? else {
        return Ok(());
    };
    let store = AuditStore::new(db.pool());
    for (id, tail, expected) in [
        (
            "empty-exhaustion",
            vec![],
            DeliverOutcome::Quarantined(QuarantineReason::MarkerMissing),
        ),
        (
            "short-exhaustion",
            vec![row("650"), row("500")],
            DeliverOutcome::Quarantined(QuarantineReason::MarkerMissing),
        ),
        (
            "healthy-pagination",
            vec![marked("650", &event("healthy-pagination")), row("500")],
            DeliverOutcome::Reconciled {
                message_id: "650".to_owned(),
            },
        ),
        (
            "foreign-author",
            vec![
                MirrorMessage {
                    author_id: "5555".to_owned(),
                    ..marked("650", &event("foreign-author"))
                },
                row("500"),
            ],
            DeliverOutcome::Quarantined(QuarantineReason::MarkerMissing),
        ),
        (
            "wrong-marker",
            vec![marked("650", &event("other-entry")), row("500")],
            DeliverOutcome::Quarantined(QuarantineReason::MarkerMissing),
        ),
        (
            "below-boundary",
            vec![marked("500", &event("below-boundary"))],
            DeliverOutcome::Quarantined(QuarantineReason::MarkerMissing),
        ),
    ] {
        let ev = event(id);
        store.record(&ev, Some(CHANNEL)).await?;
        let owner = store.claim(id).await?.expect("send claim");
        assert_eq!(
            store.prepare_send(&owner, "501").await?,
            PrepareSend::Prepared
        );
        retry_now(db.pool(), id).await?;
        let mirror = Mirror::default();
        mirror.script(vec![full_page(), tail]);
        assert_eq!(
            deliver_bounded(&service(db.pool(), mirror.clone()), id).await?,
            expected,
            "{id}"
        );
        assert_eq!(mirror.cursors(), vec![None, Some("701".to_owned())]);
        assert!(mirror
            .reads
            .lock()
            .unwrap()
            .iter()
            .all(|(_, limit)| *limit == 100));
        assert!(mirror.posts.lock().unwrap().is_empty());
    }
    // Valid pre-send pagination seeds newest+1, and a valid dedup hit seeds
    // the MATCHING id; empty content in unrelated rows is valid evidence.
    for (id, pages, boundary, receipt, posts) in [
        ("send-empty", vec![vec![]], "0", "900000", 1),
        ("send-pages", vec![full_page(), vec![]], "801", "900000", 1),
        (
            "adopt-pages",
            vec![full_page(), vec![marked("650", &event("adopt-pages"))]],
            "650",
            "650",
            0,
        ),
    ] {
        let mirror = Mirror::default();
        mirror.script(pages);
        let svc = service(db.pool(), mirror.clone());
        svc.record(&event(id)).await?;
        let outcome = deliver_bounded(&svc, id).await?;
        assert!(matches!(
            outcome,
            DeliverOutcome::Delivered { .. } | DeliverOutcome::Reconciled { .. }
        ));
        let saved = store.get(id).await?.unwrap();
        assert_eq!(saved.state, DeliveryState::Delivered);
        assert_eq!(saved.search_before.as_deref(), Some(boundary));
        assert_eq!(saved.mirror_message_id.as_deref(), Some(receipt));
        assert_eq!(mirror.posts.lock().unwrap().len(), posts);
    }
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn accepted_receipt_halt_and_permission_refusal_keep_their_meaning() -> TestResult {
    let Some(db) = database().await? else {
        return Ok(());
    };
    let store = AuditStore::new(db.pool());
    store.record(&event("accepted"), Some(CHANNEL)).await?;
    let owner = store.claim("accepted").await?.unwrap();
    assert_eq!(
        store.prepare_send(&owner, "501").await?,
        PrepareSend::Prepared
    );
    assert!(store.note_accepted(&owner, "650").await?);
    let before = store.get("accepted").await?.unwrap();
    retry_now(db.pool(), "accepted").await?;
    let mirror = Mirror::default();
    mirror.script(vec![vec![row("0")]]);
    assert_eq!(
        deliver_bounded(&service(db.pool(), mirror.clone()), "accepted").await?,
        DeliverOutcome::Reconciled {
            message_id: "650".to_owned()
        }
    );
    let after = store.get("accepted").await?.unwrap();
    assert_eq!(after.accepted_at, before.accepted_at);
    assert_eq!(after.nonce, before.nonce);
    assert_eq!(after.search_before, before.search_before);
    assert!(
        mirror.cursors().is_empty(),
        "durable receipt requires no history"
    );
    assert!(mirror.posts.lock().unwrap().is_empty());

    store.record(&event("halted"), Some(CHANNEL)).await?;
    let held = store.claim("halted").await?.unwrap();
    store.engage_halt("5555").await?;
    assert_eq!(
        service(db.pool(), mirror.clone()).deliver(&held).await?,
        DeliverOutcome::Held
    );
    assert!(mirror.cursors().is_empty());
    assert_eq!(store.get("halted").await?.unwrap().attempts, 0);
    store.disengage_halt().await?;

    for recovery in [false, true] {
        let id = if recovery {
            "refused-recovery"
        } else {
            "refused-send"
        };
        store.record(&event(id), Some(CHANNEL)).await?;
        if recovery {
            let owner = store.claim(id).await?.unwrap();
            assert_eq!(
                store.prepare_send(&owner, "501").await?,
                PrepareSend::Prepared
            );
            retry_now(db.pool(), id).await?;
        }
        let refused = Mirror {
            refused: true,
            ..Mirror::default()
        };
        assert_eq!(
            deliver_bounded(&service(db.pool(), refused.clone()), id).await?,
            DeliverOutcome::Quarantined(QuarantineReason::PermissionRevoked)
        );
        assert!(refused.posts.lock().unwrap().is_empty());
    }
    db.close().await?;
    Ok(())
}
