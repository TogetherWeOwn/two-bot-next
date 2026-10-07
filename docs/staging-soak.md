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

The approved policy specifies four ACTIVE hours, zero loss across all four event
families, and 120 expected one-minute samples in hours 3–4, but it does not
operationally define what makes an hour ACTIVE, the source or record shape of a
one-minute sample, or a sampling requirement for hours 1–2. Do not infer an
activity threshold, a `/readyz`-based definition, or an unapproved cadence. The
four-family zero-loss requirement remains in force throughout; the first-two-hour
sample rule is unspecified, not waived.

The existing documented evidence route is an offline seam, is not wired into the
running bot, and does not cover slash. Its bounded 15-minute packet example does
not identify a reviewed live source or schema for the required one-minute
samples. The GET-only smoke invokes no slash command and supplies no such
samples. No currently verified reviewed route establishes the four-family live
evidence or the required 120 samples. These gaps keep the acceptance **NOT
VERIFIED** and `T` unset; they do not authorize a new collector, receipt path,
origin, overflow path or fault method. The approved policy names no owner for
defining or authorizing a live sample method. The separate existing
technical-policy question remains pending with the CTO; the CEO-held fixture
authorization remains an execution hold. Neither is treated as the missing
method's owner or as resolved or widened by this document.

## Deployment interruption and recovery gap

The current staging deploy workflow documents a **95–139-second gateway drop
per deploy** ([workflow comment](../.github/workflows/deploy-staging.yml#L10)); a
separate historical staging note records a **92–139-second redeploy**
([boot window](discord-send-admission.md#boot-window)). Keep both labelled as
documented historical interruption ranges, not as a verified
outage-start-to-recovery measurement. They exceed the 60-second limit if the
planned redeploy interruption is in scope. The approved policy does not say
whether a planned redeploy is an “actual outage,” and names no owner for that
classification. The separate existing technical-policy question remains
pending with the CTO; the CEO-held fixture authorization is the execution hold,
not an interpretation of the recovery rule. Do not infer a classification or
assign a new owner here. Until the existing policy path resolves classification
and a qualifying under-60-second recovery is independently measured, these
redeploy exercises cannot establish a B2 PASS. Workflow finish-to-first-ready
remains distinct from outage-start-to-verified-recovery.

## Provisioning (operator, once)

Secrets are GitHub Actions secrets on the repo and Worker secrets per
environment. Names only — values never go in cards, logs, or PRs:

- `CLOUDFLARE_API_TOKEN` / `CLOUDFLARE_ACCOUNT_ID` — GitHub Actions secrets,
  used by `deploy-staging.yml`.
- `DISCORD_TOKEN` — Worker secret (`wrangler secret put --env staging`),
  the staging bot token.
- `GUILD_ID` — Worker secret (`wrangler secret put --env staging`),
  the TWO Staging guild (`1545644954272137297`).
- `DATABASE_URL` — Worker secret (`wrangler secret put --env staging`),
  the direct, unpooled URL for the bot's dedicated `two_bot` database and role
  on the Neon **staging** branch (`br-summer-sunset`, project
  `rough-dream-43123587`), separate from the web's `two` / `two_app`.
  Required for gateway checkpoint persistence; never use a production URL.
- `STAGING_WORKER_URL` — GitHub Actions **variable** (not a secret), the
  staging Worker URL, used by the `/readyz` gate. If it is missing the gate
  fails closed with instructions instead of deploying blind.

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
- Hours 3–4 require **120 expected one-minute samples**: one expected sample
  for each minute across those two hours. They are required evidence, not
  optional or best-effort. Do not invent or add a collector to produce them.
- Preserve every observation and gap in the approved record, if verified and
  available through existing work. A missed event fails zero-loss acceptance;
  missing evidence remains UNKNOWN. Record unready intervals and restarts, and
  evaluate them under the approved recovery criteria. Do not infer pause/reset
  semantics or erase, reclassify or silently reset historical evidence.

## Acceptance

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
  Both exceed 60 seconds if a planned redeploy is in scope; neither provides an
  exact outage-start-to-verified-recovery measurement. Classification remains
  unresolved as above.

The approved exercise set—three redeploys, two drops, Neon idle-hit and actual
429 categories—still requires exact reviewed budgets, an independent stop and
restore arrangements before execution. This documentation does not authorize
live faults, load, dispatch, or any new evidence/fault mechanism.

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
  without **both** `reason` and `approver`, or any Markdown drift fails the
  gate.

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
