# B2 soak entry gates — start the 7-day clock the hour staging heals

This doc **defines** the gates; it does not execute them. Executing the soak
belongs to the live 7-day evidence-collection card. When staging heals, the
executor walks gates 1–4 in order, records the qualifying start **T**, and the
clock runs `[T, T+7d]` with no planning round.

Companions: [staging-soak.md](staging-soak.md) (lifecycle and acceptance),
[evidence-route.md](evidence-route.md) (expected/processed collection),
[container-readiness.md](container-readiness.md) (keepalive monitoring),
[gateway-recovery.md](gateway-recovery.md) (session persistence),
[migrations.md](migrations.md) (numbering and lock guard).

## Fence (applies to every gate)

- Staging only: the TWO Staging guild and the staging bot identity. The
  production bot and production guild are never touched.
- No tests, SQL, probes, migrations, imports, backup or restore against
  staging/production databases, except the single operator-held read-only
  query in [evidence-route.md](evidence-route.md) step 2.
- After any credential denial: stop, record the exact error and owner, never
  substitute another credential.

## Gate 1 — container healthy at a pinned SHA

The staging deployment is the `main` head it claims to be, and the process
answers truthfully.

| Check | Verification command | Expected value |
|---|---|---|
| Deploy green | `gh run list --workflow deploy-staging.yml --branch main --limit 1` | `conclusion: success`, head SHA recorded as the pinned SHA |
| Liveness | `curl -s -o /tmp/health.json -w '%{http_code}' "$STAGING_WORKER_URL/health"` | `200`, body `{"status":"ok"}` |
| Readiness | `curl -s -o /tmp/readyz.json -w '%{http_code}' "$STAGING_WORKER_URL/readyz"` | `200` with `components` `[["process","ready"],["gateway","ready"],["database","ready"]]` (plus informational `jobs`) |

`503` means the gateway is parked or starting — the gate is closed, not broken.
Any other status (500/1101/…) means the deploy is broken: file a repair card,
do not start the clock. `/health` 200 alone never passes this gate.

## Gate 2 — gateway stable window

A 60-minute clean window immediately before **T**: two `/readyz` 200 probes at
least 60 minutes apart, with no `container_unready_alert` and no
`deploy-staging` run between them. Sixty minutes exceeds the keepalive alert
horizon (10 consecutive failed 60 s samples, ~10 minutes), so a clean window
proves the session is not flapping.

| Check | Verification command | Expected value |
|---|---|---|
| Probe A then B | same `/readyz` curl as gate 1, twice, timestamps recorded | both `200`, all three components `ready` |
| No alerts | Worker-log query for `container_unready_alert` over the window | `0` alerts (`container_unready_recovery` without a preceding alert is fine) |
| No restarts | `gh run list --workflow deploy-staging.yml --limit 5` | no run started inside the window |

Any alert, restart, or non-200 probe discards the window: open a new 60-minute
window after the cause is repaired.

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

## Gate 4 — evidence ledger ready to receive T

The clock needs somewhere to live before it starts.

| Check | Verification command | Expected value |
|---|---|---|
| Ledger document | `GET /api/issues/<live-soak-id>/documents/ledger` (or the board document view) | document exists with columns: window, expected/processed counts per family, gaps, deployed SHA, packet links |
| Start announcement | comment on the parent epic after gates 1–4 pass | `Soak start T=<UTC ISO-8601>, pinned SHA=<sha>` with probe timestamps and receipt links |
| Packet tooling | soak-evidence offline suite (`test_soak_evidence.py`, landing with the parser slice) | green on the pinned SHA once that slice has merged; until then record tooling NOT VERIFIED (proves the offline reconcile seam, not staging coverage) |

Per-window sanitized packets attach to the live soak card as they are
collected, per [evidence-route.md](evidence-route.md). Intervals without a
packet stay UNKNOWN.

## Gate 5 — qualifying-clock rules (CEO Oct-1 decision)

- The `2026-09-30T20:11:06Z` gateway-ready observation is a **failed
  historical attempt, not a qualifying clock**. The outage breaks continuity;
  October 7 20:11:06Z is invalid as a qualifying finish.
- A new **T** starts only after gates 1–4 all PASS, recorded as a UTC timestamp
  in the ledger plus the start announcement. Earliest completion is **T+7 days**.
- Any confirmed missed event fails the interval: file a fix card with evidence,
  redeploy, and explicitly restart the clock after repair. Do not restart merely
  to fill a table.
- Missing evidence yields NOT VERIFIED, never PASS. Failed/UNKNOWN intervals are
  preserved in the ledger, never overwritten.
- The redeploy RESUME/IDENTIFY gap (deploy finish to first `/readyz` 200) is
  measured once on a deploy justified on its own, against the **<60 s** budget.
  Until measured it is NOT MEASURED, not zero.
- Final acceptance additionally needs the B1 `lite`-placement RSS receipt and
  seven consecutive days with zero missed gateway events (join / voice /
  message); voice slices consume the dedicated voice-soak receipt rather than
  duplicating its scenario.
