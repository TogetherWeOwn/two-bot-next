"""Check an isolated predecessor app and its effective writer before launch."""

from dataclasses import dataclass
from uuid import UUID

from pre_inventory import RESOURCE_NAME
from pre_registry_cleanup import RegistryReservation
from pre_start_stop import Deployment
from pre_target import ProtectedRouteReadback, Target, TargetError, confirm_protected_route, require


@dataclass(frozen=True)
class RuntimeBinding:
    """An exact deployment, verified image reference and scoped writer identity.

    The protected adapter pins the deployment and namespace independently of
    launch readback. The image pin comes from verified registry publication.
    The database authority supplies the opaque database and writer IDs for
    this reservation independently of the upload's connection string.
    """

    deployment: Deployment
    namespace_id: str
    image: RegistryReservation
    database_id: str
    writer_id: str


@dataclass(frozen=True)
class RuntimeBindingReadback:
    """Fresh app and effective database observations for one launch request."""

    request: object
    binding: RuntimeBinding
    route: ProtectedRouteReadback
    disposable_database: bool
    can_write: bool
    no_shared_writer: bool


def _check_binding(binding):
    require(type(binding) is RuntimeBinding and type(binding.deployment) is Deployment,
            "invalid_pre_runtime_binding")
    deployment = binding.deployment
    target = deployment.target
    require(type(target) is Target
            and all(type(value) is UUID and value.int
                    for value in (deployment.start_id, deployment.worker_version, deployment.container_id))
            and all(type(value) is str and RESOURCE_NAME.fullmatch(value)
                    for value in (binding.namespace_id, binding.database_id, binding.writer_id)),
            "invalid_pre_runtime_identity")
    require(type(binding.image) is RegistryReservation and binding.image.target == target,
            "wrong_pre_runtime_image_scope")


def launch_with_verified_binding(binding, *, observe, recheck, launch):
    """Order fresh binding verification before the injected process start.

    Observe the actual app image and private route, and independently attest
    the effective disposable database and writer from the authenticated
    launch context. The request marker is an in-process replay
    check. The adapter must take a fresh observation under the original
    reservation and preserve its binding through the effect. Recheck the
    exact deployment with the independent authority after observation. The
    authority confirms the admitted start ID, open fence and unexpired
    cutoff from current state. Launch still enforces those conditions at
    effect time. Use the boundary at every cold or replacement start. A
    mismatched launch reply leaves the outcome uncertain for independent stop.
    """
    _check_binding(binding)
    request = object()
    try:
        readback = observe(binding.deployment, request=request)
    except Exception:
        readback = None
    require(type(readback) is RuntimeBindingReadback and readback.request is request,
            "pre_runtime_binding_unconfirmed")
    observed = readback.binding
    _check_binding(observed)
    require(observed.deployment == binding.deployment and observed.namespace_id == binding.namespace_id,
            "wrong_pre_runtime_deployment")
    confirm_protected_route(binding.deployment.target, pinned_app_id=str(binding.deployment.container_id),
                            readback=readback.route)
    require(observed.image == binding.image, "wrong_pre_runtime_image")
    require(observed.database_id == binding.database_id and observed.writer_id == binding.writer_id
            and readback.disposable_database is True and readback.can_write is True
            and readback.no_shared_writer is True, "pre_runtime_writer_unconfirmed")
    try:
        current = recheck(binding.deployment)
    except Exception:
        current = None
    require(current is True, "pre_runtime_launch_not_authorized")
    deployment = launch(binding)
    require(type(deployment) is Deployment and deployment == binding.deployment,
            "pre_runtime_launch_unconfirmed")
    return deployment
