# TOG-9694 — B1 baseline measurement (Node discord.js bot)

Date: 2026-09-29. Method: real `two-bot` process (unmodified `src/index.ts`),
mock-discord gateway double, scratch Postgres on agent-testdb.
No production services touched. No docker available in this env — no
`--memory=256m` cgroup cap; numbers below are unconstrained RSS, which is a
conservative (high) bound for the lite gate.

## Load profile

- 180 s idle → 480 s scripted load → 120 s settle (13 min total).
- Load: 1,199 gateway dispatches at ~2.5/s (400 joins + 400 messages + 399
  voice-state) — roughly half a day of guild traffic compressed into 8 min.
- DB proof the pipeline ran: `events` = 1,998 rows
  (400 member_join + 400 gate_cleared + 400 first_message + 399
  voice_session_start + 399 first_voice_session), `members` = 1,199 rows.
- Bot exited 0 on SIGTERM after the run.

## RSS / CPU (sampled every 5 s from /proc, % of one core)

| phase  | n  | RSS min | RSS p50 | RSS p95 | RSS max | CPU p50 | CPU p95 | CPU max |
|--------|----|---------|---------|---------|---------|---------|---------|---------|
| idle   | 35 | 133     | 153     | 153     | 153     | 0.0%    | 0.2%    | 0.8%    |
| load   | 97 | 133     | 140     | 141     | 142     | 0.4%    | 0.8%    | 1.2%    |
| settle | 24 | 138     | 138     | 138     | 138     | 0.0%    | 0.0%    | 0.2%    |

(All MiB.)

Caveats: mock guild has 3 members, no cache pressure from a real 107-member
guild, no privileged member-chunk payloads, no TLS to Discord, pool max 5 on a
local test DB. Real-guild RSS will be *higher* — cache and chunk buffers scale
with member count — so treat ~140 MiB as a floor, not a ceiling.

## Gate verdict: `basic` (safe default)

Peak measured RSS ≈ 153 MiB < 200 MiB nominally passes the lite gate — but the
mock guild is ~3% of the real member count and discord.js caches (members,
messages, voice states) grow with it. TOG-9408 already flagged "RSS likely
>256 MiB → moves to basic ≈ $7–9". One OOM-kill of the gateway drops joins;
the $5.30/mo lite→basic step is not worth that risk. **Ship `basic`.** Revisit
`lite` only after the B2 staging-guild soak measures a real 107-member cache.

## Cost (Cloudflare Container pricing, per TOG-9408/ADR-0001 model)

| Setup | Memory | Disk | CPU | Total/mo |
|---|---|---|---|---|
| Node on **`basic`** (1 GiB·4 GB) | $6.26 | $0.68 | $0 | **≈ $6.9** |
| Node on `lite` (if a later staging soak proves RSS) | $1.40 | $0.31 | $0 | ≈ $1.7 |
| Rust/twilight on `lite` (S2 prototype to confirm) | $1.40 | $0.31 | $0 | ≈ $1.7 |

Rust delta vs Node-on-basic: ≈ $5.2/mo (≈ $63/yr) — same as ADR 0001. Honest
caveat stands: $0 company-bill delta while the Coolify VPS exists.

## Events/day estimate (no prod DB read — credential absent in this env)

Guild: ~84 humans / 107 members. Bot subscribes to 11 gateway event families
(joins, messages+reactions, voice, invites, audit log, interactions). Model:
~1.5k–3.5k gateway dispatches/day (presence/typing noise excluded; Discord
does not send those for these intents). At 2.5 dispatches/s in this soak, one
day of traffic ≈ 10–25 min of processing; CPU is noise either way (<1% of even
the 1/16-vCPU lite slice). B2's 7-day staging soak should replace this estimate
with a counted number.

## Container keepalive verdict: DO-alarm keepalive REQUIRED

Verified against `@cloudflare/containers@0.3.7` source (`dist/lib/container.js`)
plus the Workers Containers architecture doc:

- Default `sleepAfter` is 10 min; `onActivityExpired()` default calls `stop()`
  (SIGTERM → 15-min grace → SIGKILL).
- The inactivity clock (`sleepAfterMs`) is renewed **only** by inbound request
  paths (`containerFetch` proxy, `decrementInflight` reaching 0) and explicit
  `renewActivityTimeout()` calls — 10 call sites, all on the DO fetch/proxy or
  schedule path. Nothing in the container's outbound traffic (including the
  Discord gateway WebSocket) touches it.
- `isActivityExpired()` returns true with zero in-flight requests once the
  deadline passes, regardless of how busy the gateway socket is.

So the top risk from TOG-9408 is confirmed: an always-on Discord gateway —
pure outbound WS, zero inbound HTTP — **will be slept after `sleepAfter` with
no warning to the bot**, silently dropping joins/voice events. The keepalive is
not optional.

Required design (matches the S1 `wrangler/` skeleton on
`feat/s1-cargo-scaffold-container-skeleton`): a self-perpetuating DO
`schedule()` tick (not a raw `alarm()` override — the Container base class owns
`alarm()`) every ≤60 s that calls `renewActivityTimeout()` and probes the
container's `/readyz`. Belt-and-braces `sleepAfter = "30m"` so a missed tick or
two never costs the session. Inbound `/health` + `/readyz` fetches renew
activity implicitly. This needs proving on a real deployment (B2): deploy the
skeleton with a scratch Worker, hold the gateway open with zero inbound
traffic for >30 min, assert no `onActivityExpired` fires and no RESUME gap
appears. A live-network scratch deploy was not possible from this sandbox
(no wrangler, no outbound CF API writes attempted — read-only verify only).

## Reproduce

Driver: `/tmp/tog9694/soak.mjs` (kept on the run host, not committed — it
points at an absolute checkout path). Scratch DB `tog9694_baseline` on
agent-testdb left intact for B2 cross-checks.
