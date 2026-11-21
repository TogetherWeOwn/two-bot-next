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
    project_attendance, AttendanceRefusal, AttendanceScope, AttendanceWindowMs, SessionMs,
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

fn millis(value: OffsetDateTime) -> i64 {
    value.unix_timestamp_nanos().div_euclid(1_000_000) as i64
}

fn project(scope: AttendanceScope<'_>, record: &FinalizedAttendance) -> Result<Value, ReadRefusal> {
    let window = AttendanceWindowMs {
        scope,
        starts_ms: millis(record.starts_at),
        ends_ms: millis(record.ends_at),
    };
    let sessions: Vec<_> = record
        .sessions
        .iter()
        .map(|row| SessionMs {
            scope,
            member_id: row.member_id,
            channel_id: row.channel_id,
            // An unknown stamp earns attendance credit, not a room name.
            // Sort it before all real stamps so it cannot replace a known name.
            channel_name: row
                .name_observed_at
                .map_or("", |_| row.channel_name.as_str()),
            name_observed_ns: row
                .name_observed_at
                .map_or(i128::MIN, OffsetDateTime::unix_timestamp_nanos),
            is_bot: row.is_bot,
            afk: row.afk,
            joined_ms: millis(row.joined_at),
            left_ms: Some(millis(row.left_at)),
        })
        .collect();
    project_attendance(&window, &sessions, millis(OffsetDateTime::now_utc())).map_err(|refusal| {
        match refusal {
            AttendanceRefusal::NotFinal | AttendanceRefusal::NoRecord => ReadRefusal::NotFound,
            AttendanceRefusal::InvalidWindow | AttendanceRefusal::OverBudget => {
                ReadRefusal::Internal
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use two_bot_cutover::event_attendance::AttendanceSession;

    #[test]
    fn unknown_name_stamps_keep_attendance_but_never_supply_a_room_name() {
        let scope = AttendanceScope {
            guild_id: 1,
            event_key: "fixture:1",
        };
        let starts_at = OffsetDateTime::UNIX_EPOCH;
        let ends_at = starts_at + time::Duration::hours(1);
        let session =
            |channel_id, channel_name: &str, name_observed_at, joined_at| AttendanceSession {
                member_id: 2,
                channel_id,
                channel_name: channel_name.to_owned(),
                name_observed_at,
                is_bot: false,
                afk: false,
                joined_at,
                left_at: joined_at + time::Duration::minutes(10),
            };
        let second = starts_at + time::Duration::minutes(10);
        let third = second + time::Duration::minutes(10);
        let result = project(
            scope,
            &FinalizedAttendance {
                starts_at,
                ends_at,
                sessions: vec![
                    session(3, "observed", Some(starts_at), starts_at),
                    session(3, "unknown rename", None, second),
                    session(4, "unknown room", None, third),
                ],
            },
        )
        .unwrap();
        assert_eq!(result["members"][0]["minutes"], 30);
        assert_eq!(
            result["rooms"],
            serde_json::json!([
                {"channel_id": "3", "name_at_close": "observed"},
                {"channel_id": "4", "name_at_close": ""}
            ])
        );
    }

    #[tokio::test]
    async fn readiness_and_lookup_share_the_fixed_guild_and_validated_key() {
        let body = serde_json::json!({"event_key": "fixture:1"});
        let key =
            two_bot_core::internal_actions::validate_event_key(body.as_object().unwrap()).unwrap();
        assert_eq!(
            read_with(
                key,
                |guild| {
                    assert_eq!(guild.to_string(), TWO_STAGING_GUILD_ID);
                    Some(|| true)
                },
                |scope| async move {
                    assert_eq!(scope.guild_id.to_string(), TWO_STAGING_GUILD_ID);
                    assert_eq!(scope.event_key, "fixture:1");
                    Ok(None)
                }
            )
            .await,
            Err(ReadRefusal::NotFound)
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
        let starts_at = OffsetDateTime::UNIX_EPOCH;
        let ends_at = starts_at + time::Duration::hours(1);
        for found in [false, true] {
            for invalidate in [false, true] {
                let epoch = Cell::new(0);
                let result = read_with(
                    key,
                    |_| Some(|| epoch.get() == 0),
                    |_| async {
                        tokio::task::yield_now().await;
                        epoch.set(u32::from(invalidate));
                        Ok(found.then(|| FinalizedAttendance {
                            starts_at,
                            ends_at,
                            sessions: vec![AttendanceSession {
                                member_id: 2,
                                channel_id: 3,
                                channel_name: "fixture".to_owned(),
                                name_observed_at: Some(starts_at),
                                is_bot: false,
                                afk: false,
                                joined_at: starts_at,
                                left_at: ends_at,
                            }],
                        }))
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
