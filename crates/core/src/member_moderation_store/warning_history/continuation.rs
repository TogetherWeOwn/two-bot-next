//! Process-local warning history admission and continuation for future private delivery.

use std::{
    collections::HashMap,
    num::NonZeroU64,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use rand::{rngs::SysRng, TryRng};
use tokio::time::Instant;

use super::WarningHistoryCursor;

const LIFETIME: Duration = Duration::from_secs(120);
const COOLDOWN: Duration = Duration::from_secs(5);
const MAX_PAGES: u8 = 5;
const MAX_SESSIONS: usize = 128;
const MAX_COOLDOWNS: usize = 1024;
const HANDLE_PREFIX: &str = "wh1:";

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct Principal {
    guild: NonZeroU64,
    actor: NonZeroU64,
}

#[derive(Clone, Copy)]
struct Invocation {
    principal: Principal,
    channel: NonZeroU64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Handle([u8; 16]);

impl Handle {
    fn generate() -> Result<Self, Status> {
        let mut bytes = [0; 16];
        SysRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| Status::Unavailable)?;
        Ok(Self(bytes))
    }

    fn parse(input: &str) -> Result<Self, Status> {
        let hex = input.strip_prefix(HANDLE_PREFIX).ok_or(Status::Expired)?;
        let mut bytes = [0; 16];
        if hex.len() != 32 || hex.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(Status::Expired);
        }
        hex::decode_to_slice(hex, &mut bytes).map_err(|_| Status::Expired)?;
        Ok(Self(bytes))
    }

    fn encode(self) -> String {
        format!("{HANDLE_PREFIX}{}", hex::encode(self.0))
    }
}

// The corresponding WH_EXPIRED, WH_BUSY and WH_UNAVAILABLE outcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Expired,
    Busy,
    Unavailable,
}

enum Controls {
    Next(Handle),
    Done,
    Limit, // WH_LIMIT: the fifth page had more rows.
}

enum Page {
    Ready {
        handle: Handle,
        cursor: WarningHistoryCursor,
    },
    InFlight {
        owner: Arc<()>,
        previous: Option<Handle>,
        prepared: Option<Handle>,
    },
}

struct Session {
    channel: NonZeroU64,
    target: NonZeroU64,
    expires: Instant,
    shown: u8,
    page: Page,
}

#[derive(Default)]
struct Sessions {
    sessions: HashMap<Principal, Session>,
    cooldowns: HashMap<Principal, Instant>,
}

impl Sessions {
    fn admit(&mut self, principal: Principal, now: Instant) -> Result<(), Status> {
        self.cooldowns.retain(|_, until| *until > now);
        if self.cooldowns.len() >= MAX_COOLDOWNS || self.cooldowns.contains_key(&principal) {
            return Err(Status::Busy);
        }
        self.cooldowns.insert(principal, now + COOLDOWN);
        Ok(())
    }

    fn find(&mut self, invocation: Invocation, now: Instant) -> Result<&mut Session, Status> {
        self.sessions.retain(|_, session| session.expires > now);
        self.sessions
            .get_mut(&invocation.principal)
            .filter(|session| session.channel == invocation.channel)
            .ok_or(Status::Expired)
    }
}

#[derive(Default)]
struct HistoryState(Mutex<Sessions>);

impl HistoryState {
    fn lock(&self) -> Result<MutexGuard<'_, Sessions>, Status> {
        self.0.lock().map_err(|_| Status::Unavailable)
    }

    fn start(&self, invocation: Invocation, target: NonZeroU64) -> Result<Attempt<'_>, Status> {
        let mut state = self.lock()?;
        let now = Instant::now();
        state.admit(invocation.principal, now)?;
        state.sessions.retain(|_, session| session.expires > now);
        if state.sessions.len() >= MAX_SESSIONS
            || state.sessions.contains_key(&invocation.principal)
        {
            return Err(Status::Busy);
        }
        let attempt = Attempt::new(self, invocation.principal, target);
        state.sessions.insert(
            invocation.principal,
            Session {
                channel: invocation.channel,
                target,
                expires: now + LIFETIME,
                shown: 0,
                page: attempt.in_flight(None),
            },
        );
        Ok(attempt)
    }

    fn next(&self, invocation: Invocation, input: &str) -> Result<Attempt<'_>, Status> {
        let handle = Handle::parse(input)?;
        let mut state = self.lock()?;
        let now = Instant::now();
        let session = state.find(invocation, now)?;
        if !matches!(session.page, Page::Ready { handle: expected, .. } if handle == expected) {
            return Err(Status::Expired);
        }
        // Charge before authority work. Busy preserves the current handle.
        state.admit(invocation.principal, now)?;
        let session = state.find(invocation, now)?;
        let mut attempt = Attempt::new(self, invocation.principal, session.target);
        if let Page::Ready { cursor, .. } =
            std::mem::replace(&mut session.page, attempt.in_flight(Some(handle)))
        {
            attempt.cursor = Some(cursor);
        }
        Ok(attempt)
    }

    fn stop(&self, invocation: Invocation, input: &str) -> Result<(), Status> {
        let handle = Handle::parse(input)?;
        let mut state = self.lock()?;
        let session = state.find(invocation, Instant::now())?;
        let matches = match session.page {
            Page::Ready {
                handle: expected, ..
            } => handle == expected,
            Page::InFlight {
                previous, prepared, ..
            } => previous == Some(handle) || prepared == Some(handle),
        };
        if !matches {
            return Err(Status::Expired);
        }
        state.sessions.remove(&invocation.principal);
        Ok(())
    }
}

// A lease carries the cursor for one read. Dropping it after ACK, authority,
// read or delivery failure discards the session and leaves the cooldown charged.
struct Attempt<'a> {
    state: &'a HistoryState,
    principal: Principal,
    owner: Arc<()>,
    target: NonZeroU64,
    cursor: Option<WarningHistoryCursor>,
}

impl<'a> Attempt<'a> {
    fn new(state: &'a HistoryState, principal: Principal, target: NonZeroU64) -> Self {
        Self {
            state,
            principal,
            target,
            owner: Arc::new(()),
            cursor: None,
        }
    }

    fn in_flight(&self, previous: Option<Handle>) -> Page {
        Page::InFlight {
            owner: self.owner.clone(),
            previous,
            prepared: None,
        }
    }

    fn owns(&self, session: &Session) -> bool {
        matches!(&session.page, Page::InFlight { owner, .. } if Arc::ptr_eq(owner, &self.owner))
    }

    fn active<'s>(&self, state: &'s mut Sessions) -> Result<&'s mut Session, Status> {
        state
            .sessions
            .get_mut(&self.principal)
            .filter(|session| self.owns(session) && session.expires > Instant::now())
            .ok_or(Status::Expired)
    }

    fn check_active(&self) -> Result<(), Status> {
        let mut state = self.state.lock()?;
        self.active(&mut state).map(|_| ())
    }

    fn request(&self) -> Result<(NonZeroU64, Option<&WarningHistoryCursor>), Status> {
        self.check_active()?;
        Ok((self.target, self.cursor.as_ref()))
    }

    fn prepare(mut self, next: Option<WarningHistoryCursor>) -> Result<PreparedPage<'a>, Status> {
        let mut state = self.state.lock()?;
        let session = self.active(&mut state)?;
        let controls = if let Some(cursor) = &next {
            if cursor.guild != self.principal.guild.to_string()
                || cursor.target != self.target.to_string()
            {
                return Err(Status::Unavailable);
            }
            if session.shown + 1 == MAX_PAGES {
                Controls::Limit
            } else {
                let handle = Handle::generate()?;
                if let Page::InFlight {
                    previous, prepared, ..
                } = &mut session.page
                {
                    if *previous == Some(handle) {
                        return Err(Status::Unavailable);
                    }
                    *prepared = Some(handle);
                }
                Controls::Next(handle)
            }
        } else {
            Controls::Done
        };
        self.cursor = None;
        Ok(PreparedPage {
            attempt: self,
            controls,
            next,
        })
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if state
                .sessions
                .get(&self.principal)
                .is_some_and(|session| self.owns(session))
            {
                state.sessions.remove(&self.principal);
            }
        }
    }
}

struct PreparedPage<'a> {
    attempt: Attempt<'a>,
    controls: Controls,
    next: Option<WarningHistoryCursor>,
}

impl PreparedPage<'_> {
    // Call at the private delivery boundary, after the final authority refresh.
    fn controls(&self) -> Result<&Controls, Status> {
        self.attempt.check_active()?;
        Ok(&self.controls)
    }

    // Publish the new handle only after the private delivery is confirmed.
    fn confirm(self) -> Result<(), Status> {
        let mut state = self.attempt.state.lock()?;
        let session = self.attempt.active(&mut state)?;
        if let (Controls::Next(handle), Some(cursor)) = (self.controls, self.next) {
            session.shown += 1;
            session.page = Page::Ready { handle, cursor };
        } else {
            state.sessions.remove(&self.attempt.principal);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
