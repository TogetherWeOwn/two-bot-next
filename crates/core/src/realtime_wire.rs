//! Closed producer wire format for Discord realtime v1.

use std::{io, num::NonZeroU64};

use serde::{ser::Error as _, Serialize, Serializer};
use time::{format_description::well_known::Rfc3339, OffsetDateTime, UtcOffset};

pub mod recovery;
mod snapshot;
mod validation;

pub use snapshot::{encode_snapshot, Topics, MAX_SNAPSHOT_EVENTS};
pub use validation::event_text_is_valid;

const DELTA_BYTES: usize = 64 * 1024;
pub const MAX_SEQ: u64 = (1 << 53) - 1;

/// A Discord snowflake, encoded as a decimal string to preserve all its bits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub NonZeroU64);

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

#[derive(Clone, Copy)]
pub struct Permissions(pub u64);

impl Serialize for Permissions {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

#[derive(Clone, Copy)]
pub struct WireTime(pub OffsetDateTime);

impl Serialize for WireTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let utc = self
            .0
            .checked_to_offset(UtcOffset::UTC)
            .ok_or_else(|| S::Error::custom("timestamp outside UTC range"))?;
        serializer.serialize_str(&utc.format(&Rfc3339).map_err(S::Error::custom)?)
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Guild {
    pub owner_id: Id,
    pub afk_channel_id: Option<Id>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Voice,
    Stage,
    Category,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverwriteKind {
    Role,
    Member,
}

#[derive(Clone, Serialize)]
pub struct Overwrite {
    pub kind: OverwriteKind,
    pub id: Id,
    pub allow: Permissions,
    pub deny: Permissions,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub id: Id,
    pub name: String,
    pub kind: ChannelKind,
    pub parent_id: Option<Id>,
    pub position: Option<u32>,
    pub overwrites: Vec<Overwrite>,
}

#[derive(Clone, Serialize)]
pub struct Role {
    pub id: Id,
    pub permissions: Permissions,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub user_id: Id,
    pub role_ids: Vec<Id>,
    pub nick: Option<String>,
    pub bot: bool,
    pub pending: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Voice {
    pub user_id: Id,
    pub channel_id: Id,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    Scheduled,
    Active,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: Id,
    pub channel_id: Option<Id>,
    pub status: EventStatus,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub starts_at: WireTime,
    pub ends_at: Option<WireTime>,
}

#[derive(Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change<Row> {
    Upsert { id: Id, value: Row },
    Remove { id: Id },
}

impl<Row> Change<Row> {
    fn is_valid(&self, key: impl FnOnce(&Row) -> Id, valid: impl FnOnce(&Row) -> bool) -> bool {
        match self {
            Self::Upsert { id, value } => *id == key(value) && valid(value),
            Self::Remove { .. } => true,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "topic", rename_all = "snake_case")]
pub enum Operation {
    /// The guild singleton can only be replaced, never removed.
    Guild(Change<Guild>),
    Channels(Change<Channel>),
    Roles(Change<Role>),
    Members(Change<Member>),
    Voice(Change<Voice>),
    Events(Change<Event>),
}

impl Operation {
    fn is_valid(&self, guild_id: Id) -> bool {
        match self {
            Self::Guild(change) => matches!(change, Change::Upsert { id, .. } if *id == guild_id),
            Self::Channels(change) => change.is_valid(|row| row.id, validation::channel),
            Self::Roles(change) => change.is_valid(|row| row.id, |_| true),
            Self::Members(change) => change.is_valid(|row| row.user_id, validation::member),
            Self::Voice(change) => change.is_valid(|row| row.user_id, |_| true),
            Self::Events(change) => change.is_valid(|row| row.id, validation::event),
        }
    }
}

#[derive(Serialize)]
struct Delta<'a> {
    v: u8,
    guild_id: Id,
    seq: u64,
    occurred_at: WireTime,
    ops: &'a [Operation],
}

/// Encode normalized operations once. Sign and send the returned bytes unchanged.
pub fn encode_delta(
    guild_id: Id,
    seq: u64,
    occurred_at: WireTime,
    ops: &[Operation],
) -> Option<Vec<u8>> {
    if !(1..=MAX_SEQ).contains(&seq)
        || !(1..=128).contains(&ops.len())
        || !ops.iter().all(|op| op.is_valid(guild_id))
    {
        return None;
    }
    let delta = Delta {
        v: 1,
        guild_id,
        seq,
        occurred_at,
        ops,
    };
    encode(DELTA_BYTES, &delta)
}

struct LimitedBytes(usize, Vec<u8>);

impl io::Write for LimitedBytes {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > self.0 - self.1.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.1.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode(limit: usize, payload: &impl Serialize) -> Option<Vec<u8>> {
    let mut bytes = LimitedBytes(limit, Vec::new());
    serde_json::to_writer(&mut bytes, payload).ok()?;
    Some(bytes.1)
}

#[cfg(test)]
mod tests;
