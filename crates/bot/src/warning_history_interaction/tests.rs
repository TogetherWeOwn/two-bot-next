use std::cell::{Cell, RefCell};

use tokio::time::{advance, sleep};
use twilight_model::id::Id;
use two_bot_core::member_moderation_store::{
    continuation::synthetic_cursor, WarningHistoryCursor, WarningHistoryEntry, WarningHistoryPage,
};
use two_bot_discord::ActionExecutor;

use super::handle as run;
use super::*;
use crate::member_runtime::warning_history_authorizer::authorize;

#[path = "fresh_context.rs"]
mod fresh_context;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Pass,
    Wait(u64),
    Fail,
    Public,
    Empty,
}
use Action::{Empty, Fail, Pass, Public, Wait};

pub(super) struct Fake<'a> {
    pub(super) caller: Invocation,
    executor: Option<&'a ActionExecutor>,
    script: [Action; 5], // ACK, first authority, read, final authority, original edit
    trace: RefCell<Vec<&'static str>>,
    reply: RefCell<Option<Reply>>,
    delivered: Cell<bool>,
}

const fn id(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

const CALLER: Invocation = Invocation {
    principal: (id(22), id(44)),
    channel: id(77),
};
const TARGET: NonZeroU64 = id(88);
const INITIAL: Input<'_> = Input::Initial("88");
const EXPIRED: Outcome = Outcome::Session(Status::Expired);
const DENIED: Outcome = Outcome::Authority(Refusal::Denied);

impl Fake<'_> {
    fn new(script: [Action; 5]) -> Self {
        Self {
            script,
            trace: RefCell::default(),
            reply: RefCell::default(),
            delivered: Cell::default(),
            caller: CALLER,
            executor: None,
        }
    }

    fn next(&self, reference: usize) -> String {
        let reply = self.reply.borrow();
        let Some(Reply::Page(content, Controls::Next(handle))) = reply.as_ref() else {
            panic!("expected a page with Next");
        };
        assert!(content.allowed_mentions.is_some());
        let expected = format!("W-{reference:064x}");
        let body = content.content.as_deref().unwrap_or_default();
        assert!(body.contains(&expected));
        handle.clone()
    }

    fn status(&self, expected: Outcome) {
        let reply = self.reply.borrow();
        assert!(matches!(&*reply, Some(Reply::Status(actual)) if *actual == expected));
    }

    fn control_status(&self, expected: Outcome) {
        assert_eq!(*self.trace.borrow(), ["ack", "edit"]);
        self.status(expected);
    }

    async fn step(&self, event: &'static str, phase: usize) -> Action {
        self.trace.borrow_mut().push(event);
        let action = self.script[phase];
        if let Wait(seconds) = action {
            sleep(Duration::from_secs(seconds)).await;
        }
        action
    }

    pub(super) async fn defer(&self) -> Option<bool> {
        match self.step("ack", 0).await {
            Fail => None,
            Public => Some(false),
            _ => Some(true),
        }
    }

    pub(super) async fn fresh(
        &self,
        who: Invocation,
        target: NonZeroU64,
        deadline: Instant,
    ) -> Result<(), Refusal> {
        if let Some(executor) = self.executor {
            self.trace.borrow_mut().push("fresh");
            return authorize(
                executor,
                Id::new(CALLER.principal.0.get()),
                Some(Id::new(who.principal.0.get())),
                Id::new(who.channel.get()),
                Id::new(who.principal.1.get()),
                Id::new(target.get()),
                deadline,
            )
            .await;
        }
        assert_eq!(who.principal, CALLER.principal);
        assert_eq!((who.channel, target), (CALLER.channel, TARGET));
        let read_closed = self.trace.borrow().contains(&"closed");
        let phase = if read_closed { 3 } else { 1 };
        (self.step("fresh", phase).await != Fail)
            .then_some(())
            .ok_or(Refusal::Denied)
    }

    pub(super) async fn read_closed(
        &self,
        scope: (NonZeroU64, NonZeroU64),
        cursor: Option<&WarningHistoryCursor>,
    ) -> WarningHistoryPage {
        assert_eq!(scope, (CALLER.principal.0, TARGET));
        let next = cursor.is_some();
        let event = if next { "next" } else { "read" };
        let start = if next { 5 } else { 0 };
        let empty = self.step(event, 2).await == Empty;
        self.trace.borrow_mut().push("closed");
        let entries = (start..start + 5 * usize::from(!empty))
            .map(|index| WarningHistoryEntry {
                reference: format!("W-{index:064x}"),
                recorded_at_utc: "1970-01-01T00:00:00Z".into(),
                recorder_id: None,
            })
            .collect();
        WarningHistoryPage {
            entries,
            next: (!empty).then(|| synthetic_cursor(scope.0, scope.1)),
        }
    }

    pub(super) async fn edit_original(&self, reply: Reply) -> bool {
        *self.reply.borrow_mut() = Some(reply);
        let delivered = self.step("edit", 4).await != Fail;
        self.delivered.set(delivered);
        delivered
    }
}

fn ordered(fake: &Fake<'_>, read: &str) {
    let expected = ["ack", "fresh", read, "closed", "fresh", "edit"];
    assert_eq!(*fake.trace.borrow(), expected);
}

async fn offer(state: &HistoryState) -> String {
    let fake = Fake::new([Pass; 5]);
    run(state, &fake, INITIAL).await;
    ordered(&fake, "read");
    fake.next(0)
}

#[tokio::test(start_paused = true)]
async fn initial_and_next_deliver_after_read_closes_and_authority_refreshes() {
    let state = HistoryState::default();
    let handle = offer(&state).await;
    advance(Duration::from_secs(5)).await;
    let fake = Fake::new([Pass; 5]);
    run(&state, &fake, Input::Next(&handle)).await;
    ordered(&fake, "next");
    let next = fake.next(5);
    assert_ne!(next, handle);
    for (input, outcome) in [
        (Input::Next(&next), Outcome::End),
        (INITIAL, Outcome::Empty),
    ] {
        advance(Duration::from_secs(5)).await;
        let fake = Fake::new([Pass, Pass, Empty, Pass, Pass]);
        run(&state, &fake, input).await;
        fake.status(outcome);
    }
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_ack_or_send_never_replays() {
    let state = HistoryState::default();
    for action in [Fail, Public] {
        let fake = Fake::new([action, Pass, Pass, Pass, Pass]);
        run(&state, &fake, INITIAL).await;
        assert_eq!(*fake.trace.borrow(), ["ack"]);
        advance(Duration::from_secs(5)).await;
    }
    for script in [
        [Pass, Pass, Pass, Pass, Fail],
        [Wait(2), Wait(4), Wait(1), Wait(2), Wait(4)],
    ] {
        let handle = offer(&state).await;
        advance(Duration::from_secs(5)).await;
        let failed = Fake::new(script);
        run(&state, &failed, Input::Next(&handle)).await;
        ordered(&failed, "next");
        for handle in [handle, failed.next(5)] {
            let retry = Fake::new([Pass; 5]);
            run(&state, &retry, Input::Next(&handle)).await;
            retry.status(EXPIRED);
            assert_eq!(*retry.trace.borrow(), ["ack", "edit"]);
        }
        advance(Duration::from_secs(5)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn foreign_and_expiring_handles_fail_closed_at_each_boundary() {
    for phase in [1, 3] {
        let state = HistoryState::default();
        let handle = offer(&state).await;
        let mut foreign = Fake::new([Pass; 5]);
        foreign.caller.channel = id(9);
        run(&state, &foreign, Input::Next(&handle)).await;
        foreign.status(EXPIRED);
        advance(Duration::from_secs(119)).await;
        let mut fake = Fake::new([Pass; 5]);
        fake.script[phase] = Wait(2);
        run(&state, &fake, Input::Next(&handle)).await;
        fake.status(EXPIRED);
        assert_eq!(fake.trace.borrow().contains(&"next"), phase == 3);
    }
}

#[tokio::test(start_paused = true)]
async fn deadlines_and_revocation_discard_the_projection() {
    for (first, phase, action, outcome, read) in [
        (Pass, 1, Fail, DENIED, false),
        (Pass, 3, Fail, DENIED, true),
        (Pass, 1, Wait(5), Outcome::Unavailable, false),
        (Wait(4), 3, Wait(3), Outcome::Unavailable, true),
        (Pass, 2, Wait(2), Outcome::Unavailable, true),
    ] {
        let mut fake = Fake::new([Pass, first, Pass, Pass, Pass]);
        fake.script[phase] = action;
        run(&HistoryState::default(), &fake, INITIAL).await;
        fake.status(outcome);
        assert_eq!(fake.trace.borrow().contains(&"read"), read);
    }
}

async fn assert_retired(state: &HistoryState, handle: &str) {
    for input in [Input::Next(handle), Input::Stop(handle)] {
        let fake = Fake::new([Pass; 5]);
        run(state, &fake, input).await;
        fake.control_status(EXPIRED);
    }
}

#[tokio::test(start_paused = true)]
async fn stop_requires_private_ack_before_retiring_the_binding() {
    let state = HistoryState::default();
    let initial = Fake::new([Pass; 5]);
    run(&state, &initial, INITIAL).await;
    let handle = initial.next(0);

    for action in [Fail, Public, Wait(12)] {
        let fake = Fake::new([action, Pass, Pass, Pass, Pass]);
        run(&state, &fake, Input::Stop(&handle)).await;
        assert_eq!(*fake.trace.borrow(), ["ack"]);
        assert!(fake.reply.borrow().is_none());
    }
    let stopped = Fake::new([Pass; 5]);
    run(&state, &stopped, Input::Stop(&handle)).await;
    stopped.control_status(Outcome::Stopped);
    assert!(stopped.delivered.get());
    assert_retired(&state, &handle).await;

    // The earlier ephemeral copy can still contain the retired controls.
    assert!(initial.delivered.get());
    assert!(initial.next(0) == handle);
    assert!(state.start(CALLER, TARGET).is_ok());
}

#[tokio::test(start_paused = true)]
async fn stop_uses_the_same_expired_status_for_invalid_scope_and_handles() {
    let state = HistoryState::default();
    let handle = offer(&state).await;
    let wrong = handle.replacen("wh1:", "wh2:", 1);
    let last = if handle.ends_with('0') { '1' } else { '0' };
    let unknown = format!("{}{last}", &handle[..handle.len() - 1]);
    for (caller, token) in [
        (
            Invocation {
                principal: (id(9), CALLER.principal.1),
                ..CALLER
            },
            handle.as_str(),
        ),
        (
            Invocation {
                principal: (CALLER.principal.0, id(9)),
                ..CALLER
            },
            &handle,
        ),
        (
            Invocation {
                channel: id(9),
                ..CALLER
            },
            &handle,
        ),
        (CALLER, "wh1:☃"),
        (CALLER, &wrong),
        (CALLER, &unknown),
    ] {
        let mut fake = Fake::new([Pass; 5]);
        fake.caller = caller;
        run(&state, &fake, Input::Stop(token)).await;
        fake.control_status(EXPIRED);
    }
    // A failed retirement edit spends the binding and is attempted only once.
    let failed = Fake::new([Pass, Pass, Pass, Pass, Fail]);
    run(&state, &failed, Input::Stop(&handle)).await;
    failed.control_status(Outcome::Stopped);
    assert!(!failed.delivered.get());
    assert_retired(&state, &handle).await;

    advance(Duration::from_secs(5)).await;
    let handle = offer(&state).await;
    advance(Duration::from_secs(119)).await;
    let late_ack = Fake::new([Wait(2), Pass, Pass, Pass, Pass]);
    run(&state, &late_ack, Input::Stop(&handle)).await;
    late_ack.control_status(EXPIRED);
}

#[tokio::test(start_paused = true)]
async fn stop_during_next_discards_prepared_continuation() {
    for phase in [3, 4] {
        let state = HistoryState::default();
        let handle = offer(&state).await;
        advance(Duration::from_secs(5)).await;
        let mut next = Fake::new([Pass; 5]);
        next.script[phase] = Wait(2);
        let stopped = Fake::new([Pass; 5]);
        tokio::join!(run(&state, &next, Input::Next(&handle)), async {
            sleep(Duration::from_secs(1)).await;
            // Stop uses the handle already seen by the caller while Next owns
            // the rotated one. Final authority or delivery is still pending.
            assert!(!next.delivered.get());
            assert_eq!(next.reply.borrow().is_some(), phase == 4);
            run(&state, &stopped, Input::Stop(&handle)).await;
            stopped.control_status(Outcome::Stopped);
        });
        ordered(&next, "next");
        advance(Duration::from_secs(5)).await;
        assert_retired(&state, &handle).await;
        if phase == 3 {
            next.status(EXPIRED);
        } else {
            // An edit already in flight can land. Its controls are inert.
            assert!(next.delivered.get());
            assert_retired(&state, &next.next(5)).await;
        }
        assert!(state.start(CALLER, TARGET).is_ok());
    }
}
