//! Backup/restore + sealed guild-config snapshot (TOG-9881).
//!
//! Rust port of the legacy `two-bot` cutover-data surface (frozen source
//! `main @ d5d11793`): the v3 dump format (`src/store/dump.ts`), retention
//! math (`backupRetention.ts`), SigV4 single-PUT upload (`s3Sign.ts`,
//! `s3Config.ts`, `backup-upload-s3.ts`), the sealed guild-config snapshot
//! (`guildConfig.ts`, `guildConfigApi.ts`, `guildConfigRestore.ts`) and the
//! nightly/drill timer equivalents (`deploy/`, `docs/backup.md`).
//!
//! Layout mirrors the legacy split between "file-only" and "database" code:
//! [`dump_file`] never touches a database (so refusal tests run without
//! Postgres, exactly like legacy `test/unit.dumpread.test.ts`); [`dump`]
//! holds the live dump/restore and needs the crate `db` feature.
//!
//! Never production services or tokens: CLI entry points take explicit target
//! URLs and the only databases tests may touch are agent-testdb scratch
//! databases (see `docs/backup.md`).

pub mod dump_file;
pub mod guild_config;
pub mod guild_config_api;
pub mod guild_config_restore;
pub mod http;
pub mod retention;
pub mod s3;

#[cfg(feature = "db")]
pub mod dump;

pub use dump_file::{DumpTable, DUMP_TABLES, DUMP_VERSION};
pub use retention::{parse_keep, to_prune, RetentionError, DEFAULT_KEEP};
