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

#[cfg(test)]
mod tests {
    //! Unit pins for the closed admin gateway: every `refuse_closed` refusal
    //! path plus the reserved-key gate, exercised in-process so the HARD
    //! patch-coverage gate measures them. The subprocess acceptance suite
    //! boots a separate binary whose profiles llvm-cov never merges, so
    //! receiver-only code needs these in-process pins to count as covered.
    use super::*;
    use two_bot_testsupport::TestDatabase;

    const GUILD: &str = "100000000000000001";
    const GUILD_NUM: i64 = 100000000000000001;
    const READ_ID: &str = "adm_aaaaaaaaaaaaaaaa";
    const REVOKED_ID: &str = "adm_bbbbbbbbbbbbbbbb";
    const MODERATE_ID: &str = "adm_cccccccccccccccc";
    const UNKNOWN_ID: &str = "adm_ffffffffffffffff";
    const SECRET: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCD";
    const OTHER_SECRET: &str = "ABCDEFGHIJ0123456789abcdefghij0123456789";

    fn lazy_pool() -> sqlx::PgPool {
        sqlx::postgres::PgPoolOptions::new().connect_lazy_with(
            sqlx::postgres::PgConnectOptions::new()
                .host("agent-testdb")
                .port(5432)
                .username("agent_test")
                .database("internal_admin_unit"),
        )
    }

    fn decision(bearer: Option<String>, guild_id: Option<&str>, action: &str) -> AuthDecision {
        let mut body = serde_json::Map::new();
        if let Some(token) = bearer {
            body.insert("admin_bearer".to_string(), serde_json::Value::String(token));
        }
        if let Some(guild) = guild_id {
            body.insert(
                "guild_id".to_string(),
                serde_json::Value::String(guild.to_string()),
            );
        }
        AuthDecision {
            key_id: "test-admin-service".to_string(),
            action: action.to_string(),
            body,
        }
    }

    fn claim<'a>(scope: &'a str, guild: &'a str) -> ScopeClaim<'a> {
        ScopeClaim { scope, guild }
    }

    fn bearer(token_id: &str, secret: &str) -> String {
        format!("{token_id}.{secret}")
    }

    async fn database() -> Option<TestDatabase> {
        let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
            assert!(std::env::var("GITHUB_ACTIONS").is_err());
            eprintln!("SKIP closed-admin unit tests: TWO_TEST_DATABASE_URL is unset");
            return None;
        };
        Some(
            TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                .await
                .unwrap(),
        )
    }

    async fn insert_token(
        pool: &sqlx::PgPool,
        token_id: &str,
        secret: &str,
        scopes: &[&str],
        guilds: &[i64],
        revoked: bool,
    ) {
        let hash = InternalFlags::hash_admin_token_secret(secret.as_bytes()).unwrap();
        let scopes: Vec<String> = scopes.iter().map(ToString::to_string).collect();
        let guilds: Vec<i64> = guilds.to_vec();
        sqlx::query(
            "INSERT INTO admin_api_tokens
             (token_id, secret_hash, scopes, guild_ids, holder_id, holder_kind,
              issued_at, expires_at, revoked_at)
             VALUES ($1, $2, $3, $4, 'unit-holder', 'human', now(),
                     now() + interval '1 hour',
                     CASE WHEN $5 THEN now() ELSE NULL END)",
        )
        .bind(token_id)
        .bind(hash)
        .bind(scopes)
        .bind(guilds)
        .bind(revoked)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn requires_scope_matches_the_reserved_service_suffix() {
        assert!(requires_scope("worker-admin-service"));
        assert!(!requires_scope("worker-admin"));
        assert!(!requires_scope("admin-service"));
        assert!(!requires_scope(""));
    }

    #[tokio::test]
    async fn refuse_closed_rejects_a_missing_bearer() {
        let deciding = decision(None, None, "settings.get");
        let error = refuse_closed(&lazy_pool(), &deciding, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::Unauthorized);
        assert_eq!(error.message, AUTH_FAILURE_MESSAGE);
        assert_eq!(error.log_reason, "admin_bearer_refused");
    }

    #[tokio::test]
    async fn refuse_closed_rejects_an_oversize_bearer() {
        let huge = format!("{READ_ID}.{}", "a".repeat(4096));
        let deciding = decision(Some(huge), None, "settings.get");
        let error = refuse_closed(&lazy_pool(), &deciding, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::Unauthorized);
    }

    #[tokio::test]
    async fn refuse_closed_rejects_a_bearer_without_a_separator() {
        let deciding = decision(Some("separatortooken".to_string()), None, "settings.get");
        let error = refuse_closed(&lazy_pool(), &deciding, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::Unauthorized);
    }

    #[tokio::test]
    async fn refuse_closed_rejects_malformed_token_shapes() {
        let bad_id = decision(Some(bearer("not-an-id", SECRET)), None, "settings.get");
        let error = refuse_closed(&lazy_pool(), &bad_id, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::Unauthorized);
        let bad_secret = decision(Some(format!("{READ_ID}.has a space")), None, "settings.get");
        let error = refuse_closed(&lazy_pool(), &bad_secret, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::Unauthorized);
    }

    #[tokio::test]
    async fn refuse_closed_rejects_a_short_secret() {
        let deciding = decision(Some(bearer(READ_ID, &"a".repeat(31))), None, "settings.get");
        let error = refuse_closed(&lazy_pool(), &deciding, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::Unauthorized);
    }

    #[tokio::test]
    async fn refuse_closed_reports_an_unavailable_registry() {
        let pool = lazy_pool();
        pool.close().await;
        let deciding = decision(Some(bearer(READ_ID, SECRET)), None, "settings.get");
        let error = refuse_closed(&pool, &deciding, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(error.log_reason, "admin_registry_unavailable");
    }

    #[tokio::test]
    async fn refuse_closed_rejects_expired_future_and_scopeless_authority() {
        let Some(db) = database().await else { return };
        let pool = db.pool();
        for (token_id, issued, expires, scopes) in [
            ("adm_1111111111111111", -7200_i32, -3600_i32, vec!["read"]),
            ("adm_2222222222222222", 3600, 7200, vec!["read"]),
            ("adm_3333333333333333", -60, 3600, vec![]),
        ] {
            insert_token(pool, token_id, SECRET, &scopes, &[GUILD_NUM], false).await;
            sqlx::query(
                "UPDATE admin_api_tokens SET issued_at = now() + $2::int * interval '1 second',
                         expires_at = now() + $3::int * interval '1 second' WHERE token_id = $1",
            )
            .bind(token_id)
            .bind(issued)
            .bind(expires)
            .execute(pool)
            .await
            .unwrap();
            let deciding = decision(Some(bearer(token_id, SECRET)), Some(GUILD), "settings.get");
            let error = refuse_closed(pool, &deciding, claim("read", GUILD)).await;
            assert_eq!(
                error.code,
                ErrorCode::Unauthorized,
                "dead authority: {token_id}"
            );
            assert_eq!(error.log_reason, "admin_bearer_refused");
        }
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn refuse_closed_registry_paths_stay_closed() {
        let Some(db) = database().await else { return };
        let pool = db.pool();
        insert_token(pool, READ_ID, SECRET, &["read"], &[GUILD_NUM], false).await;
        insert_token(pool, REVOKED_ID, SECRET, &["read"], &[GUILD_NUM], true).await;
        insert_token(
            pool,
            MODERATE_ID,
            SECRET,
            &["moderate"],
            &[GUILD_NUM],
            false,
        )
        .await;
        let unknown = decision(
            Some(bearer(UNKNOWN_ID, SECRET)),
            Some(GUILD),
            "settings.get",
        );
        let error = refuse_closed(pool, &unknown, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::Unauthorized);
        let mismatched = decision(
            Some(bearer(READ_ID, OTHER_SECRET)),
            Some(GUILD),
            "settings.get",
        );
        let error = refuse_closed(pool, &mismatched, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::Unauthorized);
        let revoked = decision(
            Some(bearer(REVOKED_ID, SECRET)),
            Some(GUILD),
            "settings.get",
        );
        let error = refuse_closed(pool, &revoked, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::Unauthorized);
        let unparsable_guild = decision(Some(bearer(READ_ID, SECRET)), Some(GUILD), "settings.get");
        let error = refuse_closed(pool, &unparsable_guild, claim("read", "not-a-guild")).await;
        assert_eq!(error.code, ErrorCode::ActionNotAllowed);
        let guild_mismatch = decision(Some(bearer(READ_ID, SECRET)), Some("999"), "settings.get");
        let error = refuse_closed(pool, &guild_mismatch, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::ActionNotAllowed);
        let guild_unlisted = decision(Some(bearer(READ_ID, SECRET)), Some("999"), "settings.get");
        let error = refuse_closed(pool, &guild_unlisted, claim("read", "999")).await;
        assert_eq!(error.code, ErrorCode::ActionNotAllowed);
        let scope_mismatch = decision(Some(bearer(READ_ID, SECRET)), Some(GUILD), "settings.get");
        let error = refuse_closed(pool, &scope_mismatch, claim("moderate", GUILD)).await;
        assert_eq!(error.code, ErrorCode::ActionNotAllowed);
        let write_ungranted = decision(
            Some(bearer(READ_ID, SECRET)),
            Some(GUILD),
            "moderation.warn",
        );
        let error = refuse_closed(pool, &write_ungranted, claim("moderate", GUILD)).await;
        assert_eq!(error.code, ErrorCode::ActionNotAllowed);
        let granted_read = decision(Some(bearer(READ_ID, SECRET)), Some(GUILD), "settings.get");
        let error = refuse_closed(pool, &granted_read, claim("read", GUILD)).await;
        assert_eq!(error.code, ErrorCode::ActionNotAllowed);
        assert_eq!(error.message, "Internal action refused");
        assert_eq!(error.log_reason, "admin_gateway_closed");
        let granted_write = decision(
            Some(bearer(MODERATE_ID, SECRET)),
            Some(GUILD),
            "moderation.warn",
        );
        let error = refuse_closed(pool, &granted_write, claim("moderate", GUILD)).await;
        assert_eq!(error.code, ErrorCode::ActionNotAllowed);
    }
}
