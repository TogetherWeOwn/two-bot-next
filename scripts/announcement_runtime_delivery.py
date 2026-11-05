"""Offline one-item delivery accounting, NOT send admission or recovery authority.

Reconstructs a declared attempt's claim/post/completion/replay sequence. Inputs
are unauthenticated observations; this in-memory model supplies no persistence,
external fence, transport, trusted nonce state or source/SQL/history evidence.
It cannot prevent a real sender, qualify runtime or authorize cleanup. The
installed runtime entry point remains unconditionally unsupported.
"""

import copy
from dataclasses import dataclass

from announcement_runtime_schema import REF, RUN, SERVICE_ID, Refusal, _validate, enum, require


_COMMON = {"run": RUN, "key_ref": REF, "nonce_ref": REF, "claim_ref": REF}
_EVENTS = {
    "claim": {},
    "wire_attempt": {"operation_id": REF},
    "accepted": {"operation_id": REF, "message_id": SERVICE_ID},
    "committed": {"message_id": SERVICE_ID},
    "replay": {"message_id": SERVICE_ID},
    "prewire_failure": {},
    "wire_failure": {},
    "completion_unknown": {},
    "history_unknown": {},
    "abort": {},
}


@dataclass(frozen=True)
class DeliveryCounts:
    attempted: int
    accepted: int
    committed: bool
    replay_observed: bool
    adverse: bool


class FeedDeliveryAccounting:
    """Bounded, terminal-on-adversity accounting for one declared run/item/nonce.

    C distinguishes fresh claims from recovered claims and retains uncertain
    sends (feeds_store.rs:475-591; feed_jobs.rs:675-838). This narrower model
    accepts only a fresh claim, one wire attempt, one known accepted message,
    one known DB completion and one unchanged replay observation. Even a
    pre-wire failure is adverse for this qualification profile: it never grants
    a second attempt. Recovery, history misses, cleanup and restarts cannot
    manufacture a successful attempt here. A new object is NOT a new runtime
    authorization, persisted counter or permission to reuse this attempt.
    """

    def __init__(self, run, key_ref, nonce_ref, operation_id):
        _validate(run, RUN)
        for ref in (key_ref, nonce_ref, operation_id):
            _validate(ref, REF)
        self._run = copy.deepcopy(run)
        self._key = key_ref
        self._nonce = nonce_ref
        self._operation = operation_id
        self._claim = None
        self._message = None
        self._state = "initial"
        self._attempted = 0
        self._accepted = 0
        self._replay = False
        self._adverse = False

    def counts(self):
        """Return immutable model counters, never an authenticated receipt."""
        return DeliveryCounts(self._attempted, self._accepted,
                              self._state == "delivered", self._replay, self._adverse)

    def observe(self, event):
        """Consume one closed event. Every rejected event poisons qualification.

        A caller catching a Refusal cannot continue this model to success. The
        attempted counter records a declared wire attempt before its outcome,
        including failed/unknown sends; it is not an external wire counter.
        """
        try:
            self._observe(event)
        except Refusal:
            self._adverse = True
            raise

    def _observe(self, event):
        require(not self._adverse, "delivery_adverse")
        require(type(event) is dict and type(event.get("kind")) is str and
                event["kind"] in _EVENTS, "delivery_event")
        kind = event["kind"]
        _validate(event, {"kind": enum(kind), **_COMMON, **_EVENTS[kind]})
        require(event["run"] == self._run and event["key_ref"] == self._key and
                event["nonce_ref"] == self._nonce, "delivery_scope")
        if kind == "claim":
            require(self._state == "initial", "delivery_claim")
            self._claim = event["claim_ref"]
            self._state = "claimed"
            return
        require(self._claim is not None and event["claim_ref"] == self._claim,
                "delivery_claim")
        if kind in ("prewire_failure", "wire_failure", "completion_unknown",
                    "history_unknown", "abort"):
            # Unknown history never establishes absence; no failure clears a
            # claim, resets the post count or reopens the wire path.
            raise Refusal("delivery_uncertain")
        if kind == "wire_attempt":
            # This observes an attempted operation, not a request for permission.
            # Count a repeated in-scope attempt even though it makes the model fail.
            self._attempted += 1
            require(self._state == "claimed" and self._attempted == 1 and
                    event["operation_id"] == self._operation, "delivery_repost")
            self._state = "in_flight"
        elif kind == "accepted":
            require(self._state == "in_flight" and
                    event["operation_id"] == self._operation, "delivery_acceptance")
            require(0 < int(event["message_id"]) <= 2**64 - 1, "delivery_message")
            self._message = event["message_id"]
            self._accepted = 1
            self._state = "accepted"
        elif kind == "committed":
            require(self._state == "accepted" and event["message_id"] == self._message,
                    "delivery_completion")
            self._state = "delivered"
        else:
            require(kind == "replay" and self._state == "delivered" and not self._replay and
                    event["message_id"] == self._message, "delivery_replay")
            self._replay = True

    def require_complete(self):
        """Require the narrow model only; this never grants runtime admission."""
        require(not self._adverse and self._state == "delivered" and self._replay and
                self._attempted == self._accepted == 1, "delivery_incomplete")
