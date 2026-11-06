"""SYNTHETIC authority claims. No real root, decision, nonce or fence exists here.

These explicitly written claims exercise only graph consistency. The fixture
remains deliberately incomplete for actual resource/effect/lifecycle acceptance.
"""

import copy
from datetime import datetime, timezone

from announcement_provenance_fixtures import at, envelope, provenance_fixture, run
from announcement_runtime_evidence import canonical_digest


ROLES = ("author", "executor", "caller", "security", "qa", "controller", "verifier")
CLOCKS = ("00:00:10", "00:02:00", "00:03:00", "00:08:00", "00:08:00", "00:24:00")
PHASES = ("credentials", "setup", "rollout", "transition", "effect_batch", "final")


def principal(role):
    digit = str(ROLES.index(role) + 1)
    return {"ref": "principal-" + role,
            "subject_id": digit * 8 + "-0000-4000-8000-000000000000",
            "identity_root_ref": "native-mapping-ref"}


def decision(role, scope_digest, *, installation=False, recovery=False):
    digit = (("e" if role == "security" else "f") if recovery else
             ("a" if role == "security" else "b") if installation else
             ("c" if role == "security" else "d"))
    prefix = "recovery-" if recovery else "install-" if installation else "runtime-"
    sec = ("e" if recovery else "a" if installation else "c") * 8 + "-1111-4111-8111-111111111111"
    return {"ref": prefix + role, "role": role,
            "decision_id": digit * 8 + "-1111-4111-8111-111111111111",
            "stage_id": digit * 8 + "-2222-4222-8222-222222222222",
            "participant_id": digit * 8 + "-3333-4333-8333-333333333333", "round": 2,
            "principal": principal(role), "scope_sha256": scope_digest,
            "epoch": "11" if recovery else "3" if installation else "7", "security_decision_id": sec,
            "outcome": "approved",
            "decided_at": "2025-12-31T23:" + ("41:00" if role == "security" else "42:00") + "Z"}


def authority_fixture():
    receipt, records, _ = provenance_fixture()
    # Nonempty schema placeholders are not actual ordinary effects or cleanup.
    # Keep them disjoint so this partial graph does not select recovery checks.
    for phase in ("setup", "cleanup"):
        receipt["lifecycle"][phase][0]["operation_id"] = "unresolved-" + phase + "-operation"
    a, control, attempt = receipt["authority"], receipt["control"], receipt["attempt"]

    def add(ref, kind, data):
        records[ref] = envelope(ref, kind, copy.deepcopy(data))

    for role in ("author", "executor", "caller"):
        a[role] = principal(role)
    attempt["actor"] = principal("caller")
    a.update(source_ref="authority-source", epoch="7")
    a["execution"].update(ref="execution-ref", nonce_ref="one-shot-nonce",
                          consumption_ref="consumption-ref")
    attempt["enforcement_epoch"] = "9"
    implementation = {"ref": "implementation-ref", "revision": control["installation"]["head"]["commit"],
                      "sha256": "6" * 64}
    inventory = {"ref": "inventory-ref", "revision": control["version"]["commit"], "sha256": "7" * 64}
    a["implementation"] = copy.deepcopy(implementation)
    receipt["resources"]["inventory_manifest"] = copy.deepcopy(inventory)
    for manifest in (implementation, inventory):
        add(manifest["ref"], "manifest", {k: v for k, v in manifest.items() if k != "ref"})
    refs = ["principal-" + role for role in ROLES]
    bindings = [{"principal": principal(role), "roles": [role]} for role in ROLES]
    for ref, binding in zip(refs, bindings):
        add(ref, "principal_binding", binding)
    consumers = {}
    for role in ("controller", "verifier"):
        consumers[role] = {"principal": principal(role), "role": role,
                           "provisioning_ref": role + "-provisioning"}
        add(role + "-consumer", "authority_consumer", consumers[role])
    stages = []
    for purpose in ("runtime", "installation", "recovery"):
        for role in ("security", "qa"):
            d = decision(role, "0" * 64, installation=purpose == "installation",
                         recovery=purpose == "recovery")
            stages.append({"purpose": purpose, "role": role, "principal_ref": "principal-" + role,
                           "stage_id": d["stage_id"], "participant_id": d["participant_id"]})
    source = {"repository": receipt["scope"]["repository"], "policy": a["policy"],
              "principal_mapping_sha256": canonical_digest(bindings), "principal_refs": refs,
              "controller_ref": "controller-consumer", "verifier_ref": "verifier-consumer",
              "installation_mapping_ref": control["installation"]["mapping_ref"],
              "history_ref": "runtime-history-5", "stage_bindings": stages}
    add("authority-source", "authority_source", source)
    install_refs = ["installation-approval-" + str(i) for i in range(6)]
    a["installation_approval_refs"] = install_refs[:]
    control["installation"]["authority_receipt_refs"] = install_refs[:]
    fixture_keys = ("feed_ref", "item_ref", "setting_ref", "rsvp_ref", "nonce_ref",
                    "source_content_sha256", "item_content_sha256", "interaction_plan_ref",
                    "scheduler_plan_ref")
    # Literal field inventories, not auto-generated from production schemas.
    budgets = {"entries": receipt["resources"]["entries"],
               "fixtures": {k: receipt["fixtures"][k] for k in fixture_keys}}
    a["execution"]["budgets_sha256"] = canonical_digest(budgets)
    scope = {"scope": receipt["scope"], "control": control, "policy": a["policy"],
             "source_sha256": canonical_digest(source), "consumers_sha256": canonical_digest(consumers),
             "implementation": implementation, "resource_plan": a["resource_plan"],
             "identity_map": receipt["resources"]["identity_map"], "inventory_manifest": inventory,
             "components_manifest": receipt["build"]["components_manifest"],
             "image_digest": receipt["build"]["image_digest"],
             "budgets_sha256": canonical_digest(budgets), "epoch": "7", "resource_epoch": "9",
             "principal_refs": refs}
    add("runtime-authority-scope", "authority_scope", scope)
    a["scope_sha256"] = canonical_digest(scope)
    a["decisions"] = [decision(role, a["scope_sha256"]) for role in ("security", "qa")]
    for d in a["decisions"]:
        add(d["ref"], "authority_decision", {"claim": d, "source_ref": "authority-source",
            "security_round": 2, "generation": "8"})
    add("execution-ref", "execution_authority", {"claim": a["execution"],
        "source_ref": "authority-source", "scope_sha256": a["scope_sha256"], "epoch": "7",
        "resource_epoch": "9", "generation": "8", "installation_approval_refs": install_refs})
    add("consumption-ref", "nonce_consumption", {"source_ref": "authority-source",
        "execution_ref": "execution-ref", "nonce_ref": "one-shot-nonce", "run": run(),
        "owner_ref": "principal-controller", "consumed_at": at("00:00:01"),
        "authority_epoch": "7", "resource_epoch": "9", "outcome": "consumed"})
    a["reads"] = []
    attempt["checkpoints"] = []

    def history_and_reads(prefix, i, clock, scope_digest, decisions, epoch, generation):
        history_ref = prefix + "-history-" + str(i)
        history = {"source_ref": "authority-source", "scope_sha256": scope_digest,
            "epoch": epoch, "resource_epoch": "9", "generation": generation,
            "effective_decision_refs": [x["ref"] for x in decisions],
            "started_at": "2025-12-31T23:00:00Z", "ended_at": at(clock),
            "first_sequence": 1, "last_sequence": 1, "coverage": "complete",
            "events": [{"sequence": 1, "kind": "push", "observed_at": "2025-12-31T23:40:00Z",
                        "scope_sha256": "0" * 64, "invalidated_decision_refs": ["old-decision"]}]}
        add(history_ref, "authority_history", history)
        reads = []
        for role in ("controller", "verifier"):
            ref = prefix + "-read-" + role + "-" + str(i)
            read = {"ref": ref, "consumer": role, "source_ref": "authority-source",
                    "scope_sha256": scope_digest, "epoch": epoch, "resource_epoch": "9",
                    "history_sha256": canonical_digest(history), "read_at": at(clock),
                    "effective_decision_refs": [x["ref"] for x in decisions]}
            reads.append(read)
            add(ref, "authority_read", {"claim": read, "history_ref": history_ref,
                                       "consumer_ref": role + "-consumer"})
        return history_ref, reads

    for i, (phase, clock) in enumerate(zip(PHASES, CLOCKS)):
        _, reads = history_and_reads("runtime", i, clock, a["scope_sha256"], a["decisions"], "7", "8")
        a["reads"].extend(copy.deepcopy(reads))
        checkpoint = {"ref": "checkpoint-" + str(i), "phase": phase,
            "controller_read_ref": reads[0]["ref"], "verifier_read_ref": reads[1]["ref"],
            "checked_at": at(clock), "authority_epoch": "7", "resource_epoch": "9",
            "mutation_fence_ref": "fence-" + str(i)}
        attempt["checkpoints"].append(checkpoint)
        # Thirty-second validity; this records a claim, never creates a fence.
        until = at(clock[:-2] + "30") if clock.endswith("00") else at("00:00:40")
        add("fence-" + str(i), "mutation_fence", {"source_ref": "authority-source",
            "execution_ref": "execution-ref", "checkpoint_ref": checkpoint["ref"], "run": run(),
            "phase": phase, "authority_epoch": "7", "resource_epoch": "9",
            "nonce_ref": "one-shot-nonce", "consumption_ref": "consumption-ref",
            "observed_at": at(clock), "valid_until": until, "boundary": "external_atomic"})
        install = {"source_ref": "authority-source", "mapping_ref": control["installation"]["mapping_ref"],
            "policy": a["policy"], "implementation": implementation, "control": control["version"],
            "epoch": "3"}
        digest = canonical_digest(install)
        ds = [decision(role, digest, installation=True) for role in ("security", "qa")]
        history_ref, ir = history_and_reads("install", i, clock, digest, ds, "3", "4")
        install.update(scope_sha256=digest, decisions=ds, security_round=2, history_ref=history_ref,
                       controller_read_ref=ir[0]["ref"], verifier_read_ref=ir[1]["ref"],
                       checked_at=at(clock))
        add(install_refs[i], "installation_approval", install)
    return receipt, records, datetime(2026, 1, 1, 0, 24, 30, tzinfo=timezone.utc)
