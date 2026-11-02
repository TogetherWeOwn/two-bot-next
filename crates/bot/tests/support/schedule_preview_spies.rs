//! External-boundary spies for the unchanged, path-included schedule handler.
//! Current-thread tests isolate state without process environment or a database.

use std::cell::RefCell;

pub const NOW: i64 = 1_806_193_800_125; // 2027-03-28T00:30:00.125Z, before EU DST.

#[derive(Clone, Debug, Default)]
pub struct Trace {
    pub clock_reads: usize,
    pub ids: usize,
    pub calls: Vec<String>,
    pub writes: Vec<real_core::scheduled_store::ScheduledWrite>,
    pub audits: Vec<real_core::scheduled_store::ScheduledAuditInput>,
    pub full: bool,
}

thread_local! {
    static TRACE: RefCell<Trace> = RefCell::new(Trace::default());
}

pub fn reset() {
    TRACE.with(|trace| *trace.borrow_mut() = Trace::default());
}

pub fn snapshot() -> Trace {
    TRACE.with(|trace| trace.borrow().clone())
}

pub fn rewind_clock() {
    TRACE.with(|trace| trace.borrow_mut().clock_reads = 0);
}

pub fn set_full() {
    TRACE.with(|trace| trace.borrow_mut().full = true);
}

fn call(name: &str) {
    TRACE.with(|trace| trace.borrow_mut().calls.push(name.into()));
}

pub mod funnel {
    pub use real_core::funnel::*;

    pub fn now_millis_for_test() -> i64 {
        super::TRACE.with(|trace| {
            let mut trace = trace.borrow_mut();
            let now = super::NOW + trace.clock_reads as i64 * 60_000;
            trace.clock_reads += 1;
            now
        })
    }
}

pub mod command_runtime {
    use twilight_model::application::interaction::Interaction;

    pub fn actor_id(interaction: &Interaction) -> String {
        interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
            .or(interaction.user.as_ref())
            .map(|user| user.id.to_string())
            .unwrap_or_default()
    }

    pub fn new_id() -> String {
        super::TRACE.with(|trace| {
            let mut trace = trace.borrow_mut();
            trace.ids += 1;
            format!("acceptance-persistent-id-{}", trace.ids)
        })
    }
}

pub mod db_log {
    use real_core::automation_quota::QuotaWriteError;

    pub fn db_class(error: &sqlx::Error) -> &'static str {
        real_core::error_chain::db_cause(error)
    }

    pub fn quota_write_class(error: &QuotaWriteError) -> &'static str {
        match error {
            QuotaWriteError::Capacity(_) => "capacity",
            QuotaWriteError::Database(inner) => db_class(inner),
        }
    }

    pub fn discord_class(error: &two_bot_discord::executor::DiscordError) -> &'static str {
        error.cause()
    }
}

pub mod scheduled_store {
    use real_core::automation_quota::{AutomationQuota, QuotaWriteError};
    pub use real_core::scheduled_store::*;
    use sqlx::{Pool, Postgres};

    pub async fn get_scheduled(
        _pool: &Pool<Postgres>,
        guild: &str,
        id: &str,
    ) -> Result<Option<ScheduledMessageRow>, sqlx::Error> {
        super::call(&format!("get:{guild}:{id}"));
        Ok(None)
    }

    pub async fn put_scheduled(
        _pool: &Pool<Postgres>,
        row: &ScheduledWrite,
    ) -> Result<bool, QuotaWriteError> {
        super::call("put:quota-and-queue-admission");
        super::TRACE.with(|trace| {
            let mut trace = trace.borrow_mut();
            if trace.full {
                return Err(QuotaWriteError::Capacity(AutomationQuota::Schedules));
            }
            trace.writes.push(row.clone());
            Ok(true)
        })
    }

    pub async fn audit_scheduled(
        _pool: &Pool<Postgres>,
        row: &ScheduledAuditInput,
        _at: &str,
        _id: &str,
    ) -> Result<(), sqlx::Error> {
        super::call("audit");
        super::TRACE.with(|trace| trace.borrow_mut().audits.push(row.clone()));
        Ok(())
    }

    pub async fn list_scheduled(
        _pool: &Pool<Postgres>,
        _guild: &str,
    ) -> Result<Vec<ScheduledMessageRow>, sqlx::Error> {
        super::call("list");
        Ok(Vec::new())
    }

    pub async fn list_scheduled_window(
        _pool: &Pool<Postgres>,
        _guild: &str,
        _limit: i64,
        _offset: i64,
    ) -> Result<Vec<ScheduledMessageRow>, sqlx::Error> {
        super::call("list-window");
        Ok(Vec::new())
    }

    pub async fn count_scheduled(_pool: &Pool<Postgres>, _guild: &str) -> Result<i64, sqlx::Error> {
        super::call("count");
        Ok(0)
    }

    pub async fn resolve_scheduled_id(
        _pool: &Pool<Postgres>,
        _guild: &str,
        _id: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        super::call("resolve");
        Ok(None)
    }

    pub async fn delete_scheduled(
        _pool: &Pool<Postgres>,
        _guild: &str,
        _id: &str,
    ) -> Result<bool, sqlx::Error> {
        super::call("delete");
        Ok(false)
    }
}
