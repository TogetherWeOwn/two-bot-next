//! Pure live-rooms board: one viewer's joinable, occupied temporary voice
//! rooms as mention-inert text.
//!
//! No Discord I/O, clock, storage or intent. The caller supplies one snapshot
//! generation of facts and gets back the entries actually rendered plus the
//! whole reply content, so a reply can never mix generations. A room appears
//! only when the viewer holds both `VIEW_CHANNEL` and `CONNECT` through
//! [`effective_permissions`]; an unresolvable role means the viewer's
//! permissions are unknown, which is a refusal and never an allow.

use twilight_model::{
    channel::permission_overwrite::PermissionOverwrite,
    guild::{Permissions, Role},
    id::{marker::RoleMarker, Id},
};
use two_bot_core::{
    message_safety::{text_len, CONTENT_LIMIT},
    voice_alias::AliasTable,
    voice_conditions::ConditionFacts,
    voice_naming::{GameOptions, RoomContext},
    voice_presence::{apply_room_presence, MemberPresence, OccupantPresence},
    Snowflake,
};

use crate::voice_rooms::effective_permissions;

/// Rooms listed at most; the rest are counted, never silently dropped.
pub const MAX_BOARD_ROOMS: usize = 25;
/// Unicode scalar values of a game label before it is shortened.
pub const GAME_DISPLAY_CHARS: usize = 48;
/// Shortest game label tried before whole rows are dropped to fit.
const MIN_GAME_CHARS: usize = 4;

const CATCHING_UP: &str = "Live rooms are still catching up \u{2014} try again in a moment.";
const EMPTY_STATE: &str = "No public rooms are live right now.";

/// One occupant of a room; bots never count toward seats or games.
#[derive(Debug, Clone, Copy)]
pub struct BoardOccupant<'a> {
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub presence: Option<&'a MemberPresence>,
}

/// A bot-tracked temporary room as the snapshot holds it.
#[derive(Debug, Clone)]
pub struct BoardRoom<'a> {
    pub channel_id: Snowflake,
    pub tracked: bool,
    pub owner_id: Snowflake,
    /// `None` and `0` are both unlimited.
    pub user_limit: Option<u16>,
    pub overwrites: &'a [PermissionOverwrite],
    pub occupants: Vec<BoardOccupant<'a>>,
}

/// Every fact one render reads, from a single snapshot generation.
#[derive(Debug, Clone)]
pub struct BoardSnapshot<'a> {
    pub ready: bool,
    pub generation: u64,
    pub as_of_unix: i64,
    pub guild_id: Snowflake,
    pub guild_owner_id: Snowflake,
    pub viewer_id: Snowflake,
    /// `None` is an absent member-role snapshot, not a member without roles.
    pub viewer_roles: Option<&'a [Id<RoleMarker>]>,
    pub roles: &'a [Role],
    pub presence_enabled: bool,
    pub aliases: &'a AliasTable,
    pub game_options: &'a GameOptions,
    pub rooms: Vec<BoardRoom<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardState {
    Ready,
    /// Snapshot not authoritative for this viewer: nothing is listed.
    CatchingUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seats {
    Open,
    Remaining(usize),
    Full,
}

impl Seats {
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Seats::Open => "Open".to_owned(),
            Seats::Remaining(1) => "1 seat open".to_owned(),
            Seats::Remaining(n) => format!("{n} seats open"),
            Seats::Full => "Full".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardEntry {
    pub channel_id: Snowflake,
    pub humans: usize,
    pub seats: Seats,
    /// Bounded display label before escaping; `None` when no presence fact
    /// names a majority game. The naming "General" fallback never lands here.
    pub game: Option<String>,
    /// Humans with a game fact; `Some` only together with `game`.
    pub members_playing: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    pub generation: u64,
    pub state: BoardState,
    /// Entries actually present in `content`, in display order.
    pub entries: Vec<BoardEntry>,
    /// Eligible rooms not shown, so `entries.len() + omitted` is the total.
    pub omitted: usize,
    pub content: String,
}

#[derive(Debug, Clone)]
struct Candidate {
    channel_id: Snowflake,
    humans: usize,
    seats: Seats,
    game: Option<String>,
    members_playing: usize,
}

#[must_use]
pub fn render_board(snapshot: &BoardSnapshot<'_>) -> Board {
    let Some(viewer_roles) = snapshot.viewer_roles.filter(|_| snapshot.ready) else {
        return catching_up(snapshot);
    };
    // With no overwrites the resolver fails only on an incomplete role table.
    if resolve(snapshot, viewer_roles, &[]).is_none() {
        return catching_up(snapshot);
    }

    let mut eligible: Vec<Candidate> = snapshot
        .rooms
        .iter()
        .filter(|room| room.tracked)
        .filter(|room| viewer_may_join(snapshot, viewer_roles, room))
        .filter_map(|room| candidate(snapshot, room))
        .collect();
    eligible.sort_by(|a, b| {
        (a.seats == Seats::Full)
            .cmp(&(b.seats == Seats::Full))
            .then_with(|| b.humans.cmp(&a.humans))
            .then_with(|| a.channel_id.cmp(&b.channel_id))
    });
    fit(snapshot, &eligible)
}

fn catching_up(snapshot: &BoardSnapshot<'_>) -> Board {
    Board {
        generation: snapshot.generation,
        state: BoardState::CatchingUp,
        entries: Vec::new(),
        omitted: 0,
        content: CATCHING_UP.to_owned(),
    }
}

fn resolve(
    snapshot: &BoardSnapshot<'_>,
    viewer_roles: &[Id<RoleMarker>],
    overwrites: &[PermissionOverwrite],
) -> Option<Permissions> {
    effective_permissions(
        snapshot.guild_id,
        snapshot.guild_owner_id,
        snapshot.viewer_id,
        viewer_roles,
        snapshot.roles,
        overwrites,
    )
}

/// View AND Connect: a private room only denies Connect, and staff powers
/// such as Manage Channels never widen what a viewer may see here.
fn viewer_may_join(
    snapshot: &BoardSnapshot<'_>,
    viewer_roles: &[Id<RoleMarker>],
    room: &BoardRoom<'_>,
) -> bool {
    resolve(snapshot, viewer_roles, room.overwrites).is_some_and(|permissions| {
        permissions.contains(Permissions::VIEW_CHANNEL | Permissions::CONNECT)
    })
}

/// `None` for rooms without a human: bot-only and empty rooms are absent.
fn candidate(snapshot: &BoardSnapshot<'_>, room: &BoardRoom<'_>) -> Option<Candidate> {
    let humans: Vec<&BoardOccupant<'_>> = room
        .occupants
        .iter()
        .filter(|occupant| !occupant.is_bot)
        .collect();
    if humans.is_empty() {
        return None;
    }
    let count = humans.len();
    let seats = match room.user_limit {
        None | Some(0) => Seats::Open,
        Some(limit) if count < usize::from(limit) => Seats::Remaining(usize::from(limit) - count),
        Some(_) => Seats::Full,
    };

    // Same inputs and majority rules as the room-name game, so the two agree.
    let occupants: Vec<OccupantPresence<'_>> = humans
        .iter()
        .map(|occupant| OccupantPresence {
            presence: occupant.presence.filter(|_| snapshot.presence_enabled),
            live_discord: false,
            is_owner: occupant.member_id == room.owner_id,
        })
        .collect();
    let mut context = RoomContext::default();
    let mut conditions = ConditionFacts::default();
    apply_room_presence(
        &mut context,
        &mut conditions,
        &occupants,
        snapshot.aliases,
        snapshot.game_options,
    );
    let game = Some(display_text(&conditions.games.join(" & "))).filter(|label| !label.is_empty());
    let members_playing = game.as_ref().map_or(0, |_| {
        usize::try_from(context.members_playing).unwrap_or(usize::MAX)
    });
    Some(Candidate {
        channel_id: room.channel_id,
        humans: count,
        seats,
        game,
        members_playing,
    })
}

/// Shorten game text before dropping rows: the longest label that lets the
/// whole listing fit wins, and rows go only when even the shortest cannot.
fn fit(snapshot: &BoardSnapshot<'_>, eligible: &[Candidate]) -> Board {
    let total = eligible.len();
    let mut shown = total.min(MAX_BOARD_ROOMS);
    loop {
        for limit in (MIN_GAME_CHARS..=GAME_DISPLAY_CHARS).rev() {
            let entries: Vec<BoardEntry> = eligible[..shown]
                .iter()
                .map(|candidate| entry(candidate, limit))
                .collect();
            let content = render_content(snapshot, &entries, total - shown);
            if text_len(&content) <= CONTENT_LIMIT {
                return Board {
                    generation: snapshot.generation,
                    state: BoardState::Ready,
                    entries,
                    omitted: total - shown,
                    content,
                };
            }
        }
        // Zero rows always fit (header plus one overflow line), so this ends.
        shown = shown.saturating_sub(1);
    }
}

fn entry(candidate: &Candidate, limit: usize) -> BoardEntry {
    let game = candidate.game.as_deref().map(|label| shorten(label, limit));
    BoardEntry {
        channel_id: candidate.channel_id,
        humans: candidate.humans,
        seats: candidate.seats,
        members_playing: game.as_ref().map(|_| candidate.members_playing),
        game,
    }
}

fn render_content(snapshot: &BoardSnapshot<'_>, entries: &[BoardEntry], omitted: usize) -> String {
    let mut out = format!("Live rooms as of <t:{}:t>", snapshot.as_of_unix);
    if entries.is_empty() && omitted == 0 {
        out.push('\n');
        out.push_str(EMPTY_STATE);
        return out;
    }
    for entry in entries {
        out.push_str(&format!(
            "\n<#{}> \u{b7} {} in voice \u{b7} {}",
            entry.channel_id,
            entry.humans,
            entry.seats.label()
        ));
        if let (Some(game), Some(playing)) = (&entry.game, entry.members_playing) {
            out.push_str(&format!(" \u{b7} {} \u{b7} {playing} playing", inert(game)));
        }
    }
    if omitted > 0 {
        out.push_str(&format!("\n\u{2026}and {omitted} more rooms"));
    }
    out
}

/// Control and bidi-override characters become spaces; ends are trimmed.
fn display_text(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                ' '
            } else {
                ch
            }
        })
        .collect();
    flat.trim().to_owned()
}

/// At most `limit` Unicode scalar values, ending in a visible ellipsis when cut.
fn shorten(label: &str, limit: usize) -> String {
    if label.chars().count() <= limit {
        return label.to_owned();
    }
    let kept: String = label.chars().take(limit.saturating_sub(1)).collect();
    format!("{}\u{2026}", kept.trim_end())
}

/// Keeps readable text but no mention, channel pill, link or active markdown.
/// `<` and `@` are escaped separately, so neither can start a pill.
fn inert(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '@' => safe.push_str("\\@\u{200b}"),
            ':' => safe.push_str(":\u{200b}"),
            '\\' | '*' | '_' | '~' | '`' | '|' | '[' | ']' | '(' | ')' | '<' | '>' | '#' | '!' => {
                safe.push('\\');
                safe.push(ch);
            }
            _ => safe.push(ch),
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_keeps_limit_in_scalars_and_marks_the_cut() {
        assert_eq!(shorten("Apex", 4), "Apex");
        assert_eq!(shorten("Apex Legends", 5), "Apex\u{2026}");
        let cut = shorten(&"\u{1f3ae}".repeat(90), GAME_DISPLAY_CHARS);
        assert_eq!(cut.chars().count(), GAME_DISPLAY_CHARS);
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn inert_never_leaves_a_pill_or_mass_mention_start() {
        let safe = inert("<@1> <#2> @everyone @here **b** [x](y) https://z");
        for token in ["<@", "<#", "@everyone", "@here", "**", "](", "://"] {
            assert!(!safe.contains(token), "{token} survived in {safe}");
        }
    }

    #[test]
    fn display_text_flattens_controls_and_bidi_overrides() {
        assert_eq!(display_text(" a\nb\u{202e}c\0 "), "a b c");
    }

    #[test]
    fn seat_labels_are_exact() {
        assert_eq!(Seats::Open.label(), "Open");
        assert_eq!(Seats::Remaining(1).label(), "1 seat open");
        assert_eq!(Seats::Remaining(3).label(), "3 seats open");
        assert_eq!(Seats::Full.label(), "Full");
    }
}
