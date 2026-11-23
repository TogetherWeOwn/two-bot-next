//! Dormant internal-action adapter; no HTTP dispatch or gateway dependency.

use std::future::Future;

use serde_json::Value;
use time::OffsetDateTime;
use two_bot_core::{
    backup::guild_config::TWO_STAGING_GUILD_ID, internal_action_store::InternalStoreError,
    internal_actions::AttendanceReadRequest, Snowflake,
};
use two_bot_cutover::event_attendance::{finalized_attendance, FinalizedAttendance};

use super::{
    project_attendance, AttendanceRefusal, AttendanceScope, AttendanceWindowUs, SessionUs, GRACE_US,
};

#[derive(Debug, PartialEq, Eq)]
pub enum ReadRefusal {
    NotFound,
    Unavailable,
    Internal,
}

/// `ticket` must issue a current epoch check for this guild, not a fresh
/// readiness sample. Once stale, an issued ticket must stay stale.
pub async fn read<T: Fn() -> bool>(
    request: AttendanceReadRequest,
    pool: &sqlx::PgPool,
    ticket: impl FnOnce(Snowflake) -> Option<T>,
) -> Result<Value, ReadRefusal> {
    read_with(request.event_key(), ticket, |scope| {
        finalized_attendance(pool, scope.guild_id, scope.event_key)
    })
    .await
}

async fn read_with<
    'a,
    T: Fn() -> bool,
    F: Future<Output = Result<Option<FinalizedAttendance>, InternalStoreError>>,
>(
    event_key: &'a str,
    ticket: impl FnOnce(Snowflake) -> Option<T>,
    lookup: impl FnOnce(AttendanceScope<'a>) -> F,
) -> Result<Value, ReadRefusal> {
    let guild_id = TWO_STAGING_GUILD_ID
        .parse()
        .map_err(|_| ReadRefusal::Internal)?;
    let scope = AttendanceScope {
        guild_id,
        event_key,
    };
    let ticket = ticket(scope.guild_id)
        .filter(|current| current())
        .ok_or(ReadRefusal::Unavailable)?;
    let result = match lookup(scope).await {
        Ok(Some(record)) => project(scope, &record),
        Ok(None) => Err(ReadRefusal::NotFound),
        Err(_) => Err(ReadRefusal::Internal),
    };
    if !ticket() {
        return Err(ReadRefusal::Unavailable);
    }
    result
}

fn micros(value: OffsetDateTime) -> i64 {
    value.unix_timestamp_nanos().div_euclid(1_000) as i64
}

fn project(scope: AttendanceScope<'_>, record: &FinalizedAttendance) -> Result<Value, ReadRefusal> {
    let window = AttendanceWindowUs {
        scope,
        starts_us: micros(record.starts_at),
        ends_us: micros(record.ends_at),
    };
    let sessions: Vec<_> = record
        .sessions
        .iter()
        .map(|row| SessionUs {
            scope,
            member_id: row.member_id,
            channel_id: row.channel_id,
            // An unknown stamp earns attendance credit, not a room name.
            channel_name: row
                .name_observed_at
                .map_or("", |_| row.channel_name.as_str()),
            name_observed_ns: row
                .name_observed_at
                .map_or(i128::MIN, OffsetDateTime::unix_timestamp_nanos),
            is_bot: row.is_bot,
            afk: row.afk,
            joined_us: micros(row.joined_at),
            left_us: Some(micros(row.left_at)),
        })
        .collect();
    // The store already proved finality at the inclusive cutoff. Use that
    // bound, not the bot clock, to project the store-authorized snapshot.
    let cutoff = window.ends_us.saturating_add(GRACE_US);
    project_attendance(&window, &sessions, cutoff).map_err(|refusal| match refusal {
        AttendanceRefusal::NoRecord => ReadRefusal::NotFound,
        _ => ReadRefusal::Internal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use two_bot_cutover::event_attendance::AttendanceSession;

    fn finalized_fixture() -> FinalizedAttendance {
        use time::Duration;

        // Pretend the store's clock has reached cutoff while the bot's hasn't.
        let starts_at = OffsetDateTime::from_unix_timestamp(253_370_764_800).unwrap(); // Year 9999.
        let ten = starts_at + Duration::minutes(10);
        let session =
            |member_id, channel_id, channel_name: &str, stamped: bool, joined_at, left_at| {
                AttendanceSession {
                    member_id,
                    channel_id,
                    channel_name: channel_name.into(),
                    name_observed_at: stamped.then_some(left_at),
                    is_bot: false,
                    afk: false,
                    joined_at,
                    left_at,
                }
            };
        FinalizedAttendance {
            starts_at,
            ends_at: starts_at + Duration::hours(1),
            sessions: vec![
                session(
                    2,
                    3,
                    "observed",
                    true,
                    starts_at + Duration::microseconds(900),
                    ten + Duration::microseconds(100),
                ),
                session(
                    3,
                    4,
                    "unknown room",
                    false,
                    ten,
                    ten + Duration::minutes(10),
                ),
                session(2, 3, "unknown rename", false, ten, ten),
                session(
                    2,
                    3,
                    "",
                    true,
                    ten + Duration::seconds(1),
                    ten + Duration::seconds(1),
                ),
            ],
        }
    }

    #[tokio::test]
    async fn finalized_lookup_preserves_scope_precision_and_known_names() {
        let raw = serde_json::json!({"event_key": "fixture:1"});
        let key =
            two_bot_core::internal_actions::validate_event_key(raw.as_object().unwrap()).unwrap();
        let result = read_with(
            key,
            |guild| {
                assert_eq!(guild.to_string(), TWO_STAGING_GUILD_ID);
                Some(|| true)
            },
            |scope| async move {
                assert_eq!(scope.guild_id.to_string(), TWO_STAGING_GUILD_ID);
                assert_eq!(scope.event_key, "fixture:1");
                Ok(Some(finalized_fixture()))
            },
        )
        .await
        .unwrap();
        assert_eq!(result["final"], true);
        assert_eq!(result["members"][0]["minutes"], 9);
        assert_eq!(result["members"][1]["minutes"], 10);
        assert_eq!(result["peak_concurrent"], 2);
        assert_eq!(
            result["rooms"],
            serde_json::json!([{"channel_id": "3", "name_at_close": "observed"}])
        );
    }

    #[tokio::test]
    async fn absent_and_invalidated_tickets_never_release_results() {
        let key = "fixture:1";
        for ticket in [None, Some((|| false) as fn() -> bool)] {
            assert_eq!(
                read_with(key, |_| ticket, |_| async { panic!("lookup must not run") }).await,
                Err(ReadRefusal::Unavailable)
            );
        }
        assert_eq!(
            read_with(
                key,
                |_| Some(|| true),
                |_| async { Err(InternalStoreError::Unavailable) }
            )
            .await,
            Err(ReadRefusal::Internal)
        );
        for found in [false, true] {
            for invalidate in [false, true] {
                let epoch = Cell::new(0);
                let result = read_with(
                    key,
                    |_| Some(|| epoch.get() == 0),
                    |_| async {
                        tokio::task::yield_now().await;
                        epoch.set(u32::from(invalidate));
                        Ok(found.then(finalized_fixture))
                    },
                )
                .await;
                match (invalidate, found) {
                    (true, _) => assert_eq!(result, Err(ReadRefusal::Unavailable)),
                    (false, false) => assert_eq!(result, Err(ReadRefusal::NotFound)),
                    (false, true) => assert_eq!(result.unwrap()["final"], true),
                }
            }
        }
    }
}
