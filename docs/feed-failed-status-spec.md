# Dead-feed failed status and staff notice

Status: **Proposed; design only**, 2026-10-26. Intent review by 2026-11-02.
Source inventory: `baa387910bf8191f1632f07b03bf77c2c4713804`.
This is the repository source of truth for a future failed-feed status, not an
available status, command, notice, schema migration or activation. Nothing here
authorizes a build or changes a relay. Source-shipped is not deployed.

## Decision and no-build alternative

A relay whose source dies currently fails every poll forever: each failure
collapses into a process-wide metric class, an audit reason and a log line, the
relay keeps consuming its slot in every pass, and no staff-facing surface says
the relay is dead. Staff should learn once, privately, that a specific relay
has stopped being polled because its source kept failing — and be able to probe
it back to life or remove it — without a notice per poll and without ever
printing the raw source.

| Option | Consequence | Recommendation |
| --- | --- | --- |
| A. No build | Dead relays stay silent; staff triage aggregate metrics and logs per [feed operations](feed-supervisor.md), then pause/remove by hand | **Default now** until exact intent and security disposition |
| B. Display-only derived label | `/feed-list` could guess "failing" from `last_checked_at`; that stamp advances on failure too, so the label would be a fabricated health verdict | Reject as stated; display only over persisted evidence (slice `FF-W1`) |
| C. Persisted failure evidence + derived `failed` + fetch fail-stop + one staff notice per episode + refresh probe | Smallest surface that actually ends the silence and the wasted polls; needs a migration, a fenced transition and a notice claim | **Recommended conditional future option**, specified below |
| D. DM the creator, or email | Creator may have left; no consent/opt-in store exists; email needs member PII and new delivery infrastructure | Reject |
| E. Auto-remove after a threshold | Destroys delivery history and bypasses the pending-evidence removal fence | Reject |
| F. Notice on every failed poll | At a 60 s interval one dead relay produces 1,440 notices a day | Reject |

Precedent considered (MonitoRSS): a FAILED status, a time-based fail-stop
(`hoursUntilFail`), in-channel `notifyFail` alerts, refresh-to-recover, email
opt-in. This proposal adopts the status, the time-dominant fail-stop and
refresh; it deliberately departs on notice placement (staff channel, never the
relay's possibly-public destination channel) and drops email/DM entirely.
Demand is unmeasured: no count of currently-dead relays exists today, which is
itself part of the problem statement. Slice `FF-W1` is observe-only precisely
so thresholds are tuned on recorded streaks before any notice can fire.

## Observed source and limits

Repository inspection is not deployed-state evidence. No relay, channel,
database or live source was read.

| Current seam | Evidence and limit |
| --- | --- |
| Relay state | `feed_relays` carries only `enabled` and `last_checked_at` (`crates/cutover/migrations/0180_feeds.sql:2-28`); `FeedRelay` mirrors that (`crates/core/src/feeds.rs:66-77`). Deliveries are only `pending`/`delivered`. No failure count, last-success, last-error or failed state exists |
| Failure collapse | Production fetch maps every `FeedConnectError` to job-level `ErrorClass::Feed` (`crates/bot/src/feed_jobs.rs:140-164`); non-2xx and XML parse failures land in the same class. The fine-grained vocabulary survives only in process metrics (`crates/core/src/feeds_connector.rs:105-133`, `crates/core/src/metrics.rs:534-553`), never per relay |
| Attempt stamp | `mark_checked` advances `last_checked_at` after success **and** failure (`crates/core/src/feeds_store.rs:559-568`, called unconditionally in the poll tail); the inspect copy already warns the stamp is attempt evidence, never a health verdict (`crates/core/src/feeds.rs:350-362`). No failed derivation may reuse it |
| Poll audit | A failed fetch audits `poll_failed`/`fetch_or_parse`, every pass audits `poll_result` (`crates/bot/src/feed_jobs.rs:493-504`); audit writes are separate best-effort calls whose failure only sets a job error class (`crates/bot/src/feed_jobs.rs:753-778`). Audit rows are evidence, not a status store |
| Job counter | The supervisor tracks `consecutive_failures` per **job**, in memory (`crates/bot/src/jobs.rs:24-31`, reset/increment at `crates/bot/src/jobs.rs:176-181`). It mixes all relays, database and REST failures, and dies on restart; it cannot seed per-relay health |
| Lifecycle | Pause/resume flip only `enabled`/`updated_at` and preserve history (`crates/core/src/feeds_store.rs:178-235`); pause refuses while pending delivery evidence exists (`crates/core/src/feeds_store.rs:287-303`); resume never refuses on pending because recovery needs an enabled relay (`crates/core/src/feeds_store.rs:320-361`). The poller re-reads `enabled` before any fetch (`crates/bot/src/feed_jobs.rs:441-450`) |
| Cadence | One guild-wide interval, 60–86,400 s (`crates/core/src/feeds.rs:1911-1924`). A pure attempt-count threshold therefore means "10 minutes dead" in one guild and "10 days dead" in another; derivation must be time-dominant |
| Scale bound | At most 25 relays per guild, disabled included (`crates/core/src/feeds_store.rs:25-30`). Worst-case notice volume is bounded by construction |
| Source safety | Fetch validation refuses credential-bearing userinfo URLs (`crates/core/src/feeds_http.rs:58-87`), but stored legacy rows and credential-like **query** values remain possible; the inspect projection drops userinfo/fragments and redacts credential-like query values (`crates/core/src/feeds.rs:364-388`, `crates/core/src/feeds.rs:505-558`, rendered by `crates/core/src/feeds.rs:588-622`) |
| Staff channel | `DISCORD_STAFF_ALERT_CHANNEL_ID` already carries raid/containment alerts: log first, then post with mentions disabled, log-only when unset (`crates/bot/src/raid_runtime.rs:301-340`). No feed code references it; feeds have zero notice path today |
| Dashboard states | No dashboard feed-configuration spec or status enum exists in this baseline tree. A separate parity proposal is not assumed merged, and its final states are **unverified**; §Dashboard mapping defines the contract it must meet, not its contents |

## Failed-status derivation (proposed)

Health is a new dimension, never an overload of `enabled`. Proposed persisted
evidence per relay (future migration; names illustrative): countable-failure
streak, `last_success_at`, `last_failure_at`, bounded `last_failure_class`,
`failed_at`, and a notice claim/outcome pair. Display derives from evidence:

- `unknown` — no recorded attempt outcome yet (every pre-migration row starts
  here; migration **never** backfills failure from `last_checked_at` age, so
  upgrade day cannot mass-fail or mass-notify).
- `active` — enabled, not failed; inspect additionally shows a nonzero streak
  as "failing (n)" evidence without changing the list label.
- `failed` — `failed_at` set; fetch fail-stopped (below).
- `paused` — `enabled=false`; dominates display, health evidence is retained
  but dormant and no transition occurs while paused.

**Countable failure** = a completed, source-attributable fetch/parse outcome:
`invalid_source`, `non_public_address`, `redirect_refused`, `too_large`,
`invalid_encoding`, `unsupported_content_type`, `unsupported_encoding`,
`http_error`, plus a new bounded `parse` class (today a parse failure after a
2xx fetch records a metrics *success* and only a job error — recording joint
fetch+parse outcome per relay is part of `FF-W1`). Fetch policy classes come
from `crates/core/src/feeds_http.rs:15-28` via the existing metrics vocabulary.

**Neutral outcome** (neither increments nor resets the streak): `resolve`,
`transport`, `deadline`, pass/relay `timeout`, `database`, `rest`,
`recovery_required`, `configuration`, `other`. Rationale: the operator guide
already treats resolve/transport/deadline spread as platform trouble, and a
Discord or database outage must never mass-fail healthy relays. Accepted
false negative, stated plainly: a feed whose host stops resolving never
becomes `failed` in v1. Delivery/send failures are likewise never countable —
a Discord outage is not a dead source, and pending recovery keeps its own path.

**Success** = 2xx fetch whose body parses for the relay kind (zero new items
still succeeds). Success resets the streak to 0 and stamps `last_success_at`.

**Transition to `failed`** (all must hold, evaluated on a countable failure):
relay enabled, `failed_at` unset, streak ≥ 3 countable failures, and
now − anchor ≥ 24 h, where anchor is `last_success_at`, or relay creation when
it has never succeeded. The time arm makes the threshold interval-invariant;
the count arm prevents one slow-cadence guild's single bad day from failing a
relay on first sight. Both numbers are decision questions below, tunable only
as reviewed constants — no per-relay or dashboard threshold editing in v1.
No HTTP-status fast path (404/410): status codes are not retained today
(`http_error` collapses them), and retaining them is a separate evidence
decision.

**Fail-stop** stops the **fetch** phase only. A failed relay's recovery phase
keeps running each pass until pending evidence settles, because recovery needs
no source and pausing already established that stranding recovery is
unacceptable. Fail-stop never flips `enabled`, deletes history, releases a
claim or reposts. Resource claim is modest and honest: one fetch slot,
one audit pair and one stamp write saved per dead relay per pass.

Outcome recording is one conditional update per attempted relay per pass,
carrying the attempt time; an outcome older than the stored last attempt is
discarded, so a stale poll cannot re-fail a just-refreshed relay. The
`failed` transition is a conditional update (`failed_at IS NULL AND enabled`)
whose single winner claims the notice episode in the same transaction.
Outcomes recorded against a since-removed relay affect zero rows. A success
from a fetch admitted before a racing pause may still land: it is true
evidence, `paused` dominates display, and resume starts from a clean streak.

## Staff notice (proposed)

Exactly one notice claim per failed episode, sent to the existing
`DISCORD_STAFF_ALERT_CHANNEL_ID` through the shared executor with mentions
disabled, log-first like raid alerts. Unset or invalid channel → the status
still transitions, the outcome records `not_configured`, and inspect says so;
no DM, no destination-channel fallback, no retry per poll (a failed relay is
not polled). Notice send failure records a bounded outcome
(`failed`/`unknown`) and is never auto-resent within the episode; an ambiguous
send stays `unknown` rather than risking duplicates. Restart cannot duplicate
a notice: the persisted claim precedes the send.

Rate bounds, all structural rather than timer-based:

- At most one notice per relay per episode; a new episode requires a
  successful refresh (or removal/re-add) followed by a fresh transition.
- Transitions in one pass coalesce into **one** staff message per guild
  listing at most 10 relays, then "and N more — run `/feed-list`". With the
  25-relay cap, the absolute worst case is 25 episodes, still one message
  per pass.
- No reminder, escalation or recovery notice in v1. Recovery is visible in
  list/inspect; a "recovered" ping is a separate decision.

Content (synthetic example, ≤2,000 UTF-16 units, inert text): relay id, kind,
destination channel, **host only** (never the full source, path or query),
bounded failure class, streak count, first/last failure and last-success
times, and next actions. Host-only is deliberate: staff-channel readers are a
wider audience than `/feed-inspect`'s Manage Server gate, and the existing
projection redacts credential-like query values but cannot prove a path
segment is not a secret. Full redacted source stays behind `/feed-inspect`.
No raw HTTP status text, body, header, DNS answer or error string ever
reaches the notice, logs or audit reasons. Example:

```text
Feed relay failed: `relay-1` (rss → #announcements, host feeds.example.com).
3+ consecutive failures over 24h+ (last class: http_error; last success: never).
Polling for this relay has stopped; its history is kept and nothing was posted.
Next: /feed-inspect id:relay-1, fix or replace the source, then refresh — or /feed-remove id:relay-1.
```

## Refresh, remove, and pause interplay

- **Refresh** (new, Manage Server, guild-scoped id, same non-disclosing
  missing reply as today): one fetch+parse **probe** through the existing
  pinned connector. A probe never claims, posts, advances high-water state
  or audits as a poll. Probe success clears the streak and `failed_at`,
  stamps `last_success_at`, and returns the relay to normal polling on the
  next ordinary pass (existing 20-posts-per-pass budget bounds catch-up).
  Probe countable failure records evidence; an already-failed relay stays
  failed and sends **no** second notice for the same episode. Probe neutral
  outcome changes nothing and says so. Refresh on an active relay is allowed
  as a manual probe but is not a "poll now" and posts nothing.
- **Pause**: unchanged, including the pending-evidence refusal. Pausing a
  failed relay keeps its evidence; it displays `paused`.
- **Resume**: unchanged enabled flip. Resuming a paused relay whose
  `failed_at` is set returns it to `failed` (still no fetch) — staff refresh
  to recover; resume never silently clears failure evidence.
- **Refresh while paused**: allowed (probe only); success clears failure
  evidence, relay stays paused until resumed.
- **Remove**: unchanged, including refusal while pending evidence exists.
  Removal of a failed relay needs no refresh first. Its audit history is
  retained by the existing audit table; health columns cascade with the row.
- **Pending evidence + failed**: recovery continues (above); refresh probe is
  fetch-only and cannot settle, release or repost pending rows.

| Interleaving | Required outcome |
| --- | --- |
| Countable failure lands after refresh success | Stale-attempt discard: newer success stands, no instant re-fail |
| Pause commits mid-fetch | Fetch outcome may record; no post after pause (existing admission fence); no transition while paused |
| Two passes race the threshold | One conditional-update winner; one episode, one notice claim |
| Notice send crashes mid-flight | Claim persists, outcome `unknown`, never auto-resent; restart does not duplicate |
| Staff channel deleted after claim | Outcome `failed`; status stays `failed`; inspect shows notice outcome |
| Platform DNS outage for a day | All outcomes neutral; zero transitions, zero notices |
| Popular host 5xx for 24 h+, many relays | Each relay transitions on its own evidence; one coalesced message per pass |
| Relay removed while failed | Existing removal rules; no notice, no resurrection by late outcome writes |

## Dashboard mapping (contract, not a build)

Whatever states the separate feed-configuration dashboard work lands, it must
render lifecycle (`enabled`/`paused`) and health
(`unknown`/`active`/`failed`, with streak evidence) as **separate dimensions
with observation times**, never collapse `failed` into disabled/paused, and
never present `last_checked_at` as health. Dashboard refresh/remove actions
inherit the fences above; a dashboard cannot clear `failed_at` except through
a successful probe. Before any build card, re-inventory that proposal's
merged states at its exact revision; if it defines a single status enum, it
gains the health dimension rather than reusing `enabled`. No web repository
was inspected; its session and signer surfaces are unverified.

## Proposed bounded JSON and finite reasons

Offline proposal objects only — no endpoint, verb or credential is defined.
`mode=local_review` is the only mode the document oracle accepts.

```json
{
  "schema":"feed.health.v1",
  "source_sha":"baa387910bf8191f1632f07b03bf77c2c4713804",
  "limits":{"min_countable_failures":3,"failed_after_ms":86400000,"max_relays_per_guild":25,"max_notice_listed":10,"notice_utf16":2000},
  "countable":["invalid_source","non_public_address","redirect_refused","too_large","invalid_encoding","unsupported_content_type","unsupported_encoding","http_error","parse"],
  "neutral":["resolve","transport","deadline","timeout","database","rest","recovery_required","configuration","other"],
  "read_example":{
    "request_id":"fixture-read-01","relay_id":"relay-1","guild_id":"100000000000000001",
    "lifecycle":"enabled","health":"unknown","streak":null,"last_success_ms":null,
    "last_failure_class":null,"failed_at_ms":null,"notice":"none",
    "reason":"READ_ONLY","retryable":false,"next_action":"await_accepted_contract"
  },
  "review_example":{
    "mode":"local_review","request_id":"fixture-review-01","relay_id":"relay-1",
    "operation":"refresh_probe","posts":false,"effects":[],
    "reason":"PREVIEW_ONLY","retryable":false,"next_action":"review_without_sending"
  },
  "reasons":{
    "PREVIEW_ONLY":{"retryable":false,"next_action":"review_without_sending"},
    "READ_ONLY":{"retryable":false,"next_action":"await_accepted_contract"},
    "NOT_AVAILABLE":{"retryable":false,"next_action":"check_authorized_context"},
    "PAUSED":{"retryable":false,"next_action":"resume_then_refresh"},
    "PROBE_SUCCEEDED":{"retryable":false,"next_action":"none"},
    "PROBE_FAILED":{"retryable":false,"next_action":"fix_source_then_refresh_or_remove"},
    "PROBE_NEUTRAL":{"retryable":true,"next_action":"retry_probe_when_platform_healthy"},
    "UNAVAILABLE":{"retryable":true,"next_action":"retry_authorized_read"},
    "NOTICE_NOT_CONFIGURED":{"retryable":false,"next_action":"use_feed_list_and_inspect"}
  }
}
```

## Acceptance matrix and runnable checks

Future protected-test oracles, **not executed runtime acceptance**. Use
independent acceptance ownership, mock Discord and disposable test databases
only; environment/compile failures are not behavioral RED.

| ID | Input / interleaving | Required oracle |
| --- | --- | --- |
| `FF01` | Legacy row, ancient `last_checked_at`, no evidence | Health `unknown`; never failed, never noticed at migration |
| `FF02` | 2 countable failures, then success | Streak resets; `last_success_at` stamps; no transition |
| `FF03` | 3+ countable failures inside 24 h of last success | Streak grows, still `active` (time arm unmet), fetch continues |
| `FF04` | 3rd countable failure at ≥24 h from anchor | Single transition to `failed`; fetch stops next pass; one notice claim |
| `FF05` | Never-succeeded relay, countable failures from creation | Anchor is creation; same transition rule |
| `FF06` | Flapping: fail, fail, success, fail, fail across days | Never failed; each success resets streak and anchor |
| `FF07` | 24 h+ of resolve/transport/deadline outcomes only | Streak unchanged (not reset, not grown); no transition |
| `FF08` | Delivery `recovery_required` while fetch succeeds | Health untouched by delivery state; recovery path unchanged |
| `FF09` | Failed relay with pending deliveries | Fetch skipped, recovery still attempted each pass; nothing released/reposted |
| `FF10` | Refresh probe success on failed relay | Evidence cleared, zero posts from probe, normal polling resumes next pass |
| `FF11` | Refresh probe countable failure on failed relay | Stays failed; no second notice in the episode |
| `FF12` | Pause failed relay; resume it; refresh while paused | Displays paused → failed on resume; probe-while-paused clears evidence only |
| `FF13` | Remove failed relay with and without pending evidence | Existing refusal with pending; clean removal without; no notice either way |
| `FF14` | 25 relays transition in one pass | One staff message, ≤10 listed + overflow count; 25 claims, 25 episodes |
| `FF15` | Stored source with userinfo + `?access_token=...` + secret-looking path | Notice contains host only; token/userinfo/path secret appear in no notice, log, audit or metric label |
| `FF16` | Notice channel unset, then deleted, then send-ambiguous | `not_configured` / `failed` / `unknown`; status unaffected; no resend, no DM fallback |
| `FF17` | Stale poll outcome after refresh; outcome for removed relay | Discarded by attempt-time fence; zero-row update, no resurrection |
| `FF18` | Same local review twice; unknown/apply mode | Deterministic no-effect review/refusal; zero network, SQL, audit or Discord effects |

Run from repository root, Python stdlib only:

```sh
python3 -m doctest -v docs/feed-failed-status-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

```pycon
>>> import json, pathlib, re
>>> doc = pathlib.Path("docs/feed-failed-status-spec.md").read_text()
>>> fence = "`" * 3
>>> m = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> assert m["schema"] == "feed.health.v1" and re.fullmatch(r"[0-9a-f]{40}", m["source_sha"])
>>> L = m["limits"]; HOUR = 3_600_000
>>> assert L["failed_after_ms"] == 24 * HOUR and L["min_countable_failures"] == 3
>>> metrics = pathlib.Path("crates/core/src/metrics.rs").read_text()
>>> classes = re.search(r"FEED_FETCH_ERROR_CLASSES: &\[&str\] = &\[(.*?)\];", metrics, re.S).group(1)
>>> vocab = re.findall(r'"([a-z_]+)"', classes)
>>> assert set(m["countable"]) - {"parse"} <= set(vocab) and set(m["neutral"]) - {"timeout", "database", "rest", "recovery_required", "configuration"} <= set(vocab)
>>> assert not set(m["countable"]) & set(m["neutral"])
>>> def fresh(**kw):
...     s = dict(enabled=True, streak=0, last_success=None, created=0, failed_at=None, notice="none", last_attempt=-1, evidence=True)
...     s.update(kw); return s
>>> def health(s):
...     if not s["evidence"]: return "unknown"
...     if not s["enabled"]: return "paused"
...     return "failed" if s["failed_at"] is not None else "active"
>>> def poll_action(s):
...     return {"paused": "skip", "failed": "recovery_only"}.get(health(s), "fetch")
>>> def record(s, outcome, now):
...     if now < s["last_attempt"]: return "stale_discarded"          # FF17
...     s["last_attempt"] = now
...     if outcome == "success":
...         s["streak"], s["last_success"] = 0, now; return "success"
...     if outcome in m["neutral"]: return "neutral"                  # FF07/FF08
...     assert outcome in m["countable"]
...     s["streak"] += 1
...     anchor = s["last_success"] if s["last_success"] is not None else s["created"]
...     if s["enabled"] and s["failed_at"] is None and s["streak"] >= L["min_countable_failures"] and now - anchor >= L["failed_after_ms"]:
...         s["failed_at"], s["notice"] = now, "claimed"; return "transition"
...     return "countable"
>>> s = fresh(evidence=False); assert health(s) == "unknown" and poll_action(s) == "fetch"   # FF01
>>> s = fresh(last_success=0)
>>> assert [record(s, "http_error", t) for t in (HOUR, 2*HOUR)] == ["countable"] * 2
>>> assert record(s, "success", 3*HOUR) == "success" and s["streak"] == 0                     # FF02
>>> s = fresh(last_success=0)
>>> assert [record(s, "too_large", t) for t in (HOUR, 2*HOUR, 3*HOUR)] == ["countable"] * 3 and health(s) == "active"  # FF03
>>> assert record(s, "http_error", 24*HOUR) == "transition" and health(s) == "failed" and poll_action(s) == "recovery_only"  # FF04/FF09
>>> assert record(s, "http_error", 25*HOUR) == "countable" and s["notice"] == "claimed"       # FF11 no second claim
>>> s = fresh(created=0)
>>> assert [record(s, "parse", t) for t in (HOUR, 12*HOUR, 24*HOUR)][-1] == "transition"      # FF05
>>> s = fresh(last_success=0)
>>> seq = [record(s, o, t) for o, t in [("http_error", 20*HOUR), ("http_error", 23*HOUR), ("success", 24*HOUR), ("http_error", 40*HOUR), ("http_error", 47*HOUR)]]
>>> assert health(s) == "active" and s["streak"] == 2                                          # FF06 flapping
>>> s = fresh(last_success=0, streak=2)
>>> assert all(record(s, o, t) == "neutral" for o, t in [("resolve", 30*HOUR), ("transport", 40*HOUR), ("recovery_required", 50*HOUR)]) and s["streak"] == 2  # FF07/FF08
>>> def refresh(s, probe, now):
...     if probe == "success":
...         s["streak"], s["last_success"], s["failed_at"], s["notice"], s["last_attempt"] = 0, now, None, "none", now
...         return "PROBE_SUCCEEDED"
...     return "PROBE_FAILED" if probe in m["countable"] else "PROBE_NEUTRAL"
>>> s = fresh(last_success=0, streak=4, failed_at=24*HOUR, notice="sent", last_attempt=24*HOUR)
>>> assert refresh(s, "http_error", 30*HOUR) == "PROBE_FAILED" and health(s) == "failed"      # FF11
>>> assert refresh(s, "success", 31*HOUR) == "PROBE_SUCCEEDED" and health(s) == "active"     # FF10
>>> assert record(s, "http_error", 25*HOUR) == "stale_discarded" and s["streak"] == 0         # FF17
>>> s = fresh(enabled=False, failed_at=24*HOUR, streak=4, last_attempt=24*HOUR)
>>> assert health(s) == "paused" and refresh(s, "success", 30*HOUR) == "PROBE_SUCCEEDED" and health(s) == "paused" and s["failed_at"] is None  # FF12
>>> s["enabled"] = True; assert health(s) == "active"
>>> def coalesce(ids): return {"messages": 1, "listed": ids[:L["max_notice_listed"]], "overflow": max(0, len(ids) - L["max_notice_listed"])}
>>> storm = coalesce([f"relay-{i}" for i in range(L["max_relays_per_guild"])])
>>> assert storm["messages"] == 1 and len(storm["listed"]) == 10 and storm["overflow"] == 15  # FF14
>>> hostile = "https://user:hunter2@feeds.example.com/secretpath/feed.xml?access_token=abc123&format=xml"
>>> host = re.search(r"https://(?:[^@]*@)?([^/]+)", hostile).group(1)
>>> notice = f"Feed relay failed: `relay-1` (rss, host {host}). Last class: http_error."
>>> assert all(x not in notice for x in ("hunter2", "abc123", "secretpath", "access_token")) and host == "feeds.example.com"  # FF15
>>> units = lambda t: len(t.encode("utf-16-le")) // 2
>>> example = re.search(fence + r"text\n(.*?)\n" + fence, doc, re.S).group(1)
>>> assert 0 < units(example) <= L["notice_utf16"]
>>> read, review = m["read_example"], m["review_example"]
>>> assert read["health"] == "unknown" and read["failed_at_ms"] is None and review["posts"] is False and review["effects"] == []
>>> local_result = lambda o: "PREVIEW_ONLY" if o == review else "UNSUPPORTED"
>>> assert local_result(json.loads(json.dumps(review))) == "PREVIEW_ONLY"
>>> assert all(local_result(dict(review, mode=x)) == "UNSUPPORTED" for x in ("apply", "production", None))  # FF18
>>> assert all(isinstance(v["retryable"], bool) and v["next_action"] for v in m["reasons"].values())
>>> assert all(o["reason"] in m["reasons"] and all(o[k] == m["reasons"][o["reason"]][k] for k in ("retryable", "next_action")) for o in (read, review))
>>> store = pathlib.Path("crates/core/src/feeds_store.rs").read_text()
>>> assert "25-definition cap" in store and L["max_relays_per_guild"] == 25
>>> jobs = pathlib.Path("crates/bot/src/feed_jobs.rs").read_text()
>>> assert '"poll_failed", "fetch_or_parse"' in jobs and "DISCORD_STAFF_ALERT_CHANNEL_ID" not in jobs
>>> refs = re.findall(r"(crates/[a-zA-Z0-9_./-]+\.(?:rs|sql)):(\d+)(?:-(\d+))?", doc)
>>> assert refs and all(pathlib.Path(p).is_file() and 1 <= int(a) <= int(b or a) <= len(pathlib.Path(p).read_text().splitlines()) for p, a, b in refs)
>>> assert len(set(re.findall(r"^\| `(FF\d+)` \|", doc, re.M))) == 18
>>> print(json.dumps({"schema": m["schema"], "planned_fixtures": 18, "status": "pass"}, sort_keys=True))
{"planned_fixtures": 18, "schema": "feed.health.v1", "status": "pass"}

```

This checks the document model, the metrics vocabulary, bounds arithmetic and
source gaps only. It is not an implementation validator, an execution of
FF01–FF18, or staging evidence. Rust/DB/Discord behavior is not claimed.

## Exact-revision intent/security gate and bounded future slices

Before any build card, record host/CEO intent alignment against the exact
spec revision, repository commit and file SHA-256. Missing acceptance defaults
**no-build**, never timeout consent. Record CISO disposition of: staff-channel
as the notice audience with host-only source identity; health evidence
retention (bounded class + times only, cascades on removal, member erasure
removes creator linkage through existing paths); and the neutral-class
false-negative tradeoff. Use the established alignment path; a saved request
is not accepted intent.

Decision questions: accept 24 h + 3 countable failures (alternatives: 72 h, or
count-only)? Accept fetch-only fail-stop with continuing recovery? Accept
manual refresh only, no slow-cadence auto-probe in v1? Accept one coalesced
staff notice per episode with no recovery notice? Accept host-only in the
notice with the redacted source confined to Manage Server surfaces?

After alignment only, independently tested **bot-side slices, each ≤1 day**:

1. `FF-W1` observe-only evidence: migration + joint fetch/parse outcome
   recording (incl. `parse` class) + list/inspect health display; no
   fail-stop, no notice. Produces the streak data that tunes thresholds.
2. `FF-W2` fenced `failed` transition + fetch fail-stop with continuing
   recovery; stale-attempt discard; no notice yet.
3. `FF-W3` staff notice: episode claim/receipt, coalescing, host-only content
   via the existing staff channel; log-only when unconfigured.
4. `FF-W4` `/feed-refresh` probe command with the reactivation rules above.
5. `FF-W5` dashboard read mapping, in its own web project after
   re-inventorying that proposal's merged states; no web code here.

These are recommendations, not created/authorized build cards. Review metric
by 2026-11-02: eighteen fixture oracles decided, thresholds and notice
audience explicitly accepted or rejected, zero claims that attempt evidence
is health. Unverified: deployed flags/activation, current staff-channel
readership, actual dead-relay counts, the dashboard proposal's final states,
and real Discord delivery.
ACT: publish proposed contract. WHY: end silent dead relays without per-poll
spam, source leakage or stranded recovery. UNDO: revert documentation; no
operational state changed.
