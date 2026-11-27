"""Offline retention and private-ingress evidence for a native PRE replacement."""

from dataclasses import dataclass
from hashlib import sha256
import json
from uuid import UUID

from pre_native_first_create import InitialDeployment, PrivateBindingPin, PrivateBindingReadback
from pre_start_stop import Deployment, EffectWindow
from pre_target import Target, require


@dataclass(frozen=True)
class RetainedVersionInventory:
    """Authenticated version inventory for the exact reserved predecessor.

    The protected caller supplies a fresh, complete inventory in the reserved
    account and independent history for `predecessor_version`. Set
    `predecessor_untouched` only if that history excludes pruning and overwrite
    through the observation. Current UUID membership alone cannot prove this.
    """

    target: Target
    predecessor_version: UUID
    retained_versions: tuple[UUID, ...]
    inventory_complete: bool
    predecessor_untouched: bool


@dataclass(frozen=True)
class NativeRetentionReceipt:
    retention_digest: str


def _exact_target(value):
    return (type(value) is Target and type(value.scope) is str and type(value.shape) is str
            and type(value.run_id) is int and type(value.started_at) is int)


def _matches_target(value, target):
    return _exact_target(value) and value == target


def _matches_destination(evidence, target, environment):
    return (_matches_target(evidence.target, target)
            and type(evidence.worker_service) is str and evidence.worker_service == target.worker_name
            and type(evidence.environment) is str and evidence.environment == environment)


def _valid_uuid(value):
    if type(value) is not UUID:
        return False
    number = getattr(value, "int", None)
    return type(number) is int and 0 < number < 2**128


def verify_native_pre_retention_guard(
    target: Target, *, reserved_environment: str, window: EffectWindow, now: int,
    promotion_disabled: bool, predecessor: Deployment, inventory: RetainedVersionInventory,
    initial: InitialDeployment, binding: PrivateBindingPin, serving: PrivateBindingReadback,
) -> NativeRetentionReceipt:
    """Verify evidence for one reservation and return a digest of its pins.

    The protected caller supplies the reservation, clock, promotion hold and
    authenticated observations. The receipt carries no authority for a provider
    effect. The caller still enforces ownership, isolation and the stop fence.
    """
    require(promotion_disabled is True, "pre_native_promotion_not_disabled")
    require(_exact_target(target), "invalid_pre_native_target")
    # Revalidate frozen values before accepting evidence that refers to them.
    Target(target.scope, target.run_id, target.started_at, target.shape)
    require(type(window) is EffectWindow and _matches_target(window.target, target),
            "invalid_pre_effect_window")
    window = EffectWindow(target, window.cutoff)
    window.require_open(now)

    require(type(predecessor) is Deployment and _matches_target(predecessor.target, target)
            and all(_valid_uuid(value) for value in
                    (predecessor.start_id, predecessor.worker_version, predecessor.container_id)),
            "invalid_pre_native_predecessor")
    require(type(inventory) is RetainedVersionInventory and _matches_target(inventory.target, target)
            and _valid_uuid(inventory.predecessor_version)
            and inventory.predecessor_version == predecessor.worker_version
            and inventory.inventory_complete is True and inventory.predecessor_untouched is True,
            "pre_native_retention_unconfirmed")
    versions = inventory.retained_versions
    require(type(versions) is tuple and all(_valid_uuid(version) for version in versions),
            "invalid_pre_native_retained_versions")
    require(predecessor.worker_version in versions, "pre_native_predecessor_missing")

    require(type(reserved_environment) is str and reserved_environment.isprintable()
            and reserved_environment == reserved_environment.strip(), "invalid_pre_native_environment")
    require(type(initial) is InitialDeployment and _matches_destination(initial, target, reserved_environment),
            "wrong_pre_native_create_target")
    require(_valid_uuid(initial.version) and initial.version != predecessor.worker_version,
            "invalid_pre_native_replacement_version")
    require(initial.complete is True and initial.workers_dev is False and initial.preview_urls is False
            and type(initial.routes) is tuple and not initial.routes
            and type(initial.custom_domains) is tuple and not initial.custom_domains
            and initial.closed_from_first_effect is True, "pre_native_ingress_unconfirmed")
    require(type(binding) is PrivateBindingPin and _matches_destination(binding, target, reserved_environment)
            and _valid_uuid(binding.binding_id) and _valid_uuid(binding.version)
            and binding.version == initial.version, "wrong_pre_native_binding")
    require(type(serving) is PrivateBindingReadback and _matches_destination(serving, target, reserved_environment)
            and _valid_uuid(serving.binding_id) and serving.binding_id == binding.binding_id
            and _valid_uuid(serving.version) and serving.version == initial.version
            and serving.via_private_binding is True and serving.independent is True,
            "pre_native_dispatch_unconfirmed")

    pins = {
        "schema": "pre-native-retention-v1",
        "target": [target.scope, target.run_id, target.started_at, target.shape],
        "cutoff": window.cutoff,
        "worker_service": target.worker_name,
        "environment": reserved_environment,
        "predecessor_start_id": str(predecessor.start_id),
        "predecessor_version": str(predecessor.worker_version),
        "predecessor_container_id": str(predecessor.container_id),
        "replacement_version": str(initial.version),
        "binding_id": str(binding.binding_id),
    }
    canonical = json.dumps(pins, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
    return NativeRetentionReceipt("sha256:" + sha256(canonical.encode("ascii")).hexdigest())
