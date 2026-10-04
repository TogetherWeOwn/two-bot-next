#!/usr/bin/env python3
"""Offline edge-case classifier for the staging smoke commands (stdlib only).

A staging smoke run can answer with three edge shapes that are NOT generic
failures, and the harness must tell them apart by shape alone:

  permission_denied   the harness identity lacks rights; the bot is healthy.
                      Surface is the router's actionable Manage Server denial
                      (crates/core/src/router.rs: MANAGE_SERVER_REQUIRED).
  interaction_timeout the interactions endpoint did not answer in time.
                      Surface is the smoke's transport refusal
                      (scripts/staging_automation_read_smoke.py: get_json
                      raises "interactions endpoint did not respond (...)").
  empty_rank          a member with no XP row asks for rank: a valid empty
                      result, not a failure. Surface is the unranked
                      rank-text label (crates/core/src/leveling.rs: rank_text
                      with rank None renders "Rank **Unranked** (no XP
                      recorded)").

The contrast shape is the generic handler failure (the reply contract in
docs/interaction-replies.md: the
"Something went wrong (ref XXXXXXXX)..." copy): the only text that ever
carries a bot-side failure outward. Every edge case must classify AWAY from
it, or a red smoke line is untriagable by shape.

The fixture pack lives in fixtures/staging_edge_cases.json; this module maps
one observation to exactly one outcome token. Offline only: no network, no
tokens, no guild touch. Triage meaning per outcome (what to check next) is
recorded on each fixture case, not here.
"""

import json
from pathlib import Path

# Pinned surfaces, transcribed from the authorities named above. The offline
# suite asserts each literal still appears in its source file, so copy drift
# fails the test instead of silently redefining the shape.
MANAGE_SERVER_DENIAL = (
    "You need the Manage Server permission to use this command. "
    "Ask a server admin to grant it."
)
TIMEOUT_REASON_PREFIX = "interactions endpoint did not respond ("
UNRANKED_LABEL = "Rank **Unranked** (no XP recorded)"
GENERIC_FAILURE_PREFIX = "Something went wrong (ref "

PERMISSION_DENIED = "permission_denied"
INTERACTION_TIMEOUT = "interaction_timeout"
EMPTY_RANK = "empty_rank"
GENERIC_FAILURE = "generic_failure"

FIXTURES_PATH = Path(__file__).resolve().parent / "fixtures" / "staging_edge_cases.json"


def load_pack(path=None):
    """Read the JSON fixture pack; shape failures are ValueErrors."""
    with open(path or FIXTURES_PATH, encoding="utf-8") as handle:
        pack = json.load(handle)
    if not isinstance(pack, dict) or not isinstance(pack.get("cases"), list):
        raise ValueError("edge-case pack must be an object with a cases list")
    return pack


def classify_observation(observation):
    """Map one smoke observation to exactly one outcome token.

    The observation is a dict with optional "reply" (user-facing text or
    None when no reply arrived) and "reason" (the harness one-line cause).
    Unknown shapes fall closed to generic_failure; the edge arms are
    checked first so a fixed edge surface can never land there.
    """
    if not isinstance(observation, dict):
        return GENERIC_FAILURE
    reply = observation.get("reply") or ""
    reason = observation.get("reason") or ""
    if UNRANKED_LABEL in reply:
        return EMPTY_RANK
    if reply == MANAGE_SERVER_DENIAL:
        return PERMISSION_DENIED
    if reason.startswith(TIMEOUT_REASON_PREFIX):
        return INTERACTION_TIMEOUT
    return GENERIC_FAILURE


def classify_pack(pack=None):
    """Classify every fixture case; returns [(name, outcome, expected)]."""
    pack = pack if pack is not None else load_pack()
    rows = []
    for case in pack["cases"]:
        rows.append((case["name"], classify_observation(case.get("observation")),
                     case.get("expected_outcome")))
    return rows
