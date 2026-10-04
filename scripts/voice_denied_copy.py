#!/usr/bin/env python3
"""Offline denied-copy classifier for voice room commands (stdlib only).

A staging smoke run can answer with eight denied shapes that are NOT generic
failures, and the harness must tell them apart by shape alone:

  create_member_denied      the harness identity lacks Manage Channels for
                            /create (crates/bot/src/voice_rooms.rs: the
                            may_create gate on VoiceCommand::Create).
  create_bot_denied           Discord refused the channel create with 403; the
                            bot role lacks Manage Channels (create_error_text
                            on RoomHttpError::AccessDenied).
  create_required_role        the guild-wide required role gate fired first
                            (access_denied_text on
                            AccessDenyReason::RequiredRole).
  create_command_restricted   the per-command allow-list denied this member
                            (access_denied_text on
                            AccessDenyReason::CommandRestricted).
  join_not_occupant           a vote-kick start by a member who has not joined
                            the room (kick_refusal_text on InitiatorNotOccupant).
  join_not_a_room             a vote-kick target that is not in a tracked
                            temporary room (kick_refusal_text on NotARoom).
  join_not_in_room            an ownership command (reclaim/transfer) from
                            outside the room (ownership_refusal on
                            ActorNotInRoom).
  move_bot_denied             Discord refused the member move with 403; no
                            interaction reply exists on this background path,
                            so the denial surfaces as the /setup failure line
                            (failure_line on LifecycleFailure::Discord with
                            RoomHttpError::AccessDenied, whose Display lives
                            in crates/discord/src/voice_rooms.rs).

The contrast shape is the generic handler failure (the reply contract in
docs/interaction-replies.md: the
"Something went wrong (ref XXXXXXXX)..." copy): the only text that ever
carries a bot-side failure outward. Every denied case must classify AWAY
from it, or a red smoke line is untriagable by shape.

The fixture pack lives in fixtures/voice_denied_copy.json; this module maps
one observation to exactly one outcome token. Offline only: no network, no
tokens, no guild touch. Triage meaning per outcome (what to check next) is
recorded on each fixture case, not here.
"""

import json
from pathlib import Path

# Pinned surfaces, transcribed from the authorities named above. The offline
# suite asserts each literal still appears in its source file, so copy drift
# fails the test instead of silently redefining the shape.
CREATE_MEMBER_DENIAL = "You need Manage Channels to use /create."
CREATE_BOT_DENIAL = (
    "I cannot create channels here: I need Manage Channels. "
    "Tell an admin to fix my permissions."
)
CREATE_REQUIRED_ROLE = (
    "You need the server's required role to use voice-room commands."
)
CREATE_COMMAND_RESTRICTED = (
    "You do not have a role that may use this command here."
)
JOIN_NOT_OCCUPANT = "Only room occupants can start a vote."
JOIN_NOT_A_ROOM = "That member is not in a temporary voice room."
JOIN_NOT_IN_ROOM = "You need to be in the room to use this command."
MOVE_BOT_DENIED_LINE = (
    "Discord access denied; suspend until permissions change"
)
MOVE_BOT_DENIED_PREFIX = "channel <#"
GENERIC_FAILURE_PREFIX = "Something went wrong (ref "

CREATE_MEMBER_DENIED = "create_member_denied"
CREATE_BOT_DENIED = "create_bot_denied"
CREATE_REQUIRED_ROLE_OUTCOME = "create_required_role"
CREATE_COMMAND_RESTRICTED_OUTCOME = "create_command_restricted"
JOIN_NOT_OCCUPANT_OUTCOME = "join_not_occupant"
JOIN_NOT_A_ROOM_OUTCOME = "join_not_a_room"
JOIN_NOT_IN_ROOM_OUTCOME = "join_not_in_room"
MOVE_BOT_DENIED = "move_bot_denied"
GENERIC_FAILURE = "generic_failure"

# Exact-reply outcomes: the harness must surface this byte-exact string.
EXACT_REPLIES = {
    CREATE_MEMBER_DENIED: CREATE_MEMBER_DENIAL,
    CREATE_BOT_DENIED: CREATE_BOT_DENIAL,
    CREATE_REQUIRED_ROLE_OUTCOME: CREATE_REQUIRED_ROLE,
    CREATE_COMMAND_RESTRICTED_OUTCOME: CREATE_COMMAND_RESTRICTED,
    JOIN_NOT_OCCUPANT_OUTCOME: JOIN_NOT_OCCUPANT,
    JOIN_NOT_A_ROOM_OUTCOME: JOIN_NOT_A_ROOM,
    JOIN_NOT_IN_ROOM_OUTCOME: JOIN_NOT_IN_ROOM,
}

FIXTURES_PATH = Path(__file__).resolve().parent / "fixtures" / "voice_denied_copy.json"


def load_pack(path=None):
    """Read the JSON fixture pack; shape failures are ValueErrors."""
    with open(path or FIXTURES_PATH, encoding="utf-8") as handle:
        pack = json.load(handle)
    if not isinstance(pack, dict) or not isinstance(pack.get("cases"), list):
        raise ValueError("denied-copy pack must be an object with a cases list")
    return pack


def classify_observation(observation):
    """Map one smoke observation to exactly one outcome token.

    The observation is a dict with optional "reply" (user-facing text).
    Unknown shapes fall closed to generic_failure; the denied arms are
    checked first so a fixed denied surface can never land there.
    """
    if not isinstance(observation, dict):
        return GENERIC_FAILURE
    reply = observation.get("reply") or ""
    for outcome, exact in EXACT_REPLIES.items():
        if reply == exact:
            return outcome
    if reply.startswith(MOVE_BOT_DENIED_PREFIX) and MOVE_BOT_DENIED_LINE in reply:
        return MOVE_BOT_DENIED
    return GENERIC_FAILURE


def classify_pack(pack=None):
    """Classify every fixture case; returns [(name, outcome, expected)]."""
    pack = pack if pack is not None else load_pack()
    rows = []
    for case in pack["cases"]:
        rows.append((case["name"], classify_observation(case.get("observation")),
                     case.get("expected_outcome")))
    return rows
