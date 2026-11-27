"""Dormant verifier for one native PRE deployment's effective secret bindings.

The caller supplies immutable, authenticated metadata from the reservation,
promotion variable, inventory and stop authorities. The inventory covers
effective bindings, including inherited bindings and aliases. This verifier
does not read credentials, contact providers or authorize any live action.
"""

from dataclasses import dataclass
import re
from uuid import UUID

from pre_inventory import RESOURCE_NAME
from pre_start_stop import Deployment, EffectWindow
from pre_target import DIGEST, Target, check_clock, require


SECRET_NAME = re.compile(r"[A-Z_][A-Z0-9_]{0,127}")


@dataclass(frozen=True, repr=False)
class BindingScope:
    """Reserved Worker script and environment, including its deployed version."""

    deployment: Deployment
    worker_service: str
    environment: str


@dataclass(frozen=True, repr=False)
class SecretScopeReservation:
    """Policy pinned independently of the observed PRE inventory.

    Protected names and digests come from complete production and live staging
    inventories. All digests use the same fingerprint scheme for effective
    credential or backing resource identity across scopes.
    """

    scope: BindingScope
    window: EffectWindow
    allowed_secret_names: frozenset[str]
    protected_secret_names: frozenset[str]
    production_binding_digests: frozenset[str]
    live_staging_binding_digests: frozenset[str]


@dataclass(frozen=True, repr=False)
class PromotionReadback:
    """Readback of PRODUCTION_AUTO_PROMOTE_DISABLED for this repository."""

    production_auto_promote_disabled: bool
    observed_at: int


@dataclass(frozen=True, repr=False)
class SecretBinding:
    """An opaque version ID and comparable digest, both supplied as metadata."""

    name: str
    secret_version_id: str
    binding_digest: str
    disposable: bool
    plaintext_exposed: bool
    live_discord_bot_token: bool
    shared_production_resource: bool  # Includes DB, KV and R2 bindings.


@dataclass(frozen=True, repr=False)
class BindingInventory:
    scope: BindingScope
    observed_at: int
    complete: bool
    bindings: tuple[SecretBinding, ...]


@dataclass(frozen=True, repr=False)
class BindingTeardown:
    """Exact-scope absence after confirmed stop, with restarts fenced."""

    scope: BindingScope
    stopped_at: int
    bindings_removed: bool
    removed_at: int
    observed_at: int
    remaining_secret_count: int


@dataclass(frozen=True)
class SecretScopeVerdict:
    enabled: bool
    verified: bool


def _matches(value, pattern):
    return type(value) is str and pattern.fullmatch(value) is not None


def _valid_target(target):
    return (type(target) is Target and type(target.scope) is str and type(target.shape) is str
            and type(target.run_id) is int and type(target.started_at) is int)


def _valid_scope(scope):
    if type(scope) is not BindingScope or type(scope.deployment) is not Deployment:
        return False
    deployment = scope.deployment
    target = deployment.target
    return (_valid_target(target)
            and all(type(value) is UUID and value.int
                    for value in (deployment.start_id, deployment.worker_version, deployment.container_id))
            and type(scope.worker_service) is str and scope.worker_service == target.worker_name
            and _matches(scope.environment, RESOURCE_NAME))


def _verify_bindings(reservation, bindings):
    allowed, protected = reservation.allowed_secret_names, reservation.protected_secret_names
    production = reservation.production_binding_digests
    staging = reservation.live_staging_binding_digests
    for values, pattern in ((allowed, SECRET_NAME), (protected, SECRET_NAME),
                            (production, DIGEST), (staging, DIGEST)):
        require(type(values) is frozenset and all(_matches(value, pattern) for value in values),
                "invalid_pre_secret_policy")

    names, versions, digests = set(), set(), set()
    for binding in bindings:
        require(type(binding) is SecretBinding and _matches(binding.name, SECRET_NAME),
                "invalid_pre_secret_binding")
        require(binding.name in allowed and binding.name not in protected,
                "forbidden_pre_secret_name")
        require(binding.disposable is True and binding.plaintext_exposed is False
                and binding.live_discord_bot_token is False and binding.shared_production_resource is False,
                "pre_secret_binding_not_isolated")
        version, digest = binding.secret_version_id, binding.binding_digest
        require(_matches(version, RESOURCE_NAME) and _matches(digest, DIGEST), "invalid_pre_secret_identity")
        require(digest not in production and digest not in staging, "shared_pre_secret_digest")
        require(binding.name not in names and version not in versions and digest not in digests,
                "duplicate_pre_secret_binding")
        names.add(binding.name)
        versions.add(version)
        digests.add(digest)


def verify_native_pre_secret_scope(*, enabled=False, reservation=None, promotion=None, inventory=None,
                                   teardown=None, readback_not_before=None, now=None):
    """Return a redacted verdict or raise TargetError with a fixed reason code.

    The caller supplies trusted time and the minimum acceptable timestamp for
    the current promotion-variable read. All timestamps use the same clock.
    """
    if enabled is not True:
        return SecretScopeVerdict(False, False)

    require(type(reservation) is SecretScopeReservation and _valid_scope(reservation.scope)
            and type(reservation.window) is EffectWindow
            and _valid_target(reservation.window.target)
            and reservation.window.target == reservation.scope.deployment.target,
            "invalid_pre_secret_reservation")
    scope, window = reservation.scope, reservation.window
    target = scope.deployment.target
    check_clock(target, now)
    check_clock(target, readback_not_before)
    require(readback_not_before <= now, "invalid_pre_readback_window")
    require(type(promotion) is PromotionReadback and promotion.production_auto_promote_disabled is True
            and type(promotion.observed_at) is int and readback_not_before <= promotion.observed_at <= now,
            "pre_auto_promote_disable_unconfirmed")

    require(type(inventory) is BindingInventory and _valid_scope(inventory.scope) and inventory.scope == scope,
            "wrong_pre_secret_inventory_scope")
    require(inventory.complete is True and type(inventory.bindings) is tuple
            and type(inventory.observed_at) is int and target.started_at <= inventory.observed_at < window.cutoff,
            "unconfirmed_pre_secret_inventory")
    _verify_bindings(reservation, inventory.bindings)

    require(type(teardown) is BindingTeardown and _valid_scope(teardown.scope) and teardown.scope == scope,
            "wrong_pre_secret_teardown_scope")
    require(teardown.bindings_removed is True and type(teardown.remaining_secret_count) is int
            and teardown.remaining_secret_count == 0
            and all(type(value) is int for value in (teardown.stopped_at, teardown.removed_at, teardown.observed_at))
            and inventory.observed_at <= teardown.stopped_at <= teardown.removed_at <= window.cutoff
            and teardown.removed_at <= teardown.observed_at <= now,
            "pre_secret_teardown_unconfirmed")
    return SecretScopeVerdict(True, True)
