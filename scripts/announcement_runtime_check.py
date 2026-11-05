"""Bounded offline claim-checking CLI; NOT a trusted independent verifier.

Inputs, including the inventory and run identity, are unauthenticated claims.
No root, credential, network, Git child, extraction, output artifact or mutation
adapter is supplied. Even a coherent packet ends at the unconditional runtime
HOLD. This command must never replace candidate or canonical admission.
"""

import argparse
from datetime import datetime, timezone
import json
import os
import stat

from announcement_runtime_archive import RECEIPT_NAME, inspect_archive
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_cases import check_case_effect_claims
from announcement_runtime_drain import check_drain_claims
from announcement_runtime_evidence import EvidenceIndex
from announcement_runtime_feed_exchange import check_feed_exchange_claims
from announcement_runtime_inputs import check_input_graph_claims
from announcement_runtime_interactions import check_interaction_exchange_claims
from announcement_runtime_lifecycle import check_lifecycle_ledger_claims
from announcement_runtime_policy import CAPABILITY_GAPS, runtime_preflight
from announcement_runtime_provenance import check_provenance_claims
from announcement_runtime_resources import check_resource_graph_claims
from announcement_runtime_scheduler import check_scheduler_graph_claims
from announcement_runtime_schema import (
    ARCHIVE_LIMIT, RECEIPT_LIMIT, REFERENCE_LIMIT, RUN, SHA256, Array, Integer,
    Refusal, Text, _validate, parse_bounded_json, require,
)
from announcement_runtime_serialization import check_serialization_claims
from announcement_runtime_settings import check_setting_claims
from announcement_runtime_source_inputs import check_source_input_claims
from announcement_runtime_telemetry import check_telemetry_claims
from announcement_runtime_transitions import check_transition_claims


INVENTORY = {
    "run": RUN,
    "members": Array({"path": Text(r"candidate-receipt\.json|evidence/[a-z][a-z0-9_-]{0,95}\.json"),
                      "size": Integer(1, ARCHIVE_LIMIT), "sha256": SHA256},
                     1, REFERENCE_LIMIT + 1),
}
# Deliberately explicit: adding an evidence kind does not silently add a checker.
# Some checks compose earlier checks; retain each public comparison here so its
# removal from one dependency chain cannot silently omit it at this entry point.
CLAIM_CHECKS = (
    check_provenance_claims, check_authority_graph_claims,
    check_resource_graph_claims, check_input_graph_claims,
    check_scheduler_graph_claims, check_source_input_claims,
    check_telemetry_claims, check_serialization_claims,
    check_transition_claims, check_setting_claims, check_case_effect_claims,
    check_lifecycle_ledger_claims, check_drain_claims, check_interaction_exchange_claims,
    check_feed_exchange_claims,
)


def check_archive_claims(raw, inventory_raw, *, now):
    """Compose existing scoped predicates, never authenticate or grant admission.

    All-used accounting rejects extra sidecars; it does not prove every opaque
    receipt reference is resolved. Topology/service inventories, exact business
    semantics and remaining lifecycle checks are still mandatory missing work.
    A None result means only that these existing offline comparisons agree.
    """
    require(type(now) is datetime and now.tzinfo is timezone.utc, "clock")
    inventory = parse_bounded_json(inventory_raw)
    _validate(inventory, INVENTORY)
    receipt, files = inspect_archive(raw, inventory["members"])
    require(receipt["attempt"]["run"] == inventory["run"], "evidence_attempt")
    require(receipt["result"]["status"] == "pass", "adverse_result")
    evidence = EvidenceIndex({path: data for path, data in files.items()
                              if path != RECEIPT_NAME}, inventory["run"])
    for check in CLAIM_CHECKS:
        check(receipt, evidence, now)
    evidence.require_all_used()


def _read_regular(path, limit):
    # Refuse symlinks, FIFOs, devices and oversized inputs before reading. The
    # descriptor check avoids a stat/open race; NONBLOCK avoids a FIFO open wait.
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    with os.fdopen(descriptor, "rb") as stream:
        metadata = os.fstat(stream.fileno())
        require(stat.S_ISREG(metadata.st_mode) and 0 < metadata.st_size <= limit,
                "input_file")
        raw = stream.read(limit + 1)
    require(0 < len(raw) <= limit, "input_file")
    return raw


class _Parser(argparse.ArgumentParser):
    def error(self, _message):
        # argparse's normal errors echo arbitrary argv, including private paths.
        raise Refusal("cli_arguments")


def _report(reason):
    print(json.dumps({"status": "hold", "runtime_admission": False, "reason": reason,
                      "capability_gaps": [{"capability": gap.capability, "owner": gap.owner}
                                          for gap in CAPABILITY_GAPS]}, sort_keys=True))


def main(argv=None):
    parser = _Parser(prog="announcement-runtime-check", allow_abbrev=False,
                     description="Inspect unauthenticated offline claims; runtime stays HOLD.")
    parser.add_argument("--archive", required=True)
    parser.add_argument("--inventory", required=True)
    try:
        args = parser.parse_args(argv)
        inventory = _read_regular(args.inventory, RECEIPT_LIMIT)
        archive = _read_regular(args.archive, ARCHIVE_LIMIT)
        check_archive_claims(archive, inventory, now=datetime.now(timezone.utc))
    except (Refusal, OSError, ValueError):
        _report("offline_claims_refused")
        return 1
    except Exception:
        # An unexpected implementation failure remains adverse. Never expose a
        # traceback, parser value, private input path or remote body as a report.
        _report("offline_checker_error")
        return 1
    try:
        runtime_preflight()
    except Refusal:
        _report("unsupported_runtime")
        return 1
    except Exception:
        _report("offline_checker_error")
        return 1
    # Defensive even against an accidental future preflight return: this offline
    # CLI has no authority to accept, and must never acquire one implicitly.
    _report("unsupported_runtime")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
