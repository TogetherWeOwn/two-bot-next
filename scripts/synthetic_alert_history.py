#!/usr/bin/env python3
"""Decide synthetic alert transitions from this workflow's recent probe runs."""

import http.client
import json
import os
import re
import urllib.error
import urllib.parse
import urllib.request

API = "https://api.github.com"
WORKFLOW = "synthetic-user-flow.yml"
PROBE_STEPS = {
    "staging": "Run the staging user-flow probe",
    "prod": "Run the passive production probe",
}
ALERT_STEPS = {
    "staging": "File or close the incident issue on transition",
    "prod": "File or close the incident issue on transition",
}
# The post step's name before alerts moved from Discord to incident issues.
# Older runs in the reconstruction window still carry it; it is the same
# delivery signal, so both names read as the post step (at most one per job).
LEGACY_ALERT_STEP = "Alert the ops channel on incident transition"
# Decide step (writes event to GITHUB_OUTPUT, never posts). The post step
# above is the only delivery signal: success means delivered, failure means
# undelivered, skipped/None means no-op and must not move incident state.
# The decide step's own conclusion is the known-history record: a healthy
# run whose history read failed fails its decide step (see
# synthetic_alert.main), so decide=success with a skipped post is an
# explicit decided-none record — a known-closed checkpoint on a healthy
# probe, a known-open one on a failed probe (see _run_outcome) — while
# decide!=success leaves that sample ambiguous and state must not move.
# Both records also require a normally finished job (its conclusion matches
# the probe outcome): a cancelled or timed-out job concludes neither way,
# and its post step is skipped by the !cancelled() gate even when an event
# was due, so it can fake neither record.
DECIDE_STEPS = {
    "staging": "Decide the synthetic alert event",
    "prod": "Decide the synthetic alert event",
}
JOB_NAMES = {
    "staging": "staging user-flow synthetic",
    "prod": "prod passive user-flow synthetic",
}
# Bounded reconstruction: page the workflow-runs list and read jobs
# newest-first until the collected suffix determines the incident state on
# its own (a checkpoint: replaying it from any prior state converges), the
# history is exhausted, or the scan bound below is hit. Hitting the bound
# means the incident state is unknown, which is a HistoryError so the
# caller keeps the documented fail-open policy instead of acting on a
# partial guess. 200 prior runs at the five-minute schedule is roughly
# sixteen hours of history.
#
# The page size is sized to the response cap in _get, not to GitHub's
# per_page maximum of 100: a workflow-run object embeds head_commit plus
# two full repository objects and measures 15-40 KB, so a 100-run page is
# about 2 MB and would exceed the cap on every read. Fifteen runs stay
# under 1 MiB even at the largest observed run object (~38 KB).
RUNS_PAGE_SIZE = 15
MAX_HISTORY_RUNS = 200
MAX_RESPONSE_BYTES = 1 << 20


# Strings are skipped whole, so brackets inside them never count. An
# unterminated string runs to the end, which keeps the scan linear.
JSON_TOKEN = re.compile(r'"(?:[^"\\]|\\.)*(?:"|\\?\Z)|[\[\]{}]', re.DOTALL)
# CPython's old default recursion limit, which bounded the C JSON decoder.
MAX_JSON_DEPTH = 1000


def _loads_bounded(raw, **kwargs):
    """json.loads with a lexical nesting bound that raises RecursionError.

    Newer CPython sizes the decoder's recursion guard from the real C stack
    and decodes 10,000 levels, so the bound no longer depends on the
    interpreter: existing RecursionError refusals keep working unchanged.
    """
    # Decode bytes exactly as json.loads would, then scan and parse that text.
    if isinstance(raw, (bytes, bytearray)):
        raw = raw.decode(json.detect_encoding(raw), "surrogatepass")
    depth = 0
    for match in JSON_TOKEN.finditer(raw):
        token = match.group()
        if token in "[{":
            depth += 1
            if depth > MAX_JSON_DEPTH:
                raise RecursionError("JSON nesting exceeds the decoder bound")
        elif token in "]}":
            depth -= 1
    return json.loads(raw, **kwargs)


class HistoryError(Exception):
    """A safe-to-report failure reading probe history."""

    code = "history_unknown"
    next_action = "inspect_workflow_history"


def _refuse_history():
    # Never include a remote field, parser detail or URL in this diagnostic.
    raise HistoryError("synthetic workflow history is malformed or ambiguous; "
                       "incident state is unknown")


def _positive_integer(value):
    # bool is an int subclass; floats (including nonfinite ones) are not IDs.
    return type(value) is int and 0 < value <= (1 << 63) - 1


def _collection(body, key, limit=None, complete=False):
    if not isinstance(body, dict) or not isinstance(body.get(key), list):
        _refuse_history()
    rows = body[key]
    if (limit is not None and len(rows) > limit
            or any(not isinstance(row, dict) for row in rows)):
        _refuse_history()
    # Older fixture/record shapes omit counts. If supplied, a count is a
    # claim we must validate, never a permissive pagination hint. Jobs are
    # deliberately one bounded page: an incomplete page cannot prove uniqueness.
    if "total_count" in body:
        count = body["total_count"]
        if (type(count) is not int or not 0 <= count <= (1 << 63) - 1
                or count < len(rows) or (complete and count != len(rows))):
            _refuse_history()
    return rows


CONCLUSIONS = (None, "success", "failure", "neutral", "cancelled", "skipped",
               "timed_out", "action_required", "stale", "startup_failure")


def _completed_record(record):
    # Missing optional fields keep pre-decide history readable, but cannot
    # manufacture a decided-none checkpoint. Present fields must be supported.
    if ("status" in record and record["status"] != "completed"
            or record.get("conclusion") not in CONCLUSIONS):
        _refuse_history()


def _run_record(run):
    if (not isinstance(run, dict) or not _positive_integer(run.get("id"))
            or not _positive_integer(run.get("run_number"))
            or not isinstance(run.get("event"), str) or not run["event"]):
        _refuse_history()
    _completed_record(run)


def _named_record(record):
    if not isinstance(record.get("name"), str) or not record["name"]:
        _refuse_history()
    _completed_record(record)


def _unique_match(records, name):
    matches = [record for record in records if record["name"] == name]
    if len(matches) > 1:
        _refuse_history()
    return matches[0] if matches else None


class DuplicateKeyError(ValueError):
    """A JSON object repeats a key; the message never carries key/value data."""


def _reject_duplicate_keys(pairs):
    """object_pairs_hook refusing repeats instead of last-wins.

    The json decoder calls this for every object at any depth, after
    escape decoding, so {"a": 1, "\\u0061": 2} counts as a repeat. The
    raised message is fixed vocabulary: it names no key, value or path.
    """
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise DuplicateKeyError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Turn every redirect into an HTTPError without contacting its target."""

    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


def _advance(state, outcome, delivery):
    """One chronological sample applied to (failures, incident, delivered)."""
    failures, incident, alert_delivered = state
    if outcome == "failure":
        failures += 1
        # Delivery evidence counts even below the two-failure threshold, so
        # a delivered opening alert that is the oldest retained sample (the
        # failures before it rolled out of the window) is not discarded; it
        # takes effect once the retained streak reaches incident size.
        if delivery == "success":
            alert_delivered = True
        elif delivery == "failure":
            alert_delivered = False
        elif delivery == "open" and failures >= 2:
            # Decided-none record on a failed probe (see _run_outcome): at
            # incident size it can only mean the opening alert had already
            # delivered, because an undelivered incident re-alerts instead.
            # Two "open" samples in a row therefore replay to open-and-
            # delivered from every prior state, which is what lets
            # _checkpoint stop a mid-outage scan after a few jobs reads
            # instead of re-reading the whole outage on every decision.
            alert_delivered = True
        if failures >= 2:
            incident = True
    else:
        if delivery == "closed":
            # Explicit known-closed checkpoint (healthy probe, decide step
            # succeeded with known history, no event due): the incident
            # state after this sample is closed whatever came before it.
            return 0, False, False
        failures = 0
        if incident:
            if alert_delivered and delivery == "success":
                incident = False
                alert_delivered = False
            elif not alert_delivered and delivery == "success":
                alert_delivered = True
        else:
            # A healthy sample ends any sub-threshold streak, so delivery
            # evidence carried from it must not leak into the next incident.
            alert_delivered = False
    return failures, incident, alert_delivered


def _replay(chronological, initial=(0, False, False)):
    state = initial
    for outcome, delivery in chronological:
        state = _advance(state, outcome, delivery)
    return state


def _checkpoint(outcomes):
    """Whether newest-first `outcomes` fix the incident state by themselves.

    True once replaying the suffix from every prior state (failure count
    capped at the decision-relevant 0/1/2+, incident and delivered either
    way) converges to one state: older history can then no longer change
    the decision, so reconstruction may stop before it.
    """
    chronological = list(reversed(outcomes))
    finals = {
        (min(failures, 2), incident, delivered)
        for failures, incident, delivered in (
            _replay(chronological, (count, incident, delivered))
            for count in (0, 1, 2)
            for incident in (False, True)
            for delivered in (False, True)
        )
    }
    return len(finals) == 1


def incident_state(previous):
    """Rebuild whether the recent incident was alerted and then recovered.

    Only a post-step conclusion of "success" counts as delivered; "failure"
    means undelivered. Any other conclusion ("skipped", None, cancelled)
    is a no-op and leaves incident state unchanged, so a history-read
    no-op can never close an open incident or fake a delivery. The two
    synthetic exceptions come only from _run_outcome's decided-none
    records: "closed", a known-closed checkpoint that resets the state,
    and "open", a failed probe that decided no event with known history.
    """
    return _replay(reversed(previous))


def transition(current, previous, force_fail=False):
    """Return incident transitions, retrying alerts that did not deliver."""
    if current not in ("success", "failure"):
        return None
    failures, incident, alert_delivered = incident_state(previous)
    if current == "failure":
        if force_fail or (not incident and failures >= 1) or (incident and not alert_delivered):
            return "alert"
        return None
    if incident:
        return "recovery" if alert_delivered else "alert"
    return None


def _get(path, token):
    request = urllib.request.Request(
        API + path,
        headers={"Accept": "application/vnd.github+json",
                 "Authorization": "Bearer " + token,
                 "X-GitHub-Api-Version": "2022-11-28",
                 "User-Agent": "two-bot-next-synthetic-alert/1"},
    )
    # urllib follows redirects by default, which would resend the GitHub
    # Bearer token to the redirect target. Reject redirects at the opener
    # so only the api.github.com URL is ever contacted (same policy as the
    # alert POST in synthetic_alert.py).
    opener = urllib.request.build_opener(
        urllib.request.HTTPHandler(),
        urllib.request.HTTPSHandler(),
        urllib.request.HTTPErrorProcessor(),
        _NoRedirectHandler(),
    )
    try:
        with opener.open(request, timeout=10) as response:
            body = response.read(MAX_RESPONSE_BYTES + 1)
    except urllib.error.HTTPError as error:
        if error.code in (301, 302, 303, 307, 308):
            raise HistoryError(
                "could not read recent synthetic workflow history "
                f"(redirect refused with status {error.code})") from None
        raise HistoryError("could not read recent synthetic workflow history ("
                           + error.__class__.__name__ + ")") from None
    except (OSError, http.client.HTTPException) as error:
        # HTTPException covers a mid-read truncation (IncompleteRead),
        # which is not an OSError: it is unknown history, not a crash, so
        # the fail-open policy still applies.
        raise HistoryError("could not read recent synthetic workflow history ("
                           + error.__class__.__name__ + ")") from None
    if len(body) > MAX_RESPONSE_BYTES:
        # Never parse a silently truncated body: oversize is unknown
        # history, so the caller's fail-open policy applies.
        raise HistoryError("could not read recent synthetic workflow history "
                           "(response exceeds the size cap)")
    try:
        return _loads_bounded(body, object_pairs_hook=_reject_duplicate_keys)
    except (ValueError, RecursionError) as error:
        raise HistoryError("could not read recent synthetic workflow history ("
                           + error.__class__.__name__ + ")") from None


def _run_outcome(repo, run, environment, token):
    _run_record(run)
    body = _get(f"/repos/{repo}/actions/runs/{run['id']}/jobs?per_page=100", token)
    jobs = _collection(body, "jobs", 100, complete=True)
    job_ids = set()
    for item in jobs:
        _named_record(item)
        if "id" in item:
            if not _positive_integer(item["id"]) or item["id"] in job_ids:
                _refuse_history()
            job_ids.add(item["id"])
        steps = _collection(item, "steps")
        numbers = set()
        for step in steps:
            _named_record(step)
            if "number" in step:
                if not _positive_integer(step["number"]) or step["number"] in numbers:
                    _refuse_history()
                numbers.add(step["number"])
    job = _unique_match(jobs, JOB_NAMES[environment])
    if job is None:
        return None
    steps = job["steps"]
    probe = _unique_match(steps, PROBE_STEPS[environment])
    alert = _unique_match(steps, ALERT_STEPS[environment])
    legacy = _unique_match(steps, LEGACY_ALERT_STEP)
    if alert is not None and legacy is not None:
        _refuse_history()
    alert = alert or legacy
    decide = _unique_match(steps, DECIDE_STEPS[environment])
    outcome = probe.get("conclusion") if probe else None
    if outcome in ("success", "failure"):
        delivery = alert.get("conclusion") if alert else None
        # A decided-none record — the decide step succeeded (which it only
        # does when that run's history was known) and no event was due, so
        # the post step skipped — only counts from a job that finished
        # normally: its conclusion equals the probe outcome (the probe step
        # has no continue-on-error, so a failed probe fails its job). A
        # cancelled or timed-out job concludes neither way and its post
        # step is skipped by the !cancelled() gate even when an event was
        # decided, so it records nothing; a raw sample with no such record
        # stays ambiguous below, as does a missing job conclusion.
        decided_none = (delivery == "skipped" and decide is not None
                        and decide.get("conclusion") == "success"
                        and job.get("conclusion") == outcome)
        if decided_none and outcome == "success":
            # Known-closed checkpoint, not a mere skipped post: with an
            # open incident a healthy run would have decided recovery or
            # the late alert, so the post step would have run.
            delivery = "closed"
        elif decided_none:
            # Known-open record: with unknown history a failed probe would
            # have fail-open alerted and with an undelivered incident it
            # would have re-alerted, so deciding none means either a first
            # failure or an already-delivered incident; replaying two in a
            # row rules the first failure out (see _advance).
            delivery = "open"
        if (alert is legacy and outcome == "failure"
                and delivery in ("success", "open")):
            # A Discord alert reached no agent, so an outage it delivered
            # stays undelivered: the next failed probe re-alerts into an
            # incident issue instead of waiting for recovery.
            delivery = "failure"
        return outcome, delivery
    return None


def previous_probe_outcomes(env, environment):
    """Newest-first prior scheduled probe samples, rebuilt page by page.

    Pages are read lazily and jobs newest-first; the scan stops early once
    the collected suffix is a confirmed checkpoint (its incident state no
    longer depends on older history, confirmed by one further sample). A
    known-closed healthy sample is such a checkpoint on its own, and two
    known-open samples are one together, so both the healthy steady state
    and a long delivered outage stop after a couple of jobs reads instead
    of scanning back to the incident boundary on every decision. If
    MAX_HISTORY_RUNS prior runs are
    scanned with no checkpoint and more history may exist, the state is
    unknown: raise HistoryError rather than return a partial suffix that
    would silently invent one — unless every collected sample is healthy
    with no post step run, which settles closed as documented at the raise.
    """
    token = env.get("GITHUB_TOKEN")
    repository = env.get("GITHUB_REPOSITORY")
    run_id = env.get("GITHUB_RUN_ID") or env.get("RUN_ID")
    if not token or not repository or not run_id:
        raise HistoryError("workflow history context is missing")
    repo = urllib.parse.quote(repository, safe="/")
    outcomes = []
    seen = set()
    seen_numbers = set()
    oldest_number = None
    scanned = 0
    confirmed = False
    page = 1
    # Filtered rows must not turn a bounded reconstruction into unbounded
    # pagination. Allow one extra page for the current run being excluded.
    max_pages = (MAX_HISTORY_RUNS + RUNS_PAGE_SIZE - 1) // RUNS_PAGE_SIZE + 1
    while scanned < MAX_HISTORY_RUNS and page <= max_pages:
        query = urllib.parse.urlencode(
            {"branch": "main", "event": "schedule", "status": "completed",
             "per_page": RUNS_PAGE_SIZE, "page": page})
        runs = _get(f"/repos/{repo}/actions/workflows/{WORKFLOW}/runs?{query}", token)
        page_runs = _collection(runs, "workflow_runs", RUNS_PAGE_SIZE)
        # Admit the entire page, including filtered rows, before sorting or
        # reading a job. A plausible checkpoint must not hide malformed peers.
        for run in page_runs:
            _run_record(run)
            if (run["id"] in seen or run["run_number"] in seen_numbers
                    or oldest_number is not None and run["run_number"] >= oldest_number):
                _refuse_history()
            seen.add(run["id"])
            seen_numbers.add(run["run_number"])
        total = runs.get("total_count")
        returned = (page - 1) * RUNS_PAGE_SIZE + len(page_runs)
        if total is not None and (total < returned
                or len(page_runs) < RUNS_PAGE_SIZE and total != returned):
            _refuse_history()
        if page_runs:
            oldest_number = min(run["run_number"] for run in page_runs)
        prior = sorted(
            (run for run in page_runs
             if str(run["id"]) != str(run_id) and run["event"] == "schedule"),
            key=lambda run: run["run_number"], reverse=True,
        )
        bound_hit = False
        for run in prior:
            if scanned >= MAX_HISTORY_RUNS:
                bound_hit = True
                break
            scanned += 1
            sample = _run_outcome(repo, run, environment, token)
            if sample is None:
                continue
            outcomes.append(sample)
            if _checkpoint(outcomes):
                if confirmed:
                    return outcomes
                confirmed = True
            else:
                confirmed = False
        if bound_hit:
            break
        if len(page_runs) < RUNS_PAGE_SIZE:
            return outcomes
        total = runs.get("total_count")
        if isinstance(total, int) and page * RUNS_PAGE_SIZE >= total:
            return outcomes
        page += 1
    if scanned >= MAX_HISTORY_RUNS and outcomes and all(
            outcome == "success" and delivery in ("skipped", "closed")
            for outcome, delivery in outcomes):
        # Bound reached over an all-healthy suffix in which no post step
        # ever ran (conclusion exactly skipped, or the closed checkpoint):
        # settle closed (the same replay-from-closed the exhausted-history
        # path above uses) instead of reporting unknown forever. Without
        # this, MAX_HISTORY_RUNS healthy samples recorded while history was
        # unreadable would latch every later decision into HistoryError.
        # The accepted cost: an incident whose evidence rolled past the
        # bound behind MAX_HISTORY_RUNS consecutive healthy samples loses
        # its recovery post. A post step that ran (success/failure) is
        # incident evidence and still reports unknown below, as does a
        # cancelled or missing post step, which may have had an event due.
        return outcomes
    raise HistoryError(
        "synthetic workflow history is incomplete beyond the reconstruction "
        f"bound of {MAX_HISTORY_RUNS} prior runs; incident state is unknown")
