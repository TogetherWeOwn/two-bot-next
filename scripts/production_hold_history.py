"""Bounded, read-only GitHub history transport for production hold evidence.

Import is inert; callers supply the environment when constructing a reader.
Consumers must exhaust pages to establish complete traversal.
"""

import urllib.parse

import incident_issue

MAX_PAGES = 10
PAGE_SIZE = 100


class History:
    def __init__(self, env):
        self.token, self.repo = incident_issue._context(env)
        self.requests = 0

    def get(self, path, **query):
        self.requests += 1
        if self.requests > 100:
            raise incident_issue.IncidentError("production hold history exceeds the request cap")
        suffix = "?" + urllib.parse.urlencode(query) if query else ""
        return incident_issue._request("GET", f"/repos/{self.repo}/{path}{suffix}", self.token)

    def pages(self, path, key=None, **query):
        for page in range(1, MAX_PAGES + 1):
            data = self.get(path, per_page=PAGE_SIZE, page=page, **query)
            rows = data.get(key) if key is not None and isinstance(data, dict) else (
                data if key is None else None)
            if not isinstance(rows, list) or len(rows) > PAGE_SIZE or any(
                    not isinstance(row, dict) for row in rows):
                raise incident_issue.IncidentError("production hold history is malformed")
            yield from rows
            if len(rows) < PAGE_SIZE:
                return
        raise incident_issue.IncidentError("production hold history exceeds the page cap")
