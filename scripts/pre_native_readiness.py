"""Verify paired PRE readiness through injected, authenticated observations."""

from dataclasses import dataclass, field
import re

from pre_target import APP_ID, DIGEST, SHA, Target, TargetError, require
from script_intake import decode_json

SHAPES = ("0473/0465", "0475/0465")
IDENTIFIER = re.compile(r"[A-Za-z0-9][A-Za-z0-9.+_-]{0,127}")
MAX_BODY_BYTES = 32_768


@dataclass(frozen=True)
class Runtime:
    target: Target
    app_id: str
    process_id: str  # An incarnation ID that changes on every process start.


@dataclass(frozen=True)
class Build:
    build_revision: str
    build_id: str
    deploy_id: str
    image_digest: str


@dataclass(frozen=True)
class Ledger:
    cutover_fingerprint: str  # Digest of the complete successful cutover ledger.
    store_fingerprint: str
    cutover: tuple[int, int]  # Row count and final migration version.
    store: tuple[int, int]
    attendance_recovered_at: bool
    installed_0475_at: int | None  # UTC microseconds, preserving DB precision.


@dataclass(frozen=True)
class Writer:
    database_id: str
    writer_id: str
    disposable: bool
    can_write: bool
    isolated: bool  # Effective writer access is confined to this reservation.


@dataclass(frozen=True)
class Reservation:
    runtime: Runtime  # Pins come from the admitted runtime and verified archive.
    worker_version: str
    build: Build
    writer: Writer
    ledger: Ledger
    mock_session: str  # Unique to this reservation's synthetic READY event.


@dataclass(frozen=True)
class Probe:
    request: object
    runtime: Runtime
    worker_versions: tuple[str, ...]  # All x-two-worker-version field values.
    status: int
    body: bytes = field(repr=False)


@dataclass(frozen=True)
class MockActivity:
    ready_session: str | None = None
    checkpoint: tuple[str, int] | None = None  # Session and sequence in the effective DB.
    rest_session: str | None = None  # Fixture session that received the sync.
    command_count: int = 0  # Commands accepted in that synthetic REST sync.


@dataclass(frozen=True)
class Readback:
    request: object
    runtime: Runtime
    running_digest: str
    writer: Writer
    ledger: Ledger
    activity: MockActivity


@dataclass(frozen=True)
class Evidence:
    reservation: Reservation
    statuses: tuple[int, int, int]  # Health, pre-READY and post-READY.
    activity: MockActivity


def _matches(pattern, value):
    return type(value) is str and pattern.fullmatch(value) is not None


def _identifier(value):
    return _matches(IDENTIFIER, value) and value != "unknown"


def _valid_runtime(runtime):
    return (type(runtime) is Runtime and type(runtime.target) is Target
            and type(runtime.target.scope) is str and type(runtime.target.shape) is str
            and _matches(APP_ID, runtime.app_id) and _identifier(runtime.process_id))


def _check_writer(writer):
    require(type(writer) is Writer and _identifier(writer.database_id) and _identifier(writer.writer_id)
            and writer.disposable is True and writer.can_write is True and writer.isolated is True,
            "unconfirmed_pre_writer")


def _check_ledger(ledger, shape):
    retained = shape == SHAPES[1]
    require(type(ledger) is Ledger
            and all(_matches(DIGEST, value) for value in (ledger.cutover_fingerprint, ledger.store_fingerprint))
            and all(type(pair) is tuple and len(pair) == 2 and all(type(value) is int for value in pair)
                    for pair in (ledger.cutover, ledger.store))
            and ledger.cutover == ((116, 475) if retained else (115, 473)) and ledger.store == (20, 465)
            and ledger.attendance_recovered_at is retained, "wrong_pre_ledger")
    stamp = ledger.installed_0475_at
    require((type(stamp) is int and stamp > 0) if retained else stamp is None, "wrong_pre_retained_timestamp")


def _check_pin(pin):
    require(type(pin) is Reservation and _valid_runtime(pin.runtime)
            and _matches(APP_ID, pin.worker_version) and _identifier(pin.mock_session), "invalid_pre_reservation")
    build = pin.build
    require(type(build) is Build and _matches(SHA, build.build_revision) and _matches(DIGEST, build.image_digest)
            and _identifier(build.build_id) and _identifier(build.deploy_id), "invalid_pre_build")
    _check_writer(pin.writer)
    _check_ledger(pin.ledger, pin.runtime.target.shape)


def _observe(adapter, *args, **kwargs):
    try:
        return adapter(*args, **kwargs)
    except Exception:
        raise TargetError("pre_readiness_observation_failed") from None


def _probe(binding, pin, path, status):
    request = object()
    reply = _observe(binding, pin.runtime.target, pin.runtime.target.service_name, path,
                     request=request, max_body_bytes=MAX_BODY_BYTES)
    require(type(reply) is Probe and reply.request is request and _valid_runtime(reply.runtime)
            and reply.runtime == pin.runtime, "wrong_pre_probe_runtime")
    require(type(reply.worker_versions) is tuple and len(reply.worker_versions) == 1
            and type(reply.worker_versions[0]) is str and reply.worker_versions[0] == pin.worker_version,
            "wrong_pre_worker_version")
    require(type(reply.status) is int and reply.status == status, "wrong_pre_http_status")
    require(type(reply.body) is bytes and len(reply.body) <= MAX_BODY_BYTES, "invalid_pre_probe_body")
    return reply


def _check_components(reply, build, gateway_states):
    try:
        body = decode_json(reply.body.decode("utf-8"), max_depth=16)
    except (ValueError, RecursionError):
        raise TargetError("invalid_pre_probe_body") from None
    require(type(body) is dict and all(body.get(name) == getattr(build, name)
            for name in ("build_revision", "build_id", "deploy_id")), "wrong_pre_runtime_stamps")
    pairs = body.get("components")
    require(type(pairs) is list and all(type(pair) is list and len(pair) == 2
            and all(type(value) is str for value in pair) for pair in pairs), "invalid_pre_components")
    components = dict(pairs)
    require(len(components) == len(pairs) and components.get("process") == "ready"
            and components.get("database") == "ready" and components.get("gateway") in gateway_states,
            "wrong_pre_readiness_components")


def _readback(reader, pin, reply):
    observed = _observe(reader, pin.runtime.target, request=reply.request)
    require(type(observed) is Readback and observed.request is reply.request
            and _valid_runtime(observed.runtime)
            and observed.runtime == pin.runtime, "wrong_pre_readback_runtime")
    require(type(observed.running_digest) is str and observed.running_digest == pin.build.image_digest,
            "wrong_pre_running_digest")
    _check_writer(observed.writer)
    require(observed.writer == pin.writer, "wrong_pre_writer")
    _check_ledger(observed.ledger, pin.runtime.target.shape)
    require(observed.ledger == pin.ledger, "wrong_pre_ledger")
    activity = observed.activity
    require(type(activity) is MockActivity and type(activity.command_count) is int, "unconfirmed_pre_mock_activity")
    return activity


def verify_paired_readiness(original, retained, *, private_get, readback, release_ready):
    """Return evidence for both PRE layouts after all checks pass.

    Private GET takes one bounded observation through the exact Service Binding.
    Adapters authenticate the scope and serving process and bind fresh readback
    to the probe token. Confirm READY release for the supplied target and session
    with True. The caller owns timing, fencing and cleanup.
    """
    for pin in (original, retained):
        _check_pin(pin)
    first, second = original.runtime.target, retained.runtime.target
    require((first.shape, second.shape) == SHAPES and first.scope != second.scope
            and first.provider_names != second.provider_names
            and original.runtime.app_id != retained.runtime.app_id
            and original.worker_version != retained.worker_version
            and original.writer.database_id != retained.writer.database_id and original.writer.writer_id != retained.writer.writer_id
            and original.mock_session != retained.mock_session, "pre_reservations_not_separate")
    require(original.build == retained.build, "wrong_pre_archive_pair")

    evidence = []
    for pin in (original, retained):
        health = _probe(private_get, pin, "/health", 200)
        pre = _probe(private_get, pin, "/readyz", 503)
        _check_components(pre, pin.build, ("starting", "down"))
        baseline = _readback(readback, pin, pre)
        require(baseline.ready_session is baseline.rest_session is baseline.checkpoint is None
                and baseline.command_count == 0, "premature_pre_mock_activity")
        require(_observe(release_ready, pin.runtime.target, pin.mock_session) is True, "pre_mock_ready_unconfirmed")
        post = _probe(private_get, pin, "/readyz", 200)
        _check_components(post, pin.build, ("ready",))
        activity = _readback(readback, pin, post)
        require(type(activity.checkpoint) is tuple and len(activity.checkpoint) == 2
                and all(type(session) is str and session == pin.mock_session
                        for session in (activity.ready_session, activity.rest_session, activity.checkpoint[0]))
                and type(activity.checkpoint[1]) is int
                and activity.checkpoint[1] > 0 and activity.command_count > 0, "unconfirmed_pre_mock_activity")
        evidence.append(Evidence(pin, (health.status, pre.status, post.status), activity))
    return tuple(evidence)
