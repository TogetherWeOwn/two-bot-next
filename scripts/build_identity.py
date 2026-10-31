"""Canonical compiled build identity shapes shared by deploy and probe gates."""

from datetime import datetime, timezone
import re

SHA = re.compile(r"[0-9a-f]{40}")
DIGITS = re.compile(r"[0-9]+")
UNSTAMPED = "unknown"

# Remote build IDs are `<run id>-<attempt>` (digits-digits); bound both the
# total and each half so an overlong value never reaches diagnostics.
MAX_BUILD_ID_LEN = 64
MAX_BUILD_ID_PART_LEN = 20


def is_build_revision_shape(value):
    """Return whether a compiled revision is a canonical 40-character SHA."""
    return isinstance(value, str) and len(value) == 40 and SHA.fullmatch(value) is not None


def is_build_id_shape(value):
    """Digits-digits run shape, bounded so overlong never reaches output."""
    if not isinstance(value, str) or len(value) > MAX_BUILD_ID_LEN:
        return False
    parts = value.split("-")
    return (
        len(parts) == 2
        and all(part and len(part) <= MAX_BUILD_ID_PART_LEN and DIGITS.fullmatch(part)
                for part in parts)
    )


# Deploy IDs are `YYYY.MM.DD.N+sha7`: the UTC calendar date, N = the Nth
# deploy of that environment that day (starting at 1), and the 7-char short
# SHA of the deployed commit. Stamped at build (`BOT_DEPLOY_ID`), reported on
# /readyz and /version, and recorded as the GitHub Deployment description —
# never a tag or Release.
DEPLOY_ID = re.compile(r"[0-9]{4}\.[0-9]{2}\.[0-9]{2}\.[0-9]+\+[0-9a-f]{7}")
SHA7 = re.compile(r"[0-9a-f]{7}")
MAX_DEPLOY_ID_LEN = 32
MAX_DEPLOY_ID_N_LEN = 6


def parse_deploy_id(value):
    """Split a deploy ID into (`YYYY.MM.DD` date, N, sha7); ValueError unless canonical.

    Canonical means the fixed shape, a real calendar date (month 13 and
    February 30 refuse), N a positive integer without leading zeros, and a
    lowercase 7-hex short SHA.
    """
    if not isinstance(value, str) or DEPLOY_ID.fullmatch(value) is None:
        raise ValueError("not a deploy ID")
    dated, sha7 = value.split("+")
    year, month, day, num = dated.split(".")
    try:
        datetime(int(year), int(month), int(day))
    except ValueError:
        raise ValueError("deploy ID date is not a real calendar date") from None
    if len(num) > MAX_DEPLOY_ID_N_LEN or str(int(num)) != num or int(num) < 1:
        raise ValueError("deploy ID sequence is not a positive integer")
    return f"{year}.{month}.{day}", int(num), sha7


def is_deploy_id_shape(value):
    """Return whether a value is a canonical `YYYY.MM.DD.N+sha7` deploy ID."""
    if not isinstance(value, str) or len(value) > MAX_DEPLOY_ID_LEN:
        return False
    try:
        parse_deploy_id(value)
    except ValueError:
        return False
    return True


def format_deploy_id(date_text, n, sha7):
    """Build the canonical ID from parts; ValueError unless every part is canonical."""
    if not isinstance(date_text, str):
        raise ValueError("deploy ID date must be YYYY.MM.DD")
    try:
        parsed = datetime.strptime(date_text, "%Y.%m.%d")
    except ValueError:
        raise ValueError("deploy ID date must be a real YYYY.MM.DD calendar date") from None
    # strptime accepts unpadded fields ("2026.1.1"); only the zero-padded
    # spelling is canonical.
    if parsed.strftime("%Y.%m.%d") != date_text:
        raise ValueError("deploy ID date must be zero-padded YYYY.MM.DD")
    if type(n) is not int or n < 1 or len(str(n)) > MAX_DEPLOY_ID_N_LEN:
        raise ValueError("deploy ID sequence must be a positive integer")
    if not isinstance(sha7, str) or SHA7.fullmatch(sha7) is None:
        raise ValueError("deploy ID short SHA must be 7 lowercase hex chars")
    return f"{date_text}.{n}+{sha7}"


def deploy_id_sort_key(value):
    """Ordering key (date, N, sha7); ValueError unless canonical.

    The zero-padded date orders chronologically as a string; N then the
    short SHA break ties across same-day deploys and environments.
    """
    date_text, n, sha7 = parse_deploy_id(value)
    return (date_text, n, sha7)


def deployment_utc_date(row):
    """A deployment row's `created_at` as a UTC date; ValueError unless parseable.

    A row without a parseable zoned timestamp fails closed: silently
    skipping it could mint a duplicate N (in the count) or stop the listing
    early (in the fetch).
    """
    if not isinstance(row, dict):
        raise ValueError("deployment row is not an object")
    raw = row.get("created_at")
    if not isinstance(raw, str) or not raw:
        raise ValueError("deployment row has no created_at timestamp")
    try:
        moment = datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except ValueError:
        raise ValueError("deployment created_at is not ISO-8601") from None
    if moment.tzinfo is None:
        raise ValueError("deployment created_at carries no time zone")
    return moment.astimezone(timezone.utc).date()


def count_deployments_for_date(deployments, date_text):
    """How many recorded deploy IDs went out on a UTC `YYYY.MM.DD` date.

    `deployments` are GitHub deployment mappings with `created_at` ISO-8601
    timestamps. Only rows carrying a canonical deploy ID as their
    description count: those are the records `deploy_id.py record` minted.
    GitHub Actions also auto-creates Deployment rows for `environment:` jobs
    (each deploy job's own run included); those carry no deploy ID and never
    count toward N. A row without a parseable zoned timestamp fails closed
    (ValueError): silently skipping it could mint a duplicate N.
    """
    try:
        want = datetime.strptime(date_text, "%Y.%m.%d").date()
    except (ValueError, TypeError):
        raise ValueError("date must be a real YYYY.MM.DD calendar date") from None
    return sum(1 for row in deployments
               if deployment_utc_date(row) == want
               and is_deploy_id_shape(row.get("description")))
