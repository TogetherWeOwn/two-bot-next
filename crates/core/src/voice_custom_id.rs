//! Pure `two:voice:` component-id codec for voice buttons and modals.
//!
//! The runtime binds Discord components to core state through the returned
//! `custom_id` strings and parses them back as untrusted input; this module
//! performs no I/O, holds no Discord, store or clock types, and depends on no
//! voice runtime state.
//!
//! Covered surfaces (see `docs/voice-rooms.md`):
//! - V3 private-join Approve / Deny / Block buttons, bound to the room and
//!   the request ID. Request IDs are never reused within a room
//!   (`voice_private::PrivateRoom::next_request_id`), so a stale button can
//!   never answer a newer request; the runtime resolves the pair with
//!   `voice_private::PrivateRoom::decide`.
//! - V4 vote-kick Yes / No buttons, bound to the vote ID. The ID must be the
//!   unique initiating interaction ID, never just the target member ID; the
//!   runtime casts the ballot with `voice_vote_kick::VoteKickCore::cast`.
//! - `/name` panel custom / restore buttons plus the custom-name modal,
//!   bound to the room.
//! - V11 `/import` preview Confirm / Cancel buttons, bound to the uploading
//!   member and the previewed diff's content hash. Confirm re-reads current
//!   state and re-diffs; a concurrent change yields a new preview instead of
//!   applying.
//!
//! Every encoded id starts with `two:voice:` (never `two:lfg:` or
//! `two:self-role:`) and is at most 100 characters, Discord's component-id
//! limit. Decoding is strict: garbage, overlong and non-numeric input decodes
//! to `None`, as do zero ids (all voice ids are nonzero).

use crate::voice_private::JoinDecision;
use crate::voice_vote_kick::VoteBallot;
use crate::Snowflake;

/// Prefix for every voice component id. Distinct from `two:lfg:` and
/// `two:self-role:`, so the router can dispatch on prefix without overlap.
pub const VOICE_CUSTOM_ID_PREFIX: &str = "two:voice:";

/// Discord refuses component ids longer than 100 characters; longer input
/// decodes to `None` and encoders never produce it.
pub const MAX_VOICE_CUSTOM_ID_CHARS: usize = 100;

/// A decoded voice component id. Component ids are Discord-signed but still
/// parsed as untrusted input: only exactly-shaped ids decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceAction {
    /// V3 private-join Approve button for one pending request.
    JoinApprove { room_id: Snowflake, request_id: u64 },
    /// V3 private-join Deny button for one pending request.
    JoinDeny { room_id: Snowflake, request_id: u64 },
    /// V3 private-join Block button for one pending request.
    JoinBlock { room_id: Snowflake, request_id: u64 },
    /// V4 vote-kick Yes button for one vote.
    KickYes { vote_id: Snowflake },
    /// V4 vote-kick No button for one vote.
    KickNo { vote_id: Snowflake },
    /// `/name` panel button opening the custom-name modal.
    NameCustom { room_id: Snowflake },
    /// `/name` panel button restoring the template name.
    NameRestore { room_id: Snowflake },
    /// `/name` custom-name modal submit.
    NameModal { room_id: Snowflake },
    /// V11 `/import` preview Confirm button, bound to the uploading member
    /// and the previewed diff's content hash (16 lowercase hex chars).
    ImportConfirm { member_id: Snowflake, hash: String },
    /// V11 `/import` preview Cancel button, bound like Confirm. Cancel and
    /// expiry write nothing.
    ImportCancel { member_id: Snowflake, hash: String },
}

/// Length of a diff content hash in a component id: 16 lowercase hex chars.
pub const IMPORT_HASH_CHARS: usize = 16;

/// Component id for a private-join decision button, bound to the room and the
/// request id the owner is answering.
#[must_use]
pub fn join_custom_id(decision: JoinDecision, room_id: Snowflake, request_id: u64) -> String {
    let verb = match decision {
        JoinDecision::Approve => "join-approve",
        JoinDecision::Deny => "join-deny",
        JoinDecision::Block => "join-block",
    };
    format!("{VOICE_CUSTOM_ID_PREFIX}{verb}:{room_id}:{request_id}")
}

/// Component id for a vote-kick ballot button, bound to the vote id.
#[must_use]
pub fn kick_custom_id(ballot: VoteBallot, vote_id: Snowflake) -> String {
    let verb = match ballot {
        VoteBallot::Yes => "kick-yes",
        VoteBallot::No => "kick-no",
    };
    format!("{VOICE_CUSTOM_ID_PREFIX}{verb}:{vote_id}")
}

/// Component id for the `/name` panel button opening the custom-name modal.
#[must_use]
pub fn name_custom_custom_id(room_id: Snowflake) -> String {
    format!("{VOICE_CUSTOM_ID_PREFIX}name-custom:{room_id}")
}

/// Component id for the `/name` panel button restoring the template name.
#[must_use]
pub fn name_restore_custom_id(room_id: Snowflake) -> String {
    format!("{VOICE_CUSTOM_ID_PREFIX}name-restore:{room_id}")
}

/// Component id for the `/name` custom-name modal submit.
#[must_use]
pub fn name_modal_custom_id(room_id: Snowflake) -> String {
    format!("{VOICE_CUSTOM_ID_PREFIX}name-modal:{room_id}")
}

/// Component id for the `/import` preview Confirm button. `hash` is the
/// previewed diff's content hash ([`IMPORT_HASH_CHARS`] lowercase hex chars);
/// the runtime recomputes it against fresh state before applying.
#[must_use]
pub fn import_confirm_custom_id(member_id: Snowflake, hash: &str) -> String {
    format!("{VOICE_CUSTOM_ID_PREFIX}import-confirm:{member_id}:{hash}")
}

/// Component id for the `/import` preview Cancel button, bound like Confirm.
#[must_use]
pub fn import_cancel_custom_id(member_id: Snowflake, hash: &str) -> String {
    format!("{VOICE_CUSTOM_ID_PREFIX}import-cancel:{member_id}:{hash}")
}

/// Parse a component id; `None` for anything outside the voice namespace,
/// including garbage, overlong, non-numeric and zero-id input.
#[must_use]
pub fn parse_voice_custom_id(custom_id: &str) -> Option<VoiceAction> {
    if custom_id.len() > MAX_VOICE_CUSTOM_ID_CHARS {
        return None;
    }
    let rest = custom_id.strip_prefix(VOICE_CUSTOM_ID_PREFIX)?;
    let mut parts = rest.split(':');
    let verb = parts.next()?;
    let action = match verb {
        "join-approve" | "join-deny" | "join-block" => {
            let room_id = parse_id(parts.next()?)?;
            let request_id = parse_id(parts.next()?)?;
            match verb {
                "join-approve" => VoiceAction::JoinApprove {
                    room_id,
                    request_id,
                },
                "join-deny" => VoiceAction::JoinDeny {
                    room_id,
                    request_id,
                },
                _ => VoiceAction::JoinBlock {
                    room_id,
                    request_id,
                },
            }
        }
        "kick-yes" | "kick-no" => {
            let vote_id = parse_id(parts.next()?)?;
            if verb == "kick-yes" {
                VoiceAction::KickYes { vote_id }
            } else {
                VoiceAction::KickNo { vote_id }
            }
        }
        "name-custom" | "name-restore" | "name-modal" => {
            let room_id = parse_id(parts.next()?)?;
            match verb {
                "name-custom" => VoiceAction::NameCustom { room_id },
                "name-restore" => VoiceAction::NameRestore { room_id },
                _ => VoiceAction::NameModal { room_id },
            }
        }
        "import-confirm" | "import-cancel" => {
            let member_id = parse_id(parts.next()?)?;
            let hash = parse_hash(parts.next()?)?;
            if verb == "import-confirm" {
                VoiceAction::ImportConfirm { member_id, hash }
            } else {
                VoiceAction::ImportCancel { member_id, hash }
            }
        }
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(action)
}

/// Diff content hash: exactly [`IMPORT_HASH_CHARS`] lowercase hex chars, as
/// produced by the import preview. Anything else is untrusted input, not a
/// button this runtime bound.
fn parse_hash(value: &str) -> Option<String> {
    if value.len() == IMPORT_HASH_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Some(value.to_owned())
    } else {
        None
    }
}

/// Canonical nonzero decimal id. Rejects empties, signs, whitespace, hex and
/// anything else `u64` parsing would accept that is not plain digits, plus
/// zero, which no voice id ever holds.
fn parse_id(value: &str) -> Option<Snowflake> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id: Snowflake = value.parse().ok()?;
    if id == 0 {
        None
    } else {
        Some(id)
    }
}
