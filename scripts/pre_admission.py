"""Dormant admission and startup settlement for one authenticated PRE reservation.

The protected controller authenticates the full Target before using this module
and must bind each provider resource name to only one reservation. The planner's
Worker and Container names can coincide across scopes or shapes. Only the
independent settlement adapter can interpret provider or runtime proof.
There is no dispatch, provider transport, lease binding, or stop operation here.
"""

from collections.abc import Callable
from dataclasses import dataclass, replace
from enum import Enum
from typing import Protocol
from uuid import UUID, uuid4

from pre_target import Target, TargetError


class SettlementSource(Enum):
    PROVIDER = "provider"
    RUNTIME = "runtime"


@dataclass(frozen=True)
class AdmissionRecord:
    target: Target
    start_id: UUID | None
    closed: bool
    settled_by: SettlementSource | None = None


class AdmissionStore(Protocol):
    """Atomic lease-style storage scoped by all four Target fields.

    Read the current durable record. Only definite absence returns None. CAS
    compares the whole record (or definite absence) and updates atomically. For
    a start it must also verify the half-open window with trusted, nondecreasing
    authority time at commit. An uncertain clock refuses the start. Return True
    only for a confirmed durable commit, False for a definitive rejection, and
    raise on uncertainty. Never reset or evict a record while that reservation
    can be presented again, including after expiry and across restarts.
    """

    def read(self, target: Target) -> AdmissionRecord | None: ...

    def compare_and_swap(
        self, target: Target, expected: AdmissionRecord | None, replacement: AdmissionRecord,
        *, start_window: tuple[int, int] | None = None,
    ) -> bool: ...


@dataclass(frozen=True)
class SettlementEvidence:
    """Affirmative, independently verified proof obtained after durable close.

    The adapter must bind proof to this exact closed record and original start
    ID (if any). It must verify that all startup requests have finished or
    cannot take effect and that no actor or keepalive can create or restart
    these resources. Expiry, workflow completion, and absence are insufficient.
    """

    closed: AdmissionRecord
    source: SettlementSource
    startup_drained: bool
    restart_fenced: bool


class StartupAdmission:
    def __init__(self, store: AdmissionStore,
                 observe_settlement: Callable[[AdmissionRecord], SettlementEvidence | None]):
        self._store = store
        self._observe_settlement = observe_settlement

    def _read(self, target: Target) -> AdmissionRecord | None:
        if type(target) is not Target:
            raise TargetError("invalid_pre_admission_target")
        try:
            record = self._store.read(target)
        except Exception:
            raise TargetError("pre_admission_read_unconfirmed") from None
        if record is not None and (
            type(record) is not AdmissionRecord or record.target != target
            or type(record.closed) is not bool
            or (record.start_id is not None and type(record.start_id) is not UUID)
            or (not record.closed and (record.start_id is None or record.settled_by is not None))
            or (record.settled_by is not None and type(record.settled_by) is not SettlementSource)
        ):
            raise TargetError("invalid_pre_admission_record")
        return record

    def _swap(self, target, expected, replacement, *, start_window=None) -> bool:
        try:
            result = self._store.compare_and_swap(target, expected, replacement, start_window=start_window)
        except Exception:
            raise TargetError("pre_admission_write_unconfirmed") from None
        if type(result) is not bool:
            raise TargetError("pre_admission_write_unconfirmed")
        return result

    def admit_start(self, target: Target) -> UUID | None:
        """Consume the only start at the protected dispatch boundary.

        Only the winning call receives the new correlation ID. If interrupted
        or unacknowledged, reconcile read-only without resuming or retrying the
        start. An ID is not a reusable grant. The live dispatch boundary must
        enforce close and expiry itself if a request can be delayed past them.
        Keep this seam dormant until that boundary is proved.
        """
        if self._read(target) is not None:
            return None
        record = AdmissionRecord(target, uuid4(), closed=False)
        if self._swap(target, None, record, start_window=(target.started_at, target.expires_at)):
            return record.start_id
        return None

    def close(self, target: Target) -> bool:
        """Permanently fence new claims, preserving any unresolved start."""
        current = self._read(target)
        if current is not None and current.closed:
            return True
        closed = (replace(current, closed=True) if current is not None
                  else AdmissionRecord(target, None, closed=True))
        return self._swap(target, current, closed)

    def settle(self, target: Target) -> bool:
        """Persist affirmative proof against the exact closed record observed.

        A competing CAS can make this call return False even after settlement.
        Use startup_settled for the durable verdict.
        """
        current = self._read(target)
        if current is None or not current.closed:
            return False
        if current.settled_by is not None:
            return True
        try:
            evidence = self._observe_settlement(current)
        except Exception:
            raise TargetError("pre_startup_evidence_unconfirmed") from None
        if (type(evidence) is not SettlementEvidence or type(evidence.closed) is not AdmissionRecord
                or evidence.closed.closed is not True or evidence.closed != current
                or type(evidence.source) is not SettlementSource
                or evidence.startup_drained is not True or evidence.restart_fenced is not True):
            return False
        return self._swap(target, current, replace(current, settled_by=evidence.source))

    def startup_settled(self, target: Target) -> bool:
        """Supply only durable startup settlement to the cleanup inventory."""
        record = self._read(target)
        return record is not None and record.settled_by is not None
