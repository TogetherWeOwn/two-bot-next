# Truthful event timing in the anchor welcome

Status: **Proposed — design only**. Written 2026-10-26; review by **2026-11-02**.
This is the sole editable repository spec. No current welcome copy, runtime,
golden test, approved hold, permission, schedule or deployment changes here.
Merging this document is **not permission to change frozen copy**. Exact host/CEO
intent alignment and independent CISO/frozen-copy disposition must precede any
future implementation or protected acceptance-test allocation.

## Decision, value and alternatives

A newcomer should not be told that an event is happening before its start, or
promised another hour in its final minute. The schedule is evidence of a planned
time, **not evidence that humans are present or an event actually happened**.
Frequency, member impact and demand are unmeasured; no live observation is claimed.

| Option | Result | Disposition |
| --- | --- | --- |
| A. No build | Preserves the authorized frozen copy, including its near/live ambiguity and duration promises | **Default now**, until exact alignment and disposition |
| B. Three timed middle paragraphs | Distinguishes far, upcoming and scheduled-live; replaces duration promises with localized schedule timestamps | Smallest conditional truthful alternative; proposed below |
| C. Attendance-aware status, new timing service or interactive welcome | Could report observed activity, but adds reads, authority and layout changes | Out of scope; not needed for B |

B changes only the anchor welcome's middle paragraph and the bounded timing
validation needed to render it safely. It preserves the greeting, blank-line
layout, final invitation paragraph, member-only mention policy, destination,
prompt/dry-run gates, and absence of footer, buttons and picker. Legacy/session
welcome text, game roles, onboarding mode, event creation, reminders, notices and
attendance stay unchanged. The existing final paragraph's party invitation is
retained by scope, not newly verified or endorsed as an attendance guarantee.

## Source evidence, not deployment evidence

Inspected baseline: `e1868d97d3519cae3f2ed91726e83e98f77dff0d`.
All line references below describe that source revision. No production/staging,
Discord, database, event attendance or deployment reads were performed.

| Seam | Source and conclusion |
| --- | --- |
| Schedule input | [`AnchorSpec` and `SUNDAY_SQUAD`](../crates/core/src/onboarding.rs#L880-L909): name/channel, local weekday/hour/minute, duration in minutes, series epoch; Sunday 20:00 America/New_York, 60 minutes; near window is 7,200 seconds |
| Local recurrence | [Calendar conversion and occurrence selection](../crates/core/src/onboarding.rs#L948-L1023): 2007+ US Eastern rules; each local date converted independently; next start is strictly after now, previous start is at/before now |
| State boundaries | [`occurrence_context`](../crates/core/src/onboarding.rs#L1025-L1052): live is start <= now < end; live also sets near=true; otherwise near means next_start - now < 7,200 (strict) |
| Incorrect selection | [`anchor_welcome_text`](../crates/core/src/onboarding.rs#L1054-L1078) uses near, not live, for “is happening right now” and “for about another hour”; far says “for about an hour” regardless of spec duration |
| Frozen contract | Same renderer explicitly says verbatim, no append/picker/buttons/footer. [Operator guide](onboarding-operator-guide.md#gate-modes) and [domain guide](onboarding-port.md) describe the current behavior, not authorization to revise it |
| Bad-input gap | `duration_minutes` is u64 but cast to i64 and multiplied unchecked in the current context; invalid weekday falls through to a synthetic 2030 next start. There is no optional occurrence-data service or current unavailable-state API |

## Exact proposed copy (B, not accepted)

Tokens `N`, `C`, `S`, `E` below mean the existing spec name, channel ID, selected
start epoch seconds and **checked** `S + duration_minutes * 60`. They are
substitution notation, not literal output. Use the same safe existing name and
channel source; no new user template, configuration key or schedule editor.

**F — far / next occurrence** (`live=false`, `near=false`):

```text
The thing to know: **{N}**, every Sunday at 8pm Eastern in <#{C}>. We play Fall Guys. Next one is <t:{S}:F>, scheduled until <t:{E}:F>.
```

**U — upcoming-near** (`live=false`, `near=true`):

```text
The thing to know: **{N}** starts <t:{S}:R> in <#{C}> — Fall Guys, scheduled until <t:{E}:F>. Come say hi. You don't need it installed to join in.
```

**L — scheduled-live** (`live=true`, irrespective of near):

```text
The thing to know: **{N}** is scheduled for now in <#{C}> — Fall Guys, until <t:{E}:F>. Come say hi. You don't need it installed to join in.
```

“Scheduled for now” deliberately does not claim observed activity. Select L
**first by live**, then U by near, else F. Exactly started uses L; exactly ended
uses the next occurrence's F/U, never L for the ended one. No countdown loop,
post edits or new job: truth is evaluated at render time, not a promise that an
old message remains current when read later. Discord's relative timestamp may
change on the client; the surrounding sentence is static.

The whole valid welcome remains five newline-joined entries:

```text
Hey <@{member_id}> — glad you're here.

{one exact middle paragraph above}

You don't need to sign up or say anything first — just join the voice room and I'll get you into the party. Haven't got Fall Guys? Come anyway, there's something we can play right there in the room. If you can't make Sunday, hop in whenever and see who's about.
```

Nothing follows that final period. No new embed, footer, action, menu or timestamp
outside the middle paragraph. This document's code fences are examples, not a
replacement for the current golden strings.

### Timestamp, duration and supported-input semantics

- Emit UTC epoch **seconds** in Discord tokens, never milliseconds or manually
  formatted local clock strings. `F` displays a full date/time in each viewer's
  client locale/time zone; `R` is the client's relative start time. Examples here
  use English/Eastern solely for explanation; do not assert identical rendered
  language or relative rounding across clients.
- End is elapsed duration from the selected occurrence start, not from now and
  not a separately guessed “9pm”. Use checked conversion/multiplication/addition
  before context arithmetic. No “about an hour”, “another hour”, rounded remaining
  minutes, or `now + 3600`. A final-minute welcome displays the real scheduled end.
- **Proposed bound (design choice, not an existing validator):** integer duration
  1..=1,440 minutes, checked epoch arithmetic and representable Discord timestamp
  range 0..=253402300799 for now/start/end; additionally reject dates before 2007
  (the source's supported Eastern-rule era). Reject zero, 1,441 and u64::MAX
  before any cast: unchecked `as i64` can wrap, not only overflow multiplication.
  The one-day cap intentionally excludes multi-day events; this is not permission
  to make schedules configurable.
- B supports only the existing Sunday 20:00 Eastern/Fall Guys anchor identity;
  preserve its recurrence and series epoch. Test duration variants on synthetic
  copies of that identity. Other weekday/hour/minute identities are unsupported
  for this copy (“every Sunday at 8pm” would lie); do not generalize them or use
  the defensive 2030 fallback as member-facing occurrence evidence. Invalid
  weekday/hour/minute, inconsistent identity or unrepresentable dates refuse.
- Keep existing modern Eastern conversion; no new time-zone dependency or fixed
  UTC week addition. Across fall-back the two Sunday starts are 169 hours apart;
  across spring-forward they are 167. At Sunday 20:00 the wall time itself is
  neither missing nor ambiguous. Arbitrary 01:30/02:30 transition schedules and
  historical pre-2007 rules are not supported by this proposal.
- The built-in recurrence supplies occurrences without a DB/network call. An
  empty external event list is **not a current input**, nor proof this recurring
  anchor is cancelled. Do not attach scheduled-event/attendance data to B.
  If a future caller cannot supply a valid anchor/context, follow the failure
  matrix below rather than inventing a start, end, duration or activity claim.

## Synthetic state/evidence table

These are **design fixtures**, inferred from source boundaries, not Rust test
results or live events. Let `S=1792972800` (2026-10-25 20:00 EDT / October 26
00:00 UTC), `E=1792976400` (21:00 EDT), and `S2=1793581200`, `E2=1793584800`
(November 1 20:00–21:00 EST). `N=Sunday Squad`; `C` remains the existing anchor
channel, not a live channel read. F/U/L select the exact paragraphs above.

| ID | Injected now / selected occurrence | Existing near/live | Current middle result | Proposed exact template and substitutions |
| --- | --- | --- | --- | --- |
| W01 far boundary | S−7,200 = Oct 25 18:00 EDT; S/E | false/false | Far, “about an hour”, relative next start | F(N,C,1792972800,1792976400) |
| W02 just near | S−7,199; S/E | true/false | Incorrect “happening right now” | U(N,C,1792972800,1792976400) |
| W03 twenty minutes before | S−1,200 = 19:40 EDT; S/E | true/false | Incorrect “happening right now” | U(N,C,1792972800,1792976400) |
| W04 exactly started | S = 20:00 EDT; S/E | true/true | “Happening right now”, “another hour” | L(N,C,1792972800,1792976400) |
| W05 live near end | E−60 = 20:59 EDT; S/E | true/true | Incorrect “about another hour” | L(N,C,1792972800,1792976400); scheduled end is one minute away |
| W06 final second | E−1; S/E | true/true | Same hardcoded duration | L(N,C,1792972800,1792976400), no rounding up |
| W07 exactly ended | E = 21:00 EDT; S2/E2 | false/false | Far; chooses next week | F(N,C,1793581200,1793584800); no ended-event invitation |
| W08 next occurrence near | S2−1,200 = Nov 1 19:40 EST; S2/E2 | true/false | Incorrect “happening right now” | U(N,C,1793581200,1793584800) |
| W09 next exactly started | S2; S2/E2 | true/true | Near/live share current string | L(N,C,1793581200,1793584800) |
| W10 spring-forward pair | Mar 7 2027 start=1804467600; Mar 14 start=1805068800; each injected start−1,200 | true/false each | Incorrect “happening right now” each | U(N,C,1804467600,1804471200); U(N,C,1805068800,1805072400) |
| W11 duration bounds | At S with durations 1, 90, 1,440 | true/true each | Always “another hour” | L with E=1792972860, 1792978200, 1793059200 respectively |
| W12 invalid/unavailable | duration 0/1,441/u64::MAX; missing context; invalid identity; overflow | No valid proposed classification | Current API lacks a safe unavailable result | No timed welcome; exact handling below |

For clarity, W03's proposed middle is exactly:

```text
The thing to know: **Sunday Squad** starts <t:1792972800:R> in <#{C}> — Fall Guys, scheduled until <t:1792976400:F>. Come say hi. You don't need it installed to join in.
```

W05's proposed middle is exactly:

```text
The thing to know: **Sunday Squad** is scheduled for now in <#{C}> — Fall Guys, until <t:1792976400:F>. Come say hi. You don't need it installed to join in.
```

`C` is the sole remaining substitution in those two examples. There is no real
attendance assertion at either timestamp. W07 crosses the fall-back week; verify
`S2−S=608400` seconds, not 604800. W10's starts differ by 601200 seconds. The
source's normal 60-minute anchor never crosses the 02:00 transition within a run;
for synthetic longer duration, add elapsed seconds across any offset change and
let Discord localize each endpoint independently.

## Error and next-action matrix (proposed, not current behavior)

These are stable design scenario/reason IDs for future acceptance, **not an API
schema introduced here**. Member output remains ordinary prose, never JSON.
“Retryable” means eligible after remediation under existing event/prompt rules,
not permission for a new scheduler or repeated Discord sends.

| Reason | Detection | Member/runtime result | Retryable / next action |
| --- | --- | --- | --- |
| `INVALID_ANCHOR` | Unsupported identity/wall time, zero/out-of-bound duration, arithmetic/timestamp-range failure | Preview privately explains “I couldn't determine this event's time. No welcome was sent.” Runtime skips the entire anchor welcome; no fabricated fallback paragraph, no successful prompt record | No automatic retry; correct the input through existing authorized configuration/code path, then render afresh |
| `OCCURRENCE_UNAVAILABLE` | No validated start/end supplied or selection fails | Same private preview text; runtime skips welcome, records no successful prompt | Retry only on a fresh eligible event/preview after source repair; no new polling or recovery service |
| `EMPTY_EXTERNAL_LIST` | An unrelated event list has no rows | Not consulted by this renderer; valid built-in anchor still uses F/U/L | Not an error; do not infer cancellation or fetch more data |
| `PREVIEW_TIME_CHANGED` | A later invocation crosses start/end | Render the later invocation using its own injected now; never reuse cached copy as proof of current state | Fresh preview; no mutation/retroactive post edit |
| `EXISTING_DELIVERY_REFUSAL` | Current dry-run, prompt guard, channel/access or send refusal | Preserve existing suppression, logging and retry/recording semantics; never post into a fallback channel or add a picker | Existing operator guide remediation; no role/permission repair |

Proposed wiring decision: make the shared anchor renderer fallible with a typed
`Result<String, AnchorWelcomeError>`; `adjudicate_welcome` maps its error to
`WelcomeEffect::Skip` with the corresponding constant reason. Preview maps that
specific skip to the private guidance above, not its current missing-channel
fallback. The runtime already adjudicates **before** `begin_prompt`; preserve that
ordering so invalid timing acquires no guard, sends nothing and records no prompt
or success funnel row. Do not add an error post to the public channel. This safe
suppression/private guidance is **new proposed behavior requiring disposition**,
not a claim that the current String-returning renderer already supports it.

Validate identity first: name, channel ID, weekday, hour, minute and series epoch
must equal `SUNDAY_SQUAD`; only duration may vary within the proposed bounds.
Then validate now/range and duration, compute checked duration seconds, select
previous/next using the existing local-calendar algorithm, check start and end
range/addition, and compute deltas with checked subtraction. Input/range/arithmetic
failures use `INVALID_ANCHOR`; only a valid input with no selectable occurrence
uses `OCCURRENCE_UNAVAILABLE`. Reject invalid identity before recurrence, so its
2030 invalid-weekday fallback cannot reach copy. This introduces a validation read
of series epoch; it does not claim current recurrence uses that field.

## Frozen pins and preview/runtime parity

The following inventory must be rechecked at the future implementation head.
Nothing in these files is changed by this docs-only proposal. The Test Author,
not the implementer, owns changes to protected acceptance/golden tests after the
exact copy disposition is recorded.

| Pin | Authorized future change versus invariant |
| --- | --- |
| `crates/core/src/onboarding.rs::anchor_welcome_text`, lines 1054–1078 | Replace only the two middle-string branches with F/U/L; explicitly amend the verbatim contract's copy authority after alignment, not remove its no-append rule |
| `welcome_copy_is_byte_identical_to_the_pure_renderers`, lines 1767–1790 | Its far and near anchor golden strings require exact authorized replacement; add independent L/boundary/bad-input coverage. Legacy/session golden bytes stay unchanged |
| `welcome_effects_post_the_renderer_bytes_unchanged`, lines 1794–1848 | Preserve byte identity and no wrapper/footer/truncation for valid welcomes; add safe-skip cases without deleting valid-path assertions |
| `anchor_copy_has_two_voices_and_no_attachments`, `onboarding.rs:2079–2089` | Far `:R` pin at 2083 becomes F's `:F`; upcoming/live “happening right now” at 2086/2089 and upcoming no-timestamp at 2087 require authorized replacement. Rename for three voices; retain greeting, recurrence and final-invitation assertions |
| `OccurrenceContext` and recurrence pins, `onboarding.rs:2055–2075` | Preserve `sunday_evening_names_today_not_next_week`, `occurrence_context_names_the_live_event_not_next_week`, strict two-hour boundary, inclusive start/exclusive end and local-calendar recurrence for valid inputs |
| Private preview acceptance | `crates/bot/src/welcome_preview_acceptance_tests.rs:590–699`: `preview_anchor_renders_live_body_without_picker` and `preview_matches_live_join_render_for_same_config_across_modes` are dynamic renderer parity/no-picker oracles, not obsolete literal goldens. Preserve assertions; only Test Author may adapt fallible-renderer call syntax. `crates/core/tests/welcome_preview_acceptance.rs` command/permission/routing contract stays unchanged |
| Layout and wire pins | `welcome_posts_picker_per_mode_and_skips_without_a_channel` (`onboarding.rs:2095–2163`), `menu_bounds_defaults_and_anchor_absence` (`crates/discord/src/onboarding_messages.rs:120–136`), `shared_executor_renders_each_mode_and_goodbye_without_extra_messages` (`crates/discord/tests/onboarding_wire.rs:63–146`), runtime `onboarding_runtime_modes_dry_run_anchor_and_redelivery` (`crates/bot/src/onboarding_tests.rs:349–380`) and recipient acceptance helper (`crates/bot/src/gateway_tests/onboarding/recipient_acceptance.rs:247–283`) preserve no picker/buttons/footer/embeds, destination, allowed mentions, dry-run and redelivery fences |
| Current-behavior docs | Only in an authorized future build: revise `docs/onboarding-operator-guide.md:225–229` (near/live share “right now”) and qualify intentional divergence in `docs/parity.md:543`. Preserve the renderer/Post identity evidence in `docs/soak-checklist.md:1412–1415` and `docs/soak-checklist.json:2780–2782` |

Preview and actual welcome must consume **one checked renderer** with the same
`AnchorSpec`, member ID and injected epoch; identical inputs yield identical
**welcome body** bytes and no components. Preserve preview's existing private
banner/configured-destination prefix; the entire preview response is not the
literal channel message. Production preview deliberately uses a fixture member,
not the invoking admin. Same-input parity fixtures normalize that identity.

Current callers: [`OnboardingRuntime::welcome`](../crates/bot/src/onboarding.rs#L643-L719)
passes its supplied `now_secs` into adjudication before locking; the gateway path
converts `now_ms / 1000` at line 542. [Preview](../crates/bot/src/welcome_preview.rs#L85-L141)
passes `now_ms / 1000` at line 97. Keep these injected clocks; do not read time
again inside the renderer. Core `C` is `spec.channel_id`; both existing callers
then substitute the configured anchor destination (runtime lines 694–695,
preview 115–118). Preserve this matching substitution and existing validation,
never treat C as permission proof or switch to another channel on timing failure.

An invocation at a different time may legitimately switch F/U/L. Preview stays
private, never marks prompted/sends a welcome, and never fabricates membership,
channel or attendance evidence. Runtime preserves existing guard and recording
order. The [preview operator contract](onboarding-operator-guide.md#private-welcome-preview)
is the discoverable entry point; recheck actual callers at the implementation head.

## Bounded future acceptance and <=1-day proposal

**Not allocated; no follow-on build or test cards created here.** After exact
alignment and independent disposition, propose one <=1-engineering-day slice:

1. Test Author supplies authorized F/U/L goldens and W01–W12 boundary/invalid
   cases on the accepted revision, without loosening other frozen pins.
2. Builder adds checked timing at the shared rendering boundary, selects live
   before near, renders the approved three strings and threads safe suppression
   into existing preview/runtime callers. No external data dependency or service.
3. Prove same-input preview/runtime body parity, private error/no public send,
   no prompt/funnel success on invalid data, exact end/start behavior, duration
   1/60/90/1,440 plus rejected 0/1,441/u64::MAX, and the Eastern DST pairs.
4. Preserve every non-timing pin, run smallest relevant offline acceptance suites
   through the bounded Cargo wrapper, then same-head required CI/independent
   review and auto-merge. Stop/rescope if the helper/caller change exceeds one day;
   do not quietly widen this into onboarding hardening or a timing service.

The estimate includes one small core/caller change and authorized tests, excluding
CI/review latency. It is not an estimate for new scheduling or attendance work.
No staging or production test/migration is needed for this pure-copy slice; any
future rollout follows existing exact-artifact/staging/health/rollback gates.

## Disposition, review and verifiable handoff

**Host/CEO exact-copy intent: not obtained. Independent CISO/frozen-copy
disposition: not obtained.** Route the exact-revision decision brief through the
President/COO to the CEO and the established host intent path, never a new owner
approval hop. Bind repository commit, spec SHA-256 and task-document revision.
Ask explicitly about F/U/L bytes, schedule-only “scheduled for now”, duration
bounds, invalid-data suppression and unchanged frozen parts. CISO disposition
must cover frozen-copy scope and whether existing security/activation holds stay
unchanged; this document cannot waive them or infer acceptance from silence.
A queued request, docs merge or ordinary code review is not intent alignment.
Changed copy/semantics invalidate prior exact-revision disposition.

Repository spec is the source of truth; the task spec document is a revision-bound
handoff snapshot/link, not another independently edited contract. Stable scenario
IDs are W01–W12 and the reason IDs above. Review by 2026-11-02: metric is **12/12
scenarios explicitly accepted/revised/rejected, with recorded exact-copy intent
and independent disposition**. Without them the default remains A/no build; no
passive approval, build allocation or live experiment.

Runnable docs check (no compiler, network or database):

```sh
python3 scripts/check_docs_index.py --json
git diff --check
```

Expected docs result: `missing`, `stale`, `oversize` all empty; exit 0. These
checks validate discovery/formatting, **not the future renderer or actual event
activity**. Synthetic timestamps can be independently checked with Python's
standard `datetime`/`zoneinfo` using America/New_York. Future tests still need
execution; no Rust goldens or behavior tests were run for this proposal.

ACT: publish a source-backed proposed-only timing contract. WHY: separate
upcoming and scheduled-live welcomes without inventing activity or remaining
time. UNDO: revert the docs commit; runtime and live state were never changed.
