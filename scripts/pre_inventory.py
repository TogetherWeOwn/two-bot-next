"""Project synthetic Cloudflare reads onto one PRE target without I/O."""

from dataclasses import dataclass, field
import re

from pre_target import APP_ID, Resource, Target, require


RESOURCE_NAME = re.compile(r"[A-Za-z0-9_-]{1,128}")
MAX_ROWS = 1000


@dataclass(frozen=True)
class InventoryProjection:
    """Complete resource listings and direct counts when the app is present."""

    target: Target
    worker: Resource | None
    container: Resource | None
    instance_count: int | None
    assigned_count: int | None
    complete: bool = field(init=False, default=True)


def _complete_rows(listing):
    require(isinstance(listing, dict) and listing.keys() == {"complete", "total_count", "rows"},
            "unrecognized_pre_listing")
    rows = listing["rows"]
    total = listing["total_count"]
    require(listing["complete"] is True and isinstance(rows, list) and type(total) is int
            and total == len(rows) <= MAX_ROWS, "incomplete_pre_listing")
    return rows


def _resources_by_name(listing, *, container):
    by_name = {}
    ids = set()
    fields = {"name", "id"} if container else {"id"}
    for row in _complete_rows(listing):
        require(isinstance(row, dict) and row.keys() == fields, "unrecognized_pre_resource")
        resource_id = row["id"]
        name = row["name"] if container else resource_id
        require(isinstance(name, str) and RESOURCE_NAME.fullmatch(name), "invalid_pre_resource_name")
        if container:
            require(isinstance(resource_id, str) and APP_ID.fullmatch(resource_id), "invalid_pre_app_id")
        require(name not in by_name and resource_id not in ids, "duplicate_pre_resource")
        by_name[name] = Resource(name, resource_id)
        ids.add(resource_id)
    return by_name, ids


def _direct_counts(detail, app):
    require(isinstance(detail, dict) and detail.get("id") == app.id
            and detail.get("name") == app.name, "wrong_pre_app_detail")
    health = detail.get("health")
    instances = health.get("instances") if isinstance(health, dict) else None
    require(isinstance(instances, dict), "unrecognized_pre_app_health")
    active, assigned = instances.get("active"), instances.get("assigned")
    require(type(active) is int and active >= 0 and type(assigned) is int and assigned >= 0,
            "unknown_pre_instance_counts")
    return active, assigned


def normalize_inventory(target, source_target, *, workers, applications, pinned_app_id=None, app_detail=None):
    """Read only the reserved names from one authenticated observation.

    For the future adapter, source_target comes from authenticated read
    context for the whole observation. Each listing is a normalized dict with
    rows, total_count and complete. The adapter attests completion and supplies
    the assembled row total only after validated traversal. Worker rows carry
    id. App rows carry name and id. A present app needs an independently pinned
    UUID and the fresh detail response's health.instances.active and assigned.

    TargetError means unknown inventory. Complete absence supplies no counts,
    including after deletion. The adapter builds Inventory using this
    projection and its independent access, admission and prior count evidence.
    """
    require(type(target) is Target and type(source_target) is Target and source_target == target,
            "wrong_pre_inventory_target")
    require(pinned_app_id is None or (isinstance(pinned_app_id, str) and APP_ID.fullmatch(pinned_app_id)),
            "invalid_pre_app_pin")
    worker_names, _ = _resources_by_name(workers, container=False)
    app_names, app_ids = _resources_by_name(applications, container=True)
    worker = worker_names.get(target.worker_name)
    app = app_names.get(target.container_name)
    if app is None:
        require(pinned_app_id not in app_ids and app_detail is None, "inconsistent_pre_app_absence")
        return InventoryProjection(target, worker, None, None, None)

    require(app.id == pinned_app_id, "wrong_pre_app_pin")
    active, assigned = _direct_counts(app_detail, app)
    return InventoryProjection(target, worker, app, active, assigned)
