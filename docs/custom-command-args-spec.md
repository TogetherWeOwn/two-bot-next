# Custom commands: one bounded `{args}` substitution

Status: **proposed, design only**. Written 2026-10-24; review by 2026-10-31.
No runtime, command, test, migration, permission or configuration change is
included. Merging this document does not enable `{args}` or authorize build
work. This file is the repository source of truth; card documents bind its
commit and SHA-256 rather than maintaining a second editable contract.

## 1. Outcome and decision boundary

One saved response template can echo what the invoker typed (for example a
quote command: `Says {args}`) instead of needing a saved response per phrase.
The proposal adds exactly one literal, non-recursive, bounded `{args}`
substitution, shared across the slash, `!`-prefix and private preview paths.

Member-supplied text rendered into channel messages is an injection boundary
(mass mentions, role/user pills, link embeds, UTF-16 overflow), so this
contract is written from the security boundary inward: capture, bound,
neutralization, refusal, and content-free audit, before any behavior.

Recommendation rule for review (§10): proceed to build cards only if this
contract is provably injection-contained with a bounded test plan; otherwise
recommend no-build and stop. No live experiment either way.

## 2. Existing contracts and scope

Baseline: `0e904ee2f6cd17d5aa54fa35b2730154791ee1d3`.

- `crates/core/src/custom_commands.rs:19-25,94-177`: closed placeholder set
  (`user`, `username`, `server`, `channel`), definition-time
  unknown-placeholder refusal, 2,000-unit stored-template and rendered-output
  bounds, single-pass render. `{args}` is refused today as an unknown
  placeholder; the preview suite pins that refusal
  (`custom_commands.rs:1839`: `"{args}"` in the invalid-template cases).
- `crates/core/src/commands.rs:358`: custom commands publish with
  `options(Vec::new())` — slash invocations carry no arguments today.
  Advertise-plus-enforce precedent: `OCCURRENCE_ID_MAX_CHARS`
  (`commands.rs:56-58`): builders advertise `max_length`, runtime refuses.
- `crates/discord/src/custom_commands.rs:280-355`: slash run defers (public),
  renders, completes through the neutralizing path, and audits `command.run`
  ok/failed; routing refusals answer ephemeral with no audit row.
- `crates/discord/src/custom_commands.rs:382-495`: prefix path reads the first
  token only (`trigger_word`; arguments ignored), claims a durable
  `command.text_attempt`, renders, POSTs, and audits a separate
  `command.run` result. Render failure audits and sends nothing.
- `crates/discord/src/custom_commands.rs:675-691,858-870`: inspect
  `arguments` sample is read strictly (`single_string_option`:
  missing/malformed/value), validated to `MAX_INSPECT_ARGUMENT_UNITS` (500
  UTF-16 units), and never interpolated; preview completes without mention
  neutralization but with explicit empty `allowed_mentions`, and never
  records `command.run`.
- `crates/core/src/message_safety.rs:37-55`: `neutralize_mentions` breaks
  `@everyone`/`@here` (including zero-width-split forms); role/user pills
  stay readable while `allowed_mentions: { parse: [] }` disables parsing at
  the REST boundary (`crates/discord/src/executor.rs:34-37,1097`).

Out of scope: implementation, template-language features, additional
placeholders, recursion, cross-guild sharing, audit-content expansion,
production activation, spend. Adjacent saved-response edit/pause/clone/list
scopes are independent; this spec duplicates none of them.

## 3. Input contract

One bound, one capture rule per path, no normalization.

**Bound.** `MAX_ARGS_UNITS = 500` UTF-16 units (`encode_utf16().count()`),
matching the existing inspect-sample bound so the preview sample becomes the
preview input unchanged. The unit count is authoritative; byte length is
derived and total-bounded (worst case 500 BMP units × 3 bytes = 1,500 bytes),
so no separate byte check exists. The existing 2,000-unit rendered-output
check still applies after substitution and still refuses the whole render.

**Slash capture.** Every custom slash command publishes one optional string
option named `arguments` with `max_length: 500` (a client UX hint only; the
server-side unit count is authoritative). The option is read strictly with
`single_string_option` semantics: Missing → absent; duplicate or forged
non-string type → malformed. Precedence is fence, routing/gating, per-guild
definition lookup, then argument validation: unknown, deleted, disabled or
other-guild rows keep their existing silent/miss behavior and arguments are
never read for them (the custom slash path looks up before deferring so
unknown commands stay silent). Only for a known enabled row are arguments
read and validated; malformed/overflow input then receives the ephemeral
correction in §5 after one scoped store read. The inspect before-lookup
validation does not transfer here: inspect runs only after routing has
enforced guild/ManageGuild/feature gates for a known management command.
Absent and empty-string values substitute the empty string — templates
without `{args}` behave byte-identically, and there is no "required args"
concept.

**Prefix capture.** Remainder after the first token: `trigger_word` locates
the trigger, one run of leading ASCII whitespace after that token is skipped,
and the rest of the message content is the argument verbatim (interior
spacing, newlines and casing preserved). No remainder → empty string. Leading
whitespace before `!` still means "not a trigger", unchanged.

**Preview capture.** The existing inspect `arguments` sample option becomes
the preview input: same 500-unit bound, same malformed/overflow corrections
as today. The sample is the dry-run (§8); it is never sent or audited.

**Overflow and malformed handling.** Slash: private ephemeral correction
answered directly (never via the public defer), no audit row — a pre-execution
input refusal, same class as routing refusals. Prefix: the attempt is already
claimed, so overflow records a definitive failed `command.run` result with
reason `args_too_long` and sends nothing (no channel correction — failures
stay quiet, as render failures do today). Preview: the existing
`invalid_inspect_option_reply` corrections, unchanged.

## 4. Rendering and injection containment

- `{args}` joins the placeholder set as its fifth and final key; the
  unknown-placeholder refusal is unchanged for every other key. A template
  may repeat `{args}`; every occurrence receives the same value in the same
  single pass (existing multi-occurrence semantics — no new rule).
- Substitution is literal and single-pass: argument text is never re-scanned,
  so `{args}`, `{user}` or braces inside arguments render literally. No
  nesting, no second placeholder, no template-language expansion.
- New invariant for the build: run paths validate the template immediately
  before render, so a template that fails the new validation is refused
  instead of rendering a literal placeholder (for example imported or
  corrupt rows). Old binaries lack this check; their skew behavior is
  defined in §6.
- Channel-bound output (slash completion, prefix POST) keeps the existing
  double defense unchanged: `neutralize_mentions` over the rendered content
  plus `allowed_mentions: { parse: [] }` at the REST boundary. Arguments
  therefore cannot ping `@everyone`/`@here` (including zero-width-split
  forms); role/user/channel pills render as inert text. The safety claim is
  differential: argument text must receive byte-identical mention treatment
  to the same text arriving via any existing template input.
- Preview keeps inspect fidelity: Markdown-escaped, un-neutralized, with
  explicit empty `allowed_mentions`, ephemeral to the inspecting admin.
- Accepted residual: URLs in arguments auto-embed and Markdown renders in
  channel output, exactly as if the member had posted the text themselves;
  the bot performs no fetch and grants no mention that a member message
  would not carry.

## 5. Corrections

Closed reason codes with one `next_action` each; replies are bounded
private text (slash/preview) or silence (prefix). No reason, reply or audit
carries member content.

| Path | Condition | Member-visible result | Audit | `next_action` |
| --- | --- | --- | --- | --- |
| Slash | duplicate/forged `arguments` | Ephemeral `Invalid arguments: expected a single text value. Example: /quote arguments:hello` | none | `send_one_text_arguments_option` |
| Slash | args over 500 units | Ephemeral `Arguments must be at most 500 UTF-16 units. Shorten them and retry.` | none | `shorten_arguments_and_retry` |
| Prefix | args over 500 units | silence (no POST) | `command.run` failed, `args_too_long` | `shorten_and_resend` (documented; not posted) |
| Preview | malformed/over sample | existing `invalid_inspect_option_reply` text | never `command.run` | `fix_sample_and_preview` |
| Any run | rendered output over 2,000 units | existing render-failure behavior (`render_failed`) | existing failed fact | `shorten_template_or_arguments` |

`args_malformed` and `args_too_long` are the only new reason codes; every
other refusal reuses the existing vocabulary.

## 6. Guild isolation, restart and version behavior

- Guild isolation is unchanged: the configured-guild fence, per-guild row
  lookup and same-guild preview read all precede argument handling (§3
  precedence: fence, routing/gating, lookup, then argument validation),
  and arguments never influence lookup scope. One guild's arguments can
  never render into another guild's channel; unknown, other-guild-only,
  deleted and disabled rows keep their existing silent/miss behavior
  before arguments are read, including for malformed or over-long
  arguments (no validation correction for rows that miss).
- Arguments are request-scoped and never stored: no new table, column, queue
  or log field. A restart drops in-flight arguments; slash replay keeps
  first-write-wins audit (`custom:run:<interaction_id>`); prefix redelivery
  keeps `AlreadyAttempted` via the durable attempt claim. No new durable
  state means no new replay rule.
- Version: validation, render, publish and preview changes ship in one
  build. A new `{args}` template on an old binary renders the literal text
  `{args}` (old binaries lack the §4 pre-render validation check, ignore
  slash options and never read prefix arguments): no member content is
  involved, so the skew leaks nothing. Old templates (valid under the old
  four-key placeholder set) on a new binary behave byte-identically.
  Downgrade removes
  the slash option at the next full-registry republish; a stale client that
  still sends `arguments` is ignored by option-less binaries. No
  mixed-version fleet claim is made — see the unverified assumptions in §10.

## 7. Audit contract: facts, never content

| Path | Outcome | Audit row |
| --- | --- | --- |
| Slash validation refusal | malformed/overflow args | none (pre-execution, like routing refusals) |
| Slash run | delivered / failed | `command.run` ok / failed with existing reasons plus `render_failed` for args-induced render failure |
| Prefix overflow | refused before POST | `command.run` failed, `args_too_long`, id `custom:text:result:<message_id>` |
| Prefix run | delivered / failed | existing result facts; args change no field |
| Preview | any | never `command.run` (existing invariant) |

Audit rows carry only `guild_id`, `actor_id`, action, bounded `target_key`
(command name), outcome and reason code. Explicitly never recorded: argument
text, rendered content, template body, or any content-derived measurement
(no hashes, no lengths — there is no oracle or retention question to answer).
Erasure and retention manifests are untouched: with nothing stored, there is
nothing to add.

## 8. Deterministic offline fixture matrix

Not executed or implemented by this document. Synthetic IDs, clocks and
stores only; no member data, Discord send, or staging/production database.
Each ID is stable; a future Test Author owns the protected files after
approval and alignment.

| ID | Case | Deterministic oracle |
| --- | --- | --- |
| ARG-01 | slash, `arguments` absent | `{args}` renders as empty; template without `{args}` byte-identical to today |
| ARG-02 | slash, `arguments` empty string | same as ARG-01 |
| ARG-03 | slash, duplicate/forged `arguments` on a known enabled row | one scoped lookup precedes the refusal; ephemeral malformed correction; no audit row |
| ARG-04 | slash, exactly 500 units | renders; single delivery; `command.run` ok |
| ARG-05 | slash, 501 units on a known enabled row | one scoped lookup precedes the refusal; ephemeral overflow correction; no audit row |
| ARG-06 | slash, 250 emoji (500 units) vs 251 (502) | boundary obeys UTF-16 counting, not char count |
| ARG-07 | hostile mass mentions (`@everyone`, `@here`, zero-width-split forms) | byte-identical mention treatment to the same text via existing template inputs; no ping |
| ARG-08 | pills and links (`<@id>`, `<@&id>`, `<#id>`, `https://…`) | inert text under `parse: []`; URL embeds at most as a member message would |
| ARG-09 | non-recursion: args contain `{args}`, `{user}`, `{{args}}` | all render literally; output contains no substituted username |
| ARG-10 | args push render past 2,000 units | whole render refused (`render_failed`); no partial send |
| ARG-11 | prefix, no remainder / whitespace-only remainder | empty substitution; normal delivery |
| ARG-12 | prefix, remainder over 500 units | no POST; failed result `args_too_long` resolves the claimed attempt |
| ARG-13 | prefix, redelivered message | `AlreadyAttempted`; exactly one POST attempt ever |
| ARG-14 | preview sample over 500 / malformed | existing inspect correction; no `command.run`; no send |
| ARG-15 | preview renders `{args}` from sample | ephemeral escaped preview shows substituted sample; dry-run only |
| ARG-16 | slash replay, same interaction id | first-write-wins audit preserved; re-entry returns its own delivery outcome |
| ARG-17 | foreign guild / unknown / deleted / disabled row with well-formed, malformed and over-long args | existing silence/miss in all three; arguments never read; no validation correction |
| ARG-18 | `{args}` repeated in one template | every occurrence receives the same value in one pass |
| ARG-19 | slash, unknown row plus duplicate `arguments` option | existing silence/miss; no ephemeral correction; no audit row |

Runnable check now (no new target exists yet):

```sh
python3 scripts/check_docs_index.py --json
git diff --check
```

Future acceptance naming (reserved, not created): a
`custom_command_args_contract` target in the existing core/discord test
layout, owned by the Test Author after approval. A zero-test run is not
acceptance.

## 9. Agent and adapter contract (future)

- Stable fixture IDs `ARG-01…ARG-19`; closed reason codes
  (`args_malformed`, `args_too_long`, plus existing vocabulary);
  `next_action` strings from §5.
- Dry-run preview: `/command-inspect name:<name> view:preview
  arguments:<sample>` renders without sending, publishing or auditing.
- One repo source: this document. JSON manifest shape for a future fixture
  bundle (illustrative — no bundle ships here):

```json
{
  "contract": "custom-command-args/1",
  "fixtures": [
    {"id": "ARG-04", "path": "slash", "args_units": 500,
     "expect": {"outcome": "ok", "reason": null,
                "next_action": null, "audit": "command.run/ok"}}
  ]
}
```

## 10. Review assumptions, success/kill rule and undo

Unverified runtime assumptions for reviewers to accept or reject: Discord
`max_length` enforcement details (the server-side unit count is authoritative
regardless); stale-client option delivery across downgrade; pill/embed
behavior under empty `allowed_mentions` beyond the pinned executor tests;
single-version fleet (no mixed-version claim); and demand — one
administrator convenience (a quote command) with no member evidence.

Review by **2026-10-31**: proceed to build cards only if this contract is
provably injection-contained with a bounded test plan; otherwise recommend
no-build and stop. No live experiment in either case.

Before any build card, two gates in order: revision-bound review acceptance
of this spec, then recorded host intent alignment against the accepted
revision. A merged document, an intake receipt, or silence is not alignment.
No implementation, acceptance-test, migration or activation work before both.

ACT: propose one bounded `{args}` contract as an indexed design record. WHY:
one template echoing invoker text removes per-phrase saved responses without
opening a template language or a mention/overflow hole. UNDO: reviewed docs
revert only; no operational state changes.
