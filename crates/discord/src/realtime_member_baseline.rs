//! Collect one complete, explicitly screened Discord member baseline. No I/O.

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    marker::PhantomData,
};

use serde::{de::value::MapAccessDeserializer, Deserialize, Deserializer};
use twilight_model::{
    gateway::payload::outgoing::RequestGuildMembers,
    id::{
        marker::{GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};
use two_bot_core::realtime_wire as wire;

const MAX_MEMBERS: usize = 10_000;
const MAX_CHUNK_MEMBERS: usize = 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberBaselineError {
    MalformedChunk,
    InconsistentChunk,
    RetiredAttempt,
}

/// A single unfiltered roster request for one guild and gateway generation.
/// The ordered gateway owner must discard it on timeout, event loss or session
/// change. It must also report member mutations from the count observation until
/// `finish`; chunks cannot be merged safely across intervening member updates.
pub struct MemberBaseline {
    guild_id: Id<GuildMarker>,
    nonce: String,
    expected: usize,
    collection: Option<Collection>,
}

#[derive(Default)]
struct Collection {
    chunk_count: Option<usize>,
    indices: HashSet<usize>,
    members: BTreeMap<wire::Id, wire::Member>,
}

#[derive(Deserialize)]
struct Scope {
    guild_id: Id<GuildMarker>,
    nonce: Option<String>,
}

#[derive(Deserialize)]
struct Chunk {
    chunk_index: usize,
    chunk_count: usize,
    #[serde(default)]
    not_found: Vec<Id<UserMarker>>,
    members: Vec<Object<MemberInput>>,
}

#[derive(Deserialize)]
struct MemberInput {
    user: Object<UserInput>,
    roles: Vec<Id<RoleMarker>>,
    nick: Option<String>,
    // Read original JSON: Twilight defaults an omitted `pending` to false.
    pending: bool,
}

#[derive(Deserialize)]
struct UserInput {
    id: Id<UserMarker>,
    #[serde(default)] // Discord omits `bot` on human users.
    bot: bool,
}

// Serde's derived struct deserializer also accepts positional arrays. Require
// named fields so a member row must explicitly supply the `pending` key.
pub(super) struct Object<T>(pub(super) T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for ObjectVisitor<T> {
            type Value = Object<T>;

            fn expecting(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt.write_str("a JSON object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Object)
            }
        }
        de.deserialize_map(ObjectVisitor(PhantomData))
    }
}

impl MemberBaseline {
    /// Start from an exact `GUILD_CREATE.member_count`, not an approximate count.
    /// The caller must supply a fresh nonce that is never reused across attempts
    /// on the same gateway session, and send only the returned request.
    pub fn start(
        guild_id: Id<GuildMarker>,
        exact_member_count: u64,
        nonce: String,
    ) -> Option<(Self, RequestGuildMembers)> {
        let expected = usize::try_from(exact_member_count).ok()?;
        if !(1..=MAX_MEMBERS).contains(&expected) || !(1..=32).contains(&nonce.len()) {
            return None;
        }
        let request = RequestGuildMembers::builder(guild_id)
            .nonce(nonce.clone())
            .presences(false)
            .query("", None);
        Some((
            Self {
                guild_id,
                nonce,
                expected,
                collection: Some(Collection::default()),
            },
            request,
        ))
    }

    /// Observe the original `d` object of a `GUILD_MEMBERS_CHUNK` dispatch.
    /// Ignore a different guild or nonce. Any error retires this attempt so
    /// a later chunk cannot turn incomplete or ambiguous state into a baseline.
    pub fn ingest_chunk(&mut self, raw: &str) -> Result<(), MemberBaselineError> {
        let mut collection = self
            .collection
            .take()
            .ok_or(MemberBaselineError::RetiredAttempt)?;
        let Object(scope): Object<Scope> = decode(raw)?;
        if scope.guild_id == self.guild_id && scope.nonce.as_deref() == Some(self.nonce.as_str()) {
            let Object(chunk) = decode(raw)?;
            collection.insert(chunk, self.expected)?;
        }
        self.collection = Some(collection);
        Ok(())
    }

    /// Report `GUILD_MEMBER_ADD`, `UPDATE` or `REMOVE` before taking the baseline.
    pub fn member_changed(&mut self, guild_id: Id<GuildMarker>) {
        if guild_id == self.guild_id {
            self.collection = None;
        }
    }

    pub fn is_complete(&self) -> bool {
        self.collection.as_ref().is_some_and(|collection| {
            collection.chunk_count == Some(collection.indices.len())
                && collection.members.len() == self.expected
        })
    }

    /// Extract only the roster. This does not prove the completeness of any
    /// other topic or validate cross-topic references.
    pub fn finish(self) -> Option<Vec<wire::Member>> {
        if !self.is_complete() {
            return None;
        }
        self.collection
            .map(|collection| collection.members.into_values().collect())
    }
}

impl Collection {
    fn insert(&mut self, chunk: Chunk, expected: usize) -> Result<(), MemberBaselineError> {
        use MemberBaselineError::InconsistentChunk;

        if !(1..=expected).contains(&chunk.chunk_count)
            || chunk.chunk_index >= chunk.chunk_count
            || self
                .chunk_count
                .is_some_and(|count| count != chunk.chunk_count)
            || !chunk.not_found.is_empty()
            || !(1..=MAX_CHUNK_MEMBERS).contains(&chunk.members.len())
            || self.members.len() + chunk.members.len() > expected
            || !self.indices.insert(chunk.chunk_index)
        {
            return Err(InconsistentChunk);
        }
        self.chunk_count = Some(chunk.chunk_count);
        for Object(member) in chunk.members {
            // Match the snapshot codec's row-local constraints.
            let mut roles = HashSet::new();
            if member.roles.len() > 250
                || !member.roles.iter().all(|role| roles.insert(*role))
                || member.nick.as_deref().is_some_and(|nick| {
                    nick.is_empty() || nick.encode_utf16().take(101).count() > 100
                })
            {
                return Err(InconsistentChunk);
            }
            let user_id = wire::Id(member.user.0.id.into_nonzero());
            let row = wire::Member {
                user_id,
                role_ids: member
                    .roles
                    .into_iter()
                    .map(|id| wire::Id(id.into_nonzero()))
                    .collect(),
                nick: member.nick,
                bot: member.user.0.bot,
                pending: member.pending,
            };
            if self.members.insert(user_id, row).is_some() {
                return Err(InconsistentChunk);
            }
        }
        if self.indices.len() == chunk.chunk_count && self.members.len() != expected {
            return Err(InconsistentChunk);
        }
        Ok(())
    }
}

fn decode<'a, T: Deserialize<'a>>(raw: &'a str) -> Result<T, MemberBaselineError> {
    serde_json::from_str(raw).map_err(|_| MemberBaselineError::MalformedChunk)
}

#[cfg(test)]
mod tests;
