# Staging-guild soak (B2 acceptance)

Four **ACTIVE hours** of the Rust bot on Cloudflare Containers against **TWO
Staging** with the **staging bot identity**. This replaces seven passive days
and the passive 60-minute pre-entry wait only. It does not waive any acceptance,
security, release or production gate. The production bot stays on Coolify; this
soak never touches the production guild or production credentials.

## Preconditions (not this card)

`T` remains **unset** until the qualifying preconditions are independently
verified through the existing reviewed paths. The current deployed build,
health, bindings, migration/ACL state and live outcomes are **NOT VERIFIED**;
source documentation or historical evidence is not a substitute.

- Confirm the pinned staging build and a live, ready gateway. A parked
  supervisor or the first `/readyz` 200 alone does not qualify.
- Verify the existing staging bindings, database identity, migration and ACL
  state using the authorized read workflow. Do not test or migrate against
  staging from this documentation task.
- Verify an existing reviewed evidence path for every required event family:
  joins, voice, messages and slash. The current source-level receipt seam is
  not wired into the running bot and does not establish live coverage.
- Keep zero loss, actual outage-start-to-verified-recovery under 60 seconds,
  flat memory and no error spikes as acceptance requirements. Numeric RSS and
  error definitions remain unaccepted and are not criteria here.
- Security, release and production gates remain in force. No new collector,
  receipt/origin/overflow path or fault method is authorized by this runbook.

## Unresolved acceptance terms and evidence gaps

Four ACTIVE hours is the **conservative retained envelope pending a justified
replacement**, not clock acceptance or a supported shorter equivalence. ACTIVE
means the exact qualified build/process/environment has actual representative
expected-versus-processed joins, voice, messages and slash, affirmative coverage
of every family and zero loss. Empty, idle or health-only denominators cannot
pass. Keep one-minute cadence from entry through completion, including hours
1–2. Hours 3–4 retain **120 distinct mature minute-spaced temporal samples**;
rapid polling cannot substitute for temporal coverage.

Each sample must identify build/process/environment, UTC bounds, expected,
processed or no activity per family, readiness, gaps/outages, RSS/errors/reset
semantics and reviewed provenance. No activity must be explicit; it is not
all-family coverage. This describes required evidence, not a new collector.

The existing evidence seam is offline, not wired into the running bot and omits
slash. Its bounded 15-minute packet and GET-only smoke do not establish the
required live source. Numeric memory/RSS growth/headroom/error-spike criteria,
an approved all-family live sample producer/fixture executor and a long-use B2
grant remain **unsupported**. B2 stays **NOT VERIFIED**, `T` **unset**, B4
**NO-GO**. These are current gaps, not an unnamed historical CTO question.
Engineering Lead owns technical disposition and Chief of Staff continuation;
neither is falsely appointed the live qualification executor. Existing lifecycle,
shared-database rehome, credential and incident holds retain their owners and
release conditions; this source correction neither renews historical dates nor
creates a duplicate activation/data/fixture tree.

### Bounded event-read evidence contract

The existing [processed-event collector](evidence-route.md) strictly parses
psql CSV and rejects malformed quoting, impossible UTC instants and rows outside
its inclusive SQL `BETWEEN` window before writing any artifact or step summary.
That check includes the 61st returned truncation witness; the artifact validator
also checks real, ordered window bounds (at most 15 minutes) and every kept row.
Rows retain SQL ID order, not chronological order. Zero rows are a valid read,
not proof of zero loss; more than 60 rows retains `truncated: true` and UNKNOWN
coverage. The offline acceptance matrix is
`scripts/test_staging_events_read_acceptance.py`. Passing it proves this bounded
contract only: it neither authorizes a new live read nor closes the four-family,
ACTIVE-hour or one-minute-sample evidence gaps above.

## Deployment interruption and recovery gap

The current staging deploy workflow documents a **95–139-second gateway drop
per deploy** ([workflow comment](../.github/workflows/deploy-staging.yml#L10)); a
separate historical staging note records a **92–139-second redeploy**
([boot window](discord-send-admission.md#boot-window)). Keep both labelled as
documented historical interruption ranges, not as a verified
outage-start-to-recovery measurement. **Interrupting planned redeploys count as
actual outages by default**. Those historical ranges exceed the under-60-second
limit if observed in an attempt; they are warnings, not waivers. Independently
measure every actual outage; deploy-finish-to-first-ready remains a separate
workflow interval. These redeploy exercises cannot establish B2 PASS without
qualifying evidence and separate exercise authority.

## Provisioning (operator, once)

Secrets are GitHub Actions secrets on the repo and Worker secrets per
environment. Names only — values never go in cards, logs, or PRs:

- `CLOUDFLARE_API_TOKEN` / `CLOUDFLARE_ACCOUNT_ID` — `staging` Environment
  secrets (no repository-scope copies), used by `deploy-staging.yml`.
- `DISCORD_TOKEN` — Worker secret (staged with `wrangler versions secret put
  --env staging DISCORD_TOKEN --message <sha>`, then `wrangler versions deploy --env staging`),
  the staging bot token.
- `GUILD_ID` — Worker secret (staged with `wrangler versions secret put
  --env staging GUILD_ID --message <sha>`, then `wrangler versions deploy --env staging`),
  the TWO Staging guild (`1545644954272137297`).
- `DATABASE_URL` — Worker secret (staged with `wrangler versions secret put
  --env staging DATABASE_URL --message <sha>`, then `wrangler versions deploy --env staging`),
  the direct, unpooled URL for the bot's dedicated `two_bot` database and role
  on the Neon **staging** branch (`br-summer-sunset`, project
  `rough-dream-43123587`), separate from the web's `two` / `two_app`.
  Required for gateway checkpoint persistence; never use a production URL.
- `STAGING_WORKER_URL` — GitHub Actions **variable** (not a secret), the
  staging Worker URL, used by the `/readyz` gate. If it is missing the gate
  fails closed with instructions instead of deploying blind.

`<sha>` above is the live `/readyz` `build_revision` of staging: a plain
`wrangler secret put` publishes an unstamped version that can never form a
rollback receipt (see `docs/production-deploy.md`). Precheck first:
`versions secret put` builds on the newest-created version, not necessarily the
serving one — confirm the last block of `wrangler versions list --env staging`
(the list prints oldest first, so the last block is the newest version)
is the serving version (same id as `wrangler deployments status --env staging`),
and if they differ redeploy the intended SHA through the workflow first (full rule
in `docs/production-deploy.md`).

## Bounded staging use

The [no-always-on staging rule](staging-rollout-gate.md#no-always-on-staging-rule-and-activation-hold)
applies to the soak too. Reserve a finite pipeline-owned use window covering
startup, every required ACTIVE hour and cleanup. Independently verify the exact
SHA/environment and readiness before starting qualification. Keep native runtime
expiry as the runner-loss backstop; release on success, failure and cancellation.
Inventory DO alarms, Containers, database compute/suspend and shared consumers;
a stopped bot alone is not zero idle cost. Policy activation is by merge through the
[roll-forward bootstrap](staging-rollout-gate.md#first-activation-roll-forward-bootstrap);
this does not record activation or satisfy any unverified precondition above.

**No long-use grant means no start.** The original 40-minute automatic-promotion
holder does not authorize four-hour B2 use. Before any separately authorized
attempt, predeclare UTC `T`, `T+4h` as the stop for an incomplete current attempt,
and a startup/cleanup-inclusive finite long-use grant. `T+4h` grants no runtime
by itself and must fit inside that separate authorization. Native expiry is the
backstop, not permission to renew the attempt.

Expiry, an evidence gap, restart or stop during required ACTIVE evidence is an
interruption, not a passing sample or implicit pause/reset. At the boundary,
incomplete evidence is **INSUFFICIENT/FAIL**, never PASS by timeout: safely
release or expire, preserve the failed/UNKNOWN interval, hold qualification and
route the exact missing proof to its existing technical/continuation owner. Do
not silently restart the clock, extend a window indefinitely or disable normal
health/security/production monitoring. Rollback to indefinite always-on staging
needs a dated owner cost exception; it is not the default recovery route.

## Starting the soak

1. Keep the normal release and staging deployment gates. This document does
   not authorize a merge, deployment, live action or production access.
2. Through the existing reviewed paths, independently verify the deployed
   revision, health, bindings, migration/ACL state and live evidence routes for
   joins, voice, messages and slash. These facts remain **NOT VERIFIED** until
   that evidence exists.
3. Set `T` only in the existing approved soak record, if its availability and
   route are verified, after every qualifying precondition and applicable
   authorization has passed. Do not use the first `/readyz` 200 or a passive
   wait as `T`; if any fact or the record path is unknown, leave `T` unset.

## During the four ACTIVE hours

- Keep the four event families in scope throughout: joins, voice, messages and
  slash. Compare expected and processed outcomes through the existing reviewed
  evidence route; visible guild activity alone does not prove bot processing.
- Sample every minute from entry through completion. Hours 3–4 require
  **120 distinct mature minute-spaced temporal samples**: one expected sample
  for each minute across those two hours. They are required evidence, not
  optional or best-effort. Do not invent or add a collector to produce them.
- Preserve every observation and gap in the approved record, if verified and
  available through existing work. A missed event fails zero-loss acceptance;
  missing evidence remains UNKNOWN. Record unready intervals and restarts, and
  evaluate them under the approved recovery criteria. Do not infer pause/reset
  semantics or erase, reclassify or silently reset historical evidence.

## Acceptance

Exit only on the complete evidence below, with the independent preconditions
and resource criteria accepted. Idle elapsed time is never sufficient; the
bounded stop/failure rule above remains mandatory for an incomplete attempt.

- Four fully evidenced **ACTIVE hours** after qualifying `T`, with all four
  event families covered and **zero missed events**.
- Every actual outage must recover in **under 60 seconds**, measured from the
  actual outage start to verified recovery. Deploy-finish-to-first-ready is a
  separate workflow interval and cannot substitute for this measurement.
- Flat memory and no error spikes remain required. Numeric RSS/error criteria
  are not accepted by this policy.
- Hours 3–4 include all **120 expected one-minute samples**; missing or
  unverified samples mean the evidence is incomplete.
- Keep the **95–139-second documented per-deploy gateway-drop range** and the
  **92–139-second historical redeploy record** labelled historical/estimated.
  Both warn of a breach for an interrupting planned redeploy; neither provides an
  exact outage-start-to-verified-recovery measurement — measure it offline with
  `scripts/gate5_outage.py` (Gate 5 in
  [soak-entry-gates.md](soak-entry-gates.md)), which reads a committed JSONL
  tick log; there is no live producer. An offline pass verifies offline
  reconciliation only; it does not prove a deployed build, live event coverage
  or staging acceptance. Interrupting planned redeploys count as actual outages by default; the live
  measurement/source gap remains unresolved, not their classification.

The approved exercise set—three redeploys, two drops, Neon idle-hit and actual
429 categories—still requires exact reviewed budgets, an independent stop and
restore arrangements before execution. This documentation does not authorize
live faults, load, dispatch, or any new evidence/fault mechanism.

## Offline gateway evidence field contract

`scripts/soak_evidence.py` reconciles synthetic JSONL fixtures only. Dispatch
`seq` must be a nonnegative integer, excluding booleans, nulls, strings,
floats, arrays and objects. A supplied resume `seq` follows the same rule;
only omission permits a sequence-less resume, retaining the previous baseline.
Invalid sequences produce fixed UNKNOWN evidence without updating counters,
sequence baselines, the accepted timestamp window or pending outages. A
malformed resume cannot close a disconnect; a later valid recovery measures
from the original disconnect, and an unrecovered outage remains open UNKNOWN.

Only `connect`, `disconnect`, `resume`, `dispatch` and `unknown` event kinds
are accepted. Missing or unrecognised kinds remain UNKNOWN evidence but cannot
advance the accepted timestamp window or make later valid recovery appear
regressed. In the committed sample, `heartbeat_oops` at 00:00:26 remains an
UNKNOWN interval; the accepted window ends at the explicit `unknown` record's
event timestamp, 00:00:22, not its interval endpoint or the rejected event.

Event timestamps and explicit UNKNOWN start/end timestamps must normalize to
representable UTC datetimes. UTC underflow/overflow becomes bounded malformed
evidence, not a traceback or a diagnostic containing the rejected value;
following valid records are still processed. Input is not sorted. Zero and
positive sequences, ordered duplicates, equal timestamps and legitimate session
resets retain their existing behavior.

Run the bundled summarize/CLI matrix offline:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p '*soak*evidence*.py' -v
```

An offline PASS is only parser reconciliation, not a live soak verdict or proof
of deployment. Empty or dispatch-free input still refuses PASS, and the existing
under-60-second outage budget and all live acceptance requirements above remain
unchanged.

## Waiver ledger (B4 parity sign-off)

Cutover preconditions ([cutover.md](cutover.md)) require every non-DROP
parity behavior to have merged wiring plus acceptance evidence, **or an
explicitly approved waiver**. Writing a waiver down is not approval: a
`waived` checklist entry is a *proposed* waiver of staging execution only.
Parity coverage stays, and the row remains unresolved (`NEEDS WORK`) until
B4 records acceptance — reason, accepting actor and the owning slice's
isolated fixture evidence — on [TOG-9699](/TOG/issues/TOG-9699).

The ledger is machine-checked, not prose:

- Authoritative: `docs/soak-checklist.json` — one entry per non-DROP parity
  row in `docs/parity.md` §§1–8 (pure `DROP` rows are not ported, so they
  carry no soak obligation; mixed mapped/`DROP` rows stay covered), per §12
  addition under the same rule, and per non-`dropped` §13 ledger row
  (history-rewrite replays are `dropped`). §12/§13 entries name their
  `owner` cards, which must match the cards in the parity disposition.
- Rendered: `docs/soak-checklist.md` (regenerate with
  `python3 scripts/check_soak_checklist.py --render` after editing JSON).
- Gate (offline, runs in CI before Cargo):
  `python3 scripts/check_soak_checklist.py` plus
  `python3 -m unittest discover -s scripts -p 'test_soak_checklist.py' -v`.
  Any unmapped row, any missing or stale owner card, any `waived` entry
  without **both** `reason` and `approver`, any operative `scripts/` path in a
  checklist entry that does not exist (unless the entry lists it under
  `historical` with a reason), or any Markdown drift fails the gate.

Every `waived` entry carries `reason` and `approver`. The first waivers,
filed under [TOG-12140](/TOG/issues/TOG-12140), cover the 25 rows staging
cannot exercise — data-plane/operator paths (backup timers, DB stores,
internal actions, Postgres guards) and the DROP-adjacent rows
(`community_facts` with its dropped rota extensions, operator-script
runtime drops) — all with `approver: pending — CEO/DoE acceptance on
[TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)`.

Accepting a waiver means replacing `pending` with the accepting actor, the
decision reference and the attached fixture receipt. Moderation,
automation, scheduled unbans and internal actions cannot be silently
waived: their entries still require that explicit acceptance plus the
owning slice's fixture evidence — until then they stay `NEEDS WORK`.
