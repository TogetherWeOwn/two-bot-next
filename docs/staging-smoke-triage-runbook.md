# Staging smoke failure-signature triage runbook (read-only)

One page for a red staging smoke run. Read-only against staging: GETs, scrapes,
and log filters only. Never SQL/probe/restore staging or production, never print
tokens or connection strings, never substitute another credential on a 401/403.
Guild fence: TWO Staging `1545644954272137297` only; live `326474832151838730`
refuses with no request sent (`scripts/staging_automation_read_smoke.py:79-94`,
authorities `crates/core/src/backup/guild_config.rs`, `crates/cutover/src/lib.rs`).

Start every triage with these two (operator supplies `STAGING_WORKER_URL`):

```bash
curl --silent --show-error --max-time 10 --include "${STAGING_WORKER_URL}/health"
curl --silent --show-error --max-time 10 --include "${STAGING_WORKER_URL}/readyz"
npm --prefix wrangler run logs -- --env staging --format pretty
```

`/readyz` may carry `gateway_failure: <phase>:<class>` for 15 s before exit-1
(`docs/startup-diagnostics.md`; enum `crates/bot/src/gateway_failure.rs`).
Worker keepalive `container_gateway_failure` JSON (`wrangler/src/index.ts:532`)
mirrors it in Workers Logs (dashboard Workers & Pages → two-bot-next → Logs
@staging, 7-day, unsampled). Full probe/log map: `docs/runbook.md#logs-and-keepalive`
and `#common-failures`. Metric queries: `docs/watch-signal-queries.md`.

| # | Signature (what you see) | Exact check | File / dashboard | Next card |
|---|---|---|---|---|
| 1 | Timeout: `FAIL interactions: interactions endpoint did not respond (…)`; rollout `verify` timeout with `gateway_failure=…`; `/readyz` 503 `starting` persisting | `curl /health` + `/readyz` (keep status+body); count `listening` vs `two-bot container started` vs `READY`/`RESUMED` (watch pack §1, §5); `staging_rollout.py` last-observation line | Container stdout + Workers Logs @staging; `docs/staging-rollout-gate.md` steps 6–7 | Single hit then green rerun → **flake** (rerun once, cite run id). Persistent `starting`/no `READY` → **bug** (gateway/checkpoint slice). Host-move/Cloudflare incident pattern → **infra** (Operator via CEO) |
| 2 | Unexpected reply: `FAIL <cmd>: not registered` / `resource disagrees` / `200 with invalid JSON`; Discord `Something went wrong (ref XXXXXXXX)`; wrong first edit | Open the smoke evidence receipt (`commands[]` verdict/detail); find `reference` in container logs at ERROR (generic copy only — `docs/interaction-replies.md#reply-and-error-contract`); confirm slice is wired (`docs/runbook.md#containment-kill-switches-and-feature-flags`) | Evidence JSON from `--evidence`; container dashboard; `scripts/staging_automation_read_smoke.py:145-168` | **Bug** in owning feature slice (name expected vs observed command/version). Not a flake. Unwired-but-expected surface → slice card, not a waiver edit |
| 3 | Permission-denied: `credential refused (401)`; `lacks access (403)`; `refusing: token is not the staging application`; `/readyz` 503 `ownership_fenced`; `POST /internal/actions` 404/503 | `401` → stop, report secret **name** only, no retry/substitution. Verify app `1469137636663758888` via `GET /users/@me` shape (`staging_automation_read_smoke.py:137-143`); `403` → Developer Portal grants + guild fence; `ownership_fenced` → read-only `node wrangler/scripts/ownership-control.mjs status`; 404 on internal route = dark by design (needs `INTERNAL_ACTIONS_INGRESS=1` **and** secret `TWO_INTERNAL_ACTIONS=1`) | `docs/runbook.md#persisted-ownership-control`, `#secret-inventory-names-only`; `docs/internal-actions-receiver.md#staging-ingress-default-dark` | **Infra/operator** card (CEO-owned `Operator:`) for rotation/provisioning; **bug** only if fence constants or ingress wiring drift from docs. Never self-provision |
| 4 | Store-unavailable: `store_unavailable`, `gateway_pool_connect_failed`, `checkpoint_load_failed`, `database_connect_failed`; Worker 500 `container_unavailable`; `periodic job failed` / DB-pool alert | Read `gateway_failure` class on `/readyz` + keepalive JSON (fixed vocabulary only — `docs/startup-diagnostics.md` table); check Neon staging-branch status before touching the bot; watch pack §4: job-failure lines + `two_bot_job_consecutive_failures{job="audit_retry"}` streak (table counts only from a restored copy/backup artifact) | Container logs (Rust class) vs Worker tail; `wrangler/src/index.ts:201-202,705`; `docs/runbook.md#alert-db-pool` | Stuck class / pool-exhaustion streak → **bug** (store/migration/pool slice). Neon-branch or platform outage → **infra** (dependency owner). Short burst that self-clears → **flake** |
| 5 | Invalid-input (caller error, no card): `refusing: no staging guild id / live guild / not the TWO Staging guild`; `no staging bot token`; `registered entry carries no command id`; `command list answered 200 without a list` | Fix inputs and rerun; nothing was attested (exit 2 = fence, no request sent). Compare fence constants with the two authority files before claiming drift | `scripts/staging_automation_read_smoke.py:79-94,116-134`; `crates/core/src/backup/guild_config.rs` | No card when inputs were wrong. Fence constant drift → **bug** (update authorities first, then smoke) |
| 6 | Infra/CI-side: `rate limited (429); rerun later`; `endpoint answered {5xx}`; rollout `rollout_snapshot_truncated` / receipt-invalid; red `check` lane with no Rust failure | Watch pack §2: `two_bot_rest_requests_total{result="429"/"5xx"/"transport"}` hot route via `/ops/metrics` scrape; rerun failed jobs once; for rollout reds quote only the allowlisted gate code + record/session counts | `docs/watch-signal-queries.md` §2–§3, §5; `docs/staging-rollout-gate.md#bounds-and-fail-closed-cases`; Actions run logs | One 429/5xx then green → **flake**. Persistent hot route / truncated snapshot / broken receipt → **infra** (host/Discord/Cloudflare, with run id + window + hot route). Red `main` → fix `main` first, no review card |

Rerun rule: one retry for 429/transport/timeout; a second red is a bug or infra,
never a loop. Every next card names: smoke command + evidence receipt, observed
`FAIL`/class line, `/readyz` + log window, and the card type above.
