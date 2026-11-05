"""SYNTHETIC minimal work-inventory/drain claims, not a drain, fence or quiescence proof.

Built on lifecycle_fixture(). Two fabricated work surfaces, one fabricated
external-fence claim, one drain claim, three all-zero snapshots, one telemetry
window claim and the few identity records the narrow checker resolves (authority
source, controller consumer, observer, observer job, producer, resource owner,
inventory manifest). Nothing is authenticated or independently observed, and
the inventory is NOT shown to be complete. The restoration authority,
verification, postcondition, readiness, snapshot, compatibility and artifact
references deliberately have NO record: the checker must not consume them, so
this stays honestly PARTIAL and a complete packet must still refuse them.

Timeline additions (2026-01-01 UTC, on top of the lifecycle timeline): baseline
snapshot 00:00:45, drain 00:20:00-00:20:10 (starts exactly at observation end),
drained snapshot 00:20:10, cleanup write 00:20:30, final snapshot 00:30:00
(attempt end), observer job 00:00:00-00:35:00, now 00:40.
"""

import copy

from announcement_lifecycle_fixtures import NOW, lifecycle_fixture
from announcement_provenance_fixtures import at, envelope
from announcement_runtime_evidence import canonical_digest

INVENTORY = "runtime-work-inventory"
SURFACES = ("surface-feeds", "surface-queue")
OBSERVER, CONTROLLER, OWNER_SUBJECT, WRITER = "d" * 8, "c" * 8, "e" * 8, "f" * 8
SNAPSHOTS = ("queue-baseline-ref", "drained-snapshot-ref", "queue-final-ref")


def principal(ref, digit):
    return {"ref": ref, "subject_id": digit * 8 + "-0000-4000-8000-000000000000",
            "identity_root_ref": "root-ref"}


def surface(ref, path):
    return {"ref": ref, "kind": "job", "source": "candidate", "path": path,
            "blob": "4" * 40, "sha256": "5" * 64, "disposition": "selected"}


def snapshot(clock, inventory_sha256, counts=None):
    counts = counts or {}
    return {"inventory_sha256": inventory_sha256, "observer_ref": "observer-ref",
            "enforcement_epoch": "9", "observed_at": at(clock),
            "surfaces": [{"surface_ref": ref, "queued": 0, "in_flight": 0, "claimed": 0,
                          "delayed": 0, **counts.get(ref, {})} for ref in SURFACES],
            "outcome": "observed"}


def repin_drain(receipt, records):
    """Refresh ONLY derived copies after a coherent inventory/effect edit.

    Semantic fields (surfaces, counts, outcomes, owners, times, observers) are
    never touched, so a mutation reaches its semantic predicate and is not
    masked by a stale digest or coverage copy.
    """
    digest = canonical_digest(records["runtime-work-inventory"]["data"])
    manifest = receipt["resources"]["inventory_manifest"]
    manifest["sha256"] = digest
    records[manifest["ref"]]["data"]["sha256"] = digest
    records["work-fence-ref"]["data"]["inventory_sha256"] = digest
    for ref in SNAPSHOTS:
        records[ref]["data"]["inventory_sha256"] = digest
    effects = receipt["observations"]["actual_effects"]
    coverage = receipt["observations"]["coverage"][0]
    coverage.update(source_inventory_sha256=digest, last_sequence=len(effects))
    records[coverage["ref"]]["data"] = {
        "claim": copy.deepcopy(coverage), "operation_ids": [x["operation_id"] for x in effects]}


def drain_fixture(prior="absent"):
    """Return (receipt, {ref: sidecar}, now). Fresh deep objects each call."""
    receipt, records, now = lifecycle_fixture(prior)
    attempt, auth, life = receipt["attempt"], receipt["authority"], receipt["lifecycle"]
    scope, control = receipt["scope"], receipt["control"]
    resources = receipt["resources"]

    def add(ref, kind, data):
        records[ref] = envelope(ref, kind, copy.deepcopy(data))

    inventory = {"candidate": copy.deepcopy(scope["candidate"]),
                 "control": copy.deepcopy(control["version"]),
                 "source_manifest": copy.deepcopy(scope["source_manifest"]),
                 "surfaces": [surface(SURFACES[0], "src/jobs/feeds.rs"),
                              surface(SURFACES[1], "src/queue/worker.rs")]}
    digest = canonical_digest(inventory)
    resources["inventory_manifest"] = {"ref": "inventory-ref", "revision": "3" * 40,
                                       "sha256": digest}
    add("inventory-ref", "manifest", {"revision": "3" * 40, "sha256": digest})
    add(INVENTORY, "work_inventory_content", inventory)

    attempt.update(enforcement_epoch="9")
    coverage = receipt["observations"]["coverage"][0]
    coverage.update(run=copy.deepcopy(attempt["run"]), started_at=attempt["started_at"],
                    ended_at=attempt["ended_at"], first_sequence=1, last_sequence=4,
                    enforcement_epoch="9", source_inventory_sha256=digest,
                    wire_receipt_refs=[], platform_receipt_refs=[],
                    transaction_receipt_refs=[], result="complete")
    add(coverage["ref"], "telemetry_window", {"claim": coverage, "operation_ids": [
        x["operation_id"] for x in receipt["observations"]["actual_effects"]]})

    refs = ["principal-" + x for x in ("author", "executor", "caller", "security", "qa",
                                       "controller", "verifier")]
    add(auth["source_ref"], "authority_source", {
        "repository": scope["repository"], "policy": auth["policy"],
        "principal_mapping_sha256": "6" * 64, "principal_refs": refs,
        "controller_ref": "controller-consumer", "verifier_ref": "verifier-consumer",
        "installation_mapping_ref": "mapping-ref", "history_ref": "history-ref",
        "stage_bindings": [{"purpose": purpose, "role": role, "principal_ref": "principal-" + role,
                            "stage_id": str(n) * 8 + "-2222-4222-8222-222222222222",
                            "participant_id": str(n) * 8 + "-3333-4333-8333-333333333333"}
                           for n, (purpose, role) in enumerate((
                               ("runtime", "security"), ("runtime", "qa"),
                               ("installation", "security"), ("installation", "qa"),
                               ("recovery", "security"), ("recovery", "qa")), 1)]})
    add("controller-consumer", "authority_consumer", {
        "principal": principal("principal-controller", CONTROLLER[0]), "role": "controller",
        "provisioning_ref": "controller-provisioning"})
    add("observer-ref", "observer", {
        "principal": principal("principal-observer", OBSERVER[0]), "job_ref": "job-ref",
        "provisioning_ref": "observer-provisioning"})
    add("producer-ref", "producer", {
        "principal": principal("principal-observer", OBSERVER[0]),
        "workflow": copy.deepcopy(control["workflow"]),
        "repository": "TogetherWeOwn/two-bot-next"})
    add("job-ref", "job", {"id": "77", "producer_ref": "producer-ref",
        "source": "external_observer", "head": control["version"]["commit"],
        "conclusion": "success", "started_at": at("00:00:00"), "ended_at": at("00:35:00")})
    add("owner-ref", "resource_owner", {
        "principal": principal("principal-owner", OWNER_SUBJECT[0]),
        "execution_ref": auth["execution"]["ref"], "nonce_ref": auth["execution"]["nonce_ref"]})

    add("work-fence-ref", "work_fence", {
        "source_ref": auth["source_ref"], "execution_ref": auth["execution"]["ref"],
        "owner_ref": "controller-consumer", "nonce_ref": auth["execution"]["nonce_ref"],
        "authority_epoch": auth["epoch"], "resource_epoch": "9", "inventory_sha256": digest,
        "closed_at": attempt["observation_ended_at"], "held_until": attempt["ended_at"],
        "boundary": "external_all_paths", "outcome": "closed"})
    add(life["drain_ref"], "work_drain", {
        "inventory_ref": INVENTORY, "coverage_ref": coverage["ref"],
        "fence_ref": "work-fence-ref", "drained_snapshot_ref": "drained-snapshot-ref",
        "started_at": attempt["observation_ended_at"], "ended_at": at("00:20:10"),
        "unknown_operation_refs": [], "outcome": "resolved"})
    for ref, clock in zip(SNAPSHOTS, ("00:00:45", "00:20:10", "00:30:00")):
        add(ref, "work_snapshot", snapshot(clock, digest))
    return receipt, records, copy.copy(NOW)
