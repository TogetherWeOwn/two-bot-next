"""Assemble the verified predecessor's Worker multipart body offline."""

from dataclasses import dataclass, field
from hashlib import sha256
import json
from os import scandir
from pathlib import Path, PurePosixPath
import re

from bot_promotion import MAX_MEMBER_BYTES, MAX_MODULES, MAX_TOTAL_BYTES, safe_module_path
from pre_target import Target, TargetError, make_config, require
from script_intake import read_with_overflow


ENTRYPOINT = "index.js"
MODULE_TYPES = {
    ".js": "application/javascript+module",
    ".mjs": "application/javascript+module",
    ".wasm": "application/wasm",
    ".txt": "text/plain",
    ".bin": "application/octet-stream",
}
HEX_SHA256 = re.compile(r"[0-9a-f]{64}")
DNS_LABEL = r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?"
DATABASE_NAME = r"[a-zA-Z0-9_][a-zA-Z0-9_.-]{0,62}"
DATABASE_TLS = r"sslmode=(?:require|verify-ca|verify-full)"
DATABASE_CHANNEL_BINDING = "channel_binding=require"
# SQLx accepts query overrides for the destination and user. Admit only the
# TLS options used by the archived runtime and require explicit TCP identity.
DATABASE_URL = re.compile(
    rf"postgres(?:ql)?://(?P<user>{DATABASE_NAME}):(?:[a-zA-Z0-9._~-]|%[0-9a-fA-F]{{2}})+@"
    rf"(?P<host>(?:{DNS_LABEL}\.)+[a-z](?:[a-z0-9-]{{0,61}}[a-z0-9])?)"
    rf":(?P<port>[1-9][0-9]{{0,4}})/(?P<database>{DATABASE_NAME})"
    rf"\?(?:{DATABASE_TLS}(?:&{DATABASE_CHANNEL_BINDING})?|{DATABASE_CHANNEL_BINDING}&{DATABASE_TLS})"
)


@dataclass(frozen=True)
class DatabaseEndpoint:
    """Non-secret endpoint pin from authenticated provisioning for a Target."""

    target: Target
    service: str
    host: str
    port: int
    database: str
    user: str
    disposable: bool


def _database_binding(target, endpoint, url):
    require(type(endpoint) is DatabaseEndpoint and type(endpoint.target) is Target
            and endpoint.target == target and type(endpoint.service) is str and endpoint.service == target.service_name
            and endpoint.disposable is True, "wrong_pre_database_target")
    require(type(url) is str and 0 < len(url) <= 2048, "invalid_pre_database_url")
    match = DATABASE_URL.fullmatch(url)
    require(match is not None and len(match["host"]) <= 253 and int(match["port"]) <= 65535,
            "invalid_pre_database_url")
    require(type(endpoint.port) is int and endpoint.port == int(match["port"])
            and all(type(getattr(endpoint, key)) is str and getattr(endpoint, key) == match[key]
                    for key in ("host", "database", "user")), "pre_database_endpoint_mismatch")
    return {"name": "DATABASE_URL", "type": "secret_text", "text": url}


@dataclass(frozen=True)
class WorkerUpload:
    target: Target
    content_type: str
    body: bytes = field(repr=False)

    @property
    def supports_discord_fixture_overrides(self) -> bool:
        # The verified predecessor's Worker drops both DISCORD_GATEWAY_URL and
        # DISCORD_API_BASE from the default Container environment.
        return False

    def require_unrouted(self, *, observed_target, complete, workers_dev, preview_urls, routes, custom_domains):
        """Check fresh, authenticated ingress observations for this reservation.

        The caller supplies complete route and custom-domain inventories as
        tuples, including any shared entries. An unrouted result supplies no
        evidence that a restricted invocation path exists.
        """
        require(type(observed_target) is Target and observed_target == self.target, "wrong_pre_ingress_target")
        require(complete is True and workers_dev is False and preview_urls is False
                and routes == custom_domains == (), "pre_ingress_unconfirmed")


def _read_modules(root, hashes):
    require(type(hashes) is dict and ENTRYPOINT in hashes and len(hashes) <= MAX_MODULES
            and all(safe_module_path(name)
                    and isinstance(digest, str) and HEX_SHA256.fullmatch(digest)
                    for name, digest in hashes.items()), "invalid_pre_worker_manifest")
    directories = {parent.as_posix() for name in hashes for parent in PurePosixPath(name).parents}
    modules = {}
    pending = [root]
    total = 0
    while pending:
        with scandir(pending.pop()) as entries:
            for entry in entries:
                path = Path(entry.path)
                name = path.relative_to(root).as_posix()
                if entry.is_dir(follow_symlinks=False) and name in directories:
                    pending.append(path)
                    continue
                require(name in hashes and entry.is_file(follow_symlinks=False),
                        "pre_worker_inventory_mismatch")
                with path.open("rb") as handle:
                    data = read_with_overflow(handle, min(MAX_MEMBER_BYTES, MAX_TOTAL_BYTES - total))
                total += len(data)
                require(len(data) <= MAX_MEMBER_BYTES and total <= MAX_TOTAL_BYTES, "pre_worker_size_limit")
                require(sha256(data).hexdigest() == hashes[name], "pre_worker_digest_mismatch")
                modules[name] = data
    require(modules.keys() == hashes.keys(), "pre_worker_inventory_mismatch")
    return modules


def assemble_worker_upload(target, *, now, receipt, material_root, config, database_endpoint=None, database_url=None):
    """Build a raw script upload body for exactly one reserved disposable target.

    Use the authenticated Target, verified build receipt and stable private
    material directory from the same predecessor verification, with the config
    emitted by pre_target.make_config. The protected caller authenticates the
    disposable database pin independently of the supplied URL. The URL uses
    a canonical DNS name and explicit TCP port, database, user, password and
    TLS mode. Its only optional query setting is channel_binding=require in
    either order. The archived runtime's default TLS policy uses verify-full.
    Keep the result private and paired with its full Target. Hold exclusive
    access to material until assembly returns, and use the returned body
    directly at the upload boundary.

    The body matches Wrangler's script PUT module and metadata layout. The
    protected caller owns account selection, collision checks, ingress settings,
    Container application setup and proof of restricted invocation. It must
    confirm the reservation still owns the database at the startup effect
    boundary and keep the endpoint exclusive through use.
    """
    require(type(target) is Target, "invalid_pre_worker_target")
    root = Path(material_root)
    expected = make_config(target, now=now, receipt=receipt, material_root=root,
                           synthetic_service=target.service_name)
    require(type(config) is dict, "wrong_pre_worker_config")
    require(config.get("main") == expected["main"], "wrong_pre_worker_entrypoint")
    require(config.get("workers_dev") is False and config.get("preview_urls") is False
            and config == expected, "wrong_pre_worker_config")
    database_binding = _database_binding(target, database_endpoint, database_url)
    hashes = receipt.get("worker_modules_sha256")
    try:
        modules = _read_modules(root / "worker", hashes)
    except OSError:
        raise TargetError("unreadable_pre_worker_material") from None

    bindings = [{"name": name, "type": "plain_text", "text": text}
                for name, text in expected["vars"].items() if name != "DATABASE_URL"]
    bindings.append(database_binding)
    bindings.extend({"type": "durable_object_namespace", **binding}
                    for binding in expected["durable_objects"]["bindings"])
    bindings.append({"name": expected["version_metadata"]["binding"], "type": "version_metadata"})
    metadata = json.dumps({
        "main_module": ENTRYPOINT,
        "compatibility_date": expected["compatibility_date"],
        "compatibility_flags": expected["compatibility_flags"],
        "bindings": bindings,
        "exports": expected["exports"],
        "containers": [{"name": container["name"], "class_name": container["class_name"]}
                       for container in expected["containers"]],
        "code_update_strategy": {"mode": "deferred", "max_delay": 300},
        "annotations": {"workers/message": receipt["worker_message"]},
    }, separators=(",", ":")).encode()
    # Derive the public header only from verified module digests.
    boundary = sha256(b"".join(bytes.fromhex(hashes[name]) for name in sorted(modules))).hexdigest()
    delimiter = b"--" + boundary.encode()
    require(delimiter not in metadata, "pre_worker_boundary_collision")
    parts = [b'Content-Disposition: form-data; name="metadata"\r\n\r\n' + metadata]
    for name, data in sorted(modules.items()):
        require(delimiter not in data, "pre_worker_boundary_collision")
        headers = (f'Content-Disposition: form-data; name="{name}"; filename="{name}"\r\n'
                   f"Content-Type: {MODULE_TYPES[PurePosixPath(name).suffix]}\r\n\r\n")
        parts.append(headers.encode("ascii") + data)
    body = b"".join(delimiter + b"\r\n" + part + b"\r\n" for part in parts) + delimiter + b"--\r\n"
    return WorkerUpload(target, f"multipart/form-data; boundary={boundary}", body)
