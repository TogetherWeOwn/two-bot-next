//! Closed admin gateway authorization. A service MAC authenticates transport,
//! never a token holder. No request from this module can reach an executor.

use two_bot_core::internal_actions::{
    signatures_match, ActionError, AuthDecision, ErrorCode, InternalFlags, ScopeClaim,
    AUTH_FAILURE_MESSAGE,
};

fn unauthorized() -> ActionError {
    ActionError::new(
        ErrorCode::Unauthorized,
        AUTH_FAILURE_MESSAGE,
        "admin_bearer_refused",
    )
}

fn denied() -> ActionError {
    ActionError::new(
        ErrorCode::ActionNotAllowed,
        "Internal action refused",
        "admin_gateway_closed",
    )
}

/// The service-only key must never become a legacy website execution key.
pub(crate) fn requires_scope(key_id: &str) -> bool {
    // Static reserved namespace: losing/changing Worker configuration cannot
    // turn a still-installed admin key into a legacy website execution key.
    key_id.ends_with("-admin-service")
}

/// Evaluate registry authority without a positive cache. Missing schema, NULL
/// authority, lookup failure or timeout is unavailable, never a cached grant.
/// Success still refuses: gateway audit/confirm wiring must land before verbs.
pub(crate) async fn refuse_closed(
    pool: &sqlx::PgPool,
    decision: &AuthDecision,
    claim: ScopeClaim<'_>,
) -> ActionError {
    let Some(bearer) = decision
        .body
        .get("admin_bearer")
        .and_then(serde_json::Value::as_str)
    else {
        return unauthorized();
    };
    if bearer.len() > 4096 {
        return unauthorized();
    }
    let Some((token_id, secret)) = bearer.split_once('.') else {
        return unauthorized();
    };
    if !InternalFlags::is_valid_admin_token_id(token_id)
        || !secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return unauthorized();
    }
    let Some(presented_hash) = InternalFlags::hash_admin_token_secret(secret.as_bytes()) else {
        return unauthorized();
    };
    // Transient input only: neither bearer nor hash is formatted or persisted.
    let lookup = sqlx::query_as::<_, (String, Vec<String>, Vec<i64>, String, String, bool)>(
        "SELECT secret_hash, scopes, guild_ids, holder_id, holder_kind,
         (revoked_at IS NULL AND issued_at <= now() AND now() < expires_at
          AND expires_at <= issued_at + interval '90 days')
         FROM admin_api_tokens WHERE token_id = $1",
    )
    .bind(token_id)
    .fetch_optional(pool);
    let row = match tokio::time::timeout(std::time::Duration::from_secs(5), lookup).await {
        Ok(Ok(row)) => row,
        _ => {
            return ActionError::new(
                ErrorCode::Internal,
                "Internal action storage unavailable",
                "admin_registry_unavailable",
            )
        }
    };
    // A same-length comparison also runs for an unknown id. The decoy cannot
    // authorize: existence is checked separately, after comparison.
    let stored_hash = row.as_ref().map_or(
        "0000000000000000000000000000000000000000000000000000000000000000",
        |row| row.0.as_str(),
    );
    let matched = signatures_match(stored_hash, &presented_hash);
    let Some((_, scopes, guilds, holder, kind, live)) = row else {
        return unauthorized();
    };
    if !matched
        || !live
        || holder.trim().is_empty()
        || !matches!(kind.as_str(), "human" | "ai_agent")
        || (kind == "ai_agent" && scopes.iter().any(|s| s == "admin"))
        || scopes.is_empty()
        || scopes
            .iter()
            .any(|s| !matches!(s.as_str(), "read" | "moderate" | "admin"))
    {
        return unauthorized();
    }
    let guild = match claim.guild.parse::<i64>() {
        Ok(guild) if guild > 0 => guild,
        _ => return denied(),
    };
    if decision
        .body
        .get("guild_id")
        .and_then(serde_json::Value::as_str)
        != Some(claim.guild)
        || !guilds.contains(&guild)
        || InternalFlags::required_admin_scope(&decision.action) != Some(claim.scope)
    {
        return denied();
    }
    let granted = if claim.scope == "read" {
        // Read implication only; every write needs the explicitly listed scope.
        !scopes.is_empty()
    } else {
        scopes.iter().any(|s| s == claim.scope)
    };
    if !granted {
        return denied();
    }
    denied()
}
