"""Offline config and cleanup contract for disposable predecessor fixtures.

The caller supplies an independently authenticated, immutable first-attempt
reservation and observations for its exact scope. This module does not admit
live traffic, resolve a provider account, or execute cleanup operations.
The cutoff governs admission. A scheduled sweep can run late or be dropped.
"""

from dataclasses import dataclass
from pathlib import Path
import re

ADMISSION_SECONDS = 3600
LAST_UNIX_SECOND = 9_999_999_999
SCOPE = re.compile(r"pre-fixture-[a-z0-9]{1,32}")
APP_ID = re.compile(r"[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}")
SHA = re.compile(r"[0-9a-f]{40}")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")


class TargetError(ValueError):
    pass


def require(condition, reason):
    if not condition:
        raise TargetError(reason)


@dataclass(frozen=True)
class Target:
    """The caller must derive this from the reservation, never a resource name."""

    scope: str
    run_id: int
    started_at: int
    shape: str

    def __post_init__(self):
        require(isinstance(self.scope, str) and SCOPE.fullmatch(self.scope), "invalid_pre_scope")
        require(type(self.run_id) is int and 0 < self.run_id < 10**20, "invalid_pre_run")
        require(type(self.started_at) is int and 0 < self.started_at <= LAST_UNIX_SECOND - ADMISSION_SECONDS,
                "invalid_pre_start")
        require(self.shape in ("0473/0465", "0475/0465"), "unsupported_pre_shape")

    @property
    def expires_at(self):
        return self.started_at + ADMISSION_SECONDS

    @property
    def worker_name(self):
        return f"two-bot-next-pre-{self.run_id}-{self.expires_at}"

    @property
    def container_name(self):
        return self.worker_name + "-c"

    @property
    def service_name(self):
        return f"{self.worker_name}-{self.shape.replace('/', '-')}"

    @property
    def synthetic_database_url(self):
        """The canonical binding written to this reservation's Worker config."""
        return f"postgresql://pre_fixture@{self.service_name}.{self.scope}.invalid/pre"


def check_clock(target, now):
    require(type(now) is int and target.started_at <= now <= LAST_UNIX_SECOND, "invalid_pre_clock")


def make_config(target, *, now, receipt, material_root, synthetic_service):
    """Describe the verified private material and synthetic bindings offline.

    Keep the materializer's directory private and unchanged while consuming the
    result. The caller supplies the receipt from verification of that same
    archive. The digest reference uses an offline fixture hostname. The OCI
    archive stays in the material directory for the eventual caller.
    """
    check_clock(target, now)
    require(now < target.expires_at, "pre_admission_expired")
    require(synthetic_service == target.service_name, "wrong_pre_service")
    require(isinstance(receipt, dict), "wrong_pre_build_identity")
    revision, digest = receipt.get("source_sha"), receipt.get("image_manifest_digest")
    require(isinstance(revision, str) and SHA.fullmatch(revision)
            and receipt.get("worker_message") == receipt.get("BOT_BUILD_REVISION") == revision
            and isinstance(digest, str) and DIGEST.fullmatch(digest), "wrong_pre_build_identity")
    root = Path(material_root)
    image = root / "image.oci.tar"
    modules = root / "worker"
    require(not root.is_symlink() and not image.is_symlink() and image.is_file()
            and not modules.is_symlink(), "missing_pre_material")
    config = {
        "name": target.worker_name,
        "workers_dev": False,
        "preview_urls": False,
        "compatibility_date": "2026-09-29",
        "compatibility_flags": ["nodejs_compat", "nodejs_compat_do_not_populate_process_env"],
        "version_metadata": {"binding": "CF_VERSION_METADATA"},
        "durable_objects": {"bindings": [{"name": "TWO_BOT", "class_name": "TwoBotContainer"}]},
        "exports": {"TwoBotContainer": {"type": "durable-object", "storage": "sqlite"}},
        "containers": [{
            "name": target.container_name,
            "class_name": "TwoBotContainer",
            "image": f"registry.{target.scope}.invalid/{target.container_name}@{digest}",
            "instance_type": "lite",
            "max_instances": 1,
        }],
        "vars": {"BOT_PORT": "8080", "CONTAINER_RELEASE": revision,
                 "DATABASE_URL": target.synthetic_database_url},
    }
    # Bundle configuration needs tomllib. Reservation and readback checks do not.
    from bot_promotion import PromotionError, configure_bundle

    try:
        return configure_bundle(config, modules, "index.js")
    except (PromotionError, OSError):
        raise TargetError("missing_pre_material") from None


@dataclass(frozen=True)
class Resource:
    name: str
    id: str


@dataclass(frozen=True)
class Inventory:
    """A fresh, complete exact-target observation from the caller's adapter.

    None for a resource means confirmed absence. The active and assigned counts
    must come from direct proof while the exact app is readable, or affirmative
    evidence that it was never created. App deletion alone cannot supply them.
    `startup_settled` requires independent proof that admission is closed and
    no in-flight operation or keepalive can create or restart resources. Time
    and workflow status do not prove this.
    """

    target: Target
    worker: Resource | None
    container: Resource | None
    instance_count: int | None
    write_access: bool
    complete: bool
    startup_settled: bool = False
    assigned_count: int | None = None


@dataclass(frozen=True)
class Action:
    operation: str
    name: str
    id: str


def check_inventory(target, inventory):
    require(type(inventory) is Inventory and inventory.target == target, "wrong_pre_inventory_target")
    counts = (inventory.instance_count, inventory.assigned_count)
    require(inventory.complete is True
            and all(count is None or (type(count) is int and count >= 0) for count in counts)
            and type(inventory.write_access) is bool,
            "unconfirmed_pre_inventory")
    if inventory.worker is not None:
        require(type(inventory.worker) is Resource
                and inventory.worker == Resource(target.worker_name, target.worker_name), "wrong_pre_worker")
    if inventory.container is not None:
        app = inventory.container
        require(type(app) is Resource and app.name == target.container_name
                and isinstance(app.id, str) and APP_ID.fullmatch(app.id), "wrong_pre_container")


def plan_cleanup(target, inventory, *, now, run_status):
    """Return ordered actions in this target's scope, or None to spare a run.

    Use authoritative run status. Apply the plan in order and stop on an
    unconfirmed operation. Guard app deletion with `confirm_container_stopped`.
    Re-observe for each sweep, including after a lost deploy reply. Plans and
    empty snapshots do not attest teardown.
    """
    check_clock(target, now)
    require(run_status in ("active", "terminal"), "unconfirmed_pre_run_status")
    check_inventory(target, inventory)
    if run_status == "active" and now < target.expires_at:
        return None
    actions = []
    if inventory.write_access:
        actions.append(Action("revoke_write_access", target.service_name, target.service_name))
    if inventory.worker is not None:
        actions.append(Action("delete_worker", target.worker_name, inventory.worker.id))
    if inventory.container is not None:
        actions.append(Action("delete_container", target.container_name, inventory.container.id))
    return tuple(actions)


def confirm_container_stopped(target, action, inventory):
    """Guard app deletion using fresh direct health for the pinned app ID."""
    check_inventory(target, inventory)
    require(inventory.container is not None
            and action == Action("delete_container", target.container_name, inventory.container.id),
            "wrong_pre_container_delete")
    require(inventory.startup_settled is True and not inventory.write_access and inventory.worker is None
            and inventory.instance_count == inventory.assigned_count == 0, "pre_container_stop_unconfirmed")


def verify_cleanup(target, inventory):
    """Require independently settled startup and fresh post-cleanup readback."""
    check_inventory(target, inventory)
    require(inventory.startup_settled is True, "pre_startup_unsettled")
    require(not inventory.write_access and inventory.worker is None and inventory.container is None
            and inventory.instance_count == inventory.assigned_count == 0, "pre_cleanup_unconfirmed")
