use serde_json::{json, Value};
use tokio::time::{pause, resume};
use twilight_model::guild::Permissions;

use super::*;
use crate::{
    discord_test_common::{MockRest, ScriptedResponse},
    member_runtime::warning_history_authorizer::tests::{
        assert_reads, evidence, executor, overwrite,
    },
};

fn script(refreshes: impl IntoIterator<Item = [Value; 5]>) -> Vec<ScriptedResponse> {
    refreshes
        .into_iter()
        .flatten()
        .map(|doc| ScriptedResponse::json(200, doc))
        .collect()
}

fn authorized(executor: &ActionExecutor) -> Fake<'_> {
    Fake {
        executor: Some(executor),
        ..Fake::new([Pass; 5])
    }
}

async fn after_cooldown() {
    // Advance only between interactions so loopback I/O uses a running clock.
    pause();
    advance(Duration::from_secs(5)).await;
    resume();
}

async fn open(state: &HistoryState, executor: &ActionExecutor) -> String {
    let initial = authorized(executor);
    run(state, &initial, INITIAL).await;
    ordered(&initial, "read");
    assert!(initial.delivered.get());
    after_cooldown().await;
    initial.next(0)
}

async fn assert_closed(state: &HistoryState, handle: &str) {
    assert_retired(state, handle).await;
    after_cooldown().await;
    assert!(state.start(CALLER, TARGET).is_ok());
}

#[tokio::test]
async fn foreign_next_scope_leaves_the_bound_page_usable() {
    let rest = MockRest::start(
        script([evidence(), evidence(), evidence(), evidence()]),
        ScriptedResponse::status(500),
    )
    .await;
    let executor = executor(&rest);
    let state = HistoryState::default();
    let handle = open(&state, &executor).await;
    for caller in [
        Invocation {
            principal: (id(23), CALLER.principal.1),
            ..CALLER
        },
        Invocation {
            principal: (CALLER.principal.0, id(45)),
            ..CALLER
        },
        Invocation {
            channel: id(78),
            ..CALLER
        },
    ] {
        let mut foreign = authorized(&executor);
        foreign.caller = caller;
        run(&state, &foreign, Input::Next(&handle)).await;
        foreign.control_status(EXPIRED);
    }
    assert_reads(&rest, &[5, 5]);

    let next = authorized(&executor);
    run(&state, &next, Input::Next(&handle)).await;
    ordered(&next, "next");
    assert!(next.delivered.get());
    assert_ne!(next.next(5), handle);
    assert_reads(&rest, &[5, 5, 5, 5]);
}

#[tokio::test]
async fn next_refuses_stale_or_inconsistent_scope_before_the_read() {
    const UNAVAILABLE: Outcome = Outcome::Authority(Refusal::Unavailable);
    for (slot, field, value, expected, reads) in [
        (1, "roles", json!([]), DENIED, 2),
        (1, "user", json!({"id":"45"}), UNAVAILABLE, 2),
        (4, "guild_id", json!("23"), UNAVAILABLE, 5),
    ] {
        let mut changed = evidence();
        changed[slot][field] = value;
        let rest = MockRest::start(
            script([evidence(), evidence(), changed]),
            ScriptedResponse::status(500),
        )
        .await;
        let executor = executor(&rest);
        let state = HistoryState::default();
        let handle = open(&state, &executor).await;
        let next = authorized(&executor);
        run(&state, &next, Input::Next(&handle)).await;
        assert_eq!(*next.trace.borrow(), ["ack", "fresh", "edit"]);
        next.status(expected);
        assert!(next.delivered.get());
        assert_closed(&state, &handle).await;
        assert_reads(&rest, &[5, 5, reads]);
    }
}

#[tokio::test]
async fn next_discards_the_projection_when_channel_access_is_revoked_after_read() {
    let mut hidden = evidence();
    hidden[4]["permission_overwrites"] =
        json!([overwrite("44", 1, 0, Permissions::VIEW_CHANNEL.bits())]);
    let rest = MockRest::start(
        script([evidence(), evidence(), evidence(), hidden]),
        ScriptedResponse::status(500),
    )
    .await;
    let executor = executor(&rest);
    let state = HistoryState::default();
    let handle = open(&state, &executor).await;
    let next = authorized(&executor);
    run(&state, &next, Input::Next(&handle)).await;
    ordered(&next, "next");
    next.status(DENIED);
    assert!(next.delivered.get());
    assert_closed(&state, &handle).await;
    assert_reads(&rest, &[5, 5, 5, 5]);
}

#[tokio::test]
async fn stop_while_next_refreshes_authority_prevents_the_read() {
    let (rest, gate) = MockRest::start_gated_at(
        script([evidence(), evidence(), evidence()]),
        ScriptedResponse::status(500),
        14, // Hold Next's channel evidence in its first refresh.
    )
    .await;
    let executor = executor(&rest);
    let state = HistoryState::default();
    let handle = open(&state, &executor).await;
    let next = authorized(&executor);
    let mut pending = std::pin::pin!(run(&state, &next, Input::Next(&handle)));
    tokio::select! {
        () = gate.wait_for_request() => {},
        () = &mut pending => panic!("Next completed before its channel evidence was held"),
    }
    assert_eq!(*next.trace.borrow(), ["ack", "fresh"]);
    assert!(next.reply.borrow().is_none());

    let stop = authorized(&executor);
    run(&state, &stop, Input::Stop(&handle)).await;
    stop.control_status(Outcome::Stopped);
    assert!(stop.delivered.get());
    assert_reads(&rest, &[5, 5, 5]);
    gate.release();

    pending.await;
    assert_eq!(*next.trace.borrow(), ["ack", "fresh", "edit"]);
    next.status(EXPIRED);
    assert_closed(&state, &handle).await;
    assert_reads(&rest, &[5, 5, 5]);
}
