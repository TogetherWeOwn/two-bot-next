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
    let guild_id = TWO_STAGING_GUILD_ID
        .parse()
        .map_err(|_| ReadRefusal::Internal)?;
    let scope = AttendanceScope {
        guild_id,
        event_key: request.event_key(),
    };
    read_with(scope, ticket(guild_id), || {
        finalized_attendance(pool, guild_id, scope.event_key)
    })
    .await
}

async fn read_with<F: Future<Output = Result<Option<FinalizedAttendance>, InternalStoreError>>>(
    scope: AttendanceScope<'_>,
    ticket: Option<impl Fn() -> bool>,
    lookup: impl FnOnce() -> F,
) -> Result<Value, ReadRefusal> {
    let ticket = ticket
        .filter(|current| current())
        .ok_or(ReadRefusal::Unavailable)?;
    let result = match lookup().await {
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
            channel_name: &row.channel_name,
            name_observed_ns: row.name_observed_at.unix_timestamp_nanos(),
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

    #[tokio::test]
    async fn absent_and_invalidated_tickets_never_release_results() {
        let scope = AttendanceScope {
            guild_id: 1,
            event_key: "fixture:1",
        };
        assert_eq!(
            read_with(scope, None::<fn() -> bool>, || async {
                panic!("lookup must not run")
            })
            .await,
            Err(ReadRefusal::Unavailable)
        );
        let starts_at = OffsetDateTime::UNIX_EPOCH;
        let ends_at = starts_at + time::Duration::hours(1);
        for found in [false, true] {
            for invalidate in [false, true] {
                let epoch = Cell::new(0);
                let result = read_with(scope, Some(|| epoch.get() == 0), || async {
                    tokio::task::yield_now().await;
                    epoch.set(u32::from(invalidate));
                    Ok(found.then(|| FinalizedAttendance {
                        starts_at,
                        ends_at,
                        sessions: vec![AttendanceSession {
                            member_id: 2,
                            channel_id: 3,
                            channel_name: "fixture".to_owned(),
                            name_observed_at: starts_at,
                            is_bot: false,
                            afk: false,
                            joined_at: starts_at,
                            left_at: ends_at,
                        }],
                    }))
                })
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
