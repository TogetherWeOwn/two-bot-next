"""Verify a predecessor planner's PRE migration catalog using local Git objects.

The caller binds the candidate commit to the migration bytes the planner will
read and compares source_sha, cutover_sha256 and store_sha256 with the
authenticated inventory proof. Catalog identity carries no producer, serving
build or database authority. The historical retained commit is a label and
need not be present in the local object store.
"""

from hashlib import sha256, sha384
import json
import re

from ci.staging_migrate_claim import projection_hash, store_projection_hash
from ci.staging_rollback_compatibility import MIGRATION_PATHS, Refused, git, migration_trees
from script_intake import decode_json


ORIGINAL = "d8b6c1503fd72a7533ea96979f8c2ddc794e6737"
LANDED = "28990125adbc3a636c6a70e9a99d8dbd9f77fbcd"
RETAINED = "058141c29cf135304b060d8fd89aa6e21e44a0c6"
# Historical object IDs bound the reads. The comparisons below also verify
# how those objects follow from the original catalog and the landed change.
RETAINED_CUTOVER_TREE = b"8be4d2baba589a053bacc2523cbdcb760fca4d90"
RETAINED_LOCK_BLOB = b"2e93ede5799fe3806bd2f21cb35a3544f9f23e37"
FENCE_NAME = "0475_attendance_recovery_boundary.sql"
FENCE_PATH = f"{MIGRATION_PATHS[0]}/{FENCE_NAME}"
LOCK = "migrations.lock"
REGULAR_BLOB = rb"100644 blob ([0-9a-f]{40})\t"
MIGRATION_ENTRY = re.compile(REGULAR_BLOB + rb"([0-9]{4}_[a-z][a-z0-9_]*\.sql)")


def _file_blob(repo, commit, path):
    entry = git(repo, "ls-tree", "--full-tree", "-z", commit, "--", path)
    match = re.fullmatch(REGULAR_BLOB + re.escape(path.encode()) + rb"\x00", entry)
    if match is None:
        raise Refused("retained_catalog_file_invalid")
    return match[1]


def _migration_blobs(repo, tree):
    entries = git(repo, "ls-tree", "--full-tree", "-z", tree.decode("ascii")).split(b"\x00")[:-1]
    blobs = {}
    for entry in entries:
        match = MIGRATION_ENTRY.fullmatch(entry)
        if match is None:
            raise Refused("retained_catalog_entry_invalid")
        name = match[2].decode("ascii")
        if name in blobs:
            raise Refused("retained_catalog_entry_invalid")
        blobs[name] = match[1]
    return blobs


def _read_blob(repo, blob):
    return git(repo, "cat-file", "blob", blob.decode("ascii"))


def _verify_lock(repo, lock_blob, fence_sql):
    if lock_blob != RETAINED_LOCK_BLOB:
        raise Refused("retained_catalog_lock_mismatch")
    try:
        original = decode_json(_read_blob(repo, _file_blob(repo, ORIGINAL, LOCK)))
        landed = decode_json(_read_blob(repo, _file_blob(repo, LANDED, LOCK)))
        entry = landed["migrations"][FENCE_PATH]
        entries = original["migrations"]
        checksum = entry["sha256"]
    except (KeyError, TypeError, ValueError, RecursionError):
        raise Refused("retained_catalog_lock_mismatch") from None
    if not isinstance(entries, dict) or FENCE_PATH in entries or checksum != sha256(fence_sql).hexdigest():
        raise Refused("retained_catalog_lock_mismatch")
    entries[FENCE_PATH] = entry
    expected = (json.dumps(original, indent=2) + "\n").encode()
    if _read_blob(repo, lock_blob) != expected:
        raise Refused("retained_catalog_lock_mismatch")


def _projection_rows(repo, blobs):
    rows = []
    for path, blob in sorted(blobs.items()):
        version, description = path.removesuffix(".sql").split("_", 1)
        rows.append({"version": int(version), "description": description.replace("_", " "),
                     "sha384": sha384(_read_blob(repo, blob)).hexdigest()})
    return rows


def _projection_digests(repo, source_sha, cutover, store_tree):
    projections = {"source_sha": source_sha, "pending_before": [], "pending_store_before": [],
                   "source_migrations": _projection_rows(repo, cutover),
                   "source_store_migrations": _projection_rows(repo, _migration_blobs(repo, store_tree))}
    return {"cutover_sha256": projection_hash(projections), "store_sha256": store_projection_hash(projections)}


def verify_predecessor_plan_source(repo, candidate_sha, *, shape):
    """Verify the immutable catalog that the offline planner will read for this shape."""
    if shape == "0475/0465":
        return {"candidate_sha": candidate_sha, **verify_retained_catalog(repo, candidate_sha)}
    if shape != "0473/0465":
        raise Refused("unsupported_pre_shape")
    cutover_tree, store_tree = migration_trees(repo, candidate_sha)
    if (cutover_tree, store_tree) != migration_trees(repo, ORIGINAL):
        raise Refused("original_catalog_tree_mismatch")
    return {"source_sha": ORIGINAL, "candidate_sha": candidate_sha,
            "cutover_tree": cutover_tree.decode("ascii"), "store_tree": store_tree.decode("ascii"),
            **_projection_digests(repo, ORIGINAL, _migration_blobs(repo, cutover_tree), store_tree)}


def verify_retained_catalog(repo, reconstruction_sha):
    """Return catalog evidence for a full commit SHA in a trusted local repository."""
    cutover_tree, store_tree = migration_trees(repo, reconstruction_sha)
    original_cutover, original_store = migration_trees(repo, ORIGINAL)
    if cutover_tree != RETAINED_CUTOVER_TREE or store_tree != original_store:
        raise Refused("retained_catalog_tree_mismatch")

    landed_blob = _file_blob(repo, LANDED, FENCE_PATH)
    original = _migration_blobs(repo, original_cutover)
    cutover = _migration_blobs(repo, cutover_tree)
    if FENCE_NAME in original or cutover != {**original, FENCE_NAME: landed_blob}:
        raise Refused("retained_catalog_tree_mismatch")
    lock_blob = _file_blob(repo, reconstruction_sha, LOCK)
    fence_sql = _read_blob(repo, landed_blob)
    _verify_lock(repo, lock_blob, fence_sql)

    return {"source_sha": RETAINED, "reconstruction_sha": reconstruction_sha,
            "cutover_tree": cutover_tree.decode("ascii"), "store_tree": store_tree.decode("ascii"),
            "lock_blob": lock_blob.decode("ascii"), "landed_0475_blob": landed_blob.decode("ascii"),
            "landed_0475_sha384": sha384(fence_sql).hexdigest(),
            **_projection_digests(repo, RETAINED, cutover, store_tree)}
