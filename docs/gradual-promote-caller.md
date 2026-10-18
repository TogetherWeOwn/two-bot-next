# Gradual-promote caller (passive)

Staging-first wiring for the shared central gradual-promotion action
(5% → 50% → 100% with per-stage health gates and automatic rollback).
Production wiring is intentionally deferred: the production deploy stays on
its gated cutover path (see `docs/production-deploy.md`).

## Smoke checker

`bin/smoke.mjs` is the caller's health checker. The action invokes it as:

```sh
node bin/smoke.mjs "https://$DOMAIN" [--allow-indexable] --version-override '<worker-name>="<lowercase-version-uuid>"'
```

- At the 5%, 50% and 100-override stages the override pins probes to the NEW
  version through the `Cloudflare-Workers-Version-Overrides` header.
- At the apex/live stage the override is omitted and the live split answers.
- The action retries each stage 6 times, 10 s apart, fail closed. This file
  does one pass: exit 0 healthy, 1 unhealthy, 2 usage error.

Each pass probes `GET /health` (200 JSON `{"status":"ok"}`) and `GET /readyz`
(200 only when every component is ready; 503 otherwise). Under an override,
the Worker-stamped `x-two-worker-version` header must equal the override
UUID, so a silent fallback to the old version fails instead of passing on
the wrong build. Both probes require `x-content-type-options: nosniff`.

`--allow-indexable` is accepted for the apex contract and changes no
assertion: this Worker serves no indexable HTML (health, readiness, ops and
redirect links only). The flag is logged so apex runs stay distinguishable.

Live staging probes without an override run any time:

```sh
node bin/smoke.mjs "https://<staging-worker>"
```

`--version-override` is contract-only for the isolated synthetic fixture
(`two-bot-next-gradual-proof="<version-uuid>"`, once provisioned). Do not run
it against the `two-bot-next-staging` singleton: any overridden version that
does not hold the singleton ownership fence answers `/health` and `/readyz`
with 503 `ownership_fenced` (`OwnershipFence.require` in
`wrangler/src/ownership.ts`), so the override stage fails and the run
auto-rolls back. Override mode passes only when the candidate already owns
the fence, which means it cannot serve as a canary for this Worker. Gradual
promotion here additionally needs a Durable Object config without `exports`
(this `wrangler.toml` uses `exports`, which Cloudflare documents as
unsupported for gradual deployments) or a separate non-DO fixture Worker.

Contract tests live in `wrangler/test/smoke-checker.test.ts` (loopback stub;
no staging call, no secret).

## Pinned caller

`.github/workflows/gradual-promote-proof.yml` (dispatch-only, main-only,
`staging` environment) holds the pinned invocation:

- Action: `TogetherWeOwn/.github/actions/gradual-promote@89c290be20c0044efaf6916f28341c76e75fff06`
- Worker: `two-bot-next-gradual-proof` (isolated synthetic target, not yet
  provisioned)
- `allow-indexable: "false"` (no indexable surface on this Worker)
- Error-rate inputs, all explicit: `error-rate-gate: "true"`,
  `error-rate-threshold: "0.05"`, `error-rate-min-requests: "20"`,
  `error-rate-window-minutes: "10"`, `error-rate-soak-seconds: "60"`
- Credentials: the `staging` Cloudflare pair only. Production secret names
  never appear.

## Passive status (read before claiming proof)

- The proof workflow's target-lock step refuses before any Cloudflare call
  until the synthetic fixture lands. The pinned caller is therefore the exact
  future invocation, not a completed rollout.
- What runs today: `bin/smoke.mjs` against staging, plus its contract tests.
- What remains: the isolated synthetic fixture, the success-mode sweep, the
  rollback-mode sweep, and only then any production caller discussion.
- Never claim a gradual traffic rollout here until a proof run shows the
  action's 5-50-100 sweep (or its automatic rollback) against the synthetic
  target with the evidence artifact attached.
