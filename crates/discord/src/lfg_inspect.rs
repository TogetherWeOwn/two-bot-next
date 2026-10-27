//! Read-only saved-roster inspection, separate from LFG mutation, signup
//! cooldown, audit and message-refresh paths.
//!
//! Authorization order is contractual: the router has already revalidated
//! the configured guild, the announcements gate and Manage Events for this
//! invocation; here the guild-scoped post read plus the invoking-channel
//! check must pass before any role or signup row is read, so unknown,
//! other-guild and other-channel IDs share one private miss.

use two_bot_core::{lfg_inspect, lfg_inspect::InspectSelection, lfg_store};

pub(crate) async fn inspect(
    pool: &sqlx::PgPool,
    guild: &str,
    channel: &str,
    post_id: &str,
    selection: &InspectSelection,
) -> String {
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(_) => return lfg_inspect::UNAVAILABLE.into(),
    };
    // One MVCC snapshot spans authorization, capacities and members. Plain
    // READ COMMITTED could pair an old header/role with newer signup rows.
    if sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .is_err()
    {
        return lfg_inspect::UNAVAILABLE.into();
    }
    let post = match lfg_store::get_lfg(&mut *tx, guild, post_id).await {
        Ok(Some(post)) if post.channel_id == channel => post,
        Ok(_) => return lfg_inspect::MISS.into(),
        Err(_) => return lfg_inspect::UNAVAILABLE.into(),
    };
    let roles = match lfg_store::list_lfg_roles(&mut *tx, post_id).await {
        Ok(roles) => roles,
        Err(_) => return lfg_inspect::UNAVAILABLE.into(),
    };
    let signups = match lfg_store::list_lfg_signups(&mut *tx, post_id).await {
        Ok(signups) => signups,
        Err(_) => return lfg_inspect::UNAVAILABLE.into(),
    };
    if tx.rollback().await.is_err() {
        return lfg_inspect::UNAVAILABLE.into();
    }
    lfg_inspect::render(&post, &roles, &signups, selection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use two_bot_core::lfg::{LfgPost, LfgRole, LfgStatus};
    use two_bot_testsupport::TestDatabase;

    #[tokio::test]
    async fn inspection_keeps_one_revision_across_both_roster_read_boundaries() {
        let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated fixture");
        let observer = db.independent_pool().await.expect("independent observer");
        let mut outputs = Vec::new();
        for table in ["lfg_roles", "lfg_signups"] {
            for role in [None, Some(1)] {
                let id = format!("snapshot-{table}-{role:?}");
                let post = LfgPost {
                    id: id.clone(),
                    guild_id: "2".into(),
                    channel_id: "4".into(),
                    message_id: None,
                    title: "Revision A".into(),
                    starts_at: "2099-01-02T03:04:05.000Z".into(),
                    status: LfgStatus::Open,
                    created_by: "9".into(),
                    created_at: "2020-01-01T00:00:00.000Z".into(),
                    closed_at: None,
                };
                let roles = [LfgRole {
                    lfg_id: id.clone(),
                    role_key: "seat".into(),
                    label: "Seat A".into(),
                    slots: 3,
                    position: 0,
                }];
                lfg_store::put_lfg(db.pool(), &post, &roles, true)
                    .await
                    .expect("seed revision A");
                sqlx::query("INSERT INTO lfg_signups (lfg_id, user_id, role_key, joined_at) VALUES ($1, '111', 'seat', '2020-01-01T00:00:00+00:00')")
                    .bind(&id).execute(db.pool()).await.expect("seed original signup");
                let mut writer = observer.begin().await.expect("writer transaction");
                let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut *writer)
                    .await
                    .expect("writer PID");
                let gate = match table {
                    "lfg_roles" => "LOCK TABLE lfg_roles IN ACCESS EXCLUSIVE MODE",
                    _ => "LOCK TABLE lfg_signups IN ACCESS EXCLUSIVE MODE",
                };
                sqlx::query(gate)
                    .execute(&mut *writer)
                    .await
                    .expect("hold read boundary");
                let mut inspecting = tokio::spawn({
                    let pool = db.pool().clone();
                    let id = id.clone();
                    async move {
                        inspect(&pool, "2", "4", &id, &InspectSelection { role, page: 1 }).await
                    }
                });
                // Observe the actual blocked read after authorization, not a
                // timing guess. The writer commits a complete new revision
                // between the production reader's statements.
                let reached = tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        let waiting: bool = sqlx::query_scalar(
                            "SELECT EXISTS (SELECT 1 FROM pg_locks
                             WHERE locktype = 'relation' AND relation = to_regclass($1)
                               AND mode = 'AccessShareLock' AND NOT granted
                               AND $2 = ANY(pg_blocking_pids(pid)))",
                        )
                        .bind(table)
                        .bind(writer_pid)
                        .fetch_one(&observer)
                        .await
                        .expect("observe blocked inspection");
                        if waiting {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await;
                sqlx::query("UPDATE lfg_posts SET title = 'Revision B' WHERE id = $1")
                    .bind(&id)
                    .execute(&mut *writer)
                    .await
                    .expect("change header");
                sqlx::query("UPDATE lfg_roles SET label = 'Seat B', slots = 9 WHERE lfg_id = $1")
                    .bind(&id)
                    .execute(&mut *writer)
                    .await
                    .expect("change role");
                sqlx::query("UPDATE lfg_signups SET user_id = '222' WHERE lfg_id = $1")
                    .bind(&id)
                    .execute(&mut *writer)
                    .await
                    .expect("replace member");
                sqlx::query("INSERT INTO lfg_signups (lfg_id, user_id, role_key, joined_at) VALUES ($1, '333', 'seat', '2020-01-01T00:00:00+00:00')")
                    .bind(&id).execute(&mut *writer).await.expect("add member");
                writer
                    .commit()
                    .await
                    .expect("commit revision B and release gate");
                let during = tokio::time::timeout(Duration::from_secs(5), &mut inspecting).await;
                if during.is_err() {
                    inspecting.abort();
                    let _ = inspecting.await;
                }
                let after = inspect(
                    db.pool(),
                    "2",
                    "4",
                    &id,
                    &InspectSelection { role, page: 1 },
                )
                .await;
                outputs.push((table, role, reached, during, after));
            }
        }
        observer.close().await;
        db.close().await.expect("verify teardown before assertions");
        for (table, role, reached, during, after) in outputs {
            reached.expect("inspection reached the intended read gate");
            let during = during
                .expect("bounded inspection")
                .expect("inspection task");
            assert!(during.contains("Revision A"), "{table}: {during}");
            assert!(during.contains("Seat A"), "{table}: {during}");
            assert!(!during.contains("Revision B") && !during.contains("Seat B"));
            assert!(
                after.contains("Revision B") && after.contains("Seat B"),
                "writer committed"
            );
            if role.is_some() {
                assert!(during.lines().any(|line| line == "- 111"));
                assert!(!during
                    .lines()
                    .any(|line| line == "- 222" || line == "- 333"));
                assert!(after.lines().any(|line| line == "- 222"));
                assert!(after.lines().any(|line| line == "- 333"));
                assert!(!after.lines().any(|line| line == "- 111"));
            } else {
                assert!(during.contains("1/3 — 2 remaining"), "{table}: {during}");
                assert!(after.contains("2/9 — 7 remaining"));
            }
        }
    }
}
