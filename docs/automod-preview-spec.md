# Automod verdict preview (private, dry-run)

Status: **Proposed; design only**. Written 2026-10-26; intent review **not
before 2026-11-02**. This indexed repository file is the editable contract.
Merge publishes a proposal, not permission to build, publish a command, or
change any automod scope. No code, config keys, dashboard surface, live-guild
scope change, or deploy here.

Question the preview answers, privately, for one synthetic message: *if I
posted this text now, in this channel, which single automod filter would
decide it under the current effective config — or none?* It never answers
what sanction a member would get, and it is not a member-facing self-check.

## 1. Decision, alternatives, and the no-build test

Recommend reviewing **B**: one ephemeral, ManageGuild-only slash preview
that reuses the live matcher and the live effective config, reports the
first deciding filter plus the deciding setting *name* (never its value),
records nothing in the moderation pipeline, and is rate-limited and audited
without content. **A/no-build stays in force** until the intent review.

| Option | Shape | Disposition |
| --- | --- | --- |
| A. No build | Post candidate text in the scoped guild and read dry-run outcomes/logs; offline corpus for dev | In force now; the fallback if demand is not shown (below) |
| B. Private staff preview | This spec: synthetic, ephemeral, first-match verdict, no pipeline writes | Smallest surface that gives a per-message answer before posting |
| C. Offline/local preview tool | Export effective config to a staff machine and evaluate locally | Reject: exports the bad-words list to endpoints (strictly worse disclosure than an in-bot oracle) and goes stale against the live snapshot |
| D. Dashboard preview | Web settings page evaluates sample text | Out of scope here; larger auth surface. May later reuse B's pure core behind its own decision |
| E. Member self-check | Anyone can test text before posting | Reject outright: a public evasion oracle |

**Why A is insufficient, when staff actually tune automod.** Today the only
per-message test is a real post in the scoped guild
([operator guide](automod-operator-guide.md)):

- A test post is a real message: visible to channel readers (the text may be
  the exact slur/invite under investigation), it enters the funnel
  capture-only on a dry-run match, and an inspected text CREATE **records
  repeat history** unless `bad_words` or an exemption stops it first
  (`crates/core/src/automod.rs:479-520`, `:401-470`). Testing pollutes the
  state being tested.
- In enforce mode the test costs the tester's own account a counted
  violation and a real delete (then warn/timeout ladder steps on repeats),
  unless the tester is role/channel-exempt — in which case the test proves
  nothing about a normal member — or target-protected, in which case the
  violation is still counted but nothing is deleted
  (`crates/core/src/automod_runtime.rs:345-348`, `:378-382`).
- Two filters cannot be tested by posting without side effects on others:
  `mention_spam` needs real mentions (real pings), `attachment_type` needs
  uploading a blocked-extension file.
- The answer is in logs/claim outcomes, not at the point of asking; and the
  offline golden corpus (`crates/core/tests/automod_corpus.rs`) evaluates
  synthetic config, not the deployed effective config of §2, so it cannot
  answer "what would *current* settings do?".

**Demand gate, stated plainly.** Need is asserted, not measured. The intent
review should first collect the count of incidents/tuning sessions since
automod staging activation where staff needed this answer and could not get
it from logs. If that count is below a handful, keep A; B's oracle (§4) is
not worth opening for curiosity. If B ships to staging, review its audit
counts after 30 days: fewer than 10 legitimate uses, or any probing pattern
(§7), removes the command. Removal is unpublishing one command; no
preview-state store exists to clean, and the content-free audit rows
remain under their existing retention/erasure treatment (§6).

## 2. Current facts the design must not diverge from

Inspected at the commit this file lands with; line refs are to that tree.

- **One ordered matcher.** `AutomodFilter` has six reasons in legacy order —
  `bad_words`, `repeated_message`, `mention_spam`, `invite_link`,
  `external_link`, `attachment_type` (`crates/core/src/automod.rs:37-46`) —
  and `match_automod` returns the **first** match only
  (`crates/core/src/automod.rs:479-520`). `bad_words` returns before the
  repeat observation, so a bad-words CREATE neither counts toward nor
  breaks a repeat streak; every other CREATE is repeat-observed before its
  own filter is reached.
- **Exemptions precede matching.** Bot authors, exempt channels
  (`TWO_AUTOMOD_EXEMPT_CHANNEL_IDS`) and bypass-role holders
  (`TWO_AUTOMOD_BYPASS_ROLE_IDS`) are never inspected
  (`crates/core/src/automod.rs:230-249`,
  `crates/core/src/automod_runtime.rs:268-270`).
- **Effective config is layered, not env-only.** `TWO_AUTOMOD=1` is the cold
  boot gate; dry-run holds unless `TWO_AUTOMOD_ENFORCE` is exactly `1`
  (`crates/core/src/automod.rs:816-877`). Ten hot keys — three content
  lists (bad words, allowed domains, blocked attachment extensions), the
  mention limit, repeat count and window, sanctions, bypass/exempt ids,
  and the enforce flag (`crates/bot/src/automod_gateway.rs:113-127`) — are layered
  store-first over the boot env from the live settings snapshot and applied
  per delivery (`crates/bot/src/automod_gateway.rs:172-223`, `:328`;
  `crates/core/src/automod_runtime.rs:205-216`). An unusable stored value
  falls back to its boot value, and a sanctions-shape error keeps the last
  good policy. Any preview that re-parses env on its own can answer for a
  config that is not in force.
- **Scope is staging-only.** `AutomodScope::permits` allows only the
  configured staging guild unless an externally supplied live approval
  exists, and production wiring builds the scope with `live_approved: false`
  (`crates/core/src/automod_runtime.rs:23-36`,
  `crates/bot/src/automod_gateway.rs:102-110`). This proposal preserves that
  statement exactly; see §5.
- **Sanctions are a separate, later decision.** A match leads to target
  resolution, protection checks, violation counting, and only then a plan
  (`crates/core/src/automod_runtime.rs:323-405`). In dry-run, a match is
  capture-only in Discord terms — no preservation, target fetch,
  violation count, or Discord effect — but inspection has already
  advanced the in-memory repeat tracker, and the claim is settled with a
  persisted dry-run receipt
  (`crates/discord/src/automod_activation.rs:188-197`, `:235-251`).
  "Dry-run mutates nothing" is true of Discord and the violation ledger,
  not of bot state; §5.1 keeps the preview stricter than that.
- **Content is never persisted by the pipeline.** Plans never carry
  message content or matched excerpts — ids, the filter, sanction rung,
  count, outcome, and effects only
  (`crates/core/src/automod_runtime.rs:441-451`); the repeat tracker
  stores a non-cryptographic digest, never text
  (`crates/core/src/automod.rs:348-359`). The preview must be at least as
  strict (§6).
- **Permission precedent.** Discord's `default_member_permissions` is a
  picker default, not the boundary; the router checks resolved
  invoking-member bits against `command_permissions`
  (`crates/core/src/command_permissions.rs:1-6`, `:46-57`). Private
  previews mirror the authority of what they reveal:
  `/purge-preview` (ManageMessages, the purge authority,
  `crates/core/src/moderation.rs:209-220`) and `/welcome-preview`
  (ManageGuild, the configuration authority,
  `crates/core/src/command_permissions.rs:101`). Ephemeral replies use the
  shared helper (`crates/bot/src/command_runtime.rs:3085`).

## 3. Proposed surface (future build, not shipped by this doc)

One command, guild-only, published only in the final build slice (§9):

```text
/automod-preview text:<string, required>
                 [mentions:<integer 0-100>]
                 [attachments:<string>]
```

### 3.1 Invocation context is the synthetic message — nothing else is read

- **Author = invoker, always.** The synthetic `AutomodMessage` uses the
  invoker's own id, `author_is_bot: false`, and the invoker's resolved role
  ids (for the bypass exemption). There is deliberately **no member/user
  option**: previewing "as" another member would disclose that member's
  bypass/protection posture and repeat state, and would let staff rehearse
  a real member's next violation. Rejected here, not deferred.
- **Channel = invocation channel, always.** No channel option. To test an
  exempt-channel question, run the (invisible, ephemeral) command there.
  This avoids a channel-existence/permission fetch and any cross-channel
  probing of the exempt list.
- **Guild = invocation guild**, gated by §5. DM use refuses
  (`dm_permission: false` plus the handler guild check).
- **Timestamp = invocation receipt time.** The verdict reads *"if posted
  now"*. A pasted historical phrase is evaluated as a new post, which is
  the only truthful framing for repeat windows.
- **`mentions`** is a synthetic count of explicit user mentions, default 0.
  The matcher compares `mentioned_user_ids.len()` to the limit
  (`crates/core/src/automod.rs:504-506`); Discord, not raw text, decides
  what counts as a mention in a real post, so the handler must **not**
  parse `<@…>` tokens out of `text` and must not fabricate snowflake ids
  that could reach a log. The core preview entry point (§9, slice 1) takes
  the count as data. Because the count is decoupled from `text`, modelling
  a real mention-only post means pasting the mention tokens as the text
  (so repeat identity sees the same content) *and* setting the count.
- **`attachments`** is a comma-separated list of at most 10 bare filenames,
  each 1–255 UTF-16 units, no `/`, `\`, or control characters; total
  option ≤1,000 UTF-16 units. Names pass to the same matcher helper as
  live, which extracts the extension internally
  (`crates/core/src/automod.rs:623-629`). No file upload option: the
  bot must never receive a blocked-type file to prove it is blocked.
- **`text`** is 1–2,000 UTF-16 units (Discord's message bound; slash
  `max_length` 2000, revalidated in the handler). Zero-width, full-width,
  and whitespace-obfuscated forms are accepted — fidelity requires them,
  the matcher normalizes them (`crates/core/src/automod.rs:277-293`).
  Whitespace-only text evaluates when `mentions > 0` or `attachments` is
  present (the mention check is content-independent and attachment-only
  posts exist live; blank content itself never repeat-counts,
  `crates/core/src/automod.rs:417-419`); whitespace-only with neither
  refuses as malformed input, since no postable message maps to it.
- Strict decode, as other handlers: unknown, duplicated, or wrong-typed
  options refuse before any config read, rate consumption, or evaluation.
  No autocomplete (keystroke calls would multiply oracle queries and ship
  partial text through an extra surface). No components, buttons, modals,
  or follow-up interactions.

### 3.2 Output: one ephemeral, static-shaped verdict

Exactly one ephemeral reply (flags 64), plain text, mentions closed
(`allowed_mentions` empty), embeds suppressed, built only from fixed
templates plus the verdict fields below. The reply **never echoes the
input**: no text, no normalized text, no matched token/host/filename, no
input excerpt in success, refusal, or error shapes.

| Field | Values | Notes |
| --- | --- | --- |
| Result | `matched` / `no_match` / `exempt` / `partial` / `not_evaluated` | `partial` means repeat evaluation is `indeterminate` (§5.1); `not_evaluated` covers disabled or out-of-scope (§5) with one generic line |
| Deciding filter | one of the six `as_str` names, only for `matched` | **First match only**, exactly like live; absent for `partial`, which stops at the unproven repeat check without evaluating later filters |
| Deciding knob | setting *name(s)* from the table below, only for a match or exemption | Never a value: no bad word, no threshold number, no list size, no "4 of 5 mentions" near-miss; absent for `partial` |
| Exemption class | `exempt_channel` / `bypass_role` | The invoker's own channel/roles; no ids printed |
| Mode consequence | fixed sentence for `dry_run` or `enforce` | §5; a consequence statement, not a sanction prediction; absent for `not_evaluated`, which has no effective mode |
| Config currency | `current at <UTC time>` | No hash/fingerprint of the policy (a digest of the word list is dictionary-confirmable offline), no revision contents; absent for `not_evaluated` |
| Fixed caveats | first-match order; preview not recorded (§5.1); sanction/protection not previewed | Same words every time |

For `partial`, the reply uses the fixed sentence *Repeat evaluation is
indeterminate; no complete first-match verdict is available.* It includes
mode and config currency, but no deciding filter or knob. It never reports
`no_match` or a later filter. An exemption or `bad_words` match still
short-circuits before repeat evaluation and remains a complete result;
`partial` applies only when execution reaches the unproven repeat check.
Rate refusals and unavailable errors use the generic lines specified in
§4.3 and §6, not a fabricated verdict.

Knob mapping (names only; ranges/defaults stay in
[configuration](configuration.md#settings-catalog), which preview holders
can already read in this repo):

| Filter | Deciding knob shown |
| --- | --- |
| `bad_words` | `TWO_AUTOMOD_BAD_WORDS` |
| `repeated_message` | `TWO_AUTOMOD_REPEAT_COUNT`, `TWO_AUTOMOD_REPEAT_WINDOW_SECONDS` |
| `mention_spam` | `TWO_AUTOMOD_MENTION_LIMIT` |
| `invite_link` | none — fixed rule, no setting |
| `external_link` | `TWO_AUTOMOD_ALLOWED_DOMAINS` |
| `attachment_type` | `TWO_AUTOMOD_BLOCKED_ATTACHMENT_EXTENSIONS` |
| exemption | `TWO_AUTOMOD_EXEMPT_CHANNEL_IDS` or `TWO_AUTOMOD_BYPASS_ROLE_IDS` |

What the verdict type must be *unable* to carry (enforced by a field-shape
test in slice 1): threshold values, counts-observed-so-far, matched
substrings, hosts, extension strings, word-list size, violation counts,
sanction rungs. If a future need wants any of these, that is a new
disclosure decision, not a field addition.

**Sanction is not previewed.** The rung depends on the author's counted
violation history and on target protection/hierarchy facts the preview must
not fetch (§4). In dry-run the fixed consequence sentence is complete: a
real match would be captured only — not deleted, not violation-counted. In enforce
mode the sentence states only that a real match would enter the
delete-first enforcement path *subject to* target protection and the
author's violation history, neither of which was evaluated. Claiming a
specific rung would be a fabricated precision the pipeline itself does not
have at match time.

## 4. Anti-probing boundary (the security decision)

Assets: the configured bad-words list, the allowlisted domains, threshold
values, bypass/exempt id sets, and members' repeat/violation state. The
preview is, by construction, a set-membership oracle over the first asset:
one query asks "does this text trip a listed phrase?". The boundary below
keeps that oracle narrow, slow, attributed, and honest about its residue.

### 4.1 Who may preview: ManageGuild, router-enforced

- `default_member_permissions` **and** the router row both require
  ManageGuild (32), following `/welcome-preview`, not `/purge-preview`.
  Rationale: purge-preview mirrors ManageMessages because it previews what
  the same actor may delete. Automod preview reveals *configuration*
  behavior; configuration authority in this bot is ManageGuild
  (`command_permissions.rs`: `/command`, `/schedule`, `/feed-*`, settings).
  Ordinary moderators (ManageMessages/ModerateMembers) can act on members
  but cannot view the bad-words list today; the preview must not become a
  side door around that.
- The router checks resolved invoking-member bits
  (`crates/core/src/command_permissions.rs:46-57`); no new special cases
  are added to `allows` for this command.
- Publication is not authorization: the handler re-checks guild, scope
  (§5), and enabled state at run time.
- Residual risk, accepted explicitly if B is approved: a ManageGuild
  holder determined to extract the word list can do so over time (§4.3).
  The design goal is *no access for anyone else, bulk extraction slow and
  visible*, not secrecy from preview holders. Any intent approval that
  assumes stronger secrecy should choose A instead.

### 4.2 What the answer must not leak

- **First match only.** Reporting every filter that would trip multiplies
  oracle bandwidth per query and removes the order-masking live provides
  (`bad_words` hides whether the same text was also a repeat/invite).
- **No near-miss data.** No "mentions 4, limit 5", no "2 of 3 repeats", no
  distance-to-match, no per-filter pass/fail list. A `no_match` is one bit.
- **No values.** Knob names only (§3.2). Threshold *values* are eventually
  inferable by binary search through any honest oracle — including live
  enforce-mode deletions — but the preview must not print them or shorten
  the search with counts.
- **No matched content.** No token, host, or extension echo (§3.2), and no
  input echo at all, which also kills markdown/mention injection through
  the reply: the templates interpolate only verdict fields.
- **Uniform refusals.** `not_evaluated` is the same generic line for
  automod-disabled, out-of-scope guild, and DM attempts that reach the
  handler; it must not distinguish "config exists but off" from "never
  configured".

### 4.3 Rate and batch bounds (proposed constants, not current ones)

Batching is the real amplifier: one 2,000-unit text can carry ~250 short
candidate tokens, so group testing extracts a sparse list far faster than
one-word-one-query. Per-invocation limits alone therefore do not prevent
extraction by a determined holder; they meter it. Proposed, per
(actor, guild), counters held in a bounded in-memory map that borrows
only the capacity/refuse-when-full discipline of the existing actor
cooldowns (`crates/bot/src/member_runtime.rs:168-187`, whose shape is a
single timestamp per actor, not rolling counters — one always-on
container, so an in-memory limiter is not multiplied by replicas):

- at most 1 preview per 5 s;
- at most 30 previews per rolling hour;
- at most 120 previews per rolling day, and 300 per guild per day;
- a rate refusal does not evaluate, returns one generic ephemeral line
  with no retry-after value derived from other actors' usage, and is
  itself audited (§6);
- strict-decode refusals do not consume budget (they cannot evaluate, so
  they carry no oracle bit), but every *evaluated* preview consumes,
  including `no_match` and `exempt`;
- the limiter map is capacity-bounded with expired entries reusable, and
  a full map refuses new actors rather than evicting active counters
  (fail closed), as the cooldown map does.

Metering arithmetic, so the claim stays honest: at 120/day, exhaustively
testing a 10,000-word dictionary one word per query takes 84 days;
batched group testing shortens this to days for a sparse list. Rate
limits therefore buy *detection time*, and §6's audit plus the 30-day
review (§1) are load-bearing, not decoration.

Compute bound: text ≤2,000 units, ≤10 filenames. The matcher core does
no I/O; the handler around it adds exactly the in-memory live-snapshot
refresh of §5.0, one replay read, and one audit INSERT (§6) — no Discord
fetch of any kind. The matcher cost is the same scan live already pays
per message, and the hourly cap keeps added load negligible beside
gateway traffic.

### 4.4 No second oracle

- No preview-by-message-id/link (would fetch a third party's message,
  need channel read authority, and break on edits/deletes).
- No bulk input: one text per invocation, no file, no list of texts.
- No autocomplete, no component re-runs, no "try a variation" affordances.
- No API/internal-action twin of the command; the dashboard stays out
  (option D). One surface, one permission check, one audit trail.

## 5. Scope, modes, and statefulness

### 5.0 Scope is inherited, not widened

The handler evaluates only when the *same* gates the gateway uses admit
the guild: `TWO_AUTOMOD=1` at boot and `AutomodScope::permits` for the
invocation guild (`crates/core/src/automod_runtime.rs:224-230`). That is
the staging guild today; in the live guild (no live approval) and every
other guild the answer is `not_evaluated`, even though a policy value may
exist in config. Evaluating "hypothetically" outside scope would open the
oracle precisely where automod does not run. When automod was disabled at
boot, no runtime exists and the handler answers `not_evaluated` without
building one ad hoc. Registration is part of the same boundary: slice 4
publishes the definition into a guild registry **only when that guild is
the stack's configured guild and the automod scope admits it**, through
the existing guild-registry publication path
([command-publish](command-publish.md)) — the live guild should not even
list an inert oracle in its picker. If the publish set cannot be
scope-conditioned without a large refactor, that is the same
stop-and-redesign gate as the runtime handle below, not a reason to
publish globally.

**Effective config source.** The preview runs against the same
in-memory `AutomodRuntime` the gateway inspects with, after the same
per-event live-snapshot refresh the delivery path performs
(`crates/bot/src/automod_gateway.rs:207-223`, `:323-341`), cloning the
resulting config once so one evaluation sees one config. It must not call
`AutomodConfig::from_env` itself: that would miss live-snapshot value
overlays and last-good-policy fallback (§2) and could contradict the next
real message. Neither reach nor clone exists today — the only config
accessor is test-only (`crates/core/src/automod_runtime.rs:218-222`) and
the activation's `runtime()` accessor is private
(`crates/discord/src/automod_activation.rs:217-219`) — so slice 3 adds
both a runtime handle and a config-clone API. If either cannot be added
without a large refactor, that is a stop-and-redesign gate in slice 3,
not license to build a parallel config path. A config change landing
between preview and a real post can still change the real outcome; the
`current at <UTC time>` field (§3.2) is the stated bound on the claim.

### 5.1 Repeat and mention state: read-only or explicitly indeterminate

The hard part. `match_automod` calls `RepeatTracker::observe` in-line,
and for CREATEs `observe` **mutates**: it prunes, inserts, re-sorts, and
truncates the author's rows (`crates/core/src/automod.rs:401-470`). The
preview must therefore never call `AutomodRuntime::inspect` or
`match_automod` against the live tracker. It also must not call
`RepeatTracker::expire` or advance `newest_observation_ms`
(`crates/core/src/automod_runtime.rs:232-241`, `:281-302`).

Resolved semantics:

1. The preview evaluates the repeat filter **read-only against the
   invoker's own live CREATE history**: "would this be the Nth identical
   message inside the window if I posted it now?" The codebase already
   contains the non-mutating shape this needs — non-CREATE observations
   compute the same window predicate, counting the candidate once,
   without inserting, replacing, or pruning any row
   (`crates/core/src/automod.rs:442-447`). Slice 1 exposes that as an
   explicit preview entry point rather than abusing the edit path. The
   synthetic message id is namespaced to the interaction —
   `preview:<interaction_id>` — because the window predicate excludes
   rows whose message id equals the candidate's
   (`crates/core/src/automod.rs:425-441`): an id that could ever equal a
   real snowflake would silently drop that row and undercount. The
   namespaced id cannot collide with a Discord snowflake and never
   reaches a log.
2. **Correctness gate, not an assumption.** Slice 1 must prove, over the
   golden corpus plus generated histories, that preview verdict ==
   CREATE verdict computed on a *cloned* tracker (including the
   inclusive window edge: exactly `window` old counts, 1 ms older does
   not — [repeat-window acceptance](automod-repeat-expiry.md)), and that
   the live tracker is byte-identical after any preview. If that
   equivalence cannot be shown for the cloned-CREATE reference, the
   fallback is to report the repeat filter as `indeterminate` and label
   the whole reply partial — never to silently fall through to
   `no_match` or a later filter as if repeats were evaluated.
3. **Previews never accumulate.** Because nothing is recorded, previewing
   the same text 3 times cannot manufacture a `repeated_message` verdict,
   and a preview between two real identical posts neither extends nor
   breaks the real streak. The fixed caveat says so: *preview recorded
   nothing; a real post would itself count toward repeat history, unless
   exempt or stopped by `bad_words` first* — mirroring live ordering,
   where a `bad_words` preview must short-circuit before the repeat read
   exactly as live short-circuits before the repeat write.
4. **Cross-member isolation.** The tracker key is `guild:author`
   (`crates/core/src/automod.rs:422`) and the synthetic author is the
   invoker (§3.1), so a preview can only ever read the invoker's own
   recent-message digests — information they already have. Another
   member's repeat state is unobservable through this surface by
   construction, and a test must pin it (§8).
5. **Serialization.** The read runs under the same runtime lock the
   gateway worker uses, synchronously, never held across an await
   (the existing activation contract,
   [runtime handoff](automod-runtime.md)), or on a snapshot of only the
   invoker's rows taken under that lock. Stale unswept rows need no
   expiry call: the window predicate already ignores rows outside the
   cutoff relative to the preview timestamp.
6. `mention_spam` is stateless once the count is an input (§3.1); it
   reads no history and writes none. All other filters are pure over
   text/names. The preview path creates **no** delivery claim, no
   preserved decision, no violation record, no funnel disposition, and
   no XP/capture side effect — it does not enter
   `AutomodActivation::process` at all.

### 5.2 Dry-run and enforce describe consequences, not different verdicts

The matcher is mode-blind; `dry_run` only changes what happens after a
match (`crates/core/src/automod_runtime.rs:326-332`, `:350-405`). The
preview is available in both modes under the same permission, rate, and
audit rules — in dry-run its value is checking phrases without test
posts; in enforce it additionally avoids a real delete/violation-count against the
tester. The mode shown is read from the effective config at evaluation
time (it is a hot key and can change without restart). The preview never
changes the mode, and an enforce-mode preview executes nothing: no
target fetch, no `target_gate`, no `plan`, no executor call.

## 6. Persistence, audit, and privacy

- **Previewed text is memory-only.** It exists in the interaction payload
  and the synthetic message for one synchronous evaluation. It must never
  appear in: the reply, tracing logs, metrics labels, audit rows, error
  messages, claim/violation stores, or any file. **Its hash must not be
  stored either** — a digest of a guessed phrase confirms the guess
  offline, the same reason no policy fingerprint is shown (§3.2). The
  canary test (§8) checks every sink for the raw and normalized sentinel
  and their digests (including SHA-256/MD5 hex). It must also compute the
  actual repeat digest through the same `digest_content` helper and
  normalization as the matcher, under the pinned toolchain
  (`crates/core/src/automod.rs:375-381`), and check its emitted
  representations, including decimal u64 `Debug`/`Display` and hex forms.
  `DefaultHasher::new()` does **not** use a process-random seed: its
  digest is offline-computable for a guessed phrase on that toolchain,
  even though the algorithm is not stable across Rust releases. Never
  substitute tracker immutability for this sink check: a read-only
  evaluation can log a digest without changing a row. The independent
  tracker-immutability proof (§5.1) is required as well.
- Discord itself transports the slash option; bot-side rules cannot
  govern Discord's own handling. Command help text must warn: *test the
  smallest synthetic phrase; do not paste real member messages
  containing personal data.* The ephemeral reply can still be
  screenshotted by the invoker — a residual that the no-echo rule (§3.2)
  keeps content-free.
- **Audit every admitted attempt, content-free, in the existing
  `automation_audit_log`** (created by migrations `0130`/`0150`,
  `crates/cutover/migrations/0150_sticky_messages.sql:43-53` — the sink
  sticky/feed/custom-command handlers already append ids-and-outcomes
  rows to). One row per evaluated preview and per in-scope rate
  refusal, mapped onto its columns with enum-only values:
  `id = automod-preview:<interaction_id>`, `guild_id`, `actor_id`
  (invoker), `action = automod.preview`, `target_key` (channel id),
  `outcome` (one of `matched`, `no_match`, `exempt`, `partial`,
  `rate_limited`), and `reason` built *only* from enums —
  `filter=<name|none>;mode=<dry_run|enforce>;len=<bucket>` with input
  length bucketed as `0_50` / `51_500` / `501_2000` UTF-16 units (a
  length, never content). `partial` always means repeat evaluation was
  `indeterminate`, with `filter=none`; replay reconstructs exactly the
  partial reply of §3.2, never `no_match` or a later-filter verdict.
  Plus a fixed-label metric counter with these closed vocabularies:
  `result=matched|no_match|exempt|partial|rate_limited|not_evaluated|unavailable`,
  `filter=<one of the six names|none>`, and
  `mode=dry_run|enforce|not_evaluated`. The last mode value is a sentinel
  for no effective config snapshot, not an automod runtime mode.
  `not_evaluated` attempts get the metric with
  `result=not_evaluated,filter=none,mode=not_evaluated` but no row: they
  are outside the scoped guild's audit story and carry no oracle bit.
  **Audit failure fails closed:** if the row cannot be written, the
  verdict is suppressed and the reply is the generic unavailable line;
  the metric uses `result=unavailable,filter=none`, with the effective
  mode if a snapshot exists or `mode=not_evaluated` otherwise. An
  unaudited oracle is worse than a briefly unavailable one.
  Redelivery is fenced by a read-before-evaluate gate, not by the
  insert alone (`audit_once` deduplicates facts, not executions,
  `crates/core/src/custom_command_store.rs:39-51`): after strict decode
  the handler loads any row under the interaction's audit id (the
  existing replay read, `custom_command_store.rs:66-77`) and, if one
  exists, renders the stored row — outcome, reason, and its
  `created_at` as the config-currency time — without re-evaluating or
  consuming rate budget. After its own insert, a handler likewise
  renders the first stored row, so two racing deliveries report one
  verdict. Sequential redelivery therefore evaluates once; concurrent
  redelivery may evaluate twice internally but can only ever *report*
  the first stored verdict, disclosing nothing the first answer did
  not. **Stated retention:** `automation_audit_log` has no installed
  age-based purger ([privacy](privacy.md#retention-and-explicit-exceptions)
  — no live TTL is claimed beyond the windows the retention job
  enforces, and this table is not one of them), so preview audit rows
  persist until member erasure or guild purge. That is accepted
  deliberately: the rows are content-free, and their longevity is what
  makes slow probing (§4.3) reconstructible. Member erasure already
  deletes this table's rows by `guild_id + actor_id`
  (`crates/cutover/src/member_erasure_plan.json:139-141`), so the
  invoker's actor id has an erasure path on day one. Slice 3 must still
  prove the runtime role's INSERT grant on this table through the
  existing store path before publication; if that proof fails,
  publication is blocked, not rerouted to logs-only.
- **Probing detection is a stated operator duty**, not a hope: hitting
  the hourly or daily cap (§4.3) is an auditable event visible in the
  same record stream, and the 30-day review (§1) reads per-actor counts.
  No automated paging is proposed; the staging-only scope and ManageGuild
  population keep the reviewer burden to a count glance.
- No new personal data about the *subjects* of messages exists: there is
  no subject. No member option (§3.1), no message fetch (§4.4), no
  repeat rows written (§5.1), nothing for an erasure request to find in
  preview state.

## 7. Abuse cases carried as design requirements

| # | Abuse / failure | Required behavior |
| --- | --- | --- |
| 1 | Dictionary extraction, one word per query | Slowed by §4.3, counted in §6 audit; accepted residual for ManageGuild holders (§4.1) — not claimed prevented |
| 2 | Batched group-testing (many candidates in one 2,000-unit text) | First-match-only + no near-miss keeps each query to one filter identity (or none) — never a per-filter vector; batching cannot extract thresholds or later filters in the same query; residual as #1 |
| 3 | Threshold binary search (vary mentions/repeat attempts) | No counts or limits printed; repeat history is never written, so repeat-search via previews alone cannot even build the streak it is searching for |
| 4 | Probing another member (their repeats, bypass, protection) | Impossible by construction: author/channel are the invoker's, no member option, no sanction prediction (§3.1, §5.1) |
| 5 | Tracker pollution (preview to create/break a real streak, or to frame the next real post) | Read-only repeat path; equivalence + immutability proven by test before publish (§5.1 gate) |
| 6 | Enforce-mode rehearsal (test until a slur variant slips through, then post it) | This is the oracle's inherent dual use; contained by ManageGuild-only + audit + caps. A non-staff member gets no surface. If staff misuse is observed, §1 kill rule |
| 7 | Oversized / pathological input (2,001st unit, surrogate pairs, 11 attachments, 100-mention spam, regex-bait URLs) | Hard option bounds (§3.1) refuse pre-evaluation; evaluation cost bounded by the same scan live pays, paced by §4.3 |
| 8 | Mention/markdown injection via text (`<@everyone>`, links, formatting) | Reply interpolates no input; mentions closed; embeds suppressed |
| 9 | Config race (hot settings change mid-preview or between preview and post) | One cloned config per evaluation (§5.0); `current at` timestamp bounds the claim; divergence disclosed, not hidden |
| 10 | Live-config invalid value at preview time | Same last-good/boot fallback as the gateway (§2) because the same runtime answers; preview never parses config itself |
| 11 | Audit/log leakage of tested phrases | Canary test over every sink incl. precomputable digests (§6); fail-closed audit (§6) |
| 12 | Rate-limiter abuse to deny others | Per-actor buckets; guild daily cap is the only shared bucket and is sized well above legitimate staff use; refuse-new-actor (not evict) when full |

## 8. Test plan for the future build (acceptance shape)

Offline first: no test needs Discord, a live database, or staging data
until slice 4. Each row names the proof, not a vague area.

| # | Scenario | Expected proof |
| --- | --- | --- |
| T1 | Golden corpus replayed through preview | Same verdict as `match_automod` on cloned tracker for every corpus row |
| T2 | Repeat equivalence | Generated histories (counts 2–20, window edge ±1 ms, out-of-order, duplicate ids): preview == cloned-CREATE verdict; live tracker unchanged after (rows compared) |
| T3 | Preview storm then real posts | N previews of identical text, then real CREATEs: streak counts only the real posts |
| T4 | Order masking | Text matching `bad_words` + invite + repeats → `bad_words` only; text matching mention + invite → `mention_spam` only |
| T5 | Cross-author isolation | Author B's real repeat streak never affects author A's preview verdict |
| T6 | Exemptions | Invoker with bypass role / in exempt channel → `exempt` with class only, matcher not reached |
| T7 | Scope/mode matrix | Disabled, out-of-scope guild, DM → identical `not_evaluated`, no audit row or mode/currency in reply, metric `result=not_evaluated,filter=none,mode=not_evaluated`; dry-run vs enforce → same filter, different consequence sentence |
| T8 | Permission matrix | ManageGuild admitted; ManageMessages/ModerateMembers/everyone refused by router; picker default alone insufficient if bits absent |
| T9 | Output disclosure | Verdict struct field inventory test; for a property set of inputs, replies contain no input substring and render no threshold value (only knob names) |
| T10 | Canary | Raw/normalized sentinel and digests (SHA-256/MD5 hex plus actual matcher repeat digest, including decimal u64 Debug/Display and hex forms) absent from captured logs, audit rows, metrics, errors, and reply; independently, tracker rows unchanged (§6). Deliberately logging a computed repeat digest must fail the sink check even with an unchanged tracker |
| T11 | Bounds | 2,000 units ok / 2,001 refused; astral-plane chars counted in UTF-16; 10 attachments ok / 11 refused; a filename without a dot is treated as its own extension, as live; `mentions` 100 ok / 101 refused; whitespace-only text refused bare, evaluated with attachments, and evaluated with `mentions` at the limit → `mention_spam` (§3.1) |
| T12 | Rate limiter | Paused-clock boundaries for 5 s / hourly / daily caps; refusal does not evaluate (matcher call count 0); full-map refuses new actor |
| T13 | Zero pipeline writes | Mock ledger/executor/funnel counters all 0 after matched preview in enforce mode |
| T14 | Audit fail-closed | Failing audit sink → no verdict in reply, generic unavailable, metric incremented |
| T15 | Config parity | Hot-changed word/threshold via live snapshot flips preview and next real delivery identically; invalid stored value keeps last-good in both |
| T16 | Injection | Text with `<@everyone>`, markdown, URLs → reply byte-shape is a fixed template, `allowed_mentions` empty |
| T17 | Redelivery | Same interaction delivered twice sequentially: matcher call count 1, one audit row, identical replies incl. currency time; concurrently: both replies equal the first stored row (§6) |
| T18 | Registration scope | Publish set contains `automod-preview` only for the configured guild when the automod scope admits it; absent for the live guild without approval (§5.0) |
| T19 | Unproven repeat equivalence | When repeat equivalence cannot be proven and execution reaches that check: repeat status `indeterminate`, reply result `partial` with fixed explanation and no deciding filter/knob; later filters not evaluated; audit `outcome=partial,filter=none` and effective mode/length; metric `result=partial,filter=none` and effective mode; redelivery renders the same partial reply. Earlier exemption/`bad_words` short-circuits remain complete |

## 9. Future build slices (each ≤1 day, cut-ready; not created here)

Dependencies: intent alignment at the 2026-11-02 checkpoint first (§1
demand gate). Slices land in order; the command is **published only in
slice 4**, so no slice ships a dead or half-audited surface. Auth-adjacent
slices (2–4) carry the CISO comment-gate.

1. **Core preview in `crates/core`** — preview entry point over a
   synthetic message + mention count + cloned config: first-match verdict,
   exemption class, deciding-knob names, mode; read-only repeat path per
   §5.1. Tests T1–T6, T9 (struct shape), T19. Stop/go: if T2 equivalence fails,
   implement the `indeterminate` fallback or stop — do not ship silent
   fall-through.
2. **Command definition + permission row (unpublished)** —
   `CommandDefinition` (ManageGuild, guild-only, options of §3.1), next
   free `command_permissions` parity row, router refusal text, help/
   registry count pins updated. Tests: registry acceptance, router matrix
   (T8 shape), default-permissions wire value. Handler, if wired at all,
   returns unavailable; the definition is kept out of the published set.
3. **Runtime handler** — ephemeral handler through the shared
   `AutomodRuntime` handle with the §5.0 refresh, bounded rate limiter
   (§4.3), the §6 `automation_audit_log` row + metric with fail-closed
   suppression. No migration is expected (table and erasure entry
   exist); needing one re-opens the slice boundary instead of folding
   DDL in here. Tests T7, T10–T17, T19 with mock executor/ledger.
4. **Publish + docs + staging proof** — add the definition to the
   published moderation set under the §5.0 scope condition (T18),
   regenerate `docs/commands.md`, operator-guide
   section pointing here, smoke-surface inventory row. Staging-only
   synthetic proof in the staging guild with a synthetic bad word:
   preview verdicts in dry-run (and, only if separately authorized,
   enforce), then real posts confirming previews left no repeat streak
   and no violation (T3 at system level). Rollback: remove from the
   published set (one revert); no persisted preview state exists to clean.

Out of scope for every slice: config editing from the preview, new
`TWO_AUTOMOD_*` keys, sanction prediction, member-targeted preview,
preview-by-message-id, dashboard parity, live-guild scope, and any change
to matcher semantics (corpus rows are added only as preview-parity
expectations, never as behavior changes).

## 10. Spec self-check (doctest)

The numbers asserted above, verified against themselves. This checks the
document's arithmetic and inventories only — not runtime behavior, which
does not exist yet.

```python
>>> filters = ["bad_words", "repeated_message", "mention_spam",
...            "invite_link", "external_link", "attachment_type"]
>>> len(filters)  # AutomodFilter variants, automod.rs:39-46
6
>>> knobs = {"bad_words": ["TWO_AUTOMOD_BAD_WORDS"],
...          "repeated_message": ["TWO_AUTOMOD_REPEAT_COUNT",
...                               "TWO_AUTOMOD_REPEAT_WINDOW_SECONDS"],
...          "mention_spam": ["TWO_AUTOMOD_MENTION_LIMIT"],
...          "invite_link": [],
...          "external_link": ["TWO_AUTOMOD_ALLOWED_DOMAINS"],
...          "attachment_type": ["TWO_AUTOMOD_BLOCKED_ATTACHMENT_EXTENSIONS"]}
>>> sorted(knobs) == sorted(filters)  # every filter has a knob row in 3.2
True
>>> min_interval_s, per_hour, per_day, guild_day = 5, 30, 120, 300
>>> per_hour * 24 > per_day  # daily cap binds before hourly pace does
True
>>> import math
>>> math.ceil(10_000 / per_day)  # days for a 10k one-word dictionary sweep, 4.3
84
>>> 2000 // 8  # ~250 short tokens per max-length batched query, 4.3
250

```

Run: `python3 -m doctest docs/automod-preview-spec.md`.
