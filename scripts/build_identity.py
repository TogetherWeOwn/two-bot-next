"""Canonical compiled build identity shapes shared by deploy and probe gates."""

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
