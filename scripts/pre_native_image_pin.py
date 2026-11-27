"""Dormant native PRE verifier with injected, protected runtime boundaries."""

from collections.abc import Callable
from dataclasses import dataclass, field
import re
from uuid import UUID

from pre_start_stop import Deployment, EffectWindow
from pre_target import DIGEST, NAMESPACE_ID, Target, TargetError, require


_LABEL = r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?"
_COMPONENT = r"[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*"
_REPOSITORY = re.compile(rf"{_LABEL}(?:\.{_LABEL})+/{_COMPONENT}(?:/{_COMPONENT})*")
_OBJECT_ID = re.compile(r"[0-9a-f]{64}")
_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}")
_CONTAINER_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}")
_DISCORD_ORIGIN = r"://(?:127\.0\.0\.1|\[::1\])(?::(?P<port>[1-9][0-9]{0,4}))?/?"
_DISCORD_GATEWAY = re.compile(r"ws" + _DISCORD_ORIGIN)
_DISCORD_REST = re.compile(r"http" + _DISCORD_ORIGIN)


@dataclass(frozen=True)
class NativeScope:
    deployment: Deployment
    account_id: str
    environment: str
    namespace_id: str
    object_id: str


@dataclass(frozen=True)
class ImageIdentity:
    reference: str
    manifest_digest: str
    config_digest: str
    platform: str


@dataclass(frozen=True)
class LoopbackFixtures:
    discord_gateway_url: str = field(repr=False)
    discord_api_base: str = field(repr=False)
    postgres_host: str
    postgres_port: int
    postgres_disposable: bool
    postgres_http_intercept: bool
    in_container: bool
    enable_internet: bool


@dataclass(frozen=True)
class NativeImageAdmission:
    scope: NativeScope
    window: EffectWindow
    image_alias: str
    image: ImageIdentity
    fixtures: LoopbackFixtures


@dataclass(frozen=True)
class ImageMapReadback:
    scope: NativeScope
    images: dict[str, str] = field(repr=False)


@dataclass(frozen=True)
class NativeStart:
    scope: NativeScope
    container_id: str  # Provider ID of the running instance.
    started_at: int


@dataclass(frozen=True)
class NativeInspect:
    scope: NativeScope
    container_id: str
    running: bool
    image_digest: str
    fixtures_ready_in_container: bool


@dataclass(frozen=True)
class NativeImagePinResult:
    verified: bool
    container_id: str | None = None
    image_digest: str | None = None


_DISABLED = NativeImagePinResult(False)


def _matches(pattern, value):
    return type(value) is str and pattern.fullmatch(value)


def _scope_valid(scope):
    if type(scope) is not NativeScope or type(scope.deployment) is not Deployment:
        return False
    deployment = scope.deployment
    return (type(deployment.target) is Target
            and all(type(value) is UUID and value.int for value in (
                deployment.start_id, deployment.worker_version, deployment.container_id))
            and _matches(NAMESPACE_ID, scope.account_id) and _matches(_NAME, scope.environment)
            and _matches(NAMESPACE_ID, scope.namespace_id) and _matches(_OBJECT_ID, scope.object_id))


def _image_valid(image):
    if type(image) is not ImageIdentity or type(image.reference) is not str or len(image.reference) > 327:
        return False
    repository, _, digest = image.reference.partition("@")
    return (len(repository) <= 255 and _REPOSITORY.fullmatch(repository)
            and _matches(DIGEST, image.manifest_digest) and digest == image.manifest_digest
            and _matches(DIGEST, image.config_digest)
            and type(image.platform) is str and image.platform == "linux/amd64")


def _loopback(pattern, url):
    match = _matches(pattern, url)
    return match and (match["port"] is None or int(match["port"]) <= 65535)


def _check_fixtures(fixtures):
    require(type(fixtures) is LoopbackFixtures and fixtures.enable_internet is False,
            "pre_native_image_internet_unconfirmed")
    require(fixtures.in_container is True and _loopback(_DISCORD_GATEWAY, fixtures.discord_gateway_url)
            and _loopback(_DISCORD_REST, fixtures.discord_api_base), "pre_native_image_discord_unconfirmed")
    require(type(fixtures.postgres_host) is str and fixtures.postgres_host in ("127.0.0.1", "::1")
            and type(fixtures.postgres_port) is int and 0 < fixtures.postgres_port <= 65535
            and fixtures.postgres_disposable is True and fixtures.postgres_http_intercept is False,
            "pre_native_image_postgres_unconfirmed")


def _call(operation, *args, failure):
    try:
        return operation(*args)
    except Exception:
        pass
    raise TargetError(failure)


def _now(window, clock, at_least):
    now = _call(clock, failure="pre_native_image_clock_unconfirmed")
    window.require_open(now)
    require(now >= at_least, "pre_native_image_clock_regressed")
    return now


def verify_native_image_pin(
    *, enabled: bool = False, admission: NativeImageAdmission | None = None,
    read_images: Callable[[NativeScope], ImageMapReadback] | None = None,
    resolve_image: Callable[[str], ImageIdentity] | None = None,
    clock: Callable[[], int] | None = None,
    start: Callable[[NativeImageAdmission, str], NativeStart] | None = None,
    inspect: Callable[[NativeScope, str], NativeInspect] | None = None,
) -> NativeImagePinResult:
    """Verify one explicitly enabled native start under an armed PRE reservation.

    Pin admission independently from readbacks. Adapters use fresh, bounded
    observations from the authenticated object and share trusted time.
    Read the object's complete images map. Resolve the selected reference's
    OCI manifest and config bytes without redirects or tag fallback.
    Start atomically enforces the original armed window, stop fence and one-use
    start ID, applies the selected image and effective bot and egress options,
    binds both fixtures before bot boot and reports the actual start time.
    Inspect directly observes the running instance and fixture readiness.
    The caller retains independent stop custody, including after a lost start.
    """
    if enabled is not True:
        return _DISABLED

    require(type(admission) is NativeImageAdmission and _scope_valid(admission.scope)
            and type(admission.window) is EffectWindow
            and admission.window.target == admission.scope.deployment.target
            and _matches(_NAME, admission.image_alias) and _image_valid(admission.image),
            "pre_native_image_admission_invalid")
    require(all(callable(adapter) for adapter in (read_images, resolve_image, clock, start, inspect)),
            "pre_native_image_adapter_missing")
    _check_fixtures(admission.fixtures)
    scope, window, image = admission.scope, admission.window, admission.image
    before_lookup = _now(window, clock, window.target.started_at)

    lookup = _call(read_images, scope, failure="pre_native_image_map_unconfirmed")
    require(type(lookup) is ImageMapReadback and _scope_valid(lookup.scope) and lookup.scope == scope
            and type(lookup.images) is dict and len(lookup.images) == 1, "pre_native_image_map_unconfirmed")
    images = lookup.images.copy()
    require(len(images) == 1, "pre_native_image_map_unconfirmed")
    alias, reference = next(iter(images.items()))
    require(type(alias) is str and alias == admission.image_alias
            and type(reference) is str and reference == image.reference, "pre_native_image_map_unconfirmed")
    resolved = _call(resolve_image, reference, failure="pre_native_image_resolution_unconfirmed")
    require(_image_valid(resolved) and resolved == image, "pre_native_image_resolution_unconfirmed")

    before_start = _now(window, clock, before_lookup)
    receipt = _call(start, admission, reference, failure="pre_native_image_start_unconfirmed")
    after_start = _now(window, clock, before_start)
    require(type(receipt) is NativeStart and _scope_valid(receipt.scope) and receipt.scope == scope
            and _matches(_CONTAINER_ID, receipt.container_id) and type(receipt.started_at) is int
            and before_start <= receipt.started_at <= after_start, "pre_native_image_start_unconfirmed")

    observed = _call(inspect, scope, receipt.container_id, failure="pre_native_image_inspect_unconfirmed")
    _now(window, clock, after_start)
    require(type(observed) is NativeInspect and _scope_valid(observed.scope) and observed.scope == scope
            and type(observed.container_id) is str and observed.container_id == receipt.container_id
            and observed.running is True and type(observed.image_digest) is str
            and observed.image_digest == image.manifest_digest and observed.fixtures_ready_in_container is True,
            "pre_native_image_inspect_unconfirmed")
    return NativeImagePinResult(True, observed.container_id, observed.image_digest)
