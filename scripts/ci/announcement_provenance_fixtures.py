"""Hand-written SYNTHETIC coherent provenance claims for hermetic tests.

Nothing here is live evidence, a root, an authentication or a qualification. The
values are made-up placeholders anchored only to the public source constants.
Claims and sidecars are written out literally (not derived from the schema
specs) so a schema regression cannot silently rewrite them. Agreement between
these claims shows offline consistency only.

Timeline (all 2026-01-01 UTC): lease 00:00-00:40, attempt 00:00-00:24, now 00:25,
before-sample 00:02, build job 00:01-00:02:30, rollout 00:03-00:04, candidate
samples 00:04:30 and 00:14, observation 00:05-00:15, cases 00:06-00:10,
effects 00:08, schema/roles 00:02:40, runtime gates 00:13, after-sample 00:20.
"""

import copy
from datetime import datetime, timezone
import json

import announcement_runtime_fixtures as base
from announcement_runtime_evidence import EvidenceIndex, canonical_digest
from announcement_runtime_source import (
    CANDIDATE, CANDIDATE_TREE, MERGE_BASE, RECORDED_BASE, RECORDED_BASE_TREE,
)

SCHEMA = "two.bot.announcement-evidence.v1"
CONTROL_SHA = "c" * 40
CONTROL_TREE = "e" * 40
SQUASH = {"commit": "f" * 40, "tree": "9" * 40}
OTHER_SHA = "7" * 40
IMAGE = "sha256:" + "d" * 64
RESTORED_IMAGE = "sha256:" + "5" * 64
GATES = ["schema_plan", "roles", "rollout", "health", "readiness", "voice", "help",
         "user_flow"]
NOW = datetime(2026, 1, 1, 0, 25, 0, tzinfo=timezone.utc)

SUBJECTS = {  # author, executor, caller (== actor), candidate producer, observer: all distinct
    "author": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    "executor": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
    "caller": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
    "observer": "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
    "producer": "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee",
}


def at(clock):
    return "2026-01-01T" + clock + "Z"


def run():
    return {"run_id": "9001", "attempt": 1, "control_sha": CONTROL_SHA}


def _principal(ref, role):
    return {"ref": ref, "subject_id": SUBJECTS[role], "identity_root_ref": "root-ref"}


def _doc(ref, digit):
    return {"ref": ref, "revision": "%s-0000-4000-8000-000000000000" % (digit * 8),
            "sha256": str(digit) * 64}


def _manifest(ref, revision, digit):
    return {"ref": ref, "revision": revision, "sha256": str(digit) * 64}


def worker(version_id, path, digit):
    return {"version_id": version_id, "entrypoint": path,
            "compatibility_date": "2026-01-01", "compatibility_flags": ["flag-one"],
            "modules": [{"path": path, "size": 100, "sha256": str(digit) * 64},
                        {"path": "dist/extra.mjs", "size": 50, "sha256": "4" * 64}]}


CANDIDATE_WORKER = worker("33333333-3333-4333-8333-333333333333", "dist/worker.mjs", "3")
RESTORED_WORKER = worker("66666666-6666-4666-8666-666666666666", "dist/restored.mjs", "6")
APPLICATION = "44444444-4444-4444-8444-444444444444"


def sample(ref, clock, *, candidate):
    worker_ = CANDIDATE_WORKER if candidate else RESTORED_WORKER
    return {
        "ref": ref, "run": run(), "external_source_ref": "external-" + ref,
        "observed_at": at(clock),
        "external_revision": CANDIDATE if candidate else RECORDED_BASE,
        "reported_revision": CANDIDATE if candidate else RECORDED_BASE,
        "image_digest": IMAGE if candidate else RESTORED_IMAGE,
        "worker_version_id": worker_["version_id"],
        "modules_sha256": canonical_digest(worker_["modules"]),
        "application_id": APPLICATION, "target_version": "5",
        "build_ref": "build-ref" if candidate else "restored-build-ref",
        "rollout_ref": "rollout-ref" if candidate else "restored-rollout-ref",
    }


def envelope(ref, kind, data):
    return {"schema": SCHEMA, "ref": ref, "kind": kind, "run": run(), "data": data}


def job(identifier, source, head, started, ended, producer="producer-ref"):
    return {"id": identifier, "producer_ref": producer, "source": source,
            "head": head, "conclusion": "success", "started_at": at(started),
            "ended_at": at(ended)}


def provenance_fixture():
    """Return (receipt, {ref: sidecar object}, now). Fresh deep objects each call."""
    receipt = base.receipt()
    candidate = {"commit": CANDIDATE, "tree": CANDIDATE_TREE}
    trusted = {"commit": CONTROL_SHA, "tree": CONTROL_TREE}

    def own(value):  # never alias one mutable object between claims or sidecars
        return copy.deepcopy(value)
    docs = {ref: _doc(ref, digit) for ref, digit in (
        ("contract-ref", "1"), ("policy-ref", "2"), ("plan-ref", "3"),
        ("identity-map-ref", "4"), ("clearance-ref", "5"))}
    manifests = {
        "source-manifest-ref": _manifest("source-manifest-ref", CANDIDATE, "a"),
        "components-ref": _manifest("components-ref", CANDIDATE, "b"),
        "tooling-ref": _manifest("tooling-ref", CONTROL_SHA, "c"),
        "actions-ref": _manifest("actions-ref", CONTROL_SHA, "d"),
        "dependencies-ref": _manifest("dependencies-ref", CONTROL_SHA, "e"),
        "modules-ref": _manifest("modules-ref", CONTROL_SHA, "f"),
        "configuration-ref": _manifest("configuration-ref", CONTROL_SHA, "9"),
    }
    workflow = {"id": "101", "path": ".github/workflows/staging.yml",
                "ref": "workflow-ref", "sha": CONTROL_SHA}

    scope = receipt["scope"]
    scope.update(candidate=own(candidate), contract=own(docs["contract-ref"]),
                 source_manifest=own(manifests["source-manifest-ref"]),
                 merge_base={"commit": MERGE_BASE, "tree": "33b77639411230b6c3e87b9ef170bc6a2b058295"},
                 recorded_base={"commit": RECORDED_BASE, "tree": RECORDED_BASE_TREE})
    control = receipt["control"]
    control["version"] = own(trusted)
    control["workflow"] = own(workflow)
    implementation_head = {"commit": "8" * 40, "tree": "9" * 40}
    control["installation"].update(pr="2000", head=own(implementation_head), squash=own(trusted))
    for field, ref in (("tooling_manifest", "tooling-ref"),
                       ("actions_manifest", "actions-ref"),
                       ("dependencies_manifest", "dependencies-ref"),
                       ("modules_manifest", "modules-ref"),
                       ("configuration_manifest", "configuration-ref")):
        control[field] = own(manifests[ref])

    authority = receipt["authority"]
    authority.update(policy=own(docs["policy-ref"]), resource_plan=own(docs["plan-ref"]),
                     author=_principal("author-ref", "author"),
                     executor=_principal("executor-ref", "executor"),
                     caller=_principal("actor-ref", "caller"))
    authority["execution"].update(run=run(), not_before=at("00:00:00"),
                                  expires_at=at("00:40:00"))
    receipt["resources"].update(identity_map=own(docs["identity-map-ref"]),
                                plan=own(docs["plan-ref"]))
    receipt["gates"]["readiness_clearance"] = own(docs["clearance-ref"])

    attempt = receipt["attempt"]
    attempt.update(run=run(), actor=_principal("actor-ref", "caller"),
                   started_at=at("00:00:00"), ended_at=at("00:24:00"),
                   observation_started_at=at("00:05:00"),
                   observation_ended_at=at("00:15:00"),
                   job_refs=["job-build", "job-control", "job-observer"],
                   step_refs=["step-ref"], check_refs=["check-ref"],
                   producer_refs=["producer-ref", "producer-observer"])
    for lock in attempt["locks"]:
        lock["run"] = run()

    build = receipt["build"]
    build.update(run=run(), candidate=own(candidate), image_digest=IMAGE,
                 components_manifest=own(manifests["components-ref"]),
                 worker=copy.deepcopy(CANDIDATE_WORKER))
    receipt["rollout"].update(run=run(), application_id=APPLICATION,
                              image_digest=IMAGE, started_at=at("00:03:00"),
                              ended_at=at("00:04:00"),
                              worker_version_id=CANDIDATE_WORKER["version_id"])

    one = sample("served-one", "00:04:30", candidate=True)
    two = sample("served-two", "00:14:00", candidate=True)
    before = sample("served-before", "00:02:00", candidate=False)
    after = sample("served-after", "00:20:00", candidate=False)
    receipt["served"]["samples"] = [one, two]
    life = receipt["lifecycle"]
    life.update(served_before=before, served_after=after,
                worker_before=copy.deepcopy(RESTORED_WORKER),
                worker_after=copy.deepcopy(RESTORED_WORKER))

    gate_samples, gate_assertions = {}, {}
    records = []
    for name in GATES:
        planning = name in ("schema_plan", "roles")
        sample_ref = "served-before" if planning else "served-one"
        observed_at = at("00:02:40") if planning else at("00:13:00")
        record = base.gate(name)
        record.update(job_id="77", run=run(), served_ref=sample_ref,
                      observed_at=observed_at, result="success", status=200,
                      sample_refs=["gs-" + name], assertion_refs=["ga-" + name])
        records.append(record)
        gate_samples["gs-" + name] = {
            "gate_name": name, "job_id": "77", "served_ref": sample_ref,
            "observed_at": observed_at, "status": 200, "result": "observed",
            "request_sha256": "8" * 64}
        gate_assertions["ga-" + name] = {
            "gate_name": name, "sample_refs": ["gs-" + name], "outcome": "success",
            "assertions_sha256": "9" * 64}
    receipt["gates"]["records"] = records
    obs = receipt["observations"]
    obs["cases"][0].update(phase="on", served_ref="served-one",
                           started_at=at("00:06:00"), ended_at=at("00:10:00"))
    for effect in (*obs["expected_effects"], *obs["actual_effects"]):
        effect.update(served_ref="served-one", observed_at=at("00:08:00"))
    obs["coverage"][0].update(run=run(), started_at=at("00:06:00"),
                              ended_at=at("00:10:00"))
    for lock in attempt["locks"]:
        lock["run"] = run()

    sidecars = {}

    def add(ref, kind, data):
        sidecars[ref] = envelope(ref, kind, data)

    for ref, claim in docs.items():
        add(ref, "document", {k: v for k, v in claim.items() if k != "ref"})
    for ref, claim in manifests.items():
        add(ref, "manifest", {k: v for k, v in claim.items() if k != "ref"})
    add("mapping-ref", "installation_mapping",
        {"pr": "2000", "head": own(implementation_head), "squash": own(trusted),
         "control": own(trusted)})
    add("producer-ref", "producer", {
        "principal": _principal("producer-principal", "producer"),
        "workflow": own(workflow), "repository": "TogetherWeOwn/two-bot-next"})
    add("producer-observer", "producer", {
        "principal": _principal("observer-principal", "observer"),
        "workflow": own(workflow), "repository": "TogetherWeOwn/two-bot-next"})
    add("job-build", "job", job("11", "candidate", CANDIDATE, "00:01:00", "00:02:30"))
    add("job-control", "job", job("77", "control", CONTROL_SHA, "00:00:30", "00:23:00"))
    add("job-observer", "job",
        job("88", "external_observer", CONTROL_SHA, "00:00:10", "00:23:30",
            "producer-observer"))
    add("step-ref", "step", {"id": "5", "job_ref": "job-control", "result": "success"})
    add("check-ref", "check", {"id": "6", "job_ref": "job-control",
                               "head": CONTROL_SHA, "result": "success"})
    add("provenance-ref", "build_provenance", {
        "build_ref": "build-ref", "candidate": own(candidate), "image_digest": IMAGE,
        "components_sha256": manifests["components-ref"]["sha256"],
        "worker": copy.deepcopy(CANDIDATE_WORKER)})
    add("isolation-ref", "build_isolation", {
        "build_ref": "build-ref", "candidate": own(candidate), "job_ref": "job-build",
        "boundary_id": "12121212-1212-4212-8212-121212121212",
        "credential_paths": [], "outcome": "isolated"})
    add("observer-ref", "observer", {
        "principal": _principal("observer-principal", "observer"),
        "job_ref": "job-observer", "provisioning_ref": "provisioning-ref"})
    for claim in (one, two, before, after):
        add(claim["external_source_ref"], "external_served",
            {"observer_ref": "observer-ref", "sample": copy.deepcopy(claim)})
    for ref, data in gate_samples.items():
        add(ref, "gate_sample", data)
    for ref, data in gate_assertions.items():
        add(ref, "gate_assertion", data)
    return receipt, sidecars, NOW


def evidence_files(sidecars):
    """JSON bytes keyed by the exact member path EvidenceIndex requires."""
    return {"evidence/" + ref + ".json": json.dumps(record).encode("ascii")
            for ref, record in sidecars.items()}


def evidence_index(sidecars, attempt=None):
    return EvidenceIndex(evidence_files(sidecars), attempt or run())
