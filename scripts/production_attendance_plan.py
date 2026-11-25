"""Validate read-only PRE evidence for the production attendance release."""

from ci.production_migrate_claim import Refused, validate_read_only_plan
from staging_rollout import GateError, require

# release_schema_transaction.rs requires these six objects to be absent at PRE
# and checks every other object against the role matrix. The candidate's
# unfiltered verifier emits one missing-object finding per absence.
PRE_FINDINGS = frozenset({
    "missing object: public.event_attendance_windows",
    "missing object: public.event_voice_sessions",
    "missing object: public.event_voice_sessions_id_seq",
    "missing object: public.live_sequence_watermarks",
    "missing object: public.voice_heat_hours",
    "missing object: web_v1.voice_heat",
})


def validate_attendance_plan(plan):
    """Refuse invalid PRE evidence with GateError before any writer stop.

    Validate both decoded plans. The caller authenticates the producing
    artifact, binds the request and ledgers, and compares the plans.
    """
    try:
        validate_read_only_plan(plan)
    except Refused:
        raise GateError("production_attendance_plan_invalid") from None

    connections = plan.get("role_verified_connections")
    require(plan.get("migration_target") == "production"
            and type(connections) is int and connections > 0,
            "production_attendance_plan_invalid")
    audit = plan.get("audit")
    require(isinstance(audit, dict) and "error" not in audit, "production_attendance_audit_invalid")
    findings = audit.get("verify_findings")
    require(isinstance(findings, list) and all(isinstance(finding, str) for finding in findings),
            "production_attendance_audit_invalid")
    require(len(findings) == len(PRE_FINDINGS) and set(findings) == PRE_FINDINGS,
            "production_attendance_audit_findings_disallowed")
