# B2 soak entry gates — four ACTIVE hours

This doc **defines** the gates; it does not execute them. The approved policy is
four ACTIVE hours, replacing seven passive days and the passive 60-minute
pre-entry wait only. No other acceptance, security, release or production gate
is waived. The current deployed build, health, bindings, migration/ACL state
and live outcomes remain **NOT VERIFIED**; `T` stays unset until qualifying
preconditions are independently verified through existing reviewed paths.

Companions: [staging-soak.md](staging-soak.md) (lifecycle and acceptance),
[evidence-route.md](evidence-route.md) (expected/processed collection),
[container-readiness.md](container-readiness.md) (keepalive monitoring),
[gateway-recovery.md](gateway-recovery.md) (session persistence),
[migrations.md](migrations.md) (numbering and lock guard).

## Fence (applies to every gate)

- Staging only: the TWO Staging guild and the staging bot identity. The
  production bot and production guild are never touched.
- No tests, SQL, probes, migrations, imports, backup or restore against
  staging/production databases, except the single read-only query that the
  `staging-events-read` workflow runs ([evidence-route.md](evidence-route.md)
  step 2).
- After any credential denial: stop, record the exact error and owner, never
  substitute another credential.

## Gate 1 — container healthy at a pinned SHA

The staging deployment is the `main` head it claims to be, and the process
answers truthfully.

| Check | Verification command | Expected value |
|---|---|---|
| Deploy green | `gh run list --workflow deploy-staging.yml --branch main --limit 1` | `conclusion: success`, head SHA recorded as the pinned SHA. Docs-only merges start no run, so `main` may be ahead of the pinned SHA by docs-only commits; confirm with `git diff --stat <pinned>..origin/main` that nothing outside the `deploy-staging.yml` `paths-ignore` list changed |
| Liveness | `curl -s -o /tmp/health.json -w '%{http_code}' "$STAGING_WORKER_URL/health"` | `200`, body `{"status":"ok"}` |
| Readiness | `curl -s -o /tmp/readyz.json -w '%{http_code}' "$STAGING_WORKER_URL/readyz"` | `200` with `components` `[["process","ready"],["gateway","ready"],["database","ready"]]` (plus informational `jobs`) |

`503` means the gateway is parked or starting — the gate is closed, not broken.
Any other status (500/1101/…) means the deploy is broken: file a repair card,
do not start the clock. `/health` 200 alone never passes this gate.

## Gate 2 — qualify entry (no passive wait)

There is no fixed passive wait before **T**. Passing Gate 1 or observing a
healthy interval does not by itself qualify entry. Verify the preconditions in
[staging-soak.md](staging-soak.md) through the existing reviewed paths: pinned
build and health, current bindings, database identity, migration/ACL state, and
an existing reviewed evidence path for joins, voice, messages and slash. A
missing, failed or unknown fact keeps the gate closed and `T` unset.

The approved policy does not operationally define an ACTIVE hour, the source or
record shape for its required one-minute samples, or a sample cadence for hours
1–2. The existing evidence seam is offline, is not wired into the running bot,
and omits slash; no currently verified reviewed route supplies the required
four-family live evidence and 120 samples. These terms and sources remain NOT
VERIFIED, so this gate stays closed. The approved policy names no owner for
defining or authorizing a live sample method. The separate existing
technical-policy question remains pending with the CTO; the CEO-held fixture
authorization is an execution hold, not a definition of the missing terms. No
new owner or evidence method is assigned here.

The fixed 60-minute spacing is retired; there is no elapsed-time requirement or
replacement timed window. Preserve the former window's non-timing stability
checks between the Gate 1 readiness probe and the final pre-`T` probe:

| Check | Verification | Expected value |
|---|---|---|
| No readiness alerts | Worker-log query for `container_unready_alert` between the two probes | `0`; a `container_unready_recovery` without a preceding alert is fine |
| No deployment | Existing authorized workflow execution-attempt verification for `main` over the recorded probe interval, including reruns and executions spanning either probe | `0` deployment executions; missing or incomplete attempt evidence closes the gate |
| Final readiness | Same `/readyz` curl as Gate 1, before writing `T` | `200` with all three components `ready` |

Record the Gate 1 and final readiness probe timestamps as UTC in `GATE1_UTC`
and `FINAL_UTC`. This paginated, explicitly read-only listing discovers
main-branch runs for the staging workflow; it is **not** a no-deployment proof:

```sh
gh api --method GET --paginate repos/TogetherWeOwn/two-bot-next/actions/workflows/deploy-staging.yml/runs -f branch=main -F per_page=100
```

Do not count only runs whose `created_at` falls between the probes: an older run
can be rerun during that interval, and an execution spanning a probe can deploy
inside it. Require independently verified execution-attempt and overlap coverage
through the existing authorized read path before recording zero deployments.
A successful listing or zero newly created runs is not that coverage. Any
query failure, deployment execution or missing/incomplete attempt evidence
closes Gate 2 and keeps `T` unset. No new attempt collector, verification route
or execution authority is supplied by this document.

The alert query counts only emitted `container_unready_alert` records. Short
failure streaks can recover below the configured threshold without an alert or
recovery log ([readiness runbook](container-readiness.md)); an empty alert query
does not prove there were no brief unready streaks.

Any alert, deploy or non-200 probe closes Gate 2 until the cause is resolved
and the Gate 1 preconditions pass again. Re-run the readiness checks; do not
start a new fixed-duration window. Do not treat elapsed time, a first `/readyz`
200, source-level fixtures, historical packets or this document as proof of
live coverage. The existing protected-binding and migration/ACL verification
work remains required; this page creates no duplicate route or task.

## Gate 3 — migrations applied to the pinned head

Source lock and staging database agree that every migration through the pinned
head is applied. Agents never run schema changes; the staging ledger is
reconciled through the authorized migration-verification path.

| Check | Verification command | Expected value |
|---|---|---|
| Lock clean | `PYTHONDONTWRITEBYTECODE=1 python3 scripts/check-migrations.py --base-ref origin/main` | exit `0` |
| Staging applied | operator-held migration-verification receipt for the staging `two_bot` database | applied head equals the lockfile head (re-derive from `migrations.lock` at execution time; do not hard-code) |

The checker proves source numbering/immutability only — it never queries
staging. A missing migration (e.g. a gap like the previously reported 0123 /
0200 / 0210 / 0331–0334 / 0351 family) closes the gate until the authorized
path applies it and produces a receipt. Routing evidence is not completion
evidence.

## Gate 4 — existing evidence record

Use only the existing approved soak record and reviewed evidence route. This
gate does not create a new receipt schema, collector, origin or overflow path.

| Check | Verification | Expected value |
|---|---|---|
| Existing record | Inspect the approved soak record through its existing board view | It must preserve expected/processed evidence and gaps for all four families plus pinned deployment identity; whether it currently supports this is **NOT VERIFIED** |
| Offline packet tooling | Existing offline evidence test suite on the pinned SHA | It must pass before packet tooling is used. If absent or failing, record tooling **NOT VERIFIED** and keep `T` unset. A pass verifies offline reconciliation only; it does not prove a deployed build, live event coverage or staging acceptance. |

Record `T` in the existing approved record only after Gates 1–4 and every
qualifying precondition pass, and only if that record and its route are
independently verified. Otherwise leave `T` unset. Do not announce or write to
a parent card as part of this runbook. Missing/unknown intervals remain UNKNOWN
and historical records remain intact.

## Gate 5 — qualifying-clock rules

- The `2026-09-30T20:11:06Z` gateway-ready observation is a **failed
  historical attempt, not a qualifying clock**. The outage breaks continuity;
  October 7 20:11:06Z is invalid as a qualifying finish. Preserve this historical
  record unchanged.
- `T` starts only after Gates 1–4 and all qualifying preconditions pass, recorded
  as UTC in the existing approved soak record. The first `/readyz` 200, a
  passive wait or an elapsed-time calculation is not `T`; if any input is
  unknown, `T` remains unset.
- The acceptance interval is **four ACTIVE hours**. Joins, voice, messages and
  slash remain required; zero missed events is required across the interval.
- Hours 3–4 require **120 expected one-minute samples**, one for each minute
  across those two hours. These are required, not best-effort. Missing evidence
  yields NOT VERIFIED, never PASS; preserve failed/UNKNOWN intervals and all
  historical records without erasure or reclassification.
- Every actual outage must recover in **under 60 seconds**, measured from the
  actual outage start to verified recovery. Deploy-finish-to-first-ready remains
  a separate workflow interval and cannot substitute for that recovery
  measurement.
- The staging deploy workflow documents a **95–139-second gateway drop per
  deploy**, and a separate historical staging note records a **92–139-second
  redeploy**. These are documented historical interruption ranges, not a
  verified outage-start-to-recovery measurement; both exceed 60 seconds if a
  planned redeploy is in scope. The approved policy does not classify planned
  redeploys as actual outages or name an owner for that classification. The
  existing technical-policy question remains pending with the CTO; the
  CEO-held fixture authorization remains the separate execution hold. Until the
  existing policy path resolves classification and an under-60-second recovery
  is independently measured, these redeploy exercises cannot establish PASS.
- Flat memory and no error spikes remain requirements. Numeric RSS/error
  definitions are unaccepted; the historical B1 RSS receipt is not a new B2
  numeric criterion.
- The approved exercise set—three redeploys, two drops, Neon idle-hit and
  actual 429 categories—still needs exact reviewed budgets, an independent stop
  and restore arrangements before execution. This gate does not authorize live
  faults, load, dispatch or a new evidence/fault mechanism.
