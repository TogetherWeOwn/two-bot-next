"""Offline evidence checks for the replacement native-scheduled PRE Worker.

The protected caller supplies authenticated evidence in the reserved provider
account. This verifier makes no provider calls and grants no launch authority.
The archived upload guard, independent fence and effect-time checks still apply
in their respective paths.
"""

from dataclasses import dataclass
from uuid import UUID

from pre_target import Target, TargetError, require


@dataclass(frozen=True)
class InitialDeployment:
    """Authenticated creation receipt and fresh ingress inventory.

    The same reserved service and environment must underlie both the receipt
    and the complete ingress inventory (including shared routes and domains).
    `history_complete` covers prior versions even if later removed. Set
    `closed_from_first_effect` only with provider contract and operation-order
    evidence that ingress was already closed when the first upload, version
    creation or deploy could expose the Worker. A current snapshot cannot
    supply either historical assertion.
    """

    target: Target
    worker_service: str
    environment: str
    version: UUID
    workers_dev: bool
    preview_urls: bool
    complete: bool
    routes: tuple[str, ...]
    custom_domains: tuple[str, ...]
    history_complete: bool
    prior_versions: tuple[UUID, ...]
    closed_from_first_effect: bool


@dataclass(frozen=True)
class PrivateBindingPin:
    """Protected pin for one private Service Binding before dispatch.

    `binding_id` is the caller's identity for the exact binding handle, including
    its caller context and configuration generation. It is not a provider ID.
    """

    target: Target
    worker_service: str
    environment: str
    binding_id: UUID
    version: UUID


@dataclass(frozen=True)
class PrivateBindingReadback:
    """Fresh actual destination and serving version from private dispatch.

    The protected caller independently obtains the serving UUID from the
    response over the identified binding, with no alternate dispatch path or
    redirects. A routing configuration, upload response or copied pin cannot
    supply this observation. The caller revalidates for each bounded dispatch.
    """

    target: Target
    worker_service: str
    environment: str
    binding_id: UUID
    version: UUID
    via_private_binding: bool
    independent: bool


def _is_target(value):
    return type(value) is Target and type(value.scope) is str and type(value.shape) is str


def _matches_destination(evidence, target, environment):
    return (_is_target(evidence.target) and evidence.target == target
            and type(evidence.worker_service) is str and evidence.worker_service == target.worker_name
            and type(evidence.environment) is str and evidence.environment == environment)


def _valid_uuid(value):
    return type(value) is UUID and value.int != 0


def _empty_tuple(value):
    return type(value) is tuple and value == ()


def verify_native_first_create(target: Target, *, reserved_environment: str,
                               initial: InitialDeployment, binding: PrivateBindingPin,
                               serving: PrivateBindingReadback) -> UUID:
    """Return the verified initial serving UUID or raise TargetError.

    Take the full Target and explicit environment from the protected
    reservation, not from the create, binding or serving evidence. The caller
    must still enforce reservation ownership, cutoff, isolation and stop at the
    effect boundary before any Container start.
    """
    if not _is_target(target):
        raise TargetError("invalid_pre_native_target")
    require(type(reserved_environment) is str and reserved_environment and reserved_environment.isprintable()
            and reserved_environment == reserved_environment.strip(),
            "invalid_pre_native_environment")
    require(type(initial) is InitialDeployment
            and _matches_destination(initial, target, reserved_environment), "wrong_pre_native_create_target")
    require(_valid_uuid(initial.version), "invalid_pre_native_initial_version")
    require(initial.complete is True and initial.workers_dev is False and initial.preview_urls is False
            and _empty_tuple(initial.routes) and _empty_tuple(initial.custom_domains), "pre_native_ingress_unconfirmed")
    require(initial.history_complete is True and _empty_tuple(initial.prior_versions)
            and initial.closed_from_first_effect is True, "pre_native_first_create_unconfirmed")
    require(type(binding) is PrivateBindingPin and _matches_destination(binding, target, reserved_environment)
            and _valid_uuid(binding.binding_id), "wrong_pre_native_binding")
    require(_valid_uuid(binding.version) and binding.version == initial.version, "wrong_pre_native_binding_version")
    require(type(serving) is PrivateBindingReadback and _matches_destination(serving, target, reserved_environment)
            and _valid_uuid(serving.binding_id) and serving.binding_id == binding.binding_id
            and serving.via_private_binding is True and serving.independent is True, "pre_native_dispatch_unconfirmed")
    require(_valid_uuid(serving.version) and serving.version == initial.version, "wrong_pre_native_serving_version")
    return initial.version
