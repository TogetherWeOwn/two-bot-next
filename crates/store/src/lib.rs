//! Runtime Postgres store for two-bot-next (S6, TOG-9811).
//!
//! sqlx [`sqlx::Pool<sqlx::Postgres>`] persistence behind the framework-free core seams:
//! [`FunnelStore`] (append-only funnel log + members projection) and
//! [`InviteSnapshotStore`]. Plus the checksum migration runner
//! ([`migrations`]) and the read-only `web_v1` contract views
//! ([`apply_web_contract`]).
//!
//! The core seams are **sync** (the gateway pipeline's `handle()` never
//! awaits), but sqlx is async. [`PgFunnelStore`] bridges with a blocking
//! executor: it holds a `tokio::runtime::Handle` and dispatches each store
//! call with [`tokio::task::block_in_place`], so the caller's thread parks
//! while the runtime's other threads drive the I/O. **Never call a store
//! method from inside an async task on a `current_thread` runtime** — there
//! is no other thread to drive the future and `block_in_place` will panic.
//! The runtime pipeline calls from one ordered blocking dispatch worker;
//! the shard is polled independently on the multi-thread Tokio runtime.

pub mod journal;
pub mod migrations;
pub mod pool;
pub mod snapshots;
pub mod store;
pub mod web;

pub use migrations::{migrate, MigrationError, MIGRATOR, TABLE_NAME};
pub use pool::{
    connect_pool, connect_pool_with_tls, ping, ConnectError, Store, DB_POOL_MAX,
    STATEMENT_TIMEOUT_MS,
};
pub use snapshots::PgInviteSnapshots;
pub use store::PgFunnelStore;
pub use web::apply_web_contract;

pub use two_bot_core::{FunnelStore, InviteSnapshotStore};
