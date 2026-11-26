"""Offline copy and readback contract for the verified predecessor OCI image."""

from dataclasses import dataclass
from hashlib import file_digest, sha256
import os
from pathlib import Path
import re
import tarfile

from bot_promotion import DIGEST, MAX_RECEIPT_BYTES
from build_predecessor import DESCRIPTOR_LIMIT, image_manifest
from predecessor_bundle import BUILD, IMAGE, MAX_IMAGE_BYTES
from script_intake import read_with_overflow
from staging_rollout import GateError, decode, mapping, require

MEDIA_TYPE = "application/vnd.oci.image.manifest.v1+json"
_DNS_LABEL = r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?"
_STAGING_REPOSITORY = re.compile(
    rf"(?:{_DNS_LABEL}\.)+{_DNS_LABEL}/[a-z0-9]+(?:[._-][a-z0-9]+)*/"
    r"two-bot-next-attendance-predecessor-staging"
)


@dataclass(frozen=True)
class PublicationPlan:
    copy_argv: tuple[str, ...]
    reference: str


def plan_predecessor_image(root: Path, *, staging_repository: str) -> PublicationPlan:
    """Plan a copy inside the private materialization context from #2562.

    The protected caller supplies its authorized staging repository and keeps
    the verified files stable until the copy finishes. The plan executes no I/O
    beyond reading those local files.
    """
    require(isinstance(staging_repository, str) and len(staging_repository) <= 255
            and _STAGING_REPOSITORY.fullmatch(staging_repository), "predecessor_registry_scope_refused")
    archive = (root / IMAGE).absolute()
    require(":" not in str(archive), "invalid_predecessor_image_path")
    try:
        with (root / BUILD).open("rb") as stream:
            raw = read_with_overflow(stream, MAX_RECEIPT_BYTES)
        require(0 < len(raw) <= MAX_RECEIPT_BYTES, "invalid_predecessor_image_receipt")
        receipt = mapping(decode(raw))
        with archive.open("rb") as image:
            require(0 < os.fstat(image.fileno()).st_size <= MAX_IMAGE_BYTES, "invalid_predecessor_image_archive")
            require(file_digest(image, "sha256").hexdigest() == receipt.get("image_archive_sha256"),
                    "predecessor_image_archive_digest_mismatch")
            image.seek(0)
            manifest = image_manifest(None, {
                "org.opencontainers.image.revision": receipt["BOT_BUILD_REVISION"],
                "com.togetherweown.build-id": receipt["BOT_BUILD_ID"],
                "com.togetherweown.deploy-id": receipt["BOT_DEPLOY_ID"],
            }, fileobj=image)
    except (KeyError, OSError, ValueError, tarfile.TarError):
        raise GateError("invalid_predecessor_image_material") from None
    require(DIGEST.fullmatch(manifest) and manifest == receipt.get("image_manifest_digest"),
            "predecessor_image_manifest_mismatch")
    reference = f"{staging_repository}@{manifest}"
    return PublicationPlan(
        ("skopeo", "copy", "--preserve-digests", "--dest-tls-verify=true",
         f"oci-archive:{archive}", f"docker://{reference}"),
        reference,
    )


def verify_registry_manifest(plan: PublicationPlan, *, reference: str, descriptor: dict, body: bytes) -> str:
    """Verify a direct, authenticated digest GET from the planned repository.

    The adapter supplies the effective response reference, the registry media
    type, any reported digest, and the unchanged response bytes. Pass only an
    HTTP 200 from the exact scope, with body reads capped at DESCRIPTOR_LIMIT + 1.
    """
    require(reference == plan.reference, "predecessor_registry_scope_mismatch")
    descriptor = mapping(descriptor)
    expected = plan.reference.rsplit("@", 1)[1]
    size = descriptor.get("size")
    require(descriptor.get("mediaType") == MEDIA_TYPE and descriptor.get("digest", expected) == expected
            and type(size) is int and isinstance(body, bytes) and 0 < len(body) == size <= DESCRIPTOR_LIMIT,
            "predecessor_registry_descriptor_mismatch")
    require("sha256:" + sha256(body).hexdigest() == expected, "predecessor_registry_manifest_mismatch")
    return plan.reference
