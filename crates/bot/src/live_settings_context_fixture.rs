//! Test-only constructor implemented here so Context's private observation
//! fields never need to become public, and no production constructor is edited.

#![cfg(test)]

use super::*;

impl Context {
    pub(crate) fn live_settings_fixture(
        pool: PgPool,
        rest: ActionExecutor,
        guild: &str,
    ) -> Arc<Self> {
        let cell = OnceCell::new();
        assert!(cell.set(pool).is_ok(), "fresh fixture pool cell");
        Arc::new(Self {
            url: String::new(),
            pool: cell,
            rest,
            guild: guild.into(),
            observation: Mutex::new(()),
            events_gate: JobGate::default(),
        })
    }
}
