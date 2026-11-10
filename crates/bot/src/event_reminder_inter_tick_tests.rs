//! Inter-tick eligibility and exact deadline controls use an injected wall clock.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tokio::sync::Notify;
use two_bot_core::event_reminder_store::{
    begin_event_reminder_delivery, due_event_reminders_before, has_pending_event_reminders_before,
};

use super::*;

const NOW: u64 = 1_800_000_000_000;

async fn mirror(pool: &PgPool, starts: &[u64]) {
    let events: Vec<_> = starts
        .iter()
        .zip(IDS)
        .map(|(starts, id)| ScheduledEvent {
            id: id.to_owned(),
            name: "Inter-tick RSVP".to_owned(),
            starts_at: format_iso_ms(*starts),
            channel_id: Some("1546451670500642111".to_owned()),
            description: None,
            status: EventStatus::Scheduled,
        })
        .collect();
    replace_events(pool, GUILD, &format_iso_ms(NOW), &events)
        .await
        .expect("seed mirror without attendees");
}

async fn respond(pool: &PgPool, event: &str, status: RsvpStatus, now: u64) {
    put_rsvp(
        pool,
        &RsvpRecord {
            guild_id: GUILD.to_owned(),
            event_id: event.to_owned(),
            user_id: "1546451670500642888".to_owned(),
            status,
            responded_at: format_iso_ms(now),
        },
    )
    .await
    .expect("commit first RSVP");
}

#[tokio::test]
async fn first_rsvps_after_both_initial_passes_are_not_stranded_until_next_tick() {
    let Some((_, db)) = fixture().await else {
        return;
    };
    mirror(db.pool(), &[NOW + 10_000, NOW + 10_000]).await;
    let mock = MockRest::with_responder(|_: &RestRequest| {
        ScriptedResponse::json(200, json!({"id": "9001"}))
    })
    .await;
    let wire_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let initial_passes_complete = Arc::new(Notify::new());
    let clock = Arc::new(AtomicU64::new(NOW));
    let action = tokio::spawn({
        let pool = db.pool().clone();
        let rest = executor(&mock);
        let initial_passes_complete = initial_passes_complete.clone();
        let clock = clock.clone();
        async move {
            let samples = AtomicUsize::new(0);
            tick_with_deadline_follow_up(&pool, &rest, GUILD, || {
                // Schedule claim, ordinary scan, urgent cutoff, urgent scan,
                // then the pending-event probe: both empty passes have ended.
                if samples.fetch_add(1, Ordering::SeqCst) == 4 {
                    initial_passes_complete.notify_one();
                }
                clock.load(Ordering::SeqCst)
            })
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), initial_passes_complete.notified())
        .await
        .expect("both initial passes completed without attendees");
    assert_eq!(post_count(&mock), 0);
    assert!(reminder_fences(db.pool()).await.is_empty());
    // Move the injected wall clock to t=5 only AFTER both t=0 reads. No second
    // ordinary tick is invoked; the same supervised action must observe these.
    respond(db.pool(), IDS[0], RsvpStatus::Going, NOW + 5_000).await;
    respond(db.pool(), IDS[1], RsvpStatus::Interested, NOW + 5_000).await;
    clock.store(NOW + 5_000, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(4), action)
        .await
        .expect("urgent watch finishes before the t=10 start")
        .expect("watch task")
        .expect("urgent watch");
    assert_eq!(post_count(&mock), 2);
    assert!(mock
        .requests()
        .iter()
        .filter(|request| request.method == "POST")
        .all(|request| request.received_at < wire_deadline));
    let fences = reminder_fences(db.pool()).await;
    assert_eq!(fences.len(), 2);
    let peer = db.independent_pool().await.expect("fresh restart pool");
    tick_with_deadline_follow_up(&peer, &executor(&mock), GUILD, || NOW + 6_000)
        .await
        .expect("restart while both events remain future and urgent");
    assert_eq!(post_count(&mock), 2, "no replay after restart");
    assert_eq!(reminder_fences(&peer).await, fences);
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}

#[tokio::test]
async fn urgent_cutoff_and_start_equality_have_explicit_controls() {
    let Some((_, db)) = fixture().await else {
        return;
    };
    mirror(db.pool(), &[NOW, NOW + 1, NOW + 15_000, NOW + 15_001]).await;
    for id in &IDS[..4] {
        respond(db.pool(), id, RsvpStatus::Going, NOW).await;
    }
    let due = due_event_reminders_before(db.pool(), GUILD, NOW, NOW + 15_000)
        .await
        .expect("read exact urgent boundary");
    assert_eq!(
        due.iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        [IDS[1], IDS[2]],
        "start equality is late, just-before and urgent cutoff equality are included"
    );
    assert!(
        has_pending_event_reminders_before(db.pool(), GUILD, NOW, NOW + 15_000)
            .await
            .expect("future urgent control")
    );
    assert!(
        !has_pending_event_reminders_before(db.pool(), GUILD, NOW + 15_000, NOW + 15_000)
            .await
            .expect("all urgent starts elapsed")
    );
    assert!(
        begin_event_reminder_delivery(db.pool(), GUILD, &due[1], NOW + 14_999)
            .await
            .expect("admission just before start")
    );
    assert!(
        !begin_event_reminder_delivery(db.pool(), GUILD, &due[0], NOW + 1)
            .await
            .expect("admission at start is refused")
    );
    assert_eq!(reminder_fences(db.pool()).await.len(), 1);
    db.close().await.expect("verified teardown");
}

#[tokio::test]
async fn delayed_tick_never_posts_at_or_after_start() {
    let Some((_, db)) = fixture().await else {
        return;
    };
    mirror(db.pool(), &[NOW + 10_000]).await;
    respond(db.pool(), IDS[0], RsvpStatus::Going, NOW + 5_000).await;
    let mock = MockRest::with_responder(|_: &RestRequest| {
        ScriptedResponse::json(200, json!({"id": "9001"}))
    })
    .await;
    for now in [NOW + 10_000, NOW + 15_000] {
        tick_with_deadline_follow_up(db.pool(), &executor(&mock), GUILD, || now)
            .await
            .expect("late tick degrades to no reminder");
    }
    assert_eq!(post_count(&mock), 0);
    assert!(reminder_fences(db.pool()).await.is_empty());
    mock.shutdown().await;
    db.close().await.expect("verified teardown");
}
