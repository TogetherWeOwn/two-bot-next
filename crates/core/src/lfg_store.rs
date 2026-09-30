//! LFG sqlx store: `lfg_posts`, `lfg_roles`, `lfg_signups` persistence.
//!
//! Behind the `db` feature so pure-domain unit tests never need a Postgres
//! driver. Ports the LFG half of legacy `src/announcements/store.ts` to
//! Postgres (`$n` placeholders, `::timestamptz` casts): the `putLfg` upsert
//! with its guild-fence `WHERE`, ordered role/signup listings, the
//! advisory-locked `signupLfg` transaction, `leaveLfg`, and the open-only
//! `closeLfg` update. Capacity decisions delegate to
//! [`crate::lfg::adjudicate_signup`] so the outcome order stays identical to
//! the pure function callers unit-test.
//!
//! Deliberately out of scope: `event_rsvps` + feed tables from the same
//! legacy migration file (TOG-10083 / TOG-10085 own those) and the
//! `announcements_audit_log` writes (the shared audit table lands with the
//! S6 store port, TOG-9811 — [`crate::lfg::SignupOutcome`] and the close/leave
//! booleans carry everything those rows need).

use super::lfg::{
    adjudicate_signup, iso_millis_utc, LfgPost, LfgRole, LfgSignup, LfgStatus, SignupOutcome,
};

/// Map an integer column the DDL CHECK-constrains (slots 1–99, position ≥ 0).
fn decode_range(column: &'static str, value: i32) -> Result<u8, sqlx::Error> {
    u8::try_from(value)
        .map_err(|e| sqlx::Error::Decode(Box::new(ColumnRange { column, source: e })))
}

#[derive(Debug)]
struct ColumnRange {
    column: &'static str,
    source: std::num::TryFromIntError,
}

impl std::fmt::Display for ColumnRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "column {} out of range: {}", self.column, self.source)
    }
}

impl std::error::Error for ColumnRange {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Insert or replace a post plus its roles in one transaction (legacy
/// `putLfg`): the post upserts with a guild fence (a row id that belongs to
/// another guild is never overwritten), then — when `replace_roles` — the
/// role set is deleted and re-inserted in position order. Pass
/// `replace_roles = false` for the post-accept path (legacy re-saves the
/// post with its `messageId` without touching roles).
pub async fn put_lfg(
    pool: &sqlx::PgPool,
    post: &LfgPost,
    roles: &[LfgRole],
    replace_roles: bool,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let result = sqlx::query(
        "INSERT INTO lfg_posts
           (id, guild_id, channel_id, message_id, title, starts_at, status, created_by, created_at, closed_at)
         VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $7, $8, $9::timestamptz, $10::timestamptz)
         ON CONFLICT (id) DO UPDATE SET
           channel_id = excluded.channel_id, message_id = excluded.message_id,
           title = excluded.title, starts_at = excluded.starts_at, status = excluded.status,
           closed_at = excluded.closed_at
         WHERE lfg_posts.guild_id = excluded.guild_id",
    )
    .bind(&post.id)
    .bind(&post.guild_id)
    .bind(&post.channel_id)
    .bind(&post.message_id)
    .bind(&post.title)
    .bind(&post.starts_at)
    .bind(post.status.as_str())
    .bind(&post.created_by)
    .bind(&post.created_at)
    .bind(&post.closed_at)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    if replace_roles {
        if roles.iter().any(|role| role.lfg_id != post.id) {
            return Err(sqlx::Error::Protocol(
                "LFG roles must belong to the post".to_owned(),
            ));
        }
        sqlx::query("DELETE FROM lfg_roles WHERE lfg_id = $1")
            .bind(&post.id)
            .execute(&mut *tx)
            .await?;
        for role in roles {
            sqlx::query(
                "INSERT INTO lfg_roles (lfg_id, role_key, label, slots, position)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(&role.lfg_id)
            .bind(&role.role_key)
            .bind(&role.label)
            .bind(i32::from(role.slots))
            .bind(role.position as i32)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await
}

/// Fetch one post by guild + id (legacy `getLfg`); `None` when missing or
/// fenced to another guild.
pub async fn get_lfg(
    pool: &sqlx::PgPool,
    guild_id: &str,
    id: &str,
) -> Result<Option<LfgPost>, sqlx::Error> {
    type PostRow = (
        String,
        String,
        String,
        Option<String>,
        String,
        time::OffsetDateTime,
        String,
        String,
        time::OffsetDateTime,
        Option<time::OffsetDateTime>,
    );
    let row: Option<PostRow> = sqlx::query_as(
        "SELECT id, guild_id, channel_id, message_id, title, starts_at, status,
                created_by, created_at, closed_at
         FROM lfg_posts WHERE guild_id = $1 AND id = $2",
    )
    .bind(guild_id)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.map(
        |(
            id,
            guild_id,
            channel_id,
            message_id,
            title,
            starts_at,
            status,
            created_by,
            created_at,
            closed_at,
        )| {
            // Legacy treats any non-'open' status as closed (`!== 'open'`).
            let status = LfgStatus::from_stored(&status).unwrap_or(LfgStatus::Closed);
            Ok(LfgPost {
                id,
                guild_id,
                channel_id,
                message_id,
                title,
                starts_at: iso_millis_utc(starts_at),
                status,
                created_by,
                created_at: iso_millis_utc(created_at),
                closed_at: closed_at.map(iso_millis_utc),
            })
        },
    )
    .transpose()
}

/// Delete a post (cascades to roles + signups). Legacy `deleteLfg`: the
/// failed-post path removes fenced durable state after an unreconciled
/// Discord error.
pub async fn delete_lfg(
    pool: &sqlx::PgPool,
    guild_id: &str,
    id: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM lfg_posts WHERE guild_id = $1 AND id = $2")
        .bind(guild_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Roles in position order (legacy `listLfgRoles`).
pub async fn list_lfg_roles(pool: &sqlx::PgPool, id: &str) -> Result<Vec<LfgRole>, sqlx::Error> {
    let rows: Vec<(String, String, String, i32, i32)> = sqlx::query_as(
        "SELECT lfg_id, role_key, label, slots, position
         FROM lfg_roles WHERE lfg_id = $1 ORDER BY position",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(lfg_id, role_key, label, slots, position)| {
            Ok(LfgRole {
                lfg_id,
                role_key,
                label,
                slots: decode_range("slots", slots)?,
                position: usize::try_from(position).map_err(|e| {
                    sqlx::Error::Decode(Box::new(ColumnRange {
                        column: "position",
                        source: e,
                    }))
                })?,
            })
        })
        .collect()
}

/// Signups in join order (legacy `listLfgSignups`).
pub async fn list_lfg_signups(
    pool: &sqlx::PgPool,
    id: &str,
) -> Result<Vec<LfgSignup>, sqlx::Error> {
    let rows: Vec<(String, String, String, time::OffsetDateTime)> = sqlx::query_as(
        "SELECT lfg_id, user_id, role_key, joined_at
         FROM lfg_signups WHERE lfg_id = $1 ORDER BY joined_at, user_id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(lfg_id, user_id, role_key, joined_at)| LfgSignup {
            lfg_id,
            user_id,
            role_key,
            joined_at: iso_millis_utc(joined_at),
        })
        .collect())
}

/// Signup transaction (legacy `signupLfg`): advisory-lock the post, read
/// status/role/current-signup/fill, adjudicate via
/// [`adjudicate_signup`], and upsert only on joined/moved. The `used` count
/// excludes the requesting user, so switching roles can never self-block.
pub async fn signup_lfg(
    pool: &sqlx::PgPool,
    guild_id: &str,
    id: &str,
    role_key: &str,
    user_id: &str,
    joined_at: &str,
) -> Result<SignupOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("lfg:{guild_id}:{id}"))
        .execute(&mut *tx)
        .await?;
    let post: Option<(String,)> =
        sqlx::query_as("SELECT status FROM lfg_posts WHERE guild_id = $1 AND id = $2")
            .bind(guild_id)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((status,)) = post else {
        tx.commit().await?;
        return Ok(SignupOutcome::Missing);
    };
    // Legacy `!== 'open'`: unknown stored statuses read as closed.
    let status = LfgStatus::from_stored(&status).unwrap_or(LfgStatus::Closed);
    let role: Option<(i32,)> =
        sqlx::query_as("SELECT slots FROM lfg_roles WHERE lfg_id = $1 AND role_key = $2")
            .bind(id)
            .bind(role_key)
            .fetch_optional(&mut *tx)
            .await?;
    let slots = role
        .map(|(slots,)| decode_range("slots", slots))
        .transpose()?;
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT role_key FROM lfg_signups WHERE lfg_id = $1 AND user_id = $2")
            .bind(id)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let used: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM lfg_signups WHERE lfg_id = $1 AND role_key = $2 AND user_id <> $3",
    )
    .bind(id)
    .bind(role_key)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    let outcome = adjudicate_signup(
        true,
        status,
        slots,
        existing.as_ref().map(|(key,)| key.as_str()),
        role_key,
        used.0.max(0) as u64,
    );
    if matches!(outcome, SignupOutcome::Joined | SignupOutcome::Moved)
        && existing.as_ref().map(|(key,)| key.as_str()) != Some(role_key)
    {
        sqlx::query(
            "INSERT INTO lfg_signups (lfg_id, user_id, role_key, joined_at)
             VALUES ($1, $2, $3, $4::timestamptz)
             ON CONFLICT (lfg_id, user_id) DO UPDATE SET
               role_key = excluded.role_key, joined_at = excluded.joined_at",
        )
        .bind(id)
        .bind(user_id)
        .bind(role_key)
        .bind(joined_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(outcome)
}

/// Leave a post (legacy `leaveLfg`); `false` when the user was not signed up.
pub async fn leave_lfg(pool: &sqlx::PgPool, id: &str, user_id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM lfg_signups WHERE lfg_id = $1 AND user_id = $2")
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Close a post (legacy `closeLfg`): open-only update, so a second close
/// reports `false` (`already_closed_or_missing`).
pub async fn close_lfg(
    pool: &sqlx::PgPool,
    guild_id: &str,
    id: &str,
    closed_at: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("lfg:{guild_id}:{id}"))
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query(
        "UPDATE lfg_posts SET status = 'closed', closed_at = $1::timestamptz
         WHERE guild_id = $2 AND id = $3 AND status = 'open'",
    )
    .bind(closed_at)
    .bind(guild_id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lfg::{parse_role_spec, spec_roles};

    /// Only the agent test host or the ephemeral CI service container.
    /// Never accept an application database URL or inherited credentials.
    fn test_database_url() -> &'static str {
        if std::env::var("TWO_LFG_TESTDB_CI").as_deref() == Ok("1") {
            "postgres://agent_test@127.0.0.1:5432/two_bot_test_tog10084"
        } else {
            "postgres://agent_test@agent-testdb:5432/two_bot_test_tog10084"
        }
    }

    async fn test_pool() -> sqlx::PgPool {
        let pool = sqlx::PgPool::connect(test_database_url())
            .await
            .expect("agent-testdb reachable");
        // Exercise the real migration files (0001 + 0002 + 0170), so a broken
        // DDL edit fails here, not at deploy.
        sqlx::migrate!("../cutover/migrations")
            .run(&pool)
            .await
            .expect("migrations apply");
        pool
    }

    fn test_post(id: &str) -> LfgPost {
        LfgPost {
            id: id.to_owned(),
            guild_id: "111111111111111111".to_owned(),
            channel_id: "222222222222222222".to_owned(),
            message_id: None,
            title: "Friday raid".to_owned(),
            starts_at: "2026-09-11T20:00:00.000Z".to_owned(),
            status: LfgStatus::Open,
            created_by: "444444444444444444".to_owned(),
            created_at: "2026-09-10T10:00:00.000Z".to_owned(),
            closed_at: None,
        }
    }

    fn test_roles(id: &str) -> Vec<LfgRole> {
        spec_roles(
            id,
            &parse_role_spec("tank:Tank:1,dps:DPS:1").expect("parses"),
        )
    }

    async fn reset(pool: &sqlx::PgPool, id: &str) {
        sqlx::query("DELETE FROM lfg_posts WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .expect("cleanup");
    }

    /// Mirrors legacy `LFG role slots reject overflow and let a member move
    /// atomically`: overflow refused, same-role re-signup joins, switching
    /// roles moves, and the signup list reflects the move.
    #[tokio::test]
    #[ignore = "needs agent-testdb or the CI service container"]
    async fn slots_reject_overflow_and_moves_are_atomic() {
        let pool = test_pool().await;
        let id = "lfg-store-overflow";
        reset(&pool, id).await;
        let mut post = test_post(id);
        let roles = test_roles(id);
        put_lfg(&pool, &post, &roles, true).await.expect("insert");

        // Stamp the post-accept path: message id saves without touching roles.
        post.message_id = Some("333333333333333333".to_owned());
        put_lfg(&pool, &post, &[], false).await.expect("re-save");
        assert_eq!(
            list_lfg_roles(&pool, id).await.expect("roles").len(),
            2,
            "re-save without replace keeps roles"
        );

        let join = |role: &'static str, user: &'static str| {
            signup_lfg(
                &pool,
                &post.guild_id,
                id,
                role,
                user,
                "2026-09-10T11:00:00.000Z",
            )
        };
        assert_eq!(
            join("tank", "u1").await.expect("signup"),
            SignupOutcome::Joined
        );
        assert_eq!(
            join("tank", "u2").await.expect("signup"),
            SignupOutcome::Full
        );
        assert_eq!(
            signup_lfg(
                &pool,
                &post.guild_id,
                id,
                "tank",
                "u1",
                "2026-09-10T12:00:00Z"
            )
            .await
            .expect("repeat signup"),
            SignupOutcome::Joined
        );
        assert_eq!(
            list_lfg_signups(&pool, id).await.expect("signups")[0].joined_at,
            "2026-09-10T11:00:00.000Z"
        );
        assert_eq!(
            join("dps", "u1").await.expect("signup"),
            SignupOutcome::Moved
        );
        let signups = list_lfg_signups(&pool, id).await.expect("signups");
        assert_eq!(
            signups
                .iter()
                .map(|s| (s.user_id.as_str(), s.role_key.as_str()))
                .collect::<Vec<_>>(),
            [("u1", "dps")]
        );
        assert!(leave_lfg(&pool, id, "u1").await.expect("leave"));
        assert!(!leave_lfg(&pool, id, "u1").await.expect("repeat leave"));
        assert!(list_lfg_signups(&pool, id)
            .await
            .expect("signups")
            .is_empty());
        // The freed tank slot accepts the waiter now.
        assert_eq!(
            join("tank", "u2").await.expect("signup"),
            SignupOutcome::Joined
        );
        reset(&pool, id).await;
    }

    /// Mirrors legacy `closing an LFG disables future signups`: close locks
    /// the post, the second close reports false, and unknown roles/posts
    /// answer missing.
    #[tokio::test]
    #[ignore = "needs agent-testdb or the CI service container"]
    async fn close_locks_signups_and_second_close_is_false() {
        let pool = test_pool().await;
        let id = "lfg-store-close";
        reset(&pool, id).await;
        let post = test_post(id);
        put_lfg(&pool, &post, &test_roles(id), true)
            .await
            .expect("insert");

        assert_eq!(
            signup_lfg(
                &pool,
                &post.guild_id,
                "no-such-post",
                "tank",
                "u1",
                "2026-09-10T11:00:00.000Z"
            )
            .await
            .expect("signup"),
            SignupOutcome::Missing
        );
        assert_eq!(
            signup_lfg(
                &pool,
                &post.guild_id,
                id,
                "nope",
                "u1",
                "2026-09-10T11:00:00.000Z"
            )
            .await
            .expect("signup"),
            SignupOutcome::Missing
        );
        assert!(!leave_lfg(&pool, id, "ghost").await.expect("leave"));

        assert!(
            close_lfg(&pool, &post.guild_id, id, "2026-09-10T12:00:00.000Z")
                .await
                .expect("close")
        );
        assert!(
            !close_lfg(&pool, &post.guild_id, id, "2026-09-10T12:00:00.000Z")
                .await
                .expect("close")
        );
        assert_eq!(
            signup_lfg(
                &pool,
                &post.guild_id,
                id,
                "tank",
                "u1",
                "2026-09-10T13:00:00.000Z"
            )
            .await
            .expect("signup"),
            SignupOutcome::Closed
        );
        let stored = get_lfg(&pool, &post.guild_id, id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(stored.status, LfgStatus::Closed);
        assert_eq!(
            stored.closed_at.as_deref(),
            Some("2026-09-10T12:00:00.000Z")
        );
        reset(&pool, id).await;
    }

    #[tokio::test]
    #[ignore = "needs agent-testdb"]
    async fn simultaneous_signups_cannot_overfill() {
        let pool = test_pool().await;
        let id = "lfg-store-concurrent";
        reset(&pool, id).await;
        let post = test_post(id);
        put_lfg(&pool, &post, &test_roles(id), true)
            .await
            .expect("insert");
        let (first, second) = tokio::join!(
            signup_lfg(
                &pool,
                &post.guild_id,
                id,
                "tank",
                "u1",
                "2026-09-10T11:00:00Z"
            ),
            signup_lfg(
                &pool,
                &post.guild_id,
                id,
                "tank",
                "u2",
                "2026-09-10T11:00:00Z"
            ),
        );
        let outcomes = [first.expect("first"), second.expect("second")];
        assert_eq!(
            outcomes
                .iter()
                .filter(|&&o| o == SignupOutcome::Joined)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|&&o| o == SignupOutcome::Full)
                .count(),
            1
        );
        assert_eq!(list_lfg_signups(&pool, id).await.expect("signups").len(), 1);
        reset(&pool, id).await;
    }

    #[tokio::test]
    #[ignore = "needs agent-testdb"]
    async fn close_serializes_with_signup() {
        let pool = test_pool().await;
        let id = "lfg-store-close-lock";
        reset(&pool, id).await;
        let post = test_post(id);
        put_lfg(&pool, &post, &test_roles(id), true)
            .await
            .expect("insert");
        let mut tx = pool.begin().await.expect("transaction");
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("lfg:{}:{id}", post.guild_id))
            .execute(&mut *tx)
            .await
            .expect("hold signup lock");
        // A held signup lock must fence a concurrent close as well.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                close_lfg(&pool, &post.guild_id, id, "2026-09-10T12:00:00Z"),
            )
            .await
            .is_err(),
            "close bypassed an in-flight signup"
        );
        tx.rollback().await.expect("release lock");
        assert!(close_lfg(&pool, &post.guild_id, id, "2026-09-10T12:00:00Z")
            .await
            .expect("close"));
        assert_eq!(
            signup_lfg(
                &pool,
                &post.guild_id,
                id,
                "tank",
                "u1",
                "2026-09-10T13:00:00Z"
            )
            .await
            .expect("signup"),
            SignupOutcome::Closed
        );
        reset(&pool, id).await;
    }

    /// Round trip: post fields (incl. timestamps) survive Postgres, roles
    /// come back in position order, the guild fence holds, and deletes
    /// cascade.
    #[tokio::test]
    #[ignore = "needs agent-testdb or the CI service container"]
    async fn post_round_trips_and_fence_holds() {
        let pool = test_pool().await;
        let id = "lfg-store-roundtrip";
        reset(&pool, id).await;
        let post = test_post(id);
        put_lfg(&pool, &post, &test_roles(id), true)
            .await
            .expect("insert");

        let stored = get_lfg(&pool, &post.guild_id, id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(stored, post);
        // Wrong guild sees nothing and deletes nothing.
        assert_eq!(
            get_lfg(&pool, "999999999999999999", id).await.expect("get"),
            None
        );
        assert!(!delete_lfg(&pool, "999999999999999999", id)
            .await
            .expect("delete"));

        let mut foreign_post = post.clone();
        foreign_post.guild_id = "999999999999999999".to_owned();
        let foreign_roles = spec_roles(id, &parse_role_spec("other:Other:2").expect("parses"));
        assert!(put_lfg(&pool, &foreign_post, &foreign_roles, true)
            .await
            .is_err());
        assert_eq!(
            get_lfg(&pool, &post.guild_id, id).await.expect("get"),
            Some(post.clone())
        );
        let roles = list_lfg_roles(&pool, id).await.expect("roles");
        assert_eq!(
            roles
                .iter()
                .map(|r| (r.role_key.as_str(), r.position))
                .collect::<Vec<_>>(),
            [("tank", 0), ("dps", 1)]
        );

        assert!(leave_lfg(&pool, id, "nobody").await.is_ok());
        assert!(delete_lfg(&pool, &post.guild_id, id).await.expect("delete"));
        assert_eq!(get_lfg(&pool, &post.guild_id, id).await.expect("get"), None);
        assert!(list_lfg_roles(&pool, id).await.expect("roles").is_empty());
    }
}
