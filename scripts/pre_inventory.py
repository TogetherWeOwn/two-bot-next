"""Project synthetic Cloudflare reads onto one PRE target without I/O."""

from dataclasses import dataclass, field
import re

from pre_target import APP_ID, Resource, Target, TargetError, require


RESOURCE_NAME = re.compile(r"[A-Za-z0-9_-]{1,128}")
MAX_ROWS = 1000
CONTAINER_APPLICATIONS = "/containers/applications"
APPLICATIONS_PER_PAGE = 100
MAX_APPLICATION_PAGES = MAX_ROWS // APPLICATIONS_PER_PAGE
MAX_PAGE_TOKEN_LENGTH = 4096


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


def _require_source_target(target, source_target):
    require(type(target) is Target and type(source_target) is Target and source_target == target,
            "wrong_pre_inventory_target")


def read_provider_listing(target, source_target, *, route, read_page):
    """Assemble one complete listing from an injected, authenticated read context.

    The caller binds source_target and read_page to the same exact reservation
    and provider account. read_page receives an account-relative GET path and
    structured query parameters. It must encode those parameters without
    adding filters and supply a fresh, size-bounded decoded response. Paging
    establishes traversal only. Admission and startup settlement still need
    independent proof.

    Cloudflare's Container list documents result_info with next_page_token,
    page_token and per_page. Require the requested size and cursor to be
    echoed, with no next_page_token on the final response. The Worker scripts
    list documents neither paging nor a completion marker, so refuse it.
    """
    _require_source_target(target, source_target)
    require(route == CONTAINER_APPLICATIONS, "unsupported_pre_listing_route")

    rows, seen_tokens = [], set()
    query = {"per_page": APPLICATIONS_PER_PAGE}
    for _ in range(MAX_APPLICATION_PAGES):
        try:
            response = read_page(CONTAINER_APPLICATIONS, query.copy())
        except Exception:
            raise TargetError("pre_listing_read_failed") from None
        require(isinstance(response, dict)
                and response.keys() <= {"success", "errors", "messages", "result", "result_info"}
                and response.get("success") is True and response.get("errors", []) == []
                and response.get("messages", []) == [], "unrecognized_pre_listing_page")
        page, info = response.get("result"), response.get("result_info")
        require(isinstance(page, list) and len(page) <= APPLICATIONS_PER_PAGE,
                "invalid_pre_listing_page_size")
        require(isinstance(info, dict) and info.keys() <= {"per_page", "page_token", "next_page_token"}
                and type(info.get("per_page")) is int and info["per_page"] == APPLICATIONS_PER_PAGE
                and info.get("page_token", "") == query.get("page_token", ""), "wrong_pre_listing_page")
        require(page or (not rows and "next_page_token" not in info), "empty_pre_listing_page")
        for row in page:
            require(isinstance(row, dict), "unrecognized_pre_resource")
            rows.append({"name": row.get("name"), "id": row.get("id")})

        if "next_page_token" not in info:
            listing = {"rows": rows, "total_count": len(rows), "complete": True}
            _resources_by_name(listing, container=True)
            return listing

        token = info["next_page_token"]
        require(isinstance(token, str) and 0 < len(token) <= MAX_PAGE_TOKEN_LENGTH
                and token.isascii() and token.isprintable() and not token.isspace()
                and token not in seen_tokens, "invalid_pre_listing_cursor")
        seen_tokens.add(token)
        query["page_token"] = token

    raise TargetError("pre_listing_page_limit")


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
    the assembled row total only after validated traversal. For supported
    provider routes, read_provider_listing supplies that contract. Worker rows
    carry id. App rows carry name and id. A present app needs an independently
    pinned UUID and the fresh detail response's health.instances.active and
    assigned.

    TargetError means unknown inventory. Complete absence supplies no counts,
    including after deletion. The adapter builds Inventory using this
    projection and its independent access, admission and prior count evidence.
    """
    _require_source_target(target, source_target)
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
