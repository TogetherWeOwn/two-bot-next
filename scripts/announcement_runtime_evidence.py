"""Bounded typed provenance-sidecar resolution, not authenticated evidence.

These records are inert claims. Neither a sidecar's kind, filename, digest nor
successful resolution selects a trust root or proves independent observation.
The remaining authority/resource/lifecycle records have no permissive fallback:
unimplemented kinds refuse, and runtime admission remains unsupported.
"""

import copy
import hashlib
import json

from announcement_runtime_schema import (
    ARCHIVE_LIMIT, Array, CONTROL, DOCUMENT, GATE_NAMES, Integer, MANIFEST,
    PRINCIPAL, REF, REFERENCE_LIMIT, RUN, SERVICE_ID, SERVED_PAIR, SHA, SHA256,
    TOKEN_FREE_PATH, UTC, UUID, VERSION, WORKER, _validate, enum,
    parse_bounded_json, require,
)


EVIDENCE_SCHEMA = "two.bot.announcement-evidence.v1"
# No generic record, arbitrary extensions or caller-supplied schema definition.
PROVENANCE_RECORDS = {
    "document": {"revision": UUID, "sha256": SHA256},
    "manifest": {"revision": SHA, "sha256": SHA256},
    "producer": {"principal": PRINCIPAL, "workflow": CONTROL["workflow"],
                 "repository": enum("TogetherWeOwn/two-bot-next")},
    "job": {"id": SERVICE_ID, "producer_ref": REF,
            "source": enum("candidate", "control", "independent_verifier", "external_observer"),
            "head": SHA, "conclusion": enum("success", "failure"),
            "started_at": UTC, "ended_at": UTC},
    "step": {"id": SERVICE_ID, "job_ref": REF, "result": enum("success", "failure")},
    "check": {"id": SERVICE_ID, "job_ref": REF, "head": SHA,
              "result": enum("success", "failure")},
    "build_provenance": {"build_ref": REF, "candidate": VERSION,
                         "image_digest": SERVED_PAIR["image_digest"],
                         "components_sha256": SHA256, "worker": WORKER},
    "build_isolation": {"build_ref": REF, "candidate": VERSION, "job_ref": REF,
                        "boundary_id": UUID,
                        "credential_paths": Array(enum("persisted_checkout", "host_environment",
                            "shared_cache", "docker_socket", "control_plane"), 0, 5),
                        "outcome": enum("isolated", "unknown")},
    "external_served": {"observer_ref": REF, "sample": SERVED_PAIR},
    "observer": {"principal": PRINCIPAL, "job_ref": REF, "provisioning_ref": REF},
    "gate_sample": {"gate_name": enum(*sorted(GATE_NAMES)), "job_id": SERVICE_ID,
                    "served_ref": REF, "observed_at": UTC,
                    "status": Integer(100, 599), "result": enum("observed", "unknown"),
                    "request_sha256": SHA256},
    "gate_assertion": {"gate_name": enum(*sorted(GATE_NAMES)),
                       "sample_refs": Array(REF), "outcome": enum("success", "failure"),
                       "assertions_sha256": SHA256},
    "installation_mapping": {"pr": SERVICE_ID, "head": VERSION, "squash": VERSION,
                             "control": VERSION},
    "artifact": {"id": SERVICE_ID, "name": TOKEN_FREE_PATH,
                 "size": Integer(1, ARCHIVE_LIMIT), "sha256": SHA256,
                 "producer_ref": REF, "created_at": UTC, "expires_at": UTC},
}


def canonical_digest(value):
    """SHA-256 of canonical inert JSON, not a signature or authority decision."""
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     ensure_ascii=True).encode("ascii")).hexdigest()


class EvidenceIndex:
    """An isolated snapshot of closed same-attempt sidecars, with typed lookup.

    Construct only from inert member bytes after bounded archive inspection.
    This class does not fetch URLs, import modules, open files or authenticate
    any record. Unsupported authority/control evidence must not be relabeled as
    one of these provenance types to obtain acceptance.
    """

    def __init__(self, files, run):
        _validate(run, RUN)
        require(type(files) is dict and 0 < len(files) <= REFERENCE_LIMIT,
                "evidence_inventory")
        require(all(type(data) is bytes for data in files.values()), "evidence_bytes")
        require(sum(map(len, files.values())) <= ARCHIVE_LIMIT, "evidence_size")
        self._run = copy.deepcopy(run)
        self._records = {}
        self._used = set()
        for path, raw in files.items():
            record = parse_bounded_json(raw)
            require(type(record) is dict, "evidence_record")
            kind = record.get("kind")
            require(type(kind) is str and kind in PROVENANCE_RECORDS, "evidence_kind")
            _validate(record, {"schema": enum(EVIDENCE_SCHEMA), "ref": REF,
                               "kind": enum(kind), "run": RUN,
                               "data": PROVENANCE_RECORDS[kind]})
            ref = record["ref"]
            require(path == "evidence/" + ref + ".json", "evidence_path")
            require(record["run"] == self._run, "evidence_attempt")
            require(ref not in self._records, "evidence_duplicate")
            self._records[ref] = record

    def resolve(self, ref, kind):
        _validate(ref, REF)
        require(kind in PROVENANCE_RECORDS, "evidence_kind")
        record = self._records.get(ref)
        require(record is not None, "unresolved_reference")
        require(record["kind"] == kind, "reference_kind")
        self._used.add(ref)
        return copy.deepcopy(record["data"])

    def bind(self, claim, kind):
        """Compare a complete document/manifest pin, without authenticating it."""
        require(kind in ("document", "manifest"), "evidence_kind")
        _validate(claim, DOCUMENT if kind == "document" else MANIFEST)
        data = self.resolve(claim["ref"], kind)
        require(data == {key: value for key, value in claim.items() if key != "ref"},
                "reference_binding")

    def require_run(self, run):
        require(run == self._run, "evidence_attempt")

    def require_all_used(self):
        """Final packet accounting must reject even well-formed extra evidence."""
        require(self._used == self._records.keys(), "unreferenced_evidence")
