#!/usr/bin/env python3
"""Immutable staging-byte promotion: certify the served bundle, admit it in prod.

One deterministic seam, no rebuilding of staged bytes::

    bot_promotion.main(argv, *, github=None, copy_image=None) -> exit status
    github.get_json(repo_relative_route)          # bounded managed-GitHub reads
    github.get_bytes(repo_relative_route)         # bounded managed-GitHub reads
    copy_image(staging_digest_ref, production_repository) -> copied_digest_ref

``certify`` packages the exact Worker bundle staging served (produced locally
by ``wrangler deploy --dry-run --containers-rollout=none --outdir DIR``) plus
the staging rollout evidence into an immutable ``bot-promotion-<run>-<attempt>``
artifact: ``promotion-receipt.json`` and ``modules/<path>``, nothing else.
``admit`` independently re-reads the current source run and the immutable
artifact metadata via GitHub, validates the whole ZIP before ``copy_image``,
rechecks the current run, copies the container image digest into the fixed
production repository, and writes ``DIR/{admitted.json, modules/...}``.

Fail-closed rules (pinned by scripts/test_immutable_bot_promotion.py):

- No subprocess, socket or network in the offline path: the injected
  ``github``/``copy_image`` doubles are the only I/O. No compiler, bundler
  or rebuild is ever a fallback.
- No mutation before validation: neither ``copy_image`` nor any output file
  is touched until every intake check passes, and the current run is
  rechecked after validation, immediately before the copy.
- Bounded strict intake: 64 KiB receipt, 32 modules, 8 MiB/member, 16 MiB
  total expanded. Oversize refuses before decoding or echoing anything.
- Strict closed receipt: exact key sets, exact ``int`` (never ``bool``)
  numerics, duplicate JSON keys (including ``\\u``-escaped repeats) refuse,
  hashes/sizes/run binding verified against the downloaded bytes, and the
  artifact name, ID, workflow run and archive digest are independently bound.
- Fixed-vocabulary refusals: no remote, file or secret value ever reaches
  output. Only allowlisted IDs, digests and shapes are compared, never
  echoed.
- Image discipline: staging digest refs only
  (``registry.cloudflare.com/<account>/two-bot-next-twobotcontainer-staging@sha256:<hex>``);
  the ``--account-id`` must name the same account, mutable tags refuse, the
  production image never counts as a source, and the copied ref must equal
  the production repository plus the identical digest or admission is never
  published.

Library surface for production_deploy (which imports this module lazily, so
this module must never import production_deploy, even indirectly)::

    load_admitted(path, sha, *, verify_modules=False) -> admitted mapping
    configure_bundle(config, modules_dir, entrypoint) -> mutated config

Both raise :class:`PromotionError` (a ``ValueError``) on refusal, never
``SystemExit``; production_deploy catches ``ValueError``/``OSError`` and
prints its own constant artifact refusal.

Real CLI adapters (used only when the caller passes neither double):

- GitHub reads go through the managed ``gh api`` CLI and only the bounded
  read endpoints this command needs (one run, one artifact listing, one
  artifact, one archive download), with output caps and timeouts.
- Image copy uses Wrangler's native ephemeral registry login and Docker's
  pull/tag/push, never build. Both RepoDigests and the remote manifest must
  independently confirm the unchanged digest in the production-local repo.
"""

import argparse
import hashlib
import io
import json
import os
import posixpath
import re
import selectors
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import zipfile
import zlib
from pathlib import Path, PurePosixPath

import build_identity

REPO = "TogetherWeOwn/two-bot-next"
STAGING_WORKFLOW = ".github/workflows/deploy-staging.yml"
STAGING_BRANCH = "main"
STAGING_APP = "two-bot-next-twobotcontainer-staging"
PRODUCTION_APP = "two-bot-next-twobotcontainer-production"
REGISTRY_HOST = "registry.cloudflare.com"
SCHEMA_VERSION = 1

# Intake ceilings: refuse before decoding or expanding past them.
MAX_RECEIPT_BYTES = 64 * 1024
MAX_MODULES = 32
MAX_MEMBER_BYTES = 8 * 1024 * 1024
MAX_TOTAL_BYTES = 16 * 1024 * 1024
# Compressed download cap: the expanded ceilings below are what matter, but
# an unbounded download is never started.
MAX_ARCHIVE_BYTES = 32 * 1024 * 1024
# Local evidence/config caps: operator files, still never read unbounded.
MAX_LOCAL_BYTES = 256 * 1024

UUID = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
                  r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
ACCOUNT = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}")
ACCOUNT_SEGMENT = r"[A-Za-z0-9][A-Za-z0-9_.-]*"
COMPAT_DATE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}")

class PromotionError(ValueError):
    """A promotion intake/provenance refusal. Fixed vocabulary, no remote data."""


class DuplicateKeyError(ValueError):
    """A JSON object repeats a key; the message carries no key/value data."""


def _reject_duplicate_keys(pairs):
    """object_pairs_hook refusing repeats instead of last-wins."""
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise DuplicateKeyError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


def _refuse(reason):
    """Print one fixed-vocabulary line and return the nonzero status."""
    print(reason)
    return 1


def _fail(error):
    if isinstance(error, PromotionError):
        return _refuse(f"bot promotion refused: {error}")
    return _refuse("bot promotion refused: unreadable local input")


def _is_int(value):
    return type(value) is int


def _is_sha(value):
    return build_identity.is_build_revision_shape(value)


def _is_uuid(value):
    return isinstance(value, str) and UUID.fullmatch(value) is not None


def _is_digest(value):
    return isinstance(value, str) and DIGEST.fullmatch(value) is not None


def _safe_module_path(path):
    """A certified module path stays inside modules/ as one portable name."""
    if not isinstance(path, str) or not path:
        return False
    if (len(path) > 256 or re.fullmatch(r"[A-Za-z0-9_./-]+", path) is None
            or Path(path).suffix not in (".js", ".mjs", ".wasm", ".txt", ".bin")):
        return False
    if path.startswith("/") or path.startswith("./") or "//" in path:
        return False
    if posixpath.normpath(path) != path:
        return False
    parts = PurePosixPath(path).parts
    if not parts or any(part.startswith(".") for part in parts):
        return False
    return True


def _staging_image_pattern(account):
    return re.compile(r"registry\.cloudflare\.com/" + re.escape(account) + "/"
                      + re.escape(STAGING_APP) + r"@sha256:[0-9a-f]{64}")


def _any_staging_image_pattern():
    return re.compile(r"registry\.cloudflare\.com/" + ACCOUNT_SEGMENT + "/"
                      + re.escape(STAGING_APP) + r"@sha256:[0-9a-f]{64}")


def _production_image_pattern():
    return re.compile(r"registry\.cloudflare\.com/" + ACCOUNT_SEGMENT + "/"
                      + re.escape(PRODUCTION_APP) + r"@sha256:[0-9a-f]{64}")


def _run_route(run_id):
    return f"repos/{REPO}/actions/runs/{run_id}"


def _artifacts_route(run_id):
    return _run_route(run_id) + "/artifacts?per_page=100"


def _artifact_name(run_id, attempt):
    return f"bot-promotion-{run_id}-{attempt}"


def _check_run_identity(run, sha, run_id, attempt):
    """The current source run is this promotion's run and attempt, on main.

    Returns the canonical repository id observed on the run itself: the
    head repository must be that same repository (a fork's run never
    counts), and every downstream binding (receipt repository, artifact
    workflow run) is checked against this observed id, never a constant.
    """
    if not isinstance(run, dict):
        raise PromotionError("source run is not an object")
    if run.get("id") != run_id or type(run.get("id")) is not int:
        raise PromotionError("source run id mismatch")
    if run.get("run_attempt") != attempt or type(run.get("run_attempt")) is not int:
        raise PromotionError("source run attempt mismatch")
    if run.get("head_sha") != sha:
        raise PromotionError("source run sha mismatch")
    if run.get("head_branch") != STAGING_BRANCH:
        raise PromotionError("source run branch mismatch")
    if (run.get("path") != STAGING_WORKFLOW
            or run.get("event") not in ("push", "workflow_dispatch")):
        raise PromotionError("source run workflow mismatch")
    repository = run.get("repository")
    if (not isinstance(repository, dict) or type(repository.get("id")) is not int
            or repository.get("full_name") != REPO):
        raise PromotionError("source run repository mismatch")
    repo_id = repository["id"]
    head_repository = run.get("head_repository")
    if (not isinstance(head_repository, dict)
            or head_repository.get("id") != repo_id
            or type(head_repository.get("id")) is not int
            or head_repository.get("full_name") != REPO):
        raise PromotionError("source run repository mismatch")
    return repo_id


def _check_run_completed(run):
    if run.get("status") != "completed" or run.get("conclusion") != "success":
        raise PromotionError("source run is not completed success")


def _check_evidence_staging(evidence, sha, build_id, deploy_id):
    """The staging rollout evidence the certified bundle was serving."""
    if not isinstance(evidence, dict):
        raise PromotionError("staging evidence is not an object")
    for key in ("worker_version", "application_id", "rollout_id"):
        if not _is_uuid(evidence.get(key)):
            raise PromotionError("staging evidence identity mismatch")
    if not _is_int(evidence.get("target_version")) or evidence["target_version"] <= 0:
        raise PromotionError("staging evidence target mismatch")
    image = evidence.get("image")
    if (not isinstance(image, str)
            or _any_staging_image_pattern().fullmatch(image) is None):
        raise PromotionError("staging evidence image mismatch")
    if evidence.get("revision") != sha:
        raise PromotionError("staging evidence revision mismatch")
    if evidence.get("build_id") != build_id:
        raise PromotionError("staging evidence build mismatch")
    if evidence.get("deploy_id") != deploy_id:
        raise PromotionError("staging evidence deploy mismatch")
    for key in ("readyz", "health"):
        if evidence.get(key) != 200 or type(evidence.get(key)) is not int:
            raise PromotionError("staging evidence gate mismatch")
    return evidence


def _read_local_capped(path):
    with open(path, "rb") as handle:
        raw = handle.read(MAX_LOCAL_BYTES + 1)
    if len(raw) > MAX_LOCAL_BYTES:
        raise PromotionError("local input over the body cap")
    return raw


def _parse_json_capped(raw):
    return json.loads(raw.decode("utf-8"),
                      object_pairs_hook=_reject_duplicate_keys)


def _validate_receipt(receipt, sha, run_id, attempt, repo_id):
    """Strict closed receipt: exact keys, exact types, run-bound stamps.

    ``repo_id`` is the repository id observed on the source run itself,
    never a constant: the receipt repository must name it exactly.
    """
    if not isinstance(receipt, dict):
        raise PromotionError("receipt is not an object")
    if (set(receipt) != {"schema_version", "repository", "source", "build",
                         "staging", "container", "worker"}):
        raise PromotionError("receipt has unexpected fields")
    if receipt.get("schema_version") != 1 or type(receipt.get("schema_version")) is not int:
        raise PromotionError("receipt schema mismatch")

    repository = receipt["repository"]
    if (not isinstance(repository, dict) or set(repository) != {"id", "full_name"}
            or repository.get("id") != repo_id
            or type(repository.get("id")) is not int
            or type(repo_id) is not int
            or repository.get("full_name") != REPO):
        raise PromotionError("receipt repository mismatch")

    source = receipt["source"]
    if (not isinstance(source, dict)
            or set(source) != {"sha", "run_id", "run_attempt", "branch", "workflow"}
            or source.get("sha") != sha or source.get("run_id") != run_id
            or type(source.get("run_id")) is not int
            or source.get("run_attempt") != attempt
            or type(source.get("run_attempt")) is not int
            or source.get("branch") != STAGING_BRANCH
            or source.get("workflow") != STAGING_WORKFLOW):
        raise PromotionError("receipt source mismatch")

    build = receipt["build"]
    build_id = f"{run_id}-{attempt}"
    if (not isinstance(build, dict)
            or set(build) != {"revision", "build_id", "deploy_id"}
            or build.get("revision") != sha or build.get("build_id") != build_id):
        raise PromotionError("receipt build mismatch")
    deploy_id = build.get("deploy_id")
    if (not build_identity.is_deploy_id_shape(deploy_id)
            or build_identity.parse_deploy_id(deploy_id)[2] != sha[:7]):
        raise PromotionError("receipt deploy mismatch")

    staging = receipt["staging"]
    if (not isinstance(staging, dict) or set(staging) != {
            "worker_version", "application_id", "rollout_id", "target_version", "image",
            "revision", "build_id", "deploy_id", "readyz", "health"}):
        raise PromotionError("receipt staging mismatch")
    for key in ("worker_version", "application_id", "rollout_id"):
        if not _is_uuid(staging.get(key)):
            raise PromotionError("receipt staging identity mismatch")
    if not _is_int(staging.get("target_version")) or staging["target_version"] <= 0:
        raise PromotionError("receipt staging target mismatch")
    if (staging.get("revision") != sha or staging.get("build_id") != build_id
            or staging.get("deploy_id") != deploy_id):
        raise PromotionError("receipt staging stamp mismatch")
    for key in ("readyz", "health"):
        if staging.get(key) != 200 or type(staging.get(key)) is not int:
            raise PromotionError("receipt staging gate mismatch")
    staging_image = staging.get("image")
    if (not isinstance(staging_image, str)
            or _any_staging_image_pattern().fullmatch(staging_image) is None):
        raise PromotionError("receipt staging image mismatch")

    container = receipt["container"]
    if (not isinstance(container, dict) or set(container) != {"image", "digest"}):
        raise PromotionError("receipt container mismatch")
    digest = container.get("digest")
    if not _is_digest(digest):
        raise PromotionError("receipt container digest mismatch")
    if container.get("image") != staging_image:
        raise PromotionError("receipt container image mismatch")
    if staging_image != staging_image.rsplit("@", 1)[0] + "@" + digest:
        raise PromotionError("receipt container digest mismatch")

    worker = receipt["worker"]
    if (not isinstance(worker, dict)
            or set(worker) != {"entrypoint", "compatibility_date",
                               "compatibility_flags", "modules"}):
        raise PromotionError("receipt worker mismatch")
    entrypoint = worker.get("entrypoint")
    if not _safe_module_path(entrypoint):
        raise PromotionError("receipt entrypoint mismatch")
    if (not isinstance(worker.get("compatibility_date"), str)
            or COMPAT_DATE.fullmatch(worker["compatibility_date"]) is None):
        raise PromotionError("receipt compatibility mismatch")
    flags = worker.get("compatibility_flags")
    if (not isinstance(flags, list) or not flags
            or any(not isinstance(flag, str) or not flag for flag in flags)):
        raise PromotionError("receipt compatibility mismatch")
    modules = worker.get("modules")
    if not isinstance(modules, list) or not modules or len(modules) > MAX_MODULES:
        raise PromotionError("receipt module inventory mismatch")
    seen = set()
    for member in modules:
        if (not isinstance(member, dict) or set(member) != {"path", "size", "sha256"}):
            raise PromotionError("receipt module entry mismatch")
        path = member.get("path")
        if not _safe_module_path(path) or path in seen:
            raise PromotionError("receipt module path mismatch")
        seen.add(path)
        if not _is_int(member.get("size")) or not 0 <= member["size"] <= MAX_MEMBER_BYTES:
            raise PromotionError("receipt module size mismatch")
        if (not isinstance(member.get("sha256"), str)
                or re.fullmatch(r"[0-9a-f]{64}", member["sha256"]) is None):
            raise PromotionError("receipt module hash mismatch")
    if entrypoint not in seen or Path(entrypoint).suffix not in (".js", ".mjs"):
        raise PromotionError("receipt entrypoint mismatch")
    if sum(member["size"] for member in modules) > MAX_TOTAL_BYTES:
        raise PromotionError("receipt over the expanded cap")
    return receipt


def _validate_package(blob, sha, run_id, attempt, repo_id):
    """Validate a whole artifact ZIP before any mutation; return receipt+modules."""
    try:
        archive = zipfile.ZipFile(io.BytesIO(blob))
    except (zipfile.BadZipFile, ValueError):
        raise PromotionError("artifact archive is not a ZIP") from None
    with archive:
        names = archive.namelist()
        if len(set(names)) != len(names):
            raise PromotionError("artifact archive repeats a member")
        for info in archive.infolist():
            kind = (info.external_attr >> 16) & 0o170000
            if kind not in (0, 0o100000) or info.is_dir() or info.flag_bits & 1:
                raise PromotionError("artifact archive carries an unsafe member")
        try:
            receipt_info = archive.getinfo("promotion-receipt.json")
        except KeyError:
            raise PromotionError("artifact archive has no receipt") from None
        if receipt_info.file_size > MAX_RECEIPT_BYTES:
            raise PromotionError("receipt over the body cap")
        with archive.open("promotion-receipt.json") as handle:
            raw = handle.read(MAX_RECEIPT_BYTES + 1)
        if len(raw) > MAX_RECEIPT_BYTES:
            raise PromotionError("receipt over the body cap")
        try:
            receipt = json.loads(raw.decode("utf-8"),
                                 object_pairs_hook=_reject_duplicate_keys)
        except (DuplicateKeyError, ValueError, RecursionError):
            raise PromotionError("receipt is not strict JSON") from None
        _validate_receipt(receipt, sha, run_id, attempt, repo_id)
        wanted = {member["path"] for member in receipt["worker"]["modules"]}
        if ("promotion-receipt.json" in wanted
                or {n for n in names if n != "promotion-receipt.json"}
                != {"modules/" + path for path in wanted}):
            raise PromotionError("artifact module set mismatch")
        modules = {}
        total = len(raw)
        for info in archive.infolist():
            if info.filename == "promotion-receipt.json":
                continue
            if info.file_size > MAX_MEMBER_BYTES:
                raise PromotionError("artifact member over the cap")
            with archive.open(info.filename) as handle:
                data = handle.read(MAX_MEMBER_BYTES + 1)
            if len(data) > MAX_MEMBER_BYTES:
                raise PromotionError("artifact member over the cap")
            total += len(data)
            if total > MAX_TOTAL_BYTES:
                raise PromotionError("artifact over the expanded cap")
            modules[info.filename[len("modules/"):]] = data
        for member in receipt["worker"]["modules"]:
            data = modules[member["path"]]
            if len(data) != member["size"]:
                raise PromotionError("artifact module size mismatch")
            if hashlib.sha256(data).hexdigest() != member["sha256"]:
                raise PromotionError("artifact module hash mismatch")
    return receipt, modules


def _collect_modules(modules_dir):
    """The served bundle, minus local environment material and dry-run notes."""
    root = Path(modules_dir)
    collected = {}
    if root.is_symlink() or not root.is_dir():
        raise PromotionError("bundle directory is unsafe")
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        if any((Path(dirpath) / name).is_symlink() for name in dirnames):
            raise PromotionError("bundle carries an unsafe directory")
        dirnames[:] = sorted(name for name in dirnames
                             if not name.startswith("."))
        for name in sorted(filenames):
            if name.startswith(".") or name == "README.md" or name.endswith(".map"):
                continue
            full = Path(dirpath) / name
            if full.is_symlink() or not full.is_file():
                raise PromotionError("bundle carries an unsafe member")
            rel = full.relative_to(root).as_posix()
            if not _safe_module_path(rel):
                raise PromotionError("bundle carries an unsafe member")
            with full.open("rb") as handle:
                data = handle.read(MAX_MEMBER_BYTES + 1)
            if len(data) > MAX_MEMBER_BYTES:
                raise PromotionError("bundle member over the cap")
            collected[rel] = data
            if len(collected) > MAX_MODULES or sum(map(len, collected.values())) > MAX_TOTAL_BYTES:
                raise PromotionError("bundle over the expanded cap")
    if not collected or len(collected) > MAX_MODULES:
        raise PromotionError("bundle module inventory mismatch")
    if sum(len(data) for data in collected.values()) > MAX_TOTAL_BYTES:
        raise PromotionError("bundle over the expanded cap")
    return collected


def _read_config_compat(config_path):
    try:
        config = tomllib.loads(_read_local_capped(config_path).decode("utf-8"))
    except (OSError, ValueError):
        raise PromotionError("worker config is unreadable") from None
    if not isinstance(config, dict) or config.get("name") != "two-bot-next":
        raise PromotionError("worker config mismatch")
    date = config.get("compatibility_date")
    flags = config.get("compatibility_flags")
    if (not isinstance(date, str) or COMPAT_DATE.fullmatch(date) is None
            or not isinstance(flags, list) or not flags
            or any(not isinstance(flag, str) or not flag for flag in flags)):
        raise PromotionError("worker compatibility mismatch")
    staging = config.get("env", {}).get("staging", {})
    if (staging.get("compatibility_date", date) != date
            or staging.get("compatibility_flags", flags) != flags):
        raise PromotionError("staging compatibility override refused")
    return date, flags


def _certify(args, github):
    if not _is_sha(args.sha):
        raise PromotionError("needs a 40-hex SHA")
    if build_identity.DIGITS.fullmatch(args.run_id or "") is None:
        raise PromotionError("needs a numeric run id")
    if build_identity.DIGITS.fullmatch(args.attempt or "") is None:
        raise PromotionError("needs a numeric attempt")
    run_id, attempt = int(args.run_id), int(args.attempt)
    if run_id <= 0 or attempt <= 0 or not build_identity.is_build_id_shape(f"{run_id}-{attempt}"):
        raise PromotionError("needs a bounded positive run id and attempt")
    if (not build_identity.is_deploy_id_shape(args.deploy_id)
            or build_identity.parse_deploy_id(args.deploy_id)[2] != args.sha[:7]):
        raise PromotionError("needs a canonical deploy ID naming the SHA")
    build_id = f"{run_id}-{attempt}"

    # The repository behind the receipt is verified against the live source
    # run, not trusted from local files. A staging run is still in progress
    # while it certifies, so completion is never required here.
    run = github.get_json(_run_route(run_id))
    repo_id = _check_run_identity(run, args.sha, run_id, attempt)
    if (run.get("status") not in ("in_progress", "completed")
            or run.get("conclusion") not in (None, "success")):
        raise PromotionError("source run cannot certify")
    repository = run["repository"]

    if args.readiness_result != "success":
        raise PromotionError("readiness gate did not succeed")
    if args.synthetic_result != "success":
        raise PromotionError("synthetic gate did not succeed")
    if args.fault_mode != "none":
        raise PromotionError("fault mode is not clean")

    compat_date, compat_flags = _read_config_compat(args.config)

    try:
        evidence = _parse_json_capped(_read_local_capped(args.evidence))
    except (DuplicateKeyError, ValueError, RecursionError):
        raise PromotionError("staging evidence is not strict JSON") from None
    _check_evidence_staging(evidence, args.sha, build_id, args.deploy_id)

    modules = _collect_modules(args.modules)
    if not _safe_module_path(args.entrypoint) or args.entrypoint not in modules:
        raise PromotionError("bundle entrypoint mismatch")

    image = evidence["image"]
    receipt = {
        "schema_version": SCHEMA_VERSION,
        "repository": {"id": repository["id"],
                       "full_name": repository["full_name"]},
        "source": {"sha": args.sha, "run_id": run_id, "run_attempt": attempt,
                   "branch": STAGING_BRANCH, "workflow": STAGING_WORKFLOW},
        "build": {"revision": args.sha, "build_id": build_id,
                  "deploy_id": args.deploy_id},
        "staging": {key: evidence[key] for key in (
            "worker_version", "application_id", "rollout_id", "target_version", "image",
            "revision", "build_id", "deploy_id", "readyz", "health")},
        "container": {"image": image,
                      "digest": image.rsplit("@", 1)[1]},
        "worker": {"entrypoint": args.entrypoint,
                   "compatibility_date": compat_date,
                   "compatibility_flags": list(compat_flags),
                   "modules": [{"path": path, "size": len(data),
                                "sha256": hashlib.sha256(data).hexdigest()}
                               for path, data in sorted(modules.items())]},
    }
    _validate_receipt(receipt, args.sha, run_id, attempt, repo_id)
    raw_receipt = (json.dumps(receipt, indent=2, sort_keys=True) + "\n").encode()
    if len(raw_receipt) > MAX_RECEIPT_BYTES:
        raise PromotionError("receipt over the body cap")
    if len(raw_receipt) + sum(map(len, modules.values())) > MAX_TOTAL_BYTES:
        raise PromotionError("artifact over the expanded cap")

    out = Path(args.out)
    if out.exists():
        raise PromotionError("certification output already exists")
    tmp = None
    try:
        out.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(dir=str(out.parent), prefix=".certify-",
                                         suffix=".zip", delete=False) as handle:
            tmp = Path(handle.name)
        with zipfile.ZipFile(tmp, "w",
                             compression=zipfile.ZIP_DEFLATED) as archive:
            archive.writestr("promotion-receipt.json", raw_receipt)
            for path in sorted(modules):
                archive.writestr("modules/" + path, modules[path])
        os.replace(tmp, out)
    except OSError:
        raise PromotionError("certified artifact is unwritable") from None
    finally:
        if tmp is not None and tmp.exists() and tmp != out:
            tmp.unlink(missing_ok=True)
    print("promotion certified")
    return 0


def _admit(args, github, copy_image):
    if not _is_sha(args.sha):
        raise PromotionError("needs a 40-hex SHA")
    if build_identity.DIGITS.fullmatch(args.run_id or "") is None:
        raise PromotionError("needs a numeric run id")
    if build_identity.DIGITS.fullmatch(args.attempt or "") is None:
        raise PromotionError("needs a numeric attempt")
    run_id, attempt = int(args.run_id), int(args.attempt)
    if run_id <= 0 or attempt <= 0 or not build_identity.is_build_id_shape(f"{run_id}-{attempt}"):
        raise PromotionError("needs a bounded positive run id and attempt")
    if ACCOUNT.fullmatch(args.account_id or "") is None:
        raise PromotionError("needs an approved account id")
    if Path(args.out).exists():
        raise PromotionError("admission output already exists")

    # 1. The current source attempt, before touching anything else. The
    #    repository id observed here binds every id below it.
    run = github.get_json(_run_route(run_id))
    repo_id = _check_run_identity(run, args.sha, run_id, attempt)
    _check_run_completed(run)

    # 2. Exactly one live artifact inventory entry binding run and attempt.
    listing = github.get_json(_artifacts_route(run_id))
    if not isinstance(listing, dict):
        raise PromotionError("artifact inventory is not an object")
    artifacts = listing.get("artifacts")
    if (type(listing.get("total_count")) is not int
            or not isinstance(artifacts, list) or listing["total_count"] != len(artifacts)
            or len(artifacts) > 100 or any(not isinstance(item, dict) for item in artifacts)):
        raise PromotionError("artifact inventory mismatch")
    candidates = [item for item in artifacts if item.get("name") == _artifact_name(run_id, attempt)]
    if len(candidates) != 1:
        raise PromotionError("artifact inventory mismatch")
    entry = candidates[0]
    if not isinstance(entry, dict):
        raise PromotionError("artifact inventory mismatch")
    if (not _is_int(entry.get("id")) or entry.get("name") != _artifact_name(run_id, attempt)
            or entry.get("expired") is not False):
        raise PromotionError("artifact inventory mismatch")

    # 3. The immutable artifact record itself, cross-checked with the listing.
    meta = github.get_json(f"repos/{REPO}/actions/artifacts/{entry['id']}")
    if not isinstance(meta, dict):
        raise PromotionError("artifact record is not an object")
    if (meta.get("id") != entry["id"] or type(meta.get("id")) is not int
            or meta.get("name") != entry["name"] or meta.get("expired") is not False
            or not _is_int(meta.get("size_in_bytes"))
            or not 0 < meta["size_in_bytes"] <= MAX_ARCHIVE_BYTES
            or meta.get("size_in_bytes") != entry.get("size_in_bytes")
            or meta.get("digest") != entry.get("digest")):
        raise PromotionError("artifact record mismatch")
    workflow_run = meta.get("workflow_run")
    if (not isinstance(workflow_run, dict)
            or workflow_run.get("id") != run_id
            or type(workflow_run.get("id")) is not int
            or workflow_run.get("repository_id") != repo_id
            or type(workflow_run.get("repository_id")) is not int
            or workflow_run.get("head_repository_id") != repo_id
            or type(workflow_run.get("head_repository_id")) is not int
            or workflow_run.get("head_branch") != STAGING_BRANCH
            or workflow_run.get("head_sha") != args.sha):
        raise PromotionError("artifact run binding mismatch")
    if not _is_digest(meta.get("digest")):
        raise PromotionError("artifact digest mismatch")

    # 4. The archive bytes, digest-bound before anything is unpacked.
    blob = github.get_bytes(f"repos/{REPO}/actions/artifacts/{entry['id']}/zip")
    if not isinstance(blob, bytes):
        raise PromotionError("artifact archive is not bytes")
    if len(blob) > MAX_ARCHIVE_BYTES or len(blob) != meta["size_in_bytes"]:
        raise PromotionError("artifact archive size mismatch")
    if "sha256:" + hashlib.sha256(blob).hexdigest() != meta["digest"]:
        raise PromotionError("artifact digest mismatch")

    # 5. The whole package validates before any mutation.
    receipt, modules = _validate_package(blob, args.sha, run_id, attempt, repo_id)

    # 6. Recheck the current source attempt: nothing moved underneath us,
    #    including the repository the run belongs to.
    reread = github.get_json(_run_route(run_id))
    if _check_run_identity(reread, args.sha, run_id, attempt) != repo_id:
        raise PromotionError("source run repository mismatch")
    _check_run_completed(reread)

    # 7. The staged image is a staging digest ref for this account, not a
    #    mutable tag and never the production image.
    staging_image = receipt["container"]["image"]
    if (_staging_image_pattern(args.account_id).fullmatch(staging_image) is None):
        raise PromotionError("staging image mismatch")
    digest = receipt["container"]["digest"]
    repository = f"{REGISTRY_HOST}/{args.account_id}/{PRODUCTION_APP}"
    expected = repository + "@" + digest

    # 8. Copy without rebuilding, then refuse any drift or foreign landing.
    produced = copy_image(staging_image, repository)
    if not isinstance(produced, str) or produced != expected:
        raise PromotionError("production image copy mismatch")

    # 9. Publish admission only now: admitted.json plus the exact modules.
    admitted = {"schema_version": SCHEMA_VERSION, "receipt": receipt,
                "production_image": produced}
    raw = (json.dumps(admitted, indent=2, sort_keys=True) + "\n").encode()
    out = Path(args.out)
    try:
        out.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=out.parent, prefix=".admit-") as temporary:
            staged = Path(temporary)
            for path in sorted(modules):
                target = staged / "modules" / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(modules[path])
            (staged / "admitted.json").write_bytes(raw)
            if out.exists():
                raise PromotionError("admission output already exists")
            os.rename(staged, out)
    except OSError:
        raise PromotionError("admission is unwritable") from None
    print("promotion admitted")
    return 0


def _parse_cli_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    commands = parser.add_subparsers(dest="command", required=True)

    certify = commands.add_parser("certify", help="package the served bundle")
    certify.add_argument("--modules", required=True)
    certify.add_argument("--entrypoint", required=True)
    certify.add_argument("--evidence", required=True)
    certify.add_argument("--config", required=True)
    certify.add_argument("--sha", required=True)
    certify.add_argument("--run-id", required=True)
    certify.add_argument("--attempt", required=True)
    certify.add_argument("--deploy-id", required=True)
    certify.add_argument("--readiness-result", required=True)
    certify.add_argument("--synthetic-result", required=True)
    certify.add_argument("--fault-mode", required=True)
    certify.add_argument("--out", required=True)

    admit = commands.add_parser("admit", help="admit the certified bundle")
    admit.add_argument("--sha", required=True)
    admit.add_argument("--run-id", required=True)
    admit.add_argument("--attempt", required=True)
    admit.add_argument("--account-id", required=True)
    admit.add_argument("--out", required=True)
    return parser.parse_args(argv)


class _GhApiGitHub:
    """Managed-GitHub reads through the ``gh`` CLI: bounded, read-only."""

    TIMEOUT_SECONDS = 60
    MAX_JSON_BYTES = 1 * 1024 * 1024
    MAX_ZIP_BYTES = MAX_ARCHIVE_BYTES
    _JSON_ROUTES = (
        re.compile(r"repos/" + re.escape(REPO) + r"/actions/runs/[0-9]{1,20}$"),
        re.compile(r"repos/" + re.escape(REPO) + r"/actions/runs/[0-9]{1,20}/artifacts\?per_page=100$"),
        re.compile(r"repos/" + re.escape(REPO) + r"/actions/artifacts/[0-9]{1,20}$"),
    )
    _ZIP_ROUTES = (
        re.compile(r"repos/" + re.escape(REPO) + r"/actions/artifacts/[0-9]{1,20}/zip$"),
    )

    def _allowed(self, route, patterns):
        return isinstance(route, str) and any(p.fullmatch(route) for p in patterns)

    def _run(self, route, cap):
        return _command(["gh", "api", "--method", "GET", route],
                        cap=cap, timeout=self.TIMEOUT_SECONDS)

    def get_json(self, route):
        if not self._allowed(route, self._JSON_ROUTES):
            raise PromotionError("managed github read refused")
        return _parse_json_capped(self._run(route, self.MAX_JSON_BYTES))

    def get_bytes(self, route):
        if not self._allowed(route, self._ZIP_ROUTES):
            raise PromotionError("managed github read refused")
        return self._run(route, self.MAX_ZIP_BYTES)


def _command(argv, *, cap, timeout=60, env=None, input_bytes=None):
    """Bound actual adapter output while reading it; never relay child bodies."""
    child = None
    try:
        child = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                 stdin=subprocess.PIPE if input_bytes is not None else subprocess.DEVNULL,
                                 env=env, start_new_session=True)
        if input_bytes is not None:
            if len(input_bytes) > 4096:
                raise PromotionError("managed command input over cap")
            child.stdin.write(input_bytes)
            child.stdin.close()
        deadline = time.monotonic() + timeout
        chunks = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise PromotionError("managed command timed out")
                for key, _ in selector.select(remaining):
                    data = os.read(key.fd, min(65536, cap + 1 - len(chunks)))
                    if not data:
                        selector.unregister(key.fileobj)
                    else:
                        chunks.extend(data)
                        if len(chunks) > cap:
                            raise PromotionError("managed command output over cap")
        code = child.wait(timeout=max(0.001, deadline - time.monotonic()))
        if code != 0:
            raise PromotionError(f"managed command refused (exit {code})")
        return bytes(chunks)
    except (OSError, subprocess.SubprocessError):
        raise PromotionError("managed command unavailable") from None
    finally:
        if child is not None:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            if child.stdout is not None:
                child.stdout.close()
            if child.stdin is not None and not child.stdin.closed:
                child.stdin.close()


def _copy_image(source, destination):
    """Native registry login + pull/tag/push, with both local and remote digest proof.

    Wrangler's normal 15-minute registry handshake uses the existing deploy
    credential, not a new long-lived key/grant. Its result stays in memory and
    a temporary Docker config; no credential is an argv value or log line.
    """
    account = os.environ.get("CLOUDFLARE_ACCOUNT_ID", "")
    if (_staging_image_pattern(account).fullmatch(source) is None
            or destination != f"{REGISTRY_HOST}/{account}/{PRODUCTION_APP}"):
        raise PromotionError("image copy scope refused")
    digest = source.rsplit("@", 1)[1]
    expected = destination + "@" + digest
    tag = destination + ":promotion-" + digest.split(":", 1)[1]
    creds = _parse_json_capped(_command(
        ["npx", "--no-install", "wrangler", "containers", "registries", "credentials",
         REGISTRY_HOST, "--pull", "--push", "--expiration-minutes", "15", "--json"], cap=32768))
    if (not isinstance(creds, dict) or creds.get("account_id") != account
            or creds.get("registry_host") != REGISTRY_HOST
            or not isinstance(creds.get("username"), str)
            or re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", creds["username"]) is None
            or not isinstance(creds.get("password"), str) or not creds["password"]
            or len(creds["password"].encode()) > 4096):
        raise PromotionError("registry authentication refused")
    with tempfile.TemporaryDirectory(prefix="promotion-docker-") as docker_config:
        env = dict(os.environ, DOCKER_CONFIG=docker_config)
        _command(["docker", "login", "--password-stdin", "--username", creds["username"], REGISTRY_HOST],
                 cap=32768, env=env, input_bytes=creds["password"].encode())
        _command(["docker", "pull", source], cap=1024 * 1024, timeout=600, env=env)
        _command(["docker", "tag", source, tag], cap=32768, env=env)
        _command(["docker", "push", tag], cap=1024 * 1024, timeout=600, env=env)
        digests = _parse_json_capped(_command(
            ["docker", "image", "inspect", "--format", "{{json .RepoDigests}}", tag], cap=32768, env=env))
        if not isinstance(digests, list) or expected not in digests:
            raise PromotionError("copied image digest drift")
        remote = _parse_json_capped(_command(
            ["docker", "manifest", "inspect", "--verbose", expected], cap=1024 * 1024, env=env))
        if (not isinstance(remote, dict) or not isinstance(remote.get("Descriptor"), dict)
                or remote["Descriptor"].get("digest") != digest):
            raise PromotionError("copied registry manifest digest drift")
    return expected


def load_admitted(path, sha, *, verify_modules=False):
    """Read and validate an ``admitted.json`` mapping for production reuse.

    Binds the receipt revision to ``sha`` and the run binding to the
    receipt's own source run/attempt. With ``verify_modules=True`` the
    sibling ``modules/`` directory must carry every listed module with
    matching size and sha256, including the entrypoint. Raises
    :class:`PromotionError` (a ``ValueError``) on any refusal; returns the
    validated admitted mapping.
    """
    raw = _read_local_capped(path)
    try:
        admitted = json.loads(raw.decode("utf-8"),
                              object_pairs_hook=_reject_duplicate_keys)
    except (DuplicateKeyError, ValueError, RecursionError):
        raise PromotionError("admission is not strict JSON") from None
    if not isinstance(admitted, dict):
        raise PromotionError("admission is not an object")
    if (set(admitted) != {"schema_version", "receipt", "production_image"}):
        raise PromotionError("admission has unexpected fields")
    if (admitted.get("schema_version") != 1
            or type(admitted.get("schema_version")) is not int):
        raise PromotionError("admission schema mismatch")
    receipt = admitted["receipt"]
    if not isinstance(receipt, dict):
        raise PromotionError("admission receipt mismatch")
    source = receipt.get("source")
    if not isinstance(source, dict):
        raise PromotionError("admission source mismatch")
    if not _is_sha(sha) or source.get("sha") != sha:
        raise PromotionError("admission sha mismatch")
    if not _is_int(source.get("run_id")) or not _is_int(source.get("run_attempt")):
        raise PromotionError("admission source mismatch")
    repository = receipt.get("repository")
    if (not isinstance(repository, dict)
            or not _is_int(repository.get("id"))
            or repository.get("full_name") != REPO):
        raise PromotionError("admission repository mismatch")
    _validate_receipt(receipt, sha, source["run_id"], source["run_attempt"],
                      repository["id"])
    production_image = admitted.get("production_image")
    if (not isinstance(production_image, str)
            or _production_image_pattern().fullmatch(production_image) is None):
        raise PromotionError("admission production image mismatch")
    expected_image = receipt["container"]["image"].replace(
        "/" + STAGING_APP + "@", "/" + PRODUCTION_APP + "@")
    if production_image != expected_image:
        raise PromotionError("admission production image mismatch")
    if verify_modules:
        modules = _collect_modules(Path(path).parent / "modules")
        if set(modules) != {member["path"] for member in receipt["worker"]["modules"]}:
            raise PromotionError("admitted module inventory mismatch")
        for member in receipt["worker"]["modules"]:
            data = modules[member["path"]]
            if len(data) != member["size"]:
                raise PromotionError("admitted module size mismatch")
            if hashlib.sha256(data).hexdigest() != member["sha256"]:
                raise PromotionError("admitted module hash mismatch")
    return admitted


# Explicit module-type rules for serving the admitted bundle verbatim:
# only these suffixes resolve, so an unexpected member refuses at deploy
# instead of bundling or silently dropping. The admitted directory holds
# exactly the certified modules (admit never writes README/map/dotfiles),
# so discovery below stays inside the allowlist.
_BUNDLE_RULES = (
    {"type": "ESModule", "globs": ["**/*.js", "**/*.mjs"]},
    {"type": "CompiledWasm", "globs": ["**/*.wasm"]},
    {"type": "Text", "globs": ["**/*.txt"]},
    {"type": "Data", "globs": ["**/*.bin"]},
)


def configure_bundle(config, modules_dir, entrypoint):
    """Point a parsed Wrangler config at saved modules without rebuilding.

    Mutates ``config`` in place and returns it: ``main`` becomes the saved
    entrypoint, ``no_bundle`` serves the certified bytes verbatim,
    ``base_dir`` scopes resolution to the saved (admit-produced) directory,
    ``find_additional_modules`` discovers only inside that allowlisted
    directory, ``rules`` maps exactly the servable suffixes,
    ``preserve_file_names`` keeps the certified names, and ``minify``
    stays off. Compatibility stays pinned (refuses when absent) and any
    custom ``build`` fallback is removed. Raises :class:`PromotionError`
    on bad input.
    """
    if not isinstance(config, dict):
        raise PromotionError("bundle config is not an object")
    if not _safe_module_path(entrypoint):
        raise PromotionError("bundle entrypoint mismatch")
    root = Path(modules_dir).resolve()
    if not (root / entrypoint).is_file() or (root / entrypoint).is_symlink():
        raise PromotionError("bundle entrypoint is missing")
    if (not isinstance(config.get("compatibility_date"), str)
            or not isinstance(config.get("compatibility_flags"), list)):
        raise PromotionError("bundle compatibility is unpinned")
    config.pop("build", None)
    config["main"] = str(root / entrypoint)
    config["no_bundle"] = True
    config["base_dir"] = str(root)
    config["find_additional_modules"] = True
    config["rules"] = [dict(rule, globs=list(rule["globs"]), fallthrough=False)
                       for rule in _BUNDLE_RULES]
    config["preserve_file_names"] = True
    config["minify"] = False
    for environment in config.get("env", {}).values():
        # Environment-specific source/build overrides must not select different
        # bytes. Runtime bindings and compatibility settings stay independent.
        for key in ("main", "build", "no_bundle", "base_dir", "find_additional_modules",
                    "rules", "preserve_file_names", "minify"):
            environment.pop(key, None)
    return config


def main(argv=None, *, github=None, copy_image=None):
    """Entry point and test seam: ``certify`` or ``admit``. Returns a status."""
    args = _parse_cli_args(argv)
    try:
        if args.command == "certify":
            return _certify(args, github if github is not None else _GhApiGitHub())
        if github is None:
            github = _GhApiGitHub()
        return _admit(args, github, copy_image if copy_image is not None else _copy_image)
    except PromotionError as error:
        return _refuse(f"bot promotion refused: {error}")
    except (OSError, ValueError, RecursionError, RuntimeError, zipfile.BadZipFile, zlib.error):
        return _refuse("bot promotion refused: invalid input or adapter response")


if __name__ == "__main__":
    raise SystemExit(main())
