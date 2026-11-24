use std::cell::RefCell;

use tokio::time::{advance, sleep};
use two_bot_core::member_moderation_store::{
    continuation::synthetic_cursor, WarningHistoryCursor, WarningHistoryEntry, WarningHistoryPage,
};

use super::*;

#[derive(PartialEq, Eq)]
enum Mode {
    Page,
    UnknownAck,
    PublicAck,
    Revoked,
    SendFailed,
}

pub(super) struct Fake {
    pub(super) caller: Invocation,
    mode: Mode,
    pauses: [u64; 5], // ACK, first decision, read, final decision, original edit
    trace: RefCell<Vec<&'static str>>,
    reply: RefCell<Option<Reply>>,
}

const fn id(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

const CALLER: Invocation = Invocation {
    principal: (id(1), id(2)),
    channel: id(3),
};
const INITIAL: Input<'_> = Input::Initial("4");
const EXPIRED: Outcome = Outcome::Session(Status::Expired);

impl Fake {
    fn new(mode: Mode, pauses: [u64; 5]) -> Self {
        Self {
            mode,
            pauses,
            trace: RefCell::default(),
            reply: RefCell::default(),
            caller: CALLER,
        }
    }

    fn next(&self) -> String {
        let reply = self.reply.borrow();
        let Some(Reply::Page(content, Controls::Next(handle))) = reply.as_ref() else {
            panic!("expected a page with Next");
        };
        assert!(content.allowed_mentions.is_some());
        handle.clone()
    }

    fn status(&self, expected: Outcome) {
        assert!(
            matches!(self.reply.borrow().as_ref(), Some(Reply::Status(actual)) if *actual == expected)
        );
    }

    async fn step(&self, event: &'static str, phase: usize) {
        self.trace.borrow_mut().push(event);
        if self.pauses[phase] > 0 {
            sleep(Duration::from_secs(self.pauses[phase])).await;
        }
    }

    pub(super) async fn defer(&self) -> Option<bool> {
        self.step("ack", 0).await;
        match self.mode {
            Mode::UnknownAck => None,
            Mode::PublicAck => Some(false),
            _ => Some(true),
        }
    }

    // Models one merged resolver call, which uses at most five GETs per refresh.
    pub(super) async fn fresh(&self, who: Invocation, id: NonZeroU64) -> Result<(), Refusal> {
        assert_eq!(who.principal, CALLER.principal);
        assert_eq!((who.channel, id.get()), (CALLER.channel, 4));
        let final_check = self.trace.borrow().contains(&"closed");
        self.step("fresh", if final_check { 3 } else { 1 }).await;
        if final_check && self.mode == Mode::Revoked {
            return Err(Refusal::Denied);
        }
        Ok(())
    }

    pub(super) async fn read_closed(
        &self,
        scope: (NonZeroU64, NonZeroU64),
        cursor: Option<&WarningHistoryCursor>,
    ) -> WarningHistoryPage {
        assert_eq!(scope, (id(1), id(4)));
        self.step(if cursor.is_some() { "next" } else { "read" }, 2)
            .await;
        self.trace.borrow_mut().push("closed");
        let entries = (0..5)
            .map(|_| WarningHistoryEntry {
                reference: format!("W-{}", "a".repeat(64)),
                recorded_at_utc: "1970-01-01T00:00:00Z".into(),
                recorder_id: None,
            })
            .collect();
        WarningHistoryPage {
            entries,
            next: Some(synthetic_cursor(scope.0, scope.1)),
        }
    }

    pub(super) async fn edit_original(&self, reply: Reply) -> bool {
        *self.reply.borrow_mut() = Some(reply);
        self.step("edit", 4).await;
        self.mode != Mode::SendFailed
    }
}

fn ordered(fake: &Fake, read: &str) {
    assert_eq!(
        *fake.trace.borrow(),
        ["ack", "fresh", read, "closed", "fresh", "edit"]
    );
}

async fn offer(state: &HistoryState) -> String {
    let fake = Fake::new(Mode::Page, [0; 5]);
    handle(state, &fake, INITIAL).await;
    ordered(&fake, "read");
    fake.next()
}

#[tokio::test(start_paused = true)]
async fn initial_and_next_deliver_after_read_closes_and_authority_refreshes() {
    let state = HistoryState::default();
    let handle = offer(&state).await;
    advance(Duration::from_secs(5)).await;
    let fake = Fake::new(Mode::Page, [0; 5]);
    super::handle(&state, &fake, Input::Next(&handle)).await;
    ordered(&fake, "next");
    assert_ne!(fake.next(), handle);
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_ack_or_send_never_replays() {
    let state = HistoryState::default();
    for mode in [Mode::UnknownAck, Mode::PublicAck] {
        let fake = Fake::new(mode, [0; 5]);
        super::handle(&state, &fake, INITIAL).await;
        assert_eq!(*fake.trace.borrow(), ["ack"]);
        advance(Duration::from_secs(5)).await;
    }
    let handle = offer(&state).await;
    advance(Duration::from_secs(5)).await;
    let failed = Fake::new(Mode::SendFailed, [0; 5]);
    super::handle(&state, &failed, Input::Next(&handle)).await;
    for handle in [handle, failed.next()] {
        let fake = Fake::new(Mode::Page, [0; 5]);
        super::handle(&state, &fake, Input::Next(&handle)).await;
        fake.status(EXPIRED);
        assert_eq!(*fake.trace.borrow(), ["ack", "edit"]);
    }
}

#[tokio::test(start_paused = true)]
async fn foreign_and_expiring_handles_fail_closed_at_each_boundary() {
    for phase in [1, 3] {
        let state = HistoryState::default();
        let handle = offer(&state).await;
        let mut fake = Fake::new(Mode::Page, [0; 5]);
        fake.caller.channel = id(9);
        super::handle(&state, &fake, Input::Next(&handle)).await;
        fake.status(EXPIRED);
        advance(Duration::from_secs(119)).await;
        let mut fake = Fake::new(Mode::Page, [0; 5]);
        fake.pauses[phase] = 2;
        super::handle(&state, &fake, Input::Next(&handle)).await;
        fake.status(EXPIRED);
        assert_eq!(fake.trace.borrow().contains(&"next"), phase == 3);
    }
}

#[tokio::test(start_paused = true)]
async fn deadlines_and_revocation_discard_the_projection() {
    for pauses in [
        [0; 5],
        [0, 5, 0, 0, 0],
        [0, 4, 0, 3, 0],
        [0, 0, 2, 0, 0],
        [2, 4, 1, 2, 4],
    ] {
        let state = HistoryState::default();
        let revoke = pauses == [0; 5];
        let mode = if revoke { Mode::Revoked } else { Mode::Page };
        let fake = Fake::new(mode, pauses);
        super::handle(&state, &fake, INITIAL).await;
        if pauses[4] != 0 {
            assert_eq!(
                state.next(CALLER, &fake.next()).err(),
                Some(Status::Expired)
            );
        } else {
            fake.status(if revoke {
                Outcome::Authority(Refusal::Denied)
            } else {
                Outcome::Unavailable
            });
        }
    }
}
