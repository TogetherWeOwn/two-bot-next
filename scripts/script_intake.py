"""Shared JSON and HTTP intake for repository scripts.

Callers own their schemas, endpoint allowlists, timeouts and safe diagnostics.
"""

import json
import math
import re
import urllib.request

# Consume even an unterminated string as one token so malformed input stays
# linear and brackets inside strings never count against the nesting limit.
_JSON_TOKEN = re.compile(r'"(?:[^"\\]|\\.)*(?:"|\\?\Z)|[\[\]{}]', re.DOTALL)


def json_depth_exceeds(text, limit):
    """Return whether JSON text opens more than limit nested containers."""
    depth = 0
    for match in _JSON_TOKEN.finditer(text):
        token = match.group()
        if token in "[{":
            depth += 1
            if depth > limit:
                return True
        elif token in "]}":
            depth -= 1
    return False


def unique_object(pairs, *, error=ValueError, message="duplicate JSON object key"):
    """Reject repeated decoded member names, including escaped spellings."""
    result = {}
    for key, value in pairs:
        if key in result:
            raise error(message)
        result[key] = value
    return result


def _reject_constant(_value):
    raise ValueError("non-finite JSON number")


def _finite_float(text):
    value = float(text)
    if not math.isfinite(value):
        raise ValueError("non-finite JSON number")
    return value


def decode_legacy_json(raw, *, max_depth=None, **options):
    """Preserve a migrated reader's existing decoder options and errors."""
    if max_depth is not None:
        # Match json.loads' byte encoding detection, including UTF-16/32 BOMs.
        if isinstance(raw, (bytes, bytearray)):
            raw = raw.decode(json.detect_encoding(raw), "surrogatepass")
        if isinstance(raw, str) and json_depth_exceeds(raw, max_depth):
            raise RecursionError("JSON nesting exceeds the decoder bound")
    return json.loads(raw, **options)


def decode_json(raw, *, max_depth=64):
    """Decode with a nesting bound and reject duplicate keys and non-finite numbers."""
    if max_depth is None:
        raise ValueError("missing JSON nesting limit")
    return decode_legacy_json(raw, max_depth=max_depth, object_pairs_hook=unique_object,
                              parse_constant=_reject_constant, parse_float=_finite_float)


def read_with_overflow(stream, byte_limit):
    """Read at most byte_limit + 1 bytes so callers can detect overflow."""
    if byte_limit < 0:
        raise ValueError("negative input byte limit")
    return stream.read(byte_limit + 1)


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def __init__(self, error):
        self._error = error

    def redirect_request(self, request, response, code, message, headers, new_url):
        if self._error is not None:
            raise self._error()
        return None


class _EarlyRefusal(_NoRedirect):
    def http_error_302(self, request, response, code, message, headers):
        # Some callers treat a redirect as refusal without inspecting Location
        # or reading the body, even when the response has no target.
        response.close()
        raise self._error()

    http_error_301 = http_error_303 = http_error_307 = http_error_308 = http_error_302


def no_redirect_opener(*, https_handler=None, use_proxy=True, redirect_error=None,
                       inspect_location=False):
    """Build an opener that leaves redirect status handling to the caller.

    By default, urllib surfaces a redirect as HTTPError. A redirect_error
    factory supplies a caller's fixed diagnostic instead. inspect_location
    preserves urllib's Location handling before invoking that factory.
    """
    handlers = []
    if https_handler is not None:
        handlers.append(https_handler)
    if not use_proxy:
        handlers.append(urllib.request.ProxyHandler({}))
    redirect_handler = _NoRedirect
    if redirect_error is not None and not inspect_location:
        redirect_handler = _EarlyRefusal
    handlers.append(redirect_handler(redirect_error))
    return urllib.request.build_opener(*handlers)
