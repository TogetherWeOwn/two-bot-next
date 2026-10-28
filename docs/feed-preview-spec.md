# Feeds: a bounded private sample before saving

Status: **proposed, design only**. Written 2026-10-25; review by 2026-10-31.
No executable surface, live fetch experiment, database/Discord mutation,
credential, service, spend or runtime activation. Merge publishes a proposal,
not a build grant. This file is the single repository contract; the issue
spec binds its exact commit and SHA-256, not a moving branch.

## 1. Decision and value still to validate

Consumer: staff checking a proposed unsaved source before creating a relay.
Hypothesis: seeing a few titles prevents configuring the wrong source. No
staff demand, saved time, source compatibility or engagement benefit has been
measured. No live provider was contacted for this design.

| Option | Useful result / cost | Recommendation |
| --- | --- | --- |
| A. No build; current `/feed-list` and `/feed-inspect` | Saved configuration and last-attempt evidence only, not sample content; no new network surface | **Keep now and as the fallback** |
| B. Validation-only URL preview | Checks syntax/normalization without DNS or HTTP; cannot establish reachability, safety of resolved addresses or any entry content | Do not build a standalone feature that implies a sample; retain as C's safe default if C is accepted |
| C. Explicit tightly bounded fetch | At most three entry titles from the existing RSS/Atom transport; actual unsaved-source sample, but adds a staff-triggered outbound request and resource surface | Smallest option that meets the sample need; **conditional candidate only**, not authorized to build |

**Current recommendation: NO_BUILD.** Submit C for exact-spec intent review,
not implementation. Until useful title-only content and the hard resource /
single-owner bounds below are accepted as demonstrable, A wins. Do not relax
bounds to make an arbitrary source work. No relay editor, import, saved-feed
refresh, credentialed provider, Twitch API, dashboard or new connector here.

## 2. Verified source map

Baseline: `42e5c8f3587fb43671686ecd0da3439a68dca167`. These are repository facts,
not proof of deployed behavior or live provider compatibility. Existing
[feed operations](feed-supervisor.md), [domain record](feeds-port.md) and
[command wiring gates](command-wiring-security.md) remain authoritative for
their existing surfaces.

- `crates/core/src/feeds.rs:16-40,87-117,129-186,201-217`: closed feed kinds;
  saved management plans, not an unsaved preview plan. Normalization checks
  the 2,048-byte ceiling **before** trimming; YouTube channel IDs of 20–32
  ASCII letters/digits/underscore/hyphen expand to the existing feed URL.
- `crates/core/src/feature_commands.rs:501-580` and
  `crates/core/src/router.rs:754-797`: current six feed commands require
  Manage Guild (`32`), the announcements gate and configured-guild routing.
  `crates/bot/src/command_runtime.rs:2402-2417,2680-2687` rechecks the invoking
  member's actual Manage Guild bit; do not invent a staff-role or
  Administrator-only bypass, or trust permissions supplied in JSON/options.
- `crates/bot/src/command_runtime.rs:2354-2387`: inspect performs one bounded
  guild-scoped relay-list read and finds the ID; no fetch/audit/claim renewal.
  `:2089-2139` is the **unsafe seam to reuse**: add creates an enabled relay
  and writes its audit. Never create a temporary relay to sample it.
- `crates/bot/src/command_runtime.rs:1540-1548,2453-2505`: existing owned
  commands must successfully defer privately before work, then edit the
  original response; failure never falls back to a channel message.

Actual connector: `PublicFeedFetch` → `fetch_public_feed` → core `fetch_feed`,
then `parse_fetched` (`crates/bot/src/feed_jobs.rs:121-164`). There is no
`HttpFeedClient` type to extend. Core production entry fixes the resolver and
pinned transport; injectable traits exist for **offline fixtures**, not caller
selection (`crates/core/src/feeds_connector.rs:229-284,312-390`).

| Protection | Existing behavior / precise evidence |
| --- | --- |
| Scheme/credentials | HTTPS, effective port 443, host required, userinfo refused, 2,048-byte input/canonical caps. Queries/fragments are **not** forbidden and secrets in paths/queries are not detected. No provider/domain allowlist (`crates/core/src/feeds_http.rs:58-87`) |
| Address policy | Canonical literals and localhost checked; IPv4 excludes private/link-local/loopback/CGNAT/reserved/documentation/multicast; IPv6 only global-unicast 2000::/3 minus explicit special ranges, rejecting mapped/NAT64/ULA/link-local forms (`crates/core/src/feeds_http.rs:72-133`) |
| DNS/pins/TLS | System `lookup_host`, every returned address checked; empty or any nonpublic answer refuses, literals must resolve to themselves. Pins only, no ambient re-resolution, original hostname for TLS/SNI, WebPKI roots, fresh HTTPS-only client each hop (`crates/core/src/feeds_http.rs:135-176`; `crates/core/src/feeds_connector.rs:253-270,366-390,590-724`). No explicit DNS-answer-count cap or authoritative-DNS enumeration |
| Redirect | Only 301/302/303/307/308; same host, relative allowed, **three follows / four GETs** maximum. Revalidate URL and freshly resolve/check/pin each hop; duplicate/non-ASCII Location refused (`crates/core/src/feeds_http.rs:178-191`; `crates/core/src/feeds_connector.rs:70-78,334-364,438-475`). Prior body is not drained; implementation retains it during next DNS planning, drops before next GET |
| Request identity | Fixed empty GET; only fixed User-Agent, Accept-Encoding and Accept explicitly added; no caller headers, auth/cookie jar/replay, client-certificate or proxy configuration path (`crates/core/src/feeds_connector.rs:678-724`). This is source inspection, not a wire credential-absence test |
| Media/headers/status | Missing/blank media accepted; allowlisted RSS/Atom/RDF/XML types, parameters ignored, UTF-8 required. Duplicate Content-Type/non-ASCII policy headers refuse; **no explicit aggregate header bound** (`crates/core/src/feeds_http.rs:193-219`; `crates/core/src/feeds_connector.rs:397-417,438-475`). Connector returns final status/body even for non-2xx; bot parser checks success first (`crates/bot/src/feed_jobs.rs:149-156`) |
| Wire/decode | Independent **2,000,000-byte** wire and decoded caps (not 2 MiB); declared oversize refuses, malformed length grants no bypass. Identity, gzip including concatenated members, or deflate only; stacked/unsupported coding refused, overflow never falls back (`crates/core/src/feeds_http.rs:8,221-247`; `crates/core/src/feeds_connector.rs:397-435,477-588`) |
| Deadline/gaps | Default **15,000 ms total fetch**, not per hop; absolute deadline checks and outer timeout. Decode uses `spawn_blocking`; timing out its handle does not stop running work. XML parse is **outside** this deadline; no connector concurrency/rate gate (`crates/core/src/feeds_connector.rs:212-227,286-332,525-588`; `crates/bot/src/feed_jobs.rs:128-156`) |
| XML/render | Whole-document cap, depth 64, attributes 64/element and 4,096 total, 100,000 nodes, no DTD/entity resolver, at most 200 entries before filtering (`crates/core/src/feeds.rs:943-1171`). Item links allow HTTP(S) and do **not** receive source DNS/IP controls (`:1659-1698`); parsed titles/times are untrusted, not display-ready. Delivery's 2,000-unit renderer is not an aggregate private preview (`:1727-1809`) |

Safe **network-only** seam: core `fetch_feed` plus explicit 2xx check then
`parse_xml_feed_report`, without a `FeedRelay`, pool or executor; it still
lacks C's budgets/renderer/admission, so is **not ready to wire**. Never call
job `run_once` or `poll_relay`: they advance poll position, recover/claim/send,
mark checked and audit (`crates/bot/src/feed_jobs.rs:76-118,431-518,585-678`).
`FeedConnectError` contains resolver/transport/encoding strings (`crates/core/src/feeds_connector.rs:84-103`): classify variants, never Display/Debug it to users/logs.
Existing hermetic connector doubles and parser bomb fixtures are at
`crates/core/tests/feeds_connector.rs:68-224` and
`crates/core/tests/feeds.rs:775-912`; they are not preview acceptance or live
provider evidence.

## 3. Candidate authority and typed modes (not implemented)

Proposed discovery: `/help command:feed-preview` and this indexed document.
Candidate slash shape: `/feed-preview kind:<rss|youtube>
source:<https-url-or-channel-id> mode:<validate|sample>`. Omitted mode means
`validate`; an explicit `sample` means “contact this source once, privately.”
Twitch is `unsupported` in this slice, even though saved management accepts
that kind. YouTube means its existing XML feed, never an API/key/cookie flow.

- Preserve application identity, configured-guild, publication, activation and
  announcements/feature gates through the shared router and executor. Never
  add a private dispatcher, prefix trigger, web endpoint, admin impersonation
  option or agent-triggered network tool. Missing/foreign guild or unproven
  authority refuses before inspecting the source, DNS or connector admission.
- Require the authenticated invoking member's Manage Guild bit at admission.
  No caller/guild/permission identity comes from options. Recheck current local
  feature/activation policy before outbound work and before showing content;
  invalidation discards it. Discord role changes after authenticated ingress
  cannot be made atomic with a remote request; intent review must accept that
  residual race, not claim fresh role REST validation that does not exist.
- Strictly decode exactly one kind and source, at most one mode; unknown,
  duplicate, wrong-type options or flags are invalid. Enforce 2,048 input bytes
  before trim and the existing normalization. No headers, ports override,
  credentials, cookies, proxies, alternate connector or provider selector.
  Reject userinfo and fragments; do not add/remove query parameters to make a
  failed source work. Reject any staff-known credential-bearing source; the
  feature accepts public URLs only, not a promise to detect every secret in a
  path/query. Never echo either field in a refusal.
- `validate` (including every agent `dry_run`) is pure syntax/normalization:
  **zero DNS, sockets, connector construction, stores, audit writes or sends
  other than the private reply**. It cannot label a domain reachable or SSRF
  safe. `dry_run` plus `sample`, unknown mode, or malformed flags is invalid
  before any I/O, never a fallback into sample. No CLI is added by this spec.
- `sample` requires a successful private defer, rate/concurrency admission and
  exact accepted-spec/runtime activation separately; none exists here. A
  failed/uncertain defer triggers zero fetches. No Save/Enable button or
  implicit follow-up; choosing to add later is an independent authorized act
  with fresh validation, not promotion of a preview result.

## 4. Proposed outbound and resource envelope

All numbers in this section are **requirements for C**, not observed preview
runtime guarantees. Use the existing hardened feed transport only; all
resolved addresses and every redirect target remain subject to its SSRF
checks and IP pinning. No private, loopback, link-local, metadata, reserved or
ambiguous destination; no bypass for a trusted-looking hostname, URL shortener
or final entry link. No authorization/cookie headers or cookie persistence,
client certificate, credential retry, proxy, HTML scraper or fallback client.

| Dimension | Required candidate bound |
| --- | --- |
| Concurrency | One in-flight sample per authenticated caller, one per guild, **one globally** across admitting runtimes; immediate busy refusal, no queue |
| Sliding 60-second admission | Caller 1, guild 3, global 6 sample attempts; admit all three atomically before DNS; include failures/cancellation; no token refund or automatic retry |
| Limiter memory | At most 64 caller records, 16 guild records and 6 global timestamps; expire only after their full 60-second window; refuse capacity rather than evict an unexpired record |
| Interaction freshness | Proposed sample-admission lifetime: **60 seconds from authenticated interaction creation**, not local receipt or defer. Require age >= 0 and < 60 seconds at admission and recheck before DNS; unknown/future/expired age refuses. This is a narrower candidate policy, not Discord token validity |
| Restarts / overlap | No sample admission for the first **60 monotonic seconds** after start, covering the entire supported interaction lifetime and rate window. Prove a single admitting owner with the existing runtime fence for the entire request. Process-local counters alone are **not** a global limit across overlapping owners; unknown ownership or untrustworthy age keeps sample dark |
| HTTP/DNS | Preserve three same-host redirect follows / at most four GETs, no application retries or fallback client; connector address racing stays within checked pins. Proposed inclusive maxima: **32 DNS answers per hop, 64 response headers and 16,384 aggregate header bytes (16 KiB)**. Exactly each maximum passes that size gate; any excess refuses rather than truncates/filtering to a safe-looking subset. These caps are absent today and must apply before unbounded collection |
| Total time | Eight-second monotonic budget from admission through DNS/connect/TLS/headers/all hops/body/decode/parse/render; each phase receives remaining time, never resets the overall deadline |
| Parse/input | Existing byte, XML depth/attribute/node and 200-entry rejection limits still apply to the **whole document**, not merely the three displayed entries |
| Memory | One sample total; no response cache or retained raw source/body. Preserve independent 2,000,000-byte wire/decoded caps; proposed 16 MiB incremental request-allocation ceiling (including DNS/headers/parser/output) must be proved with adversarial offline fixtures, not inferred from byte size |
| Result | First three eligible entries in feed document order; no date sorting, pagination or second request; duplicate stable entry keys removed before taking three |
| Output | One ephemeral original-response edit, at most 1,800 UTF-16 units; three titles of at most 160 units each and optional normalized timestamps; JSON at most 4,096 UTF-8 bytes |

A Tokio timeout around synchronous XML/decompression is **not** proof of a
hard deadline or CPU cancellation. Before build admission, establish how the
existing parser/decoder work fits a bounded, cancellable envelope without
uncancellable detached/blocking work; otherwise keep A, not a silently weaker
“best effort” deadline. Likewise a 2,000,000-byte body does not prove 16 MiB peak heap.
The existing decoder specifically lacks cancellation of running blocking work,
and system DNS has no explicit answer-count cap: **verified design blockers**,
not features that a shorter caller timeout fixes.
If enforcing these bounds needs another service, broad store authority or an
unbounded worker/pool, reject C rather than create that infrastructure here.

Cancellation drops the active DNS/HTTP/body work, closes its response, frees
buffers and the concurrency lease after work really stops, and never retries.
Policy disable, request abandonment observed by the runtime, shutdown or the
eight-second deadline cancels the request; do not claim Discord notifies the
bot whenever someone dismisses an ephemeral reply. A parse still running holds
its lease; no detached work can release admission early. Final reply failure
never re-fetches, sends publicly or changes any state. On restart nothing is
resumed, and no sample result or raw content survives in application storage.

## 5. Zero domain effects and safe private content

Candidate dependency shape: validated `(kind,source)` → hardened fetch → pure
parse → bounded projection → private reply. No `FeedRelay` is constructed and
no store, scheduler, delivery claimant or public REST sender is passed to that
core. In-memory admission accounting is the only preview-owned state.

Every outcome, duplicate and cancellation must leave byte-identical relay
rows (including paused/enabled and `last_checked_at`), seen cursors, deliveries,
leases, pending recovery, settings and audit rows. Never call poll/supervisor,
claim/renew/settle delivery, update poll attempt, `plan_post`, add/remove,
pause/resume or an audit writer. Scheduled delivery continues independently;
compare deltas attributable to this caller, not unrelated background progress.
No public post, webhook, DM, embed, attachment, image or link-unfurl request.

Sample display deliberately omits source URLs, raw item IDs/URLs, descriptions,
authors, HTML and images. It shows titles as untrusted **plain text**, not
Markdown: escape formatting, strip controls/bidi/zero-width deception, suppress
all role/user/channel mention pills and mass mentions, and redact any URL-like
text even inside titles. Final executor sends explicit empty `allowed_mentions`
and suppresses embeds. No clickable provider links are needed for this minimum
choice; do not treat the parser's item-link validation as DNS/SSRF proof.
Normalize valid published times to UTC; omit malformed ones, never label the
sample “latest” or “healthy.” Sanitize then truncate on UTF-16 boundaries;
reserve labels/caveats before titles. Invalid links/entries never get repaired
by fetching their targets. If nothing displayable remains, return `empty`, not
“valid source.” No provider text is executable, a command, or trusted evidence.

Header: **Preview — not saved**. Footer: “Up to three entries, in source order,
as fetched at <UTC time>. Links omitted. Content may change; this is not a
delivery test. No relay changed.” Validation instead says **Validation — not
fetched · not saved**, with “Format accepted only; reachability and content
not checked.” No channel destination or enabled/paused state is fabricated for
an unsaved source. Showing a sample is not a live health or freshness guarantee.

Logs/metrics use only closed reason/outcome classes and bounded durations.
Never log full or truncated source/redirect/item URLs, raw DNS answers, title,
body, content hash, interaction token, provider error/status body or free-form
parser error. Do not use caller/guild/URL as metric labels. HTTP/transport errors
must use closed variant-to-reason mapping, not the connector error's raw
Display/Debug; no dependency payload TRACE. No durable
sample audit/cache: short-lived source/body/title data is discarded at completion;
limiter IDs expire after 60 seconds. Discord retains the ephemeral response
under its own rules; no promise of deleting copies/screenshots is made.

## 6. Truthful outcomes and next actions

All failures are private fixed text; no provider strings or URL reflection.
Every reason carries exactly one `next_action`. `retryable` never means an
automatic retry. Every response says not saved / no relay changed.

| outcome / reason | Private meaning | retryable / next_action |
| --- | --- | --- |
| `validated / format_only` | Format accepted; not fetched, content/safety unverified | false / `choose_explicit_sample_after_activation` |
| `sampled / titles_only` | Header/footer above, zero to three never more; nonempty display only | false / `review_before_separate_add` |
| `empty / no_displayable_entries` | No displayable entries, not proof of an invalid/healthy source | false / `check_public_feed_format` |
| `denied / context_or_authority` | Preview is unavailable to this caller here | false / `ask_authorized_guild_staff` |
| `invalid / malformed_input` | Expected one public HTTPS source, supported kind and typed mode | false / `correct_options` |
| `unsupported / kind_or_format` | This kind/media/XML format cannot be sampled | false / `use_public_rss_or_youtube_xml` |
| `unsafe / target_refused` | Source or redirect refused by network safety policy | false / `choose_public_https_feed` |
| `unavailable / source_unavailable` | Source could not be sampled; no diagnostic provider text | true / `retry_privately_later` |
| `unavailable / resource_limit` | Headers/body/decode/parse/output exceeded a bound | false / `choose_smaller_public_feed` |
| `timeout / deadline` | Budget expired; no sample saved | true / `retry_privately_later` |
| `cancelled / request_stopped` | Observed cancellation or policy disable; no sample shown | false / `start_new_private_request_if_enabled` |
| `limited / rate_or_busy` | Capacity/rate admission refused; integer retry-after of 1–60 seconds | true / `wait_then_start_new_request` |
| `no_op / duplicate_request` | Interaction already admitted; never refetch to reconstruct it | false / `use_existing_reply_or_start_new_request` |

## 7. Agent handoff and offline acceptance matrix

No executable agent endpoint/CLI is authorized. Proposed diagnostic JSON is
small, content-free and not a capability to fetch on behalf of a guild:

```json
{"schema":"feeds.preview.v1","request_id":"fixture-01","guild_id":"g1","mode":"validate","outcome":"validated","reason":"format_only","retryable":false,"next_action":"choose_explicit_sample_after_activation","entry_count":0,"saved":false,"fetched":false}
```

Use the authenticated interaction ID for runtime request identity, never a
source hash or an agent-supplied ID. Omit identity fields before authorization;
no relay ID is invented. A `sample` result additionally reports actual
`fetched_at` UTC and `entry_count` 0–3; JSON diagnostics carry no raw titles or
links. Repeated validate is idempotent/no-effect. Repeated authenticated sample
interaction is admitted at most once within its 60-second admission lifetime
(section 4); keep at most six content-free admitted IDs for a full 60 monotonic
seconds after admission alongside rate state. A full dedup map refuses rather
than evicting an unexpired ID. Reject expired interactions even if their IDs
have expired from memory. The 60-second restart quarantine outlives every
pre-start interaction's supported admission lifetime; neither defer nor replay
renews that lifetime. Keep all quarantine/dedup state in memory, with no
persisted epoch or claim. No retry after unknown fetch/reply result. Dedup is
not a durable delivery claim.

Future tests use injected DNS/dial/TLS/clock/body/decoder/cancellation and reply
captures, never real providers. Every case has a zero-domain-effect oracle:
spy stores/claims/audit/public sender panic if called, paused relay and pending
scheduled claim snapshots unchanged; only the expected private reply and
bounded in-memory admission effects are allowed. No test runs production or
staging databases. These are proposed fixtures, **not runtime tests executed by
this docs PR**.

| ID | Fixture / required oracle |
| --- | --- |
| FP-01 | Foreign guild, DM, absent member bits, wrong application and Administrator-only bits: refuse before DNS/connector; no source reflection |
| FP-02 | Duplicate/wrong-type/unknown options, oversized UTF-8 before trim, userinfo/fragment/bad scheme: fixed refusal, no DNS |
| FP-03 | Validate/dry-run, including a hostile source: zero DNS/socket/connector/store calls; never claim resolved safety |
| FP-04 | Dry-run plus sample, unknown mode, missing explicit mode: refuse combination or validation-only, never fetch |
| FP-05 | All forbidden literal IPv4/IPv6, mapped forms, metadata/link-local/private/reserved targets: no dial |
| FP-06 | Mixed public/private DNS answers, empty/oversized answers and rebinding between resolve/dial: refuse or pin exact checked address, no second ambient lookup |
| FP-07 | Redirect to forbidden/credentialed/cross-host target, relative redirect, bad scheme, fourth follow: revalidate each hop, bounded closes and no forbidden dial |
| FP-08 | Missing/blank media keeps existing acceptance then XML parsing; wrong media, HTML disguised as XML, non-success response: classified refusal, no raw body/status error leak |
| FP-09 | Inclusive size gates: 32 DNS answers, 64 headers, 16,384 aggregate header bytes, and declared/streamed bodies of exactly 2,000,000 bytes pass their respective size checks; 33 answers, 65 headers, 16,385 header bytes or 2,000,001 body bytes refuse. Passing size alone never overrides address/media/XML/other gates; no truncation or unlimited collection |
| FP-10 | Gzip inflation, truncated gzip, multiple members, oversized decoded body and both wire/decoded boundaries: bounded failure, no decoder bypass |
| FP-11 | Slow DNS/TLS/headers/body and cumulative redirects: one eight-second budget, no per-hop renewal or retry |
| FP-12 | Cancel at every phase / shutdown / policy disable: work stopped, buffers freed, slot released only after stop, no follow-up fetch |
| FP-13 | DTD/entity/depth/attribute/node limits, malformed XML, 201 entries, long titles: whole-document bounds; measured allocation/CPU/cancellation evidence or no-build |
| FP-14 | RSS/Atom/YouTube fixtures with 0, 1, 3, 4 entries, duplicate keys, malformed times and unsafe links: document order, at most three eligible unique titles, no link fetch |
| FP-15 | HTML/Markdown, mass mentions, pills, bidi/zero-width/control text, URL embedded in title: inert redacted text; empty allowed mentions; no embeds; output ceilings |
| FP-16 | Caller/guild/global limits and simultaneous admissions: exact 1/3/6 per sliding minute, concurrency 1/1/1; no queue/refund, full maps fail closed |
| FP-17 | Restart/overlapping owners/unknown fence and untrustworthy age refuse. At age just under 60 seconds a fresh ID may admit if all gates pass; age exactly 60 seconds, future age and duplicate IDs refuse. Restart elapsed <60 seconds refuses; at exactly 60 seconds every pre-start interaction is expired. Full dedup memory refuses without eviction; no persisted epoch or doubled global quota |
| FP-18 | Failed/uncertain private defer or final edit: no fetch after failed defer; no repeat after edit error; no public/DM fallback |
| FP-19 | Source unavailable/unsupported/unsafe/timeout/resource exhaustion: closed reasons and next actions only; sentinel URL/provider text absent from logs/replies/JSON |
| FP-20 | Paused relay plus scheduled pending delivery, cursor and last-attempt snapshots: all preview outcomes preserve every domain byte; no add/enable/claim/audit/public send |

Runnable **document consistency** check now, from repo root (Python stdlib,
offline; no Rust compilation or runtime/network/database/Discord test):

```sh
python3 -m doctest -v docs/feed-preview-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
git diff --check
```

The doctest checks fixture discoverability, source paths, bounded JSON shape
and protected-file hashes only. It does not simulate a secure connector or
prove quotas, cancellation, peak heap, safe rendering or zero effects.

```pycon
>>> import hashlib, json, pathlib
>>> p = pathlib.Path("docs/feed-preview-spec.md")
>>> text = p.read_text()
>>> matrix = text.split("| ID | Fixture / required oracle |", 1)[1].split("\n\n", 1)[0]
>>> ids = [line.split("|")[1].strip() for line in matrix.splitlines()[2:]]
>>> assert ids == [f"FP-{i:02d}" for i in range(1, 21)]
>>> example = json.loads(text.split("```json\n", 1)[1].split("\n```", 1)[0])
>>> assert example["mode"] == "validate" and not example["fetched"] and not example["saved"]
>>> assert example["entry_count"] == 0 and len(json.dumps(example).encode()) <= 4096
>>> protected = ["crates/core/src/feeds_http.rs", "crates/core/src/feeds_connector.rs", "crates/core/src/feeds.rs", "crates/bot/src/feed_jobs.rs", "crates/bot/src/command_runtime.rs", "crates/core/src/feeds_store.rs"]
>>> hashes = {f: hashlib.sha256(pathlib.Path(f).read_bytes()).hexdigest() for f in protected}
>>> assert all(len(value) == 64 for value in hashes.values())
>>> from scripts.check_docs_index import check
>>> assert "docs/feed-preview-spec.md" not in check(pathlib.Path.cwd())["missing"]
>>> handoff = {"schema": "feeds.preview.design-check.v1", "spec_sha256": hashlib.sha256(p.read_bytes()).hexdigest(), "protected_file_hashes": hashes, "outcome": "checked", "reason": "document_only", "next_action": "request_exact_spec_intent_alignment"}
>>> assert len(json.dumps(handoff).encode()) < 4096 and json.loads(json.dumps(handoff)) == handoff
>>> print(json.dumps({"cases": len(ids), "outcome": handoff["outcome"], "reason": handoff["reason"]}, sort_keys=True))
{"cases": 20, "outcome": "checked", "reason": "document_only"}

```

## 8. Boundary recommendation, intent gate and undo

**CISO recommendation: DESIGN_ONLY / NO_BUILD now.** This assessment permits
circulation of the bounded candidate, **not approval of our own capability
request**. An independent authorized boundary decision must cover outbound
abuse/rate, sole-owner enforcement, cancellation/CPU/heap and title-only value
before any build card; independent exact-head Paperclip Review 5/5 and CI are
separate gates, never replaced by this document.

Request CEO acceptance and actual guild-host intent alignment through the
President/COO, bound to the issue spec revision plus this file's commit and
SHA-256. Record accept/revise/no-build for A versus C; explicit opt-in fetching,
title-only usefulness, public-source/credential restrictions, all numeric bounds,
rate/fence feasibility, zero effects and the 20 fixture outcomes. A routing
receipt, merged proposal or silence is **not** host alignment. Material edits
invalidate the request; rebind to the new hash. No implementation/acceptance
build cards, runtime, spend or activation before both intent records and an
independent security/rate disposition. This is not a routine-deploy approval hop.

Review by **2026-10-31**: metric is 20/20 fixture expectations accepted or
revised, named CEO/host disposition, and evidence that the resource bounds and
useful content can be established offline. These are design success criteria,
not measured user benefit. Kill rule: unresolved usefulness, owner fencing,
cancellation or memory bound means retain A, no build; never default into C.

ACT: publish one indexed proposal and hash-bound intent request. WHY: a
three-title preview must not quietly become an arbitrary fetcher or delivery
poll. UNDO: reviewed documentation revert only. No operational state changed;
future disable cancels sample admission without changing saved relay history.
