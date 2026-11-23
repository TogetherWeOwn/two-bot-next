//! Project member and scheduled-event dispatches into realtime v1 operations. No I/O.

use serde::Deserialize;
use serde_json::{value::RawValue, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use twilight_model::{
    guild::scheduled_event::{EntityType, PrivacyLevel, Status},
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker, ScheduledEventMarker, UserMarker},
        Id,
    },
};
use two_bot_core::realtime_wire as wire;
use wire::{Change, Operation};

use crate::realtime_member_baseline::Object;

mod baseline;
pub mod intake;
pub use baseline::scheduled_events;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionError {
    MalformedDispatch,
    MissingScreeningState,
    UnsupportedEvent,
}

#[derive(Deserialize)]
struct Dispatch<'a> {
    op: u8,
    #[serde(borrow)]
    t: Option<&'a RawValue>,
    #[serde(borrow)]
    d: Option<&'a RawValue>,
}

impl Dispatch<'_> {
    fn kind(&self) -> Result<Option<Kind>, ProjectionError> {
        if self.op != 0 {
            return Ok(None);
        }
        let name = self.t.ok_or(ProjectionError::MalformedDispatch)?.get();
        decode(name, '"').map(Some)
    }

    fn body(&self) -> Result<&str, ProjectionError> {
        Ok(self.d.ok_or(ProjectionError::MalformedDispatch)?.get())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Kind {
    GuildCreate,
    GuildMembersChunk,
    GuildMemberAdd,
    GuildMemberUpdate,
    GuildMemberRemove,
    GuildScheduledEventCreate,
    GuildScheduledEventUpdate,
    GuildScheduledEventDelete,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct GuildScope {
    guild_id: Id<GuildMarker>,
}

#[derive(Deserialize)]
struct MemberInput {
    user: Object<UserInput>,
    roles: Vec<Id<RoleMarker>>,
    nick: Option<String>,
    // Twilight defaults an omitted `pending` to false. Do not trust that
    // default as membership-screening evidence.
    pending: Option<bool>,
}

#[derive(Deserialize)]
struct UserInput {
    id: Id<UserMarker>,
    #[serde(default, deserialize_with = "present_bot")]
    bot: Option<bool>,
}

fn present_bot<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    bool::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct RemovedMember {
    user: Object<RemovedUser>,
}

#[derive(Deserialize)]
struct RemovedUser {
    id: Id<UserMarker>,
}

#[derive(Deserialize)]
struct RemovedEvent {
    id: Id<ScheduledEventMarker>,
}

#[derive(Deserialize)]
struct ScheduledEventInput {
    id: Id<ScheduledEventMarker>,
    channel_id: Option<Id<ChannelMarker>>,
    entity_type: EntityType,
    privacy_level: PrivacyLevel,
    status: Status,
    name: String,
    description: Option<String>,
    entity_metadata: Option<EventMetadata>,
    scheduled_start_time: String,
    scheduled_end_time: Option<String>,
}

#[derive(Deserialize)]
struct EventMetadata {
    location: Option<String>,
}

/// Read only supported operations for the configured guild from original gateway JSON.
/// `Ok(None)` means unrelated; a projection error must invalidate the affected
/// baseline rather than let stale state remain authoritative. The reducer still
/// owns codec validation, reference cleanup, sequencing and visibility.
pub fn project(guild_id: Id<GuildMarker>, raw: &str) -> Result<Option<Operation>, ProjectionError> {
    let dispatch: Dispatch<'_> = decode(raw, '{')?;
    project_dispatch(guild_id, dispatch.kind()?, &dispatch, |body| {
        event_change(decode(body, '{')?)
    })
}

/// Merge a live update into a known member of the guild validated by the caller.
/// Omission of the screening flag does not prove that screening was passed.
pub fn project_member_update<'a>(
    body: &Value,
    known: impl FnOnce(wire::Id) -> Option<&'a wire::Member>,
) -> Result<Operation, ProjectionError> {
    let mut input =
        MemberInput::deserialize(body).map_err(|_| ProjectionError::MalformedDispatch)?;
    let previous = known(wire_id(input.user.0.id)).ok_or(ProjectionError::MalformedDispatch)?;
    if body.get("pending").is_none() {
        input.pending = Some(previous.pending);
    }
    if body.get("nick").is_none() {
        input.nick.clone_from(&previous.nick);
    }
    input.user.0.bot.get_or_insert(previous.bot);
    member_change(input)
}

/// Project only scheduled-event creates, updates and deletes, with the same
/// row checks as baseline intake. The caller checks duplicate keys and
/// validates references against the active baseline before encoding.
pub fn project_scheduled_event(
    guild_id: Id<GuildMarker>,
    raw: &str,
) -> Result<Option<Operation>, ProjectionError> {
    let dispatch: Dispatch<'_> = decode(raw, '{')?;
    let kind = dispatch.kind()?;
    if !matches!(
        kind,
        Some(
            Kind::GuildScheduledEventCreate
                | Kind::GuildScheduledEventUpdate
                | Kind::GuildScheduledEventDelete
        )
    ) {
        return Ok(None);
    }
    project_dispatch(guild_id, kind, &dispatch, |body| {
        baseline::strict_event_change(guild_id, body, true)
    })
}

fn project_dispatch(
    guild_id: Id<GuildMarker>,
    kind: Option<Kind>,
    dispatch: &Dispatch<'_>,
    project_event: impl FnOnce(&str) -> Result<Change<wire::Event>, ProjectionError>,
) -> Result<Option<Operation>, ProjectionError> {
    let kind = match kind {
        None | Some(Kind::GuildCreate | Kind::GuildMembersChunk | Kind::Other) => return Ok(None),
        Some(kind) => kind,
    };
    let body = dispatch.body()?;
    if decode::<GuildScope>(body, '{')?.guild_id != guild_id {
        return Ok(None);
    }

    let operation = match kind {
        Kind::GuildMemberAdd | Kind::GuildMemberUpdate => member_change(decode(body, '{')?)?,
        Kind::GuildMemberRemove => Operation::Members(Change::Remove {
            id: wire_id(decode::<RemovedMember>(body, '{')?.user.0.id),
        }),
        Kind::GuildScheduledEventCreate | Kind::GuildScheduledEventUpdate => {
            Operation::Events(project_event(body)?)
        }
        Kind::GuildScheduledEventDelete => Operation::Events(Change::Remove {
            id: wire_id(decode::<RemovedEvent>(body, '{')?.id),
        }),
        Kind::GuildCreate | Kind::GuildMembersChunk | Kind::Other => return Ok(None),
    };
    Ok(Some(operation))
}

fn decode<'a, T: Deserialize<'a>>(input: &'a str, prefix: char) -> Result<T, ProjectionError> {
    if !input.trim_start().starts_with(prefix) {
        return Err(ProjectionError::MalformedDispatch);
    }
    serde_json::from_str(input).map_err(|_| ProjectionError::MalformedDispatch)
}

fn wire_id<M>(id: Id<M>) -> wire::Id {
    wire::Id(id.into_nonzero())
}

fn member_change(input: MemberInput) -> Result<Operation, ProjectionError> {
    let id = wire_id(input.user.0.id);
    Ok(Operation::Members(Change::Upsert {
        id,
        value: wire::Member {
            user_id: id,
            role_ids: input.roles.into_iter().map(wire_id).collect(),
            nick: input.nick,
            bot: input.user.0.bot.unwrap_or(false), // Discord omits `bot` on human users.
            pending: input
                .pending
                .ok_or(ProjectionError::MissingScreeningState)?,
        },
    }))
}

fn event_change(input: ScheduledEventInput) -> Result<Change<wire::Event>, ProjectionError> {
    let id = wire_id(input.id);
    let status = match input.status {
        Status::Scheduled => wire::EventStatus::Scheduled,
        Status::Active => wire::EventStatus::Active,
        Status::Completed | Status::Cancelled => return Ok(Change::Remove { id }),
        _ => return Err(ProjectionError::UnsupportedEvent),
    };
    if input.privacy_level != PrivacyLevel::GuildOnly {
        return Err(ProjectionError::UnsupportedEvent);
    }
    let (channel_id, location) = match (input.entity_type, input.channel_id) {
        (EntityType::StageInstance | EntityType::Voice, Some(id)) => (Some(wire_id(id)), None),
        (EntityType::External, None) => {
            (None, input.entity_metadata.and_then(|meta| meta.location))
        }
        _ => return Err(ProjectionError::UnsupportedEvent),
    };
    let (starts_at, ends_at) = event_times(
        &input.scheduled_start_time,
        input.scheduled_end_time.as_deref(),
    )?;
    Ok(Change::Upsert {
        id,
        value: wire::Event {
            id,
            channel_id,
            status,
            title: input.name,
            description: input.description,
            location,
            starts_at,
            ends_at,
        },
    })
}

fn event_times(
    start: &str,
    end: Option<&str>,
) -> Result<(wire::WireTime, Option<wire::WireTime>), ProjectionError> {
    Ok((wire_time(start)?, end.map(wire_time).transpose()?))
}

fn wire_time(input: &str) -> Result<wire::WireTime, ProjectionError> {
    // Parse the original offset; Twilight's Timestamp parser drops it.
    OffsetDateTime::parse(input, &Rfc3339)
        .map(wire::WireTime)
        .map_err(|_| ProjectionError::MalformedDispatch)
}

#[cfg(test)]
mod tests;
