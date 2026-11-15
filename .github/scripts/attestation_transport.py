"""Inactive, bounded GitHub metadata reader; not an attestation enforcement gate.

Based on the earlier base_attestation_gate.py reader by Rick7C2. This reader
provides neither freshness nor atomic consistency; callers must establish both.
"""

import json
import re
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener


MAX_RESPONSE_BYTES = 8 * 1024 * 1024
TIMEOUT_SECONDS = 30
_REPOSITORY = re.compile(r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})/[A-Za-z0-9_.-]{1,100}\Z")


class TransportError(ValueError):
    """A metadata read was refused without exposing request or response data."""


class _NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def _object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError
        result[key] = value
    return result


def _constant(_value):
    raise ValueError


class RestReader:
    """Read only PR metadata, bounded file pages, or the main branch ref."""

    def __init__(self, repository, token):
        if not (isinstance(repository, str) and _REPOSITORY.fullmatch(repository)
                and repository.split('/')[1] not in ('.', '..')
                and isinstance(token, str) and 0 < len(token) <= 4096
                and all(33 <= ord(c) <= 126 for c in token)):
            raise TransportError("Metadata request refused")
        self._prefix = f"repos/{repository}/"
        self._token = token
        self._opener = build_opener(ProxyHandler({}), _NoRedirect())

    def __call__(self, route):
        try:
            if not isinstance(route, str) or not route.startswith(self._prefix):
                raise ValueError
            suffix = route[len(self._prefix):]
            pr = r"pulls/[1-9][0-9]{0,9}"
            if not (re.fullmatch(pr, suffix)
                    or re.fullmatch(pr + r"/files\?per_page=100&page=(?:[1-9]|[12][0-9]|3[01])", suffix)
                    or suffix == "git/ref/heads/main"):
                raise ValueError
            request = Request("https://api.github.com/" + route, headers={
                "Authorization": "Bearer " + self._token,
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
            }, method="GET")
            with self._opener.open(request, timeout=TIMEOUT_SECONDS) as response:
                if response.status != 200:
                    raise ValueError
                raw = response.read(MAX_RESPONSE_BYTES + 1)
            if len(raw) > MAX_RESPONSE_BYTES:
                raise ValueError
            value = json.loads(raw.decode("utf-8"), object_pairs_hook=_object,
                               parse_constant=_constant)
            expected_type = list if "/files?" in suffix else dict
            if not isinstance(value, expected_type):
                raise ValueError
            return value
        except Exception:
            raise TransportError("Metadata request refused") from None
