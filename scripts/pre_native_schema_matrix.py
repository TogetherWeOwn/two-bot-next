"""Verify authenticated native PRE proof summaries for both schema shapes offline."""

from dataclasses import dataclass, field

from pre_target import DIGEST, LAST_UNIX_SECOND, SHA, Target, require


_SHAPES = ("0473/0465", "0475/0465")


@dataclass(frozen=True)
class NativePreSchemaProof:
    """One reservation's independently verified proof stages and receipt IDs."""

    target: Target
    source_sha: str
    container_image_digest: str
    verified_at: int
    private_first_create_ok: bool | None = None
    exact_takeover_ok: bool | None = None
    independent_stop_ok: bool | None = None
    private_first_create_receipt_id: str | None = field(default=None, repr=False)
    exact_takeover_receipt_id: str | None = field(default=None, repr=False)
    independent_stop_receipt_id: str | None = field(default=None, repr=False)


@dataclass(frozen=True)
class NativePreSchemaMatrixVerdict:
    enabled: bool
    verified_shapes: tuple[str, ...] = ()


def verify_native_pre_schema_matrix(
    *, enabled: bool = False, production_auto_promote_disabled: bool = False,
    observed_at: int | None = None, source_sha: str | None = None,
    container_image_digest: str | None = None, proofs: tuple[NativePreSchemaProof, ...] = (),
) -> NativePreSchemaMatrixVerdict:
    """Return a disabled or verified verdict, or raise TargetError with a refusal code.

    The protected caller supplies the promotion hold, common clock and separate
    build pins, and authenticates each stage receipt against its exact target,
    build and proof stage before assembling the summary. A passing verdict
    records coverage and grants no effect authority.
    """
    if enabled is not True:
        return NativePreSchemaMatrixVerdict(enabled=False)

    require(production_auto_promote_disabled is True, "pre_native_matrix_promotion_not_disabled")
    require(type(observed_at) is int and 0 < observed_at <= LAST_UNIX_SECOND,
            "invalid_pre_native_matrix_clock")
    require(type(source_sha) is str and SHA.fullmatch(source_sha), "invalid_pre_native_matrix_source_sha")
    require(type(container_image_digest) is str and DIGEST.fullmatch(container_image_digest),
            "invalid_pre_native_matrix_image_digest")

    require(type(proofs) is tuple, "invalid_pre_native_matrix_packet")
    by_shape = {}
    for proof in proofs:
        require(type(proof) is NativePreSchemaProof, "invalid_pre_native_matrix_proof")
        target = proof.target
        require(type(target) is Target and type(target.scope) is str, "invalid_pre_native_matrix_target")
        shape = target.shape
        require(type(shape) is str and shape in _SHAPES, "unsupported_pre_shape")
        Target.__post_init__(target)
        require(shape not in by_shape, "duplicate_pre_native_matrix_shape")
        by_shape[shape] = proof
    require(len(by_shape) == len(_SHAPES), "missing_pre_native_matrix_shape")

    first, second = (by_shape[shape].target for shape in _SHAPES)
    require(first.scope != second.scope and first.run_id != second.run_id
            and first.worker_name != second.worker_name, "pre_native_matrix_target_overlap")

    receipts = set()
    for shape in _SHAPES:
        proof = by_shape[shape]
        require(type(proof.source_sha) is str and proof.source_sha == source_sha,
                "pre_native_matrix_source_sha_mismatch")
        require(type(proof.container_image_digest) is str and proof.container_image_digest == container_image_digest,
                "pre_native_matrix_image_digest_mismatch")
        require(observed_at <= proof.target.expires_at, "pre_native_matrix_target_expired")
        require(type(proof.verified_at) is int and proof.target.started_at <= proof.verified_at <= observed_at,
                "invalid_pre_native_matrix_proof_clock")

        require(proof.private_first_create_ok is True, "pre_native_first_create_unconfirmed")
        require(proof.exact_takeover_ok is True, "pre_native_takeover_unconfirmed")
        require(proof.independent_stop_ok is True, "pre_native_stop_unconfirmed")
        for receipt in (proof.private_first_create_receipt_id, proof.exact_takeover_receipt_id,
                        proof.independent_stop_receipt_id):
            require(type(receipt) is str and receipt and receipt == receipt.strip(),
                    "invalid_pre_native_matrix_receipt")
            require(receipt not in receipts, "duplicate_pre_native_matrix_receipt")
            receipts.add(receipt)

    return NativePreSchemaMatrixVerdict(enabled=True, verified_shapes=_SHAPES)
