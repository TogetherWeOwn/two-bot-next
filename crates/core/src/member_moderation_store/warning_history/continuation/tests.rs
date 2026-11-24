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
        principal: Principal {
            guild: id(guild),
            actor: id(actor),
        },
        channel: id(channel),
    }
}

fn cursor(guild: u64, target: u64) -> WarningHistoryCursor {
    WarningHistoryCursor {
        guild: guild.to_string(),
        target: target.to_string(),
        high_water: Key {
            created_at: OffsetDateTime::UNIX_EPOCH,
            id: "synthetic-warning-high".into(),
        },
        last_shown: Key {
            created_at: OffsetDateTime::UNIX_EPOCH,
            id: "synthetic-warning-last".into(),
        },
    }
}

fn handle(page: &PreparedPage<'_>) -> String {
    let Controls::Next(handle) = page.controls().unwrap() else {
        panic!("expected Next");
    };
    handle.encode()
}

fn first_page(state: &HistoryState, scope: Invocation) -> String {
    let attempt = state.start(scope, id(4)).unwrap();
    let (target, key) = attempt.request().unwrap();
    assert_eq!(target, id(4));
    assert!(key.is_none());
    let page = attempt
        .prepare(Some(cursor(scope.principal.guild.get(), 4)))
        .unwrap();
    let handle = handle(&page);
    page.confirm().unwrap();
    handle
}

#[tokio::test(start_paused = true)]
async fn handles_require_the_original_context_and_canonical_shape() {
    let state = HistoryState::default();
    let scope = invocation(1, 2, 3);
    let input = first_page(&state, scope);
    assert_eq!(input.len(), 36);
    advance(COOLDOWN).await;

    for foreign in [
        invocation(9, 2, 3),
        invocation(1, 9, 3),
        invocation(1, 2, 9),
    ] {
        assert!(matches!(state.next(foreign, &input), Err(Status::Expired)));
        assert_eq!(state.stop(foreign, &input), Err(Status::Expired));
    }
    for bad in [
        input.replacen("wh1:", "wh2:", 1),
        input[..35].into(),
        format!("{input}a"),
        format!("wh1:{}A", "0".repeat(31)),
        format!("wh1:{}é", "0".repeat(30)),
        format!("wh1:{}", "1".repeat(1024)),
    ] {
        assert!(matches!(state.next(scope, &bad), Err(Status::Expired)));
    }
    assert!(matches!(
        HistoryState::default().next(scope, &input),
        Err(Status::Expired)
    ));

    let attempt = state.next(scope, &input).unwrap();
    let (target, key) = attempt.request().unwrap();
    assert_eq!(target, id(4));
    assert_eq!(key.unwrap().last_shown.id, "synthetic-warning-last");
    assert!(matches!(state.next(scope, &input), Err(Status::Expired)));
    assert!(matches!(
        attempt.prepare(Some(cursor(1, 9))),
        Err(Status::Unavailable)
    ));
    assert_eq!(state.stop(scope, &input), Err(Status::Expired));
    advance(COOLDOWN).await;
    assert!(matches!(
        state
            .start(scope, id(9))
            .unwrap()
            .prepare(Some(cursor(9, 9))),
        Err(Status::Unavailable)
    ));
}

#[tokio::test(start_paused = true)]
async fn expiry_is_absolute_and_late_work_cannot_settle_a_replacement() {
    let state = HistoryState::default();
    let scope = invocation(1, 2, 3);
    let old = first_page(&state, scope);
    advance(LIFETIME - COOLDOWN).await;
    let attempt = state.next(scope, &old).unwrap();
    let page = attempt.prepare(Some(cursor(1, 4))).unwrap();
    let new = handle(&page);
    page.confirm().unwrap();

    advance(COOLDOWN - Duration::from_millis(1)).await;
    assert!(matches!(state.next(scope, &new), Err(Status::Busy)));
    advance(Duration::from_millis(1)).await;
    assert!(matches!(state.next(scope, &new), Err(Status::Expired)));

    let attempt = state.start(scope, id(4)).unwrap();
    let late = attempt.prepare(Some(cursor(1, 4))).unwrap();
    advance(LIFETIME).await;
    let replacement = first_page(&state, scope);
    assert!(matches!(late.controls(), Err(Status::Expired)));
    assert_eq!(late.confirm(), Err(Status::Expired));
    assert_eq!(state.stop(scope, &replacement), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn simultaneous_next_consumes_once_and_rotates_after_delivery() {
    let state = HistoryState::default();
    let scope = invocation(1, 2, 3);
    let input = first_page(&state, scope);
    assert!(matches!(state.next(scope, &input), Err(Status::Busy)));
    advance(COOLDOWN).await;
    let runtime = tokio::runtime::Handle::current();
    let barrier = Barrier::new(2);

    let winner = std::thread::scope(|threads| {
        let next = || {
            let _runtime = runtime.enter();
            barrier.wait();
            state.next(scope, &input)
        };
        let left = threads.spawn(next);
        let right = threads.spawn(next);
        match (left.join().unwrap(), right.join().unwrap()) {
            (Ok(winner), Err(Status::Expired)) | (Err(Status::Expired), Ok(winner)) => winner,
            _ => panic!("expected exactly one admitted page"),
        }
    });

    let page = winner.prepare(Some(cursor(1, 4))).unwrap();
    let rotated = handle(&page);
    assert_ne!(rotated, input);
    assert!(matches!(state.next(scope, &rotated), Err(Status::Expired)));
    page.confirm().unwrap();
    assert!(matches!(state.next(scope, &input), Err(Status::Expired)));
    assert_eq!(state.stop(scope, &input), Err(Status::Expired));
    advance(COOLDOWN).await;
    assert!(state.next(scope, &rotated).is_ok());
    assert!(matches!(state.next(scope, &rotated), Err(Status::Expired)));
}

#[tokio::test(start_paused = true)]
async fn stop_invalidates_ready_and_in_flight_pages_without_refunding_admission() {
    let state = HistoryState::default();
    let scope = invocation(1, 2, 3);
    let input = first_page(&state, scope);
    assert_eq!(state.stop(scope, &input), Ok(()));
    assert_eq!(state.stop(scope, &input), Err(Status::Expired));
    assert!(matches!(state.start(scope, id(4)), Err(Status::Busy)));

    advance(COOLDOWN).await;
    let input = first_page(&state, scope);
    advance(COOLDOWN).await;
    let attempt = state.next(scope, &input).unwrap();
    let page = attempt.prepare(Some(cursor(1, 4))).unwrap();
    let pending = handle(&page);
    assert_eq!(state.stop(scope, &input), Ok(()));
    assert!(matches!(page.controls(), Err(Status::Expired)));
    assert_eq!(page.confirm(), Err(Status::Expired));
    assert!(matches!(state.next(scope, &pending), Err(Status::Expired)));
    assert!(matches!(state.start(scope, id(4)), Err(Status::Busy)));

    advance(COOLDOWN).await;
    let initial = state.start(scope, id(4)).unwrap();
    let page = initial.prepare(Some(cursor(1, 4))).unwrap();
    assert_eq!(state.stop(scope, &handle(&page)), Ok(()));
    assert_eq!(page.confirm(), Err(Status::Expired));
}

#[tokio::test(start_paused = true)]
async fn the_fifth_page_reaches_the_limit_and_an_exhausted_page_has_no_controls() {
    let state = HistoryState::default();
    let scope = invocation(1, 2, 3);
    let mut input = first_page(&state, scope);
    for _ in 2..MAX_PAGES {
        advance(COOLDOWN).await;
        let page = state
            .next(scope, &input)
            .unwrap()
            .prepare(Some(cursor(1, 4)))
            .unwrap();
        input = handle(&page);
        page.confirm().unwrap();
    }
    advance(COOLDOWN).await;
    let page = state
        .next(scope, &input)
        .unwrap()
        .prepare(Some(cursor(1, 4)))
        .unwrap();
    assert!(matches!(page.controls().unwrap(), Controls::Limit));
    page.confirm().unwrap();
    assert!(matches!(state.next(scope, &input), Err(Status::Expired)));

    advance(COOLDOWN).await;
    let page = state.start(scope, id(4)).unwrap().prepare(None).unwrap();
    assert!(matches!(page.controls().unwrap(), Controls::Done));
    page.confirm().unwrap();
    advance(COOLDOWN).await;
    assert!(state.start(scope, id(4)).is_ok());
}

#[tokio::test(start_paused = true)]
async fn session_capacity_includes_in_flight_pages_and_never_evicts_live_bindings() {
    let state = HistoryState::default();
    let scope = invocation(1, 1, 3);
    let first = first_page(&state, scope);
    let flights: Vec<_> = (2..=MAX_SESSIONS as u64)
        .map(|actor| state.start(invocation(1, actor, 3), id(4)).unwrap())
        .collect();
    let other_guild = invocation(8, 1, 3);
    assert!(matches!(state.start(other_guild, id(4)), Err(Status::Busy)));
    advance(COOLDOWN).await;
    assert!(matches!(
        state.start(invocation(1, 1, 9), id(5)),
        Err(Status::Busy)
    ));
    assert_eq!(state.stop(scope, &first), Ok(()));
    assert!(state.start(other_guild, id(4)).is_ok());
    drop(flights);
}

#[tokio::test(start_paused = true)]
async fn cooldown_capacity_refuses_new_principals_and_survives_failure() {
    let state = HistoryState::default();
    let scope = invocation(1, 1, 3);
    let first = first_page(&state, scope);
    advance(COOLDOWN).await;
    for actor in 2..=MAX_COOLDOWNS as u64 + 1 {
        drop(state.start(invocation(1, actor, 3), id(4)).unwrap());
    }
    assert!(matches!(state.next(scope, &first), Err(Status::Busy)));
    let other_guild = invocation(8, 1, 3);
    assert!(matches!(state.start(other_guild, id(4)), Err(Status::Busy)));
    advance(COOLDOWN - Duration::from_nanos(1)).await;
    assert!(matches!(
        state.start(invocation(1, 2, 9), id(5)),
        Err(Status::Busy)
    ));
    advance(Duration::from_nanos(1)).await;

    let continuation = state.next(scope, &first).unwrap();
    let page = continuation.prepare(Some(cursor(1, 4))).unwrap();
    let pending = handle(&page);
    drop(page);
    assert!(matches!(state.next(scope, &pending), Err(Status::Expired)));
    assert!(matches!(state.start(scope, id(4)), Err(Status::Busy)));
    assert!(state.start(other_guild, id(4)).is_ok());
}
