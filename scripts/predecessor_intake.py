"""Bind a verified predecessor handoff to its disposable staging config."""

from hashlib import sha256
from pathlib import Path

from bot_promotion import MAX_RECEIPT_BYTES, PromotionError, collect_modules
from build_predecessor import SOURCE, digest
from pre_target import TargetError, make_config, require
from script_intake import decode_json, read_with_overflow


# Keep the local OCI read within the protected predecessor verifier's bound.
MAX_IMAGE_BYTES = 128 * 1024 * 1024


def admit_predecessor(target, *, now, expected_producer_run_id, verified_receipt, material_root, plan):
    """Return the checked offline config for use within the handoff lifetime.

    Supply the producer run selected independently of the disposable target's
    reservation. The receipt must come from verification of the pinned ZIP.
    Keep the materializer's directory private and unchanged through use of the
    returned config and image archive. The target must come from the trusted
    reservation. This check performs only local reads.
    """
    require(type(expected_producer_run_id) is int and 0 < expected_producer_run_id < 10**20,
            "invalid_predecessor_producer_run")
    require(isinstance(verified_receipt, dict)
            and verified_receipt.get("BOT_BUILD_ID") == f"{expected_producer_run_id}-1"
            and verified_receipt.get("source_sha") == SOURCE, "unexpected_predecessor_build")

    root = Path(material_root)
    manifest = root / "build.json"
    image = root / "image.oci.tar"
    require(not root.is_symlink() and not manifest.is_symlink() and manifest.is_file(),
            "invalid_predecessor_handoff_manifest")
    try:
        with manifest.open("rb") as stream:
            raw_manifest = read_with_overflow(stream, MAX_RECEIPT_BYTES)
        require(len(raw_manifest) <= MAX_RECEIPT_BYTES, "invalid_predecessor_handoff_manifest")
        receipt = decode_json(raw_manifest)
    except (OSError, ValueError, RecursionError):
        raise TargetError("invalid_predecessor_handoff_manifest") from None
    require(receipt == verified_receipt, "predecessor_handoff_manifest_mismatch")

    expected_plan = make_config(target, now=now, receipt=verified_receipt, material_root=root,
                                synthetic_service=target.service_name)
    require(type(plan) is dict and plan == expected_plan, "predecessor_plan_mismatch")

    try:
        modules = collect_modules(root / "worker")
    except (OSError, PromotionError):
        raise TargetError("invalid_predecessor_handoff_worker") from None
    require({name: sha256(data).hexdigest() for name, data in modules.items()}
            == verified_receipt.get("worker_modules_sha256"), "predecessor_handoff_worker_mismatch")

    try:
        require(0 < image.stat().st_size <= MAX_IMAGE_BYTES, "invalid_predecessor_handoff_image")
        image_digest = digest(image)
    except OSError:
        raise TargetError("invalid_predecessor_handoff_image") from None
    require(image_digest == verified_receipt.get("image_archive_sha256"), "predecessor_handoff_image_mismatch")
    return expected_plan
