//! Fenced member execution using the existing scalar-only durable store.
//!
//! Authenticate and burn the durable nonce BEFORE calling this module. Caller
//! identity must be stable across signing-key rotation. Unknown exchanges never
//! release a slot or silently retry a mutation (docs/internal-action-store.md).

use super::{
    numeric_id, ActionError, ActionExecutor, ErrorCode, GuildAddMemberRequest, MemberError,
    MemberOutcome, RoleAssignRequest,
};
use std::collections::HashMap;
use two_bot_core::internal_action_store::{
    AuditSubject, DiscordId, InternalActionStore, InternalClaim, InternalStoreError,
    RequestIdentity, TerminalFailure, TerminalResponse,
};
use two_bot_core::internal_actions::{parse_body_object, require_field_str};

pub struct MemberActionConfig<'a> {
    pub guild_id: &'a str,
    pub bot_user_id: &'a str,
    pub role_keys: &'a HashMap<String, String>,
    /// The authorized receiver supplies its evaluated rollout flag; default off.
    pub allow_add_member: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberExecution {
    pub outcome: MemberOutcome,
    pub replayed: bool,
}

fn storage_error(_: InternalStoreError) -> ActionError {
    ActionError::new(
        ErrorCode::Internal,
        "The durable store is not available",
        "store_unavailable",
    )
}
fn pending() -> ActionError {
    ActionError::new(
        ErrorCode::InProgress,
        "An earlier attempt at this operation is still running",
        "idempotency_in_flight",
    )
}
fn replay(action: &str, response: TerminalResponse) -> Result<MemberExecution, ActionError> {
    let outcome = match response {
        TerminalResponse::Success {
            resource_id: None,
            affected,
        } if affected <= 1 => match (action, affected) {
            ("role.assign", 1) => MemberOutcome::Assigned,
            ("role.assign", 0) => MemberOutcome::AlreadyHeld,
            ("guild.add_member", 1) => MemberOutcome::Added,
            ("guild.add_member", 0) => MemberOutcome::AlreadyMember,
            _ => return Err(storage_error(InternalStoreError::Unavailable)),
        },
        TerminalResponse::Failure(failure) => {
            // Exhaustive mapping: reconciliation can record any terminal failure.
            // Provider detail is not stored; NoEffect uses the legacy 502 code.
            let (code, message, reason) = match failure {
                TerminalFailure::Malformed => (
                    ErrorCode::Malformed,
                    "The recorded request was malformed",
                    "malformed",
                ),
                TerminalFailure::ActionNotAllowed => (
                    ErrorCode::ActionNotAllowed,
                    "The recorded action was not allowed",
                    "action_not_allowed",
                ),
                TerminalFailure::DiscordRejected => (
                    ErrorCode::DiscordRejected,
                    "Discord refused the request",
                    "discord_rejected",
                ),
                TerminalFailure::NoEffect => (
                    ErrorCode::DiscordUnavailable,
                    "The recorded attempt had no effect",
                    "no_effect",
                ),
                // Member intents never record a settings CAS conflict; a stored
                // version_conflict here is a database inconsistency, not a replay.
                TerminalFailure::VersionConflict => {
                    return Err(storage_error(InternalStoreError::Unavailable));
                }
            };
            return Err(ActionError::new(code, message, reason));
        }
        _ => return Err(storage_error(InternalStoreError::Unavailable)),
    };
    Ok(MemberExecution {
        outcome,
        replayed: true,
    })
}

impl ActionExecutor {
    /// Parse validated member inputs from the exact authenticated bytes, then
    /// commit intent before REST. The OAuth token is not a request-struct field,
    /// outcome, audit subject, or stored response. The store hashes the bytes.
    pub async fn execute_stored_member(
        &self,
        store: &InternalActionStore,
        caller: &str,
        idempotency_key: &str,
        authenticated_payload: &[u8],
        config: &MemberActionConfig<'_>,
    ) -> Result<MemberExecution, ActionError> {
        // The same parser `authorize` used: one document per signed body, and
        // a repeated key at any depth refuses before any store or REST call.
        let body = &parse_body_object(authenticated_payload)?;
        let action = require_field_str(body, "action")?;
        let (user, role_request, member_request) = match action {
            "role.assign" => {
                let request = RoleAssignRequest::validate(body, config.role_keys)?;
                numeric_id(request.role_id())?;
                (request.discord_id(), Some(request), None)
            }
            "guild.add_member" if config.allow_add_member => {
                let request = GuildAddMemberRequest::validate(body)?;
                (request.discord_id(), None, Some(request))
            }
            _ => {
                return Err(ActionError::new(
                    ErrorCode::ActionNotAllowed,
                    "Action is not enabled",
                    "action_disabled",
                ))
            }
        };
        numeric_id(user)?;
        numeric_id(config.guild_id)?;
        numeric_id(config.bot_user_id)?;
        let identity = RequestIdentity::new(caller, idempotency_key, action, authenticated_payload)
            .map_err(|_| {
                ActionError::new(
                    ErrorCode::Malformed,
                    "Invalid internal-action identity",
                    "bad_idempotency_key",
                )
            })?;
        let subject = AuditSubject {
            guild_id: Some(DiscordId::new(config.guild_id).map_err(storage_error)?),
            target_id: Some(DiscordId::new(user).map_err(storage_error)?),
            actor_id: None,
            resolved_role_id: role_request
                .as_ref()
                .map(|request| DiscordId::new(request.role_id()))
                .transpose()
                .map_err(storage_error)?,
        };
        let claim = match store
            .claim(&identity, &subject)
            .await
            .map_err(storage_error)?
        {
            InternalClaim::Claimed(claim) => claim,
            InternalClaim::Replay(response) => return replay(action, response),
            InternalClaim::InFlight | InternalClaim::NeedsReconciliation => return Err(pending()),
            InternalClaim::Mismatch => {
                return Err(ActionError::new(
                    ErrorCode::Malformed,
                    "This Idempotency-Key was used for a different request",
                    "idempotency_key_reused",
                ))
            }
        };
        let result = if let Some(request) = role_request {
            self.assign_internal_role_once(config.guild_id, config.bot_user_id, &request)
                .await
        } else {
            // Already validated; never format the parsed body or this local.
            let token = require_field_str(body, "access_token")?;
            self.add_internal_member_once(
                config.guild_id,
                &member_request.expect("validated member action"),
                token,
            )
            .await
        };
        match result {
            Ok(outcome) => {
                let affected = u32::from(matches!(
                    outcome,
                    MemberOutcome::Added | MemberOutcome::Assigned
                ));
                store
                    .finish(
                        &claim,
                        &TerminalResponse::Success {
                            resource_id: None,
                            affected,
                        },
                    )
                    .await
                    .map_err(storage_error)?;
                Ok(MemberExecution {
                    outcome,
                    replayed: false,
                })
            }
            Err(error @ MemberError::Guard(_)) => {
                // Member actions have one mutation, last. A typed admission
                // refusal therefore proves it was never dispatched, even if
                // preceding hierarchy GETs succeeded. Do not release on a wire
                // 429, timeout or transport failure.
                store
                    .release_proven_not_sent(claim)
                    .await
                    .map_err(storage_error)?;
                Err(error.into_action_error())
            }
            Err(MemberError::Action(error)) => {
                if error.code == ErrorCode::DiscordRejected {
                    store
                        .finish(
                            &claim,
                            &TerminalResponse::Failure(TerminalFailure::DiscordRejected),
                        )
                        .await
                        .map_err(storage_error)?;
                } else {
                    // 429, 5xx, transport loss, timeout, or failed policy read:
                    // conservative fenced uncertainty, never automatic replay.
                    store.mark_unknown(&claim).await.map_err(storage_error)?;
                }
                Err(error)
            }
        }
    }
}
