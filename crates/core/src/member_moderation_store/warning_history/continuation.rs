//! Process-local admission and continuation for future private delivery.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rand::{rngs::SysRng, TryRng};
use tokio::time::Instant;

use super::WarningHistoryCursor;

const LIFETIME: Duration = Duration::from_secs(120);
const COOLDOWN: Duration = Duration::from_secs(5);
const MAX_PAGES: u8 = 5;
const MAX_SESSIONS: usize = 128;
const MAX_COOLDOWNS: usize = 1024;
const PREFIX: &str = "wh1:";

type Principal = (NonZeroU64, NonZeroU64); // guild, actor
type Sessions = HashMap<Principal, Session>;
type Cooldowns = HashMap<Principal, Instant>;

#[derive(Clone, Copy)]
struct Invocation {
    principal: Principal,
    channel: NonZeroU64,
}

// WH_EXPIRED, WH_BUSY and WH_UNAVAILABLE.
#[derive(Debug, PartialEq, Eq)]
enum Status {
    Expired,
    Busy,
    Unavailable,
}

enum Controls {
    Next(String),
    Done,
    Limit, // WH_LIMIT
}

struct Session {
    channel: NonZeroU64,
    target: NonZeroU64,
    expires: Instant,
    shown: u8,
    handle: Arc<str>,
    previous: Option<Arc<str>>, // spent for Next, usable for Stop during delivery
    cursor: Option<WarningHistoryCursor>, // present only when ready for Next
}

#[derive(Default)]
struct State {
    sessions: Sessions,
    cooldowns: Cooldowns,
}

#[derive(Default)]
struct HistoryState(Mutex<State>);

fn random_handle() -> Result<Arc<str>, Status> {
    let mut bytes = [0; 16];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| Status::Unavailable)?;
    Ok(Arc::from(format!("{PREFIX}{}", hex::encode(bytes))))
}

fn validate(input: &str) -> Result<(), Status> {
    let hex = input.strip_prefix(PREFIX).ok_or(Status::Expired)?;
    let lower_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    (hex.len() == 32 && hex.bytes().all(lower_hex))
        .then_some(())
        .ok_or(Status::Expired)
}

fn admit(cooldowns: &mut Cooldowns, who: Principal, now: Instant) -> Result<(), Status> {
    cooldowns.retain(|_, until| *until > now);
    if cooldowns.len() >= MAX_COOLDOWNS || cooldowns.contains_key(&who) {
        return Err(Status::Busy);
    }
    cooldowns.insert(who, now + COOLDOWN);
    Ok(())
}

impl HistoryState {
    fn lock(&self) -> Result<MutexGuard<'_, State>, Status> {
        self.0.lock().map_err(|_| Status::Unavailable)
    }

    fn start(&self, scope: Invocation, target: NonZeroU64) -> Result<Attempt<'_>, Status> {
        let mut state = self.lock()?;
        let now = Instant::now();
        admit(&mut state.cooldowns, scope.principal, now)?;
        state.sessions.retain(|_, s| s.expires > now);
        if state.sessions.len() >= MAX_SESSIONS || state.sessions.contains_key(&scope.principal) {
            return Err(Status::Busy);
        }
        let mut session = Session {
            target,
            channel: scope.channel,
            expires: now + LIFETIME,
            shown: 0,
            handle: random_handle()?,
            previous: None,
            cursor: None,
        };
        let attempt = session.attempt(self, scope.principal);
        state.sessions.insert(scope.principal, session);
        Ok(attempt)
    }

    fn next(&self, scope: Invocation, handle: &str) -> Result<Attempt<'_>, Status> {
        validate(handle)?;
        let mut state = self.lock()?;
        let now = Instant::now();
        let State {
            sessions,
            cooldowns,
        } = &mut *state;
        let session = find(sessions, scope, now)?;
        if session.handle.as_ref() != handle || session.cursor.is_none() {
            return Err(Status::Expired);
        }
        // Busy preserves the current handle for an explicit later retry.
        admit(cooldowns, scope.principal, now)?;
        let next = random_handle()?;
        if next == session.handle {
            return Err(Status::Unavailable);
        }
        session.previous = Some(std::mem::replace(&mut session.handle, next));
        Ok(session.attempt(self, scope.principal))
    }

    fn stop(&self, scope: Invocation, handle: &str) -> Result<(), Status> {
        validate(handle)?;
        let mut state = self.lock()?;
        let session = find(&mut state.sessions, scope, Instant::now())?;
        if session.handle.as_ref() != handle && session.previous.as_deref() != Some(handle) {
            return Err(Status::Expired);
        }
        state.sessions.remove(&scope.principal);
        Ok(())
    }
}

fn find(sessions: &mut Sessions, scope: Invocation, now: Instant) -> Result<&mut Session, Status> {
    sessions.retain(|_, s| s.expires > now);
    sessions
        .get_mut(&scope.principal)
        .filter(|s| s.channel == scope.channel)
        .ok_or(Status::Expired)
}

impl Session {
    fn attempt<'a>(&mut self, state: &'a HistoryState, who: Principal) -> Attempt<'a> {
        Attempt {
            state,
            who,
            handle: self.handle.clone(),
            cursor: self.cursor.take(),
        }
    }
}

// Dropping a failed or canceled attempt closes its session and preserves admission.
struct Attempt<'a> {
    state: &'a HistoryState,
    who: Principal,
    handle: Arc<str>,
    cursor: Option<WarningHistoryCursor>,
}

impl<'a> Attempt<'a> {
    fn owns(&self, session: &Session) -> bool {
        session.cursor.is_none() && Arc::ptr_eq(&session.handle, &self.handle)
    }

    fn active<'s>(&self, state: &'s mut State) -> Result<&'s mut Session, Status> {
        state
            .sessions
            .get_mut(&self.who)
            .filter(|s| self.owns(s) && s.expires > Instant::now())
            .ok_or(Status::Expired)
    }

    fn request(&self) -> Result<(NonZeroU64, Option<&WarningHistoryCursor>), Status> {
        let target = self.active(&mut *self.state.lock()?)?.target;
        Ok((target, self.cursor.as_ref()))
    }

    // Prepare at the delivery boundary after the final fresh authorization.
    fn prepare(
        mut self,
        mut next: Option<WarningHistoryCursor>,
    ) -> Result<(Prepared<'a>, Controls), Status> {
        let mut state = self.state.lock()?;
        let session = self.active(&mut state)?;
        if next.as_ref().is_some_and(|c| {
            c.guild != self.who.0.to_string() || c.target != session.target.to_string()
        }) {
            return Err(Status::Unavailable);
        }
        let controls = if next.is_none() {
            Controls::Done
        } else if session.shown + 1 == MAX_PAGES {
            next = None;
            Controls::Limit
        } else {
            Controls::Next(self.handle.to_string())
        };
        self.cursor = None;
        Ok((Prepared(self, next), controls))
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if state.sessions.get(&self.who).is_some_and(|s| self.owns(s)) {
                state.sessions.remove(&self.who);
            }
        }
    }
}

struct Prepared<'a>(Attempt<'a>, Option<WarningHistoryCursor>);

impl Prepared<'_> {
    fn confirm(self) -> Result<(), Status> {
        let mut state = self.0.state.lock()?;
        let session = self.0.active(&mut state)?;
        if let Some(cursor) = self.1 {
            session.shown += 1;
            session.previous = None;
            session.cursor = Some(cursor);
        } else {
            state.sessions.remove(&self.0.who);
        }
        Ok(())
    }
}

mod tests;
