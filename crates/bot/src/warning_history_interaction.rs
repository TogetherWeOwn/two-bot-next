//! Synthetic interaction boundary, compiled only in bot tests.

use std::{future::Future, num::NonZeroU64, time::Duration};

use tokio::time::{timeout_at, Instant};
use twilight_model::http::interaction::InteractionResponseData;
use two_bot_core::{
    lfg_list::canonical_snowflake,
    member_moderation_store::continuation::{
        Attempt, Controls, HistoryState, Invocation, Prepared, Status,
    },
};
use two_bot_discord::warning_history::render_warning_history;

use super::warning_history_authorizer::Refusal;
use tests::Fake;

const TOTAL: Duration = Duration::from_secs(12);
const AUTHORITY: Duration = Duration::from_secs(7);
const REFRESH: Duration = Duration::from_secs(5);
const READ: Duration = Duration::from_secs(2);

enum Input<'a> {
    Initial(&'a str),
    Next(&'a str),
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Invalid,
    Authority(Refusal),
    Session(Status),
    Unavailable,
    Empty,
    End,
}

enum Reply {
    Status(Outcome),
    Page(Box<InteractionResponseData>, Controls),
}

// The bound transport stands in for an authenticated human interaction in the
// configured application and guild. Its caller scope cannot come from input.
async fn handle(state: &HistoryState, io: &Fake, input: Input<'_>) {
    let deadline = Instant::now() + TOTAL;
    let caller = io.caller;
    let attempt = match input {
        Input::Initial(raw) if raw.len() <= 20 && canonical_snowflake(raw) => raw
            .parse()
            .map_err(|_| Outcome::Invalid)
            .and_then(|target| state.start(caller, target).map_err(Outcome::Session)),
        Input::Initial(_) => Err(Outcome::Invalid),
        Input::Next(handle) => state.next(caller, handle).map_err(Outcome::Session),
    };
    if within(deadline, io.defer()).await != Ok(Some(true)) {
        return;
    }
    let page = match attempt {
        Ok(attempt) => page(io, caller, attempt, deadline).await,
        Err(status) => Err(status),
    };
    let (reply, prepared) = match page {
        Ok((reply, prepared)) => (reply, Some(prepared)),
        Err(status) => (Reply::Status(status), None),
    };
    let delivered = within(deadline, io.edit_original(reply)).await;
    if let (Ok(true), Some(prepared)) = (delivered, prepared) {
        let _ = prepared.confirm();
    }
}

async fn page<'a>(
    io: &Fake,
    caller: Invocation,
    attempt: Attempt<'a>,
    deadline: Instant,
) -> Result<(Reply, Prepared<'a>), Outcome> {
    let (target, _) = attempt.request().map_err(Outcome::Session)?;
    let started = Instant::now();
    within(deadline.min(started + REFRESH), io.fresh(caller, target))
        .await?
        .map_err(Outcome::Authority)?;
    let remaining = AUTHORITY.saturating_sub(started.elapsed());
    let (target, cursor) = attempt.request().map_err(Outcome::Session)?;
    let read_deadline = deadline.min(Instant::now() + READ);
    let read = io.read_closed((caller.principal.0, target), cursor);
    let page = within(read_deadline, read).await?;
    let empty = page.entries.is_empty();
    let continuation = cursor.is_some();
    let content =
        render_warning_history(&target.to_string(), &page).map_err(|_| Outcome::Unavailable)?;
    let final_deadline = deadline.min(Instant::now() + REFRESH.min(remaining));
    within(final_deadline, io.fresh(caller, target))
        .await?
        .map_err(Outcome::Authority)?;
    let (prepared, controls) = attempt.prepare(page.next).map_err(Outcome::Session)?;
    let reply = match (empty, continuation) {
        (true, false) => Reply::Status(Outcome::Empty),
        (true, true) => Reply::Status(Outcome::End),
        (false, _) => Reply::Page(Box::new(content), controls),
    };
    Ok((reply, prepared))
}

async fn within<T>(deadline: Instant, operation: impl Future<Output = T>) -> Result<T, Outcome> {
    if Instant::now() >= deadline {
        return Err(Outcome::Unavailable);
    }
    let result = timeout_at(deadline, operation)
        .await
        .map_err(|_| Outcome::Unavailable)?;
    (Instant::now() < deadline)
        .then_some(result)
        .ok_or(Outcome::Unavailable)
}

#[path = "warning_history_interaction/tests.rs"]
mod tests;
