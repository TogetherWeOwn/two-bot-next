#![cfg(test)]

use std::sync::Barrier;
use time::OffsetDateTime;
use tokio::time::advance;

use super::super::Key;
use super::*;

fn id(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

fn invocation(guild: u64, actor: u64, channel: u64) -> Invocation {
    Invocation {
        principal: (id(guild), id(actor)),
        channel: id(channel),
    }
}

fn offer(attempt: Attempt<'_>) -> (Prepared<'_>, String) {
    let key = |id: &str| Key {
        created_at: OffsetDateTime::UNIX_EPOCH,
        id: id.into(),
    };
    let cursor = WarningHistoryCursor {
        guild: attempt.who.0.to_string(),
        target: attempt.request().unwrap().0.to_string(),
        high_water: key("synthetic-high"),
        last_shown: key("synthetic-last"),
    };
    let (page, Controls::Next(handle)) = attempt.prepare(Some(cursor)).unwrap() else {
        panic!("expected Next");
    };
    (page, handle)
}

fn show(attempt: Attempt<'_>) -> String {
    let (page, handle) = offer(attempt);
    page.confirm().unwrap();
    handle
}

#[tokio::test(start_paused = true)]
async fn binding_and_expiry_are_checked_before_cursor_access() {
    let state = HistoryState::default();
    let scope = invocation(1, 2, 3);
    let handle = show(state.start(scope, id(4)).unwrap());
    advance(LIFETIME - COOLDOWN).await;
    for (guild, actor, channel) in [(9, 2, 3), (1, 9, 3), (1, 2, 9)] {
        let foreign = invocation(guild, actor, channel);
        assert_eq!(state.next(foreign, &handle).err(), Some(Status::Expired));
    }
    let wrong_version = handle.replacen("wh1:", "wh2:", 1);
    assert_eq!(validate(&format!("{handle}a")), Err(Status::Expired));
    assert_eq!(
        state.next(scope, &wrong_version).err(),
        Some(Status::Expired)
    );
    let attempt = state.next(scope, &handle).unwrap();
    let (target, cursor) = attempt.request().unwrap();
    assert_eq!(target, id(4));
    assert_eq!(cursor.unwrap().last_shown.id, "synthetic-last");
    let rotated = show(attempt);
    assert!(matches!(state.next(scope, &handle), Err(Status::Expired)));
    advance(COOLDOWN).await;
    assert!(matches!(state.next(scope, &rotated), Err(Status::Expired)));
}

#[tokio::test(start_paused = true)]
async fn simultaneous_next_consumes_once_and_stop_cancels_pending_delivery() {
    let state = HistoryState::default();
    let scope = invocation(1, 2, 3);
    let handle = show(state.start(scope, id(4)).unwrap());
    assert!(matches!(state.next(scope, &handle), Err(Status::Busy)));
    advance(COOLDOWN).await;
    let runtime = tokio::runtime::Handle::current();
    let barrier = Barrier::new(2);
    let winner = std::thread::scope(|threads| {
        let claim = || {
            let _runtime = runtime.enter();
            barrier.wait();
            state.next(scope, &handle)
        };
        let left = threads.spawn(claim);
        let right = threads.spawn(claim);
        match (left.join().unwrap(), right.join().unwrap()) {
            (Ok(winner), Err(Status::Expired)) | (Err(Status::Expired), Ok(winner)) => winner,
            _ => panic!("expected one admitted page"),
        }
    });
    let (page, pending) = offer(winner);
    assert!(matches!(state.next(scope, &pending), Err(Status::Expired)));
    assert_eq!(state.stop(scope, &handle), Ok(()));
    assert_eq!(page.confirm(), Err(Status::Expired));
    assert_eq!(state.stop(scope, &pending), Err(Status::Expired));
    assert!(matches!(state.start(scope, id(4)), Err(Status::Busy)));
}

#[tokio::test(start_paused = true)]
async fn fifth_page_caps_and_cooldown_survives_failed_delivery() {
    let state = HistoryState::default();
    let scope = invocation(1, 2, 3);
    let mut handle = show(state.start(scope, id(4)).unwrap());
    for _ in 2..MAX_PAGES {
        advance(COOLDOWN).await;
        handle = show(state.next(scope, &handle).unwrap());
    }
    advance(COOLDOWN).await;
    let mut attempt = state.next(scope, &handle).unwrap();
    let cursor = attempt.cursor.take();
    let (page, controls) = attempt.prepare(cursor).unwrap();
    assert!(matches!(controls, Controls::Limit));
    page.confirm().unwrap();
    assert!(matches!(state.next(scope, &handle), Err(Status::Expired)));
    advance(COOLDOWN).await;

    let (page, _) = offer(state.start(scope, id(4)).unwrap());
    drop(page);
    assert!(matches!(state.start(scope, id(4)), Err(Status::Busy)));
    advance(COOLDOWN).await;
    assert!(state.start(scope, id(4)).is_ok());
}

#[tokio::test(start_paused = true)]
async fn full_session_and_cooldown_maps_refuse_without_eviction() {
    let state = HistoryState::default();
    let scope = invocation(1, 1, 3);
    let handle = show(state.start(scope, id(4)).unwrap());
    let flights: Vec<_> = (2..=MAX_SESSIONS as u64)
        .map(|actor| state.start(invocation(1, actor, 3), id(4)).unwrap())
        .collect();
    let other = invocation(8, 1, 3);
    assert!(matches!(state.start(other, id(4)), Err(Status::Busy)));
    advance(COOLDOWN).await;
    let same_actor = invocation(1, 1, 9);
    assert_eq!(state.start(same_actor, id(5)).err(), Some(Status::Busy));
    drop(flights);
    for actor in 2..=MAX_COOLDOWNS as u64 {
        drop(state.start(invocation(1, actor, 3), id(4)).unwrap());
    }
    assert!(matches!(state.start(other, id(4)), Err(Status::Busy)));
    assert_eq!(state.stop(scope, &handle), Ok(()));
    advance(COOLDOWN).await;
    assert!(state.start(other, id(4)).is_ok());
}
