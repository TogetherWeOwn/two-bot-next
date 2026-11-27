"""Default-off, offline redaction check for an authenticated native PRE artifact."""

from dataclasses import dataclass
from ipaddress import ip_address
import re
from unicodedata import category, normalize
from uuid import UUID

from pre_target import SHA, Target, TargetError, check_clock
from script_intake import decode_json

MAX_BYTES = 1_048_576
MAX_DEPTH = 64
MAX_ITEMS = 10_000
PROMOTION_READBACK_SECONDS = 60

_SECRET_KEYS = frozenset({
    "authorization", "token", "secret", "password", "apikey", "dsn",
    "cookie", "privatekey", "bearer", "webhookurl",
})
_URL_CREDENTIALS = re.compile(r"://[^:/@\s?#]*:[^/@\s?#]*@")
_PRIVATE_KEY = re.compile(r"-----BEGIN (?:[A-Z0-9-]+ )*PRIVATE KEY(?: BLOCK)?-----", re.I)
_PROVIDER_TOKEN = re.compile(
    r"(?:gh[pousr]_|github_pat_|cf[auro]t_)[A-Za-z0-9_-]{16,}"
    r"|mfa\.[A-Za-z0-9_-]{20,}"
    r"|[A-Za-z0-9_-]{23,28}\.[A-Za-z0-9_-]{6,7}\.[A-Za-z0-9_-]{27,}"
    r"|discord(?:app)?\.com/api/(?:v[0-9]+/)?webhooks/[0-9]+/[A-Za-z0-9_-]+"
    r"|v1\.0-[a-fA-F0-9]{20,}"
    r"|(?<![A-Za-z0-9_-])(?:[A-Za-z0-9_-]{40}|[a-fA-F0-9]{37})(?![A-Za-z0-9_-])"
)
_IPV4 = re.compile(r"(?<![0-9])[0-9]{1,3}(?:\.[0-9]{1,3}){3}(?![0-9])")
_HEX_RUN = re.compile(r"[0-9a-f:.]+", re.I)


@dataclass(frozen=True, repr=False)
class EvidencePin:
    """Identity pinned by the protected caller independently of the artifact."""

    target: Target
    reservation_id: UUID
    deployment_id: UUID
    sha: str


@dataclass(frozen=True, repr=False)
class ArtifactMetadata:
    reservation_id: UUID
    run_id: int
    deployment_id: UUID
    sha: str
    size_bytes: int


@dataclass(frozen=True, repr=False)
class PromotionReadback:
    """Authoritative, uncached observation of PRODUCTION_AUTO_PROMOTE_DISABLED."""

    observed_at: int
    production_auto_promote_disabled: bool


@dataclass(frozen=True)
class RedactionVerdict:
    code: str
    path: str = "$"
    inspected_items: int = 0


_DISABLED = RedactionVerdict("pre_native_redaction_disabled")


def _valid_uuid(value):
    return type(value) is UUID and type(getattr(value, "int", None)) is int and value.int != 0


def _bound_metadata(pin, artifact):
    return (
        type(artifact) is ArtifactMetadata
        and _valid_uuid(getattr(artifact, "reservation_id", None)) and artifact.reservation_id == pin.reservation_id
        and type(getattr(artifact, "run_id", None)) is int and artifact.run_id == pin.target.run_id
        and _valid_uuid(getattr(artifact, "deployment_id", None)) and artifact.deployment_id == pin.deployment_id
        and type(getattr(artifact, "sha", None)) is str and artifact.sha == pin.sha
    )


def _has_nonloopback_ip(text):
    for match in _IPV4.finditer(text):
        octets = tuple(int(part) for part in match.group().split("."))
        if max(octets) <= 255 and octets[0] != 127:
            return True
    for match in _HEX_RUN.finditer(text):
        candidate = match.group().strip(".")
        # Capture compressed and expanded IPv6 without treating times as addresses.
        if "::" not in candidate and candidate.count(":") < 7:
            continue
        try:
            address = ip_address(candidate)
        except ValueError:
            # Refuse ambiguous address-like runs conservatively.
            return True
        if not (address.ipv4_mapped or address).is_loopback:
            return True
    return False


def _text_refusal(text, *, is_key):
    if any(category(char) in ("Cc", "Cf", "Cs") for char in text):
        return "pre_native_evidence_control_byte"
    text = normalize("NFKC", text)
    if is_key and re.sub(r"[^a-z0-9]", "", text.casefold()) in _SECRET_KEYS:
        return "pre_native_evidence_secret_key"
    if _URL_CREDENTIALS.search(text):
        return "pre_native_evidence_url_credentials"
    if _PRIVATE_KEY.search(text):
        return "pre_native_evidence_private_key"
    if _PROVIDER_TOKEN.search(text):
        return "pre_native_evidence_provider_token"
    if _has_nonloopback_ip(text):
        return "pre_native_evidence_nonloopback_ip"
    return None


def _fields(value, path="$", *, is_key=False):
    yield value, path, is_key
    if type(value) is dict:
        for index, (key, child) in enumerate(value.items()):
            yield from _fields(key, f"{path}.key[{index}]", is_key=True)
            yield from _fields(child, f"{path}.value[{index}]")
    elif type(value) is list:
        for index, child in enumerate(value):
            yield from _fields(child, f"{path}[{index}]")


def verify_native_pre_evidence_redaction(
    pin=None, *, artifact=None, payload=None, promotion_readback=None, now=None,
    max_bytes=MAX_BYTES, max_depth=MAX_DEPTH, max_items=MAX_ITEMS, enabled=False,
) -> RedactionVerdict:
    """Verify the exact UTF-8 JSON bytes after the caller authenticates their source.

    The caller supplies authoritative time and the independent promotion readback.
    The readback must be within 60 seconds and obtained during this reservation.
    Depth counts nested JSON containers. Items count the root, array elements,
    object keys and object values. Paths identify entries by position in JSON order.
    """
    if enabled is not True:
        return _DISABLED
    if not all(type(value) is int and 0 < value <= ceiling for value, ceiling in (
        (max_bytes, MAX_BYTES), (max_depth, MAX_DEPTH), (max_items, MAX_ITEMS),
    )):
        return RedactionVerdict("invalid_pre_native_evidence_limits")
    if not (type(pin) is EvidencePin and type(getattr(pin, "target", None)) is Target
            and type(getattr(pin.target, "run_id", None)) is int
            and type(getattr(pin.target, "started_at", None)) is int
            and _valid_uuid(getattr(pin, "reservation_id", None))
            and _valid_uuid(getattr(pin, "deployment_id", None))
            and type(getattr(pin, "sha", None)) is str and SHA.fullmatch(pin.sha)):
        return RedactionVerdict("invalid_pre_native_evidence_pin")
    if not _bound_metadata(pin, artifact):
        return RedactionVerdict("wrong_pre_native_evidence_target")
    if not (type(payload) is bytes and type(getattr(artifact, "size_bytes", None)) is int
            and 0 < len(payload) == artifact.size_bytes <= max_bytes):
        return RedactionVerdict("invalid_pre_native_evidence_size")
    try:
        check_clock(pin.target, now)
    except TargetError:
        return RedactionVerdict("invalid_pre_clock")
    if not (type(promotion_readback) is PromotionReadback
            and getattr(promotion_readback, "production_auto_promote_disabled", None) is True
            and type(getattr(promotion_readback, "observed_at", None)) is int
            and max(pin.target.started_at, now - PROMOTION_READBACK_SECONDS)
            <= promotion_readback.observed_at <= now):
        return RedactionVerdict("pre_native_promotion_readback_unconfirmed")
    try:
        value = decode_json(payload.decode("utf-8"), max_depth=max_depth)
    except RecursionError:
        return RedactionVerdict("pre_native_evidence_depth_limit")
    except ValueError:
        return RedactionVerdict("invalid_pre_native_evidence_json")
    for count, (field, path, is_key) in enumerate(_fields(value), start=1):
        if count > max_items:
            return RedactionVerdict("pre_native_evidence_item_limit", path, max_items)
        if type(field) is str and (code := _text_refusal(field, is_key=is_key)):
            return RedactionVerdict(code, path, count)
    return RedactionVerdict("pre_native_evidence_redacted", inspected_items=count)
