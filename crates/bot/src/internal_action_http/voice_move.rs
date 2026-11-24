//! Signed, keyless terminal receipt for a staged move.
use super::{json, now_ms, one_header, AuthDecision, ErrorCode, Failure, ReceiverState, Value};
use crate::gateway::GatewayState;
use axum::http::HeaderMap;
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::{
    sync::OwnedSemaphorePermit,
    time::{timeout_at, Instant},
};
use tracing::Instrument;
use two_bot_core::{
    backup::guild_config::TWO_STAGING_GUILD_ID as GUILD,
    internal_action_config::InternalActionConfig,
    internal_actions::{body_hash, VoiceMoveOutcome as Outcome, VoiceMoveRequest},
};
use two_bot_discord::executor::member::VoiceMoveRoster;

const AUDIT_GRACE: Duration = Duration::from_secs(5);

/// The wildcard listener uses the DO's private relay. Both public and DO
/// intake rebuild the header set, so caller headers cannot supply this proof.
/// Direct private binds still use their existing explicit startup gate.
pub(super) fn lease_deadline(
    config: &InternalActionConfig,
    headers: &HeaderMap,
    wire_deadline: Instant,
) -> Option<Instant> {
    if !config.listen_addr().ip().is_unspecified() {
        return Some(wire_deadline);
    }
    let raw = one_header(headers, "x-two-voice-move-until", 16).ok()??;
    if raw.starts_with('0') || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let instant = Instant::now();
    let remaining = raw.parse::<u64>().ok()?.checked_sub(now_ms())?;
    // Matches the private relay's existing request deadline. A short proof
    // bounds the receiver even if a timed-out fetch finishes delivering late.
    if remaining == 0 || remaining > 20_000 {
        return None;
    }
    Some(wire_deadline.min(instant + Duration::from_millis(remaining)))
}

pub(super) async fn execute(
    state: &Arc<ReceiverState>,
    decision: AuthDecision,
    permit: OwnedSemaphorePermit,
    (id, nonce): (&str, &str),
    deadline: Instant,
) -> Result<Value, Failure> {
    let (done, receipt) = tokio::sync::oneshot::channel();
    let (task_state, id, delivery) = (state.clone(), id.to_owned(), body_hash(nonce.as_bytes()));
    {
        let mut children = state.children.lock().await;
        // Hold receiver capacity through audit even if the caller times out.
        children.spawn(
            async move {
                let _permit = permit;
                let _ =
                    done.send(respond(&task_state, &decision, (&delivery, &id), deadline).await);
            }
            .in_current_span(),
        );
        while children.try_join_next().is_some() {}
    }
    match timeout_at(deadline, receipt).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(Failure::code(ErrorCode::DiscordUnavailable)),
        Err(_) => Err(Failure::code(ErrorCode::UpstreamTimeout)),
    }
}

async fn respond(
    state: &ReceiverState,
    decision: &AuthDecision,
    (delivery, request_id): (&str, &str),
    deadline: Instant,
) -> Result<Value, Failure> {
    let unavailable = || Failure::code(ErrorCode::DiscordUnavailable);
    let request = VoiceMoveRequest::validate(&decision.body).map_err(Failure::from_action)?;
    let caller = state
        .config
        .caller_for(&decision.key_id)
        .ok_or_else(unavailable)?;
    let ids = [request.discord_id(), request.channel_id()];
    let api = state.voice_move.as_ref().ok_or_else(unavailable)?;
    let roster = VoiceMoveRoster::new(
        &state.voice_cache,
        &state.voice_clock.move_holds,
        &state.voice_move_turn,
    );
    let attempt = api
        .move_with_voice_evidence_until([GUILD, ids[0], ids[1]], roster, deadline, || {
            let gateway = state.gateway_state.try_read().ok()?;
            (*gateway == GatewayState::Connected && !state.draining.load(Ordering::SeqCst))
                .then_some(gateway)
        })
        .await;
    let outcome = match (attempt.outcome, attempt.retry_after_secs) {
        (Outcome::RateLimited, None) => Outcome::DiscordUnavailable,
        (outcome, _) => outcome,
    };
    if outcome == Outcome::BotNotPermitted {
        tracing::warn!("voice move denied by bot permissions");
    }
    let from = attempt.from.map(|id| id.to_string());
    let audit = sqlx::query("INSERT INTO internal_action_log (caller_hash, delivery_hash, action, phase, \
        guild_id, target_id, response_code, http_status, affected, outcome, evidence_code, from_channel_id, resource_id, request_id) \
        VALUES ($1, $2, 'voice.move_member', 'terminal', $3, $4, $5, $6, $7, $8, 'executor', $9, $10, $11)")
        .bind(body_hash(caller.as_bytes())).bind(delivery)
        .bind(GUILD).bind(ids[0]).bind(outcome.response_code()).bind(i32::from(outcome.status()))
        .bind(outcome.affected()).bind(outcome.as_str()).bind(&from).bind(ids[1]).bind(request_id)
        .execute(state.store.pool());
    if !matches!(timeout_at(deadline + AUDIT_GRACE, audit).await, Ok(Ok(_))) {
        tracing::warn!(
            error_class = "voice_move_audit_unconfirmed",
            "voice move terminal receipt unconfirmed"
        );
        return Err(unavailable());
    }
    match outcome {
        Outcome::DiscordUnavailable => Err(unavailable()),
        Outcome::RateLimited => Err(Failure {
            retry_after: attempt.retry_after_secs,
            ..Failure::code(ErrorCode::RateLimited)
        }),
        Outcome::Moved => Ok(json!({"outcome": "moved", "from_channel_id": from})),
        _ => Ok(json!({"outcome": outcome.as_str()})),
    }
}
