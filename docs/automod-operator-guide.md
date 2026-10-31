# Automod operator guide

What automod does to a message, what it never does, and where to look when
it does nothing. Deep proofs live in the linked docs; this page is the
map over them.

Safety boundary first: automod inspects only the staging guild unless an
explicit externally supplied live approval exists. It changes nothing about
that scope. Dry-run is the default; only exact `TWO_AUTOMOD_ENFORCE=1`
enforces. A deployment value other than `0` or `1` stays dry-run at boot
and logs one warning. Boot also refuses to start with an invalid automod
configuration, including a missing Owen id, rather than running
unmoderated. See the [runtime handoff](automod-runtime.md) for the full
contract.

## Filters: what trips, in what order

`AutomodFilter` (`crates/core/src/automod.rs:39`) has six reasons, evaluated
in legacy order (`crates/core/src/automod.rs:37`):

| Order | Filter (`as_str`) | What it matches | Main knobs |
| --- | --- | --- | --- |
| 1 | `bad_words` | Configured phrase, NFKC + lowercase, obfuscation-tolerant | `TWO_AUTOMOD_BAD_WORDS` |
| 2 | `repeated_message` | N identical normalized messages from one author inside the window | `TWO_AUTOMOD_REPEAT_COUNT` (2–20, default 3), `TWO_AUTOMOD_REPEAT_WINDOW_SECONDS` (1–3600, default 30) |
| 3 | `mention_spam` | Explicit mentions at/above threshold | `TWO_AUTOMOD_MENTION_LIMIT` (1–50, default 5) |
| 4 | `invite_link` | Invite schemes and Discord host variants | — |
| 5 | `external_link` | Bare/non-allowlisted hosts | `TWO_AUTOMOD_ALLOWED_DOMAINS` |
| 6 | `attachment_type` | Blocked extensions (lowercase, no leading dot) | `TWO_AUTOMOD_BLOCKED_ATTACHMENT_EXTENSIONS` (11 defaults) |

Order matters for operators reading a verdict:

- A `bad_words` hit returns **before** the repeat observation, so it is
  neither counted toward the repeat total nor treated as a break in the
  streak.
- Messages that a *later* filter matches (invite, mention spam, attachment)
  **are** observed first. Copies below the configured
  `TWO_AUTOMOD_REPEAT_COUNT` threshold report the later filter, and the copy
  that reaches the threshold reports `repeated_message` (copies 1–2, then
  copy 3, at the default threshold of 3), as in legacy
  `["invite_link","invite_link","repeated_message"]`.
- Blank content (empty or whitespace-only after normalization) never counts
  and never breaks a streak.
- Exemptions skip inspection: `TWO_AUTOMOD_BYPASS_ROLE_IDS` (roles) and
  `TWO_AUTOMOD_EXEMPT_CHANNEL_IDS` (channels). All keys, defaults and
  hot/cold classes are in [configuration](configuration.md#settings-catalog).

## Effects: the sanction ladder and what each outcome means

`AutomodEffect` (`crates/core/src/automod_runtime.rs:425`) is the only
vocabulary automod acts in:

- `DeleteMessage` — delete the exact matched message, always first.
- `WarnMember` — the counted ledger row **is** the warning; Discord gets no
  call (`crates/discord/src/automod_activation.rs:316`).
- `TimeoutMember { seconds }` — follows a *confirmed* delete; default 600 s.

The rung comes from `sanction_for`
(`crates/core/src/automod.rs:136`): the highest rung whose threshold is at
or below the violation count (count clamped to ≥ 1). The default ladder
(`crates/core/src/automod.rs:114`) is 1 → delete, 2 → warn, 3 → timeout
600 s; `TWO_AUTOMOD_SANCTIONS` replaces it (must start at violation 1,
unique integer thresholds 1–100).

`PlanOutcome` (`crates/core/src/automod_runtime.rs:432`) tells you why a
plan did what it did:

| Outcome | Meaning for the operator |
| --- | --- |
| `Ready` | Enforceable plan. Execution attempts the delete and any permitted follow-up sanction. |
| `DryRun` | Match recorded capture-only; no target fetch, no count, no mutation. |
| `AlreadyProcessed` | `(guild, message)` already counted; no effects, no second mutation. |
| `Unavailable` | Enrichment, claim or target facts failed; nothing counted or sent. |
| `Protected(reason)` | Owner, Owen, bot or staff-role target: counted per legacy, zero mutations. |
| `SanctionRefused` | Warn/timeout permission or hierarchy refused. The plan still includes the delete; execution attempts it, but no ladder step follows it. |

Two refusal shapes look alike but differ: `Protected` means the *target*
is off-limits (nothing sent at all); `SanctionRefused` means the *bot*
lacked permission or role height (delete attempted, ladder stopped). Both
settle normally with a typed receipt; neither needs reconciliation.

## Repeat windows and expiry: the operational summary

Full behavior, with the pinned edge cases, is in
[repeat-window acceptance](automod-repeat-expiry.md). What an operator
needs:

- The window cutoff is **inclusive**: with count 4 and a 10 s window, the
  fourth copy landing exactly at the edge trips `RepeatedMessage`; 1 ms
  later it does not.
- The tracker key is `guild:author`. Repeats never pool across authors or
  guilds.
- Expiry runs on the `automod_expiry` job every 60 s
  (`crates/bot/src/automod_gateway.rs:44`) and on CREATE inspection using
  the message clock. It is guild-wide and drops only rows older than the
  window; one row exactly `window` old survives.
- Only CREATEs enter or prune repeat history. Edits re-inspect all other
  filters against CREATE history but record, prune and advance nothing;
  they never award XP.

If repeats seem under-counted after a restart or traffic spike, suspect a
swept tracker plus a preserved-decision replay (same-revision retries replay
the stored IDs/reason code instead of re-running the tracker), not a lost
message.

## Corpus management

The offline golden corpus is the change-detector for matcher behavior:
fixture `crates/core/tests/fixtures/automod_corpus.json`, runner
`crates/core/tests/automod_corpus.rs`. Full inventory rules, the deferred
families and the real-adapter proof map are in
[automod-corpus](automod-corpus.md); read that doc before touching the
fixture.

Operator-relevant rules:

- Add or change a case by editing the fixture JSON with its legacy
  source reference; the coverage test fails if a row loses its mapping,
  so a removed row must go with its mapping, not silently.
- Deferred cases are preserved legacy expectations with a tracking issue,
  not waivers for activation. Green corpus CI proves only the executable
  core expectations; it does not authorize live activation.
- Never copy real guild/member/role/channel IDs, tokens or message content
  into the fixture. Bad phrases, messages, IDs and hosts are synthetic;
  the scrubbed native-rule record keeps behavioral fields only.

## Failure signals and triage

Per-delivery results are `ActivationOutcome`
(`crates/discord/src/automod_activation.rs:74`): `Bypassed`, `Duplicate`,
`Unavailable`, `Completed` (typed receipt; `deleted` is a confirmed REST
result, never an intention) or `Retained`. A `Retained` claim waits for
recorded reconciliation; nothing is retried, leased or resent automatically.
Failed strict Discord facts reads have the sampled `automod_facts_read_failed`
warning below as their `Unavailable` signal. A 404 or incomplete snapshot stays
silent, so not every unavailable result has a matching warning.

| Signal | Meaning | Triage |
| --- | --- | --- |
| `automod_config_invalid` (`gateway_failure`) | Boot rejected the automod environment | Check `TWO_AUTOMOD_*` values and `TWO_OWEN_USER_ID`; fix config, restart. Never bypass the gate. |
| `automod_executor_failed` (`gateway_failure`) | REST executor failed to build | Check executor/token/network configuration; no Discord mutation was attempted. |
| Boot warning `TWO_AUTOMOD_ENFORCE is set to a value other than 0 or 1` | Deployment value is neither `0` nor `1`; dry-run in effect | Set the deployment value to exactly `0` or `1`. The "automod activation wired" line (`crates/bot/src/main.rs:643`) reports the boot-resolved `mode` (`dry_run`/`enforce`). |
| WARN `automod_facts_read_failed` with fixed `step` and `cause` | A strict facts read failed; automod keeps the existing `Unavailable`/fail-closed behavior. `step` is `guild`, `bot_member`, `target_member` (also enrichment's author membership) or `message`. First failure then at most one line per 60 s per resolver across all steps/guilds; not a per-delivery count. | For `rejected`, check bot permissions/access; for `rate_limited`, inspect Discord limits; for `unavailable`/`timeout`, inspect upstream/network health; for guard causes, inspect shared executor guard state. No retry or live activation is authorized by this signal. See the [event catalog](observability-event-catalog.md#automod-facts-read-warnings) for fields and sampling limits. |
| `Retained(UncertainDelete)` | Delete response uncertain; no follow-up sanction sent | Recorded reconciliation case. Do not resend the sanction by hand; reconcile through the claim ledger. |
| `Retained(UncertainTimeout)` | Delete confirmed, timeout uncertain | Same as above; the delete happened, the timeout is unknown. |
| `Retained(Ledger)`, `Retained(CompletionRefused)` | Store write failed after possible counting | Inspect the claim ledger and DB health; duplicate suppression favors no repeated sanctions. |
| `Retained(FenceRefused)`, `Retained(PreserveRefused)`, `Retained(PlanUnavailable)` | Mutation fence, pre-count preservation or planning refused | Nothing was sent. Check store role grants (migrations `0220`–`0223` matrix) and the plan outcome; fail-closed is intended. |
| `Completed` with `SanctionRefused`, `deleted: true` | Warn/timeout refused: bot role too low or missing `MODERATE_MEMBERS` | Raise the bot's role above the target and grant moderation permission; the delete succeeded. |
| `Completed` with `SanctionRefused`, `deleted: false` | Delete rejected before mutation (permission, guard paused/circuit open); no ladder step | Check `MANAGE_MESSAGES` in the channel and executor guard state; the message was not deleted. |
| Expected message untouched, outcome `Bypassed` | Outside the fence (wrong guild, DM, unapproved identity) | Verify the staging guild scope and activation approval; flags alone never widen scope. |
| Expected message untouched, `Completed` with `DryRun` (matched) | Dry-run mode: matched, captured, nothing counted or sent | Check the boot-resolved `mode`; set `TWO_AUTOMOD_ENFORCE` to exactly `1` through the authorized procedure. |
| Expected message untouched, clean `Accept` | Exempt channel/role, automod disabled, or no filter matched | Check `TWO_AUTOMOD_BYPASS_ROLE_IDS`, `TWO_AUTOMOD_EXEMPT_CHANNEL_IDS` and the filter knobs. |
| `two_bot_gateway_prefix_trigger_refused_total{reason="verdict"}` rising | Text automations refused by capture-only verdicts | Expected while automod holds a message capture-only; matched, unavailable, timed-out and uninspected creates all refuse triggers. See the [observability catalog](observability-event-catalog.md). |
| `automod_expiry` job `failure` / `job_consecutive_failures` | Repeat-history expiry tick failing | Check gateway logs and the 60 s supervisor. Idle authors keep stale in-memory rows; verdicts are unaffected because every CREATE re-expires and the window check ignores rows outside the cutoff. |

General rules: never infer REST success from a plan; an uncertain response
is retention, not success. A crash after counting but before sending is a
reconciliation case, not proof Discord acted. Do not issue SQL against
staging/live services, switch tokens, or redeploy with an unreviewed wiring
change from this guide; use the authorized deployment/containment procedure
(the [runbook](runbook.md#containment-kill-switches-and-feature-flags)
owns stop boundaries).

## Verification commands

Docs-only change: run the checks that cover it, and report only runs seen.

```sh
cargo fmt --all -- --check
python3 -m unittest discover -s scripts -p 'test_cutover_doc_links.py' -v
```

Deeper behavior proofs belong to their owning docs; run the suite your edit
touches (hosted CI runs the same files):

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test automod_corpus -- --nocapture
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test automod_repeat_expiry
```

On the persistent controller, compiling Cargo commands go through the
cache wrapper from your own workspace (see [build-cache](build-cache.md));
`cargo fmt` does not compile and runs directly. Never point tests at
staging or production data; the corpus and repeat suites need no database,
Discord request, network or wall clock.
