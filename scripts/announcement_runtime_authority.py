"""Closed offline authority-claim graph; NEVER an authenticated trust root.

All inputs remain untrusted claims. This module neither acquires current native
records nor consumes a nonce or enforces a mutation fence. Even a fully coherent
fabrication cannot enable runtime_preflight(), which remains unsupported.
"""

from datetime import datetime, timedelta, timezone

from announcement_runtime_evidence import EvidenceIndex, canonical_digest
from announcement_runtime_policy import check_authority_claims, check_lease
from announcement_runtime_schema import require, utc, validate_receipt_shape


SCOPE_REF = "runtime-authority-scope"
CHECKPOINT_PHASES = ("credentials", "setup", "rollout", "transition", "effect_batch", "final")


def _unique(values, code):
    require(len(values) == len(set(values)), code)


def _mutation_reads_current(checkpoint, installation, evidence, observed):
    # Scope/history/consumer binding is checked by the authority graph first.
    # A fresh checkpoint plus a valid fence cannot extend either read's age.
    return all(timedelta(0) <= observed - utc(evidence.resolve(
        record[role + "_read_ref"], "authority_read")["claim"]["read_at"]) <= timedelta(seconds=60)
        for record in (checkpoint, installation) for role in ("controller", "verifier"))


def _db_checkpoint_phase(receipt, effect, claim):
    """Resolve setup and in-observation setting writes, never admit recovery.

    This partial classifier does not prove ledger completeness. The mandatory
    lifecycle phase checker still rejects unsupported cleanup and unassigned
    startup/drain/restoration effects.
    """
    setup = [entry for entry in receipt["lifecycle"]["setup"]
             if entry["operation_id"] == effect["operation_id"]]
    if not setup:
        resources = [entry for entry in receipt["resources"]["entries"]
                     if entry["ref"] == effect["resource_ref"]]
        require(len(resources) == 1, "lifecycle_resource")
        if (resources[0]["kind"] == "setting" and
                utc(receipt["attempt"]["observation_started_at"]) <= utc(effect["observed_at"]) <
                utc(receipt["attempt"]["observation_ended_at"])):
            return "transition"
        return "effect_batch"
    require(len(setup) == 1 and not any(entry["operation_id"] == effect["operation_id"]
            for entry in receipt["lifecycle"]["cleanup"]), "lifecycle_operation")
    require(effect["method"] in ("db_insert", "db_cas") and claim == setup[0],
            "lifecycle_transition")
    require(utc(receipt["lifecycle"]["served_before"]["observed_at"]) <=
            utc(effect["observed_at"]) <= utc(receipt["rollout"]["started_at"]),
            "lifecycle_operation_time")
    return "setup"


def budget_claims(receipt):
    """The exact finite plan being compared, not permission for its effects."""
    fixtures = receipt["fixtures"]
    return {"entries": receipt["resources"]["entries"],
            "fixtures": {key: fixtures[key] for key in (
                "feed_ref", "item_ref", "setting_ref", "rsvp_ref", "nonce_ref",
                "source_content_sha256", "item_content_sha256", "interaction_plan_ref",
                "scheduler_plan_ref")}}


def installation_scope(approval):
    """Reusable installed-code scope excludes runtime candidate/run/resources."""
    return {key: approval[key] for key in (
        "source_ref", "mapping_ref", "policy", "implementation", "control", "epoch")}


def _decisions(decisions, scope, epoch, principals, checked, stage_bindings, purpose):
    require([x["role"] for x in decisions] == ["security", "qa"], "decision_order")
    security, qa = decisions
    for field in ("ref", "decision_id", "stage_id", "participant_id"):
        _unique([x[field] for x in decisions], "decision_identity")
    require(security["decision_id"] == security["security_decision_id"] ==
            qa["security_decision_id"], "security_round_binding")
    require(utc(security["decided_at"]) < utc(qa["decided_at"]) <= checked,
            "decision_order")
    for decision in decisions:
        require(decision["principal"] == principals[decision["role"]] and
                decision["scope_sha256"] == scope and decision["epoch"] == epoch,
                "decision_scope")
        binding = stage_bindings[(purpose, decision["role"])]
        require(binding["principal_ref"] == decision["principal"]["ref"] and
                binding["stage_id"] == decision["stage_id"] and
                binding["participant_id"] == decision["participant_id"], "decision_stage")


def check_authority_graph_claims(receipt, evidence, now):
    """Resolve authority claims and reject inconsistent scope/history/fence data.

    This is not the independent verifier and never returns a runtime capability.
    A future approved adapter must authenticate every claimed principal, source,
    complete history and boundary atomically outside candidate/caller control.
    No endpoint or provisioning reference from this graph selects that adapter.
    Other receipt predicates and final all-used accounting remain mandatory.
    """
    validate_receipt_shape(receipt)
    require(type(evidence) is EvidenceIndex, "evidence_index")
    require(type(now) is datetime and now.tzinfo is timezone.utc, "clock")
    authority, attempt, control = receipt["authority"], receipt["attempt"], receipt["control"]
    run = attempt["run"]
    evidence.require_run(run)
    start, expiry = check_lease(authority["execution"]["not_before"],
                               authority["execution"]["expires_at"], now)
    attempt_start, attempt_end = utc(attempt["started_at"]), utc(attempt["ended_at"])
    require(start <= attempt_start < attempt_end <= now < expiry, "authority_window")
    require(authority["execution"]["run"] == run and
            run["control_sha"] == control["version"]["commit"] and
            attempt["actor"] == authority["caller"], "authority_attempt")
    source_ref = authority["source_ref"]
    source = evidence.resolve(source_ref, "authority_source")
    require(source["repository"] == receipt["scope"]["repository"] and
            source["policy"] == authority["policy"] and
            source["installation_mapping_ref"] == control["installation"]["mapping_ref"],
            "authority_source")
    stage_bindings = {(x["purpose"], x["role"]): x for x in source["stage_bindings"]}
    require(set(stage_bindings) == {(purpose, role) for purpose in ("runtime", "installation")
                                    for role in ("security", "qa")}, "stage_mapping")
    for key in ("stage_id", "participant_id"):
        _unique([x[key] for x in stage_bindings.values()], "stage_mapping")
    _unique(source["principal_refs"], "principal_mapping")
    bindings = [evidence.resolve(ref, "principal_binding") for ref in source["principal_refs"]]
    principals = {}
    for ref, binding in zip(source["principal_refs"], bindings):
        require(binding["principal"]["ref"] == ref and len(binding["roles"]) == 1,
                "principal_mapping")
        role = binding["roles"][0]
        require(role not in principals, "principal_mapping")
        principals[role] = binding["principal"]
    require(set(principals) == {"author", "executor", "caller", "security", "qa",
                                "controller", "verifier"}, "principal_mapping")
    _unique([x["subject_id"] for x in principals.values()], "principal_independence")
    require(canonical_digest(bindings) == source["principal_mapping_sha256"], "principal_mapping")
    for role in ("author", "executor", "caller"):
        require(authority[role] == principals[role], "principal_mapping")
    consumers = {}
    for role in ("controller", "verifier"):
        consumer = evidence.resolve(source[role + "_ref"], "authority_consumer")
        require(consumer["role"] == role and consumer["principal"] == principals[role],
                "consumer_independence")
        consumers[role] = consumer
    require(consumers["controller"]["provisioning_ref"] !=
            consumers["verifier"]["provisioning_ref"], "consumer_independence")

    for claim in (authority["policy"], authority["resource_plan"], receipt["scope"]["contract"],
                  receipt["resources"]["plan"], receipt["resources"]["identity_map"]):
        evidence.bind(claim, "document")
    evidence.bind(authority["implementation"], "manifest")
    evidence.bind(receipt["resources"]["inventory_manifest"], "manifest")
    require(authority["implementation"]["revision"] == control["installation"]["head"]["commit"]
            and authority["resource_plan"] == receipt["resources"]["plan"], "authority_scope")
    resource_epoch = attempt["enforcement_epoch"]
    scope = evidence.resolve(SCOPE_REF, "authority_scope")
    expected_scope = {
        "scope": receipt["scope"], "control": control, "policy": authority["policy"],
        "source_sha256": canonical_digest(source), "consumers_sha256": canonical_digest(consumers),
        "implementation": authority["implementation"], "resource_plan": authority["resource_plan"],
        "identity_map": receipt["resources"]["identity_map"],
        "inventory_manifest": receipt["resources"]["inventory_manifest"],
        "components_manifest": receipt["build"]["components_manifest"],
        "image_digest": receipt["build"]["image_digest"],
        "budgets_sha256": canonical_digest(budget_claims(receipt)),
        "epoch": authority["epoch"], "resource_epoch": resource_epoch,
        "principal_refs": source["principal_refs"],
    }
    require(scope == expected_scope and canonical_digest(scope) == authority["scope_sha256"]
            and scope["budgets_sha256"] == authority["execution"]["budgets_sha256"],
            "authority_scope")
    checkpoints = attempt["checkpoints"]
    _unique([x["ref"] for x in checkpoints], "checkpoint_identity")
    phases = [x["phase"] for x in checkpoints]
    require(set(phases) == set(CHECKPOINT_PHASES) and
            phases[0] == "credentials" and phases[-1] == "final" and
            phases.count("credentials") == phases.count("setup") == phases.count("rollout") ==
            phases.count("final") == 1 and
            [CHECKPOINT_PHASES.index(x) for x in phases[:3]] == [0, 1, 2],
            "checkpoint_inventory")
    require([x["checked_at"] for x in checkpoints] == sorted(x["checked_at"] for x in checkpoints),
            "checkpoint_order")
    require(utc(checkpoints[2]["checked_at"]) <= utc(receipt["rollout"]["started_at"]),
            "checkpoint_order")
    require(checkpoints[-1]["checked_at"] == attempt["ended_at"], "final_checkpoint")

    decisions = authority["decisions"]
    _decisions(decisions, authority["scope_sha256"], authority["epoch"], principals,
               utc(checkpoints[0]["checked_at"]), stage_bindings, "runtime")
    decision_records = []
    for decision in decisions:
        record = evidence.resolve(decision["ref"], "authority_decision")
        require(record["claim"] == decision and record["source_ref"] == source_ref and
                record["security_round"] == decisions[0]["round"], "decision_binding")
        decision_records.append(record)
    require(decision_records[0]["generation"] == decision_records[1]["generation"],
            "authority_generation")
    generation = decision_records[0]["generation"]
    execution = authority["execution"]
    grant = evidence.resolve(execution["ref"], "execution_authority")
    require(grant == {"claim": execution, "source_ref": source_ref,
        "scope_sha256": authority["scope_sha256"], "epoch": authority["epoch"],
        "resource_epoch": resource_epoch, "generation": generation,
        "installation_approval_refs": authority["installation_approval_refs"]}, "execution_binding")
    consumed = evidence.resolve(execution["consumption_ref"], "nonce_consumption")
    require(consumed == {"source_ref": source_ref, "execution_ref": execution["ref"],
        "nonce_ref": execution["nonce_ref"], "run": run,
        "owner_ref": principals["controller"]["ref"], "consumed_at": consumed["consumed_at"],
        "authority_epoch": authority["epoch"], "resource_epoch": resource_epoch,
        "outcome": "consumed"}, "nonce_binding")
    require(start <= utc(consumed["consumed_at"]) <= utc(checkpoints[0]["checked_at"]),
            "nonce_time")

    histories = {}
    used_reads = set()

    def read_claim(ref, expected, checked, digest, epoch, approved, expected_generation=None,
                   freshness_at=None):
        record = evidence.resolve(ref, "authority_read")
        read = record["claim"]
        require(read["ref"] == ref and (expected is None or read == expected) and
                record["consumer_ref"] == source[read["consumer"] + "_ref"], "read_binding")
        require(read["source_ref"] == source_ref and read["scope_sha256"] == digest and
                read["epoch"] == epoch and read["resource_epoch"] == resource_epoch and
                read["effective_decision_refs"] == [x["ref"] for x in approved], "read_scope")
        freshness_at = checked if freshness_at is None else freshness_at
        require(start <= utc(read["read_at"]) <= checked <= freshness_at and
                freshness_at - utc(read["read_at"]) <= timedelta(seconds=60), "stale_read")
        history = evidence.resolve(record["history_ref"], "authority_history")
        require(canonical_digest(history) == read["history_sha256"] and
                history["source_ref"] == source_ref and history["scope_sha256"] == digest and
                history["epoch"] == epoch and history["resource_epoch"] == resource_epoch and
                history["effective_decision_refs"] == read["effective_decision_refs"] and
                history["ended_at"] == read["read_at"] and history["coverage"] == "complete",
                "authority_history")
        if expected_generation is not None:
            require(history["generation"] == expected_generation, "authority_generation")
        events = history["events"]
        require(history["first_sequence"] == 1 and history["last_sequence"] == len(events) and
                [x["sequence"] for x in events] == list(range(1, len(events) + 1)),
                "authority_history_gap")
        lower, upper = utc(history["started_at"]), utc(history["ended_at"])
        require(lower < utc(approved[0]["decided_at"]) and
                all(utc(decision["decided_at"]) <= upper for decision in approved),
                "authority_history_time")
        times = [x["observed_at"] for x in events]
        require(times == sorted(times), "authority_history_time")
        for event in events:
            require(lower <= utc(event["observed_at"]) < utc(approved[0]["decided_at"]),
                    "invalidated_authority")
            _unique(event["invalidated_decision_refs"], "authority_history")
            require(not set(event["invalidated_decision_refs"]).intersection(
                    x["ref"] for x in approved), "invalidated_authority")
        histories.setdefault(digest, []).append(history)
        return read, record["history_ref"]

    reads = {x["ref"]: x for x in authority["reads"]}
    require(len(reads) == len(authority["reads"]), "read_identity")
    install_refs = authority["installation_approval_refs"]
    _unique(install_refs, "installation_approval")
    require(install_refs == control["installation"]["authority_receipt_refs"] and
            len(install_refs) == len(checkpoints), "installation_approval")
    mapping = evidence.resolve(control["installation"]["mapping_ref"], "installation_mapping")
    require(mapping == {"pr": control["installation"]["pr"], "head": control["installation"]["head"],
            "squash": control["installation"]["squash"], "control": control["version"]} and
            mapping["squash"] == control["version"], "installation_mapping")
    final_histories = []
    fence_refs = []
    fences = []
    installation_decisions = None
    for checkpoint, install_ref in zip(checkpoints, install_refs):
        checked = utc(checkpoint["checked_at"])
        require(attempt_start <= checked <= attempt_end, "checkpoint_time")
        freshness_at = now if checkpoint is checkpoints[-1] else checked
        check_authority_claims(authority, checkpoint, freshness_at)
        for role in ("controller", "verifier"):
            ref = checkpoint[role + "_read_ref"]
            read, history_ref = read_claim(ref, reads.get(ref), checked, authority["scope_sha256"],
                                          authority["epoch"], decisions, generation,
                                          freshness_at=freshness_at)
            require(ref in reads and read["consumer"] == role and ref not in used_reads,
                    "consumer_read")
            used_reads.add(ref)
            if checkpoint is checkpoints[-1]:
                final_histories.append(history_ref)
        fence = evidence.resolve(checkpoint["mutation_fence_ref"], "mutation_fence")
        fence_refs.append(checkpoint["mutation_fence_ref"])
        require(fence == {"source_ref": source_ref, "execution_ref": execution["ref"],
            "checkpoint_ref": checkpoint["ref"], "run": run, "phase": checkpoint["phase"],
            "authority_epoch": authority["epoch"], "resource_epoch": resource_epoch,
            "nonce_ref": execution["nonce_ref"], "consumption_ref": execution["consumption_ref"],
            "observed_at": checkpoint["checked_at"], "valid_until": fence["valid_until"],
            "boundary": "external_atomic"}, "mutation_fence")
        require(checked < utc(fence["valid_until"]) <= min(expiry, checked + timedelta(seconds=60)),
                "mutation_fence_time")
        install = evidence.resolve(install_ref, "installation_approval")
        require(install["source_ref"] == source_ref and install["mapping_ref"] ==
                control["installation"]["mapping_ref"] and install["policy"] == authority["policy"]
                and install["implementation"] == authority["implementation"] and
                install["control"] == control["version"] and install["checked_at"] ==
                checkpoint["checked_at"] and
                install["scope_sha256"] == canonical_digest(installation_scope(install)),
                "installation_approval")
        _decisions(install["decisions"], install["scope_sha256"], install["epoch"], principals,
                   checked, stage_bindings, "installation")
        require(install["security_round"] == install["decisions"][0]["round"],
                "security_round_binding")
        if installation_decisions is None:
            installation_decisions = install["decisions"]
        require(install["decisions"] == installation_decisions, "installation_decision_drift")
        require(not {x["decision_id"] for x in decisions}.intersection(
                x["decision_id"] for x in install["decisions"]), "installation_runtime_separation")
        install_reads = []
        for role in ("controller", "verifier"):
            read, history_ref = read_claim(install[role + "_read_ref"], None, checked,
                install["scope_sha256"], install["epoch"], install["decisions"],
                freshness_at=freshness_at)
            require(read["consumer"] == role and history_ref == install["history_ref"],
                    "installation_read")
            install_reads.append(read)
        require(install_reads[0]["history_sha256"] == install_reads[1]["history_sha256"],
                "authority_disagreement")
        fences.append((checkpoint, fence, install))
    def mutation_checkpoint(phase, timestamp, named_ref=None):
        observed = utc(timestamp)
        candidates = [(checkpoint, fence, install) for checkpoint, fence, install in fences
                      if checkpoint["phase"] == phase and
                      (named_ref is None or checkpoint["ref"] == named_ref)]
        if named_ref is not None:
            require(candidates, "transition_authority")
        matching = [(checkpoint, install) for checkpoint, fence, install in candidates
                    if utc(fence["observed_at"]) <= observed < utc(fence["valid_until"])]
        require(matching, "mutation_without_checkpoint" if named_ref is None else
                "transition_authority_time")
        require(any(_mutation_reads_current(checkpoint, install, evidence, observed)
                    for checkpoint, install in matching), "stale_mutation_read")

    mutation_checkpoint("rollout", receipt["rollout"]["started_at"])
    for effect in receipt["observations"]["actual_effects"]:
        named_ref = None
        phase = "effect_batch"
        # Missing transition evidence still refuses in the telemetry/state-chain
        # layers. When supplied here it cannot be ignored or borrow another fence.
        if (effect["method"].startswith("db_") and effect["method"] != "db_read" and
                effect["db_transition_refs"]):
            require(len(effect["db_transition_refs"]) == 1, "mutation_transition_inventory")
            transition = evidence.resolve(effect["db_transition_refs"][0], "db_transition")
            claim = transition["claim"]
            require(transition["method"] == effect["method"] and
                    claim["operation_id"] == effect["operation_id"] and
                    claim["resource_ref"] == effect["resource_ref"] and
                    claim["observed_at"] == effect["observed_at"], "mutation_transition_binding")
            named_ref = claim["authority_ref"]
            phase = _db_checkpoint_phase(receipt, effect, claim)
        mutation_checkpoint(phase, effect["observed_at"], named_ref)
    _unique(fence_refs, "mutation_fence_replay")
    require(used_reads == reads.keys(), "unreferenced_authority_read")
    require(final_histories == [source["history_ref"], source["history_ref"]], "current_history")
    for snapshots in histories.values():
        snapshots.sort(key=lambda x: x["ended_at"])
        for previous, current in zip(snapshots, snapshots[1:]):
            require(previous["started_at"] == current["started_at"] and
                    previous["generation"] == current["generation"] and
                    previous["events"] == current["events"][:len(previous["events"])] and
                    len(previous["events"]) <= len(current["events"]), "authority_history_rewrite")
            require(all(utc(event["observed_at"]) > utc(previous["ended_at"])
                        for event in current["events"][len(previous["events"]):]),
                    "authority_history_backfill")
