# Real-binary lifecycle acceptance

`crates/bot/tests/alive.rs` runs Cargo's compiled `two-bot` entrypoint, not a
replacement host or an in-process shard. No real Discord service is contacted.

```sh
TWO_GATEWAY_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test \
  cargo test -p two-bot --locked --test alive -- --ignored --nocapture
```

The shared gateway test-database guard runs before connecting. It accepts only
`agent-testdb` or the CI loopback service (`localhost` / `127.0.0.1`), with user
and database both `agent_test`. Explicit Unix sockets (including SQLx's
`?host=/path` override on an allowed TCP host) are rejected before connection.
It never falls back to runtime `DATABASE_URL`.
Each run creates its own schema; the child URL carries `options[search_path]`
so the harness migration bootstrap and the child stay isolated. The gateway
binary is DML-only and never migrates: the harness performs the operator's
migration step before spawning the child, exactly like the documented
production bootstrap. Normal failure paths
kill/reap children and delete only that schema. CI explicitly runs this ignored
test under `timeout 60s`; the lifecycle has its own 45-second deadline and
five-second step deadlines. Assertion failures print captured stdout/stderr.

## Acceptance matrix

| Step | Evidence asserted |
| --- | --- |
| Fresh process | Actual `CARGO_BIN_EXE_two-bot`, scrubbed environment, synthetic token |
| Before gateway HELLO | `/healthz` and existing `/health` 200; `/readyz` 503, gateway starting |
| Before READY | Captured IDENTIFY opcode 2; readiness still 503; no ready log |
| After READY | Checkpoint reaches sequence 2; one funnel row; readiness 200; process stays up |
| SIGTERM | Clean exit, draining log, listener port can be rebound |
| New process, unchanged database | RESUME opcode 6 with the persisted session and sequence 2 |
| Before RESUMED | Readiness still 503 despite persisted session/authentication |
| After replay and RESUMED | Checkpoint sequence 3; still one funnel row; readiness 200 |
| Logging | Listener log precedes gateway initialization; committed-ready log on each boot |
| JSON logging | Request `LOG_FORMAT=json`; validate every line if JSON is emitted; otherwise explicit skip until format support lands |

The mock uses one stable endpoint across boots. The second child receives an
unreachable bootstrap URL, proving that its unchanged persisted resume endpoint
is actually selected. HELLO and READY/RESUMED are separately gated by the test.

## Binary lifecycle seams

- The listener binds before the gateway task starts. `/healthz` is an alias for
  the existing liveness endpoint; `/readyz` semantics are unchanged.
- `DISCORD_GATEWAY_URL` is an opt-in mock bootstrap override. Only `ws://` plus
  a literal loopback IP and nonzero port is accepted (IPv4 or IPv6). Remote
  addresses, DNS names, credentials, paths and queries are rejected before a
  shard is built. Unset keeps the normal Discord endpoint; saved resume URLs
  retain precedence.
- `DISCORD_API_BASE` is set to the local mock REST origin in the child
  environment as a containment seam. Startup issues exactly one proxied REST
  read (`GET /api/v10/users/@me`) for the onboarding identity probe; the mock
  serves `{"id":"999","bot":true}` on loopback and 404 otherwise. No real
  Discord service is contacted.
- The connected log is emitted only after READY/RESUMED and its checkpoint
  commit. `LOG_FORMAT=json` is not implemented in the current binary; the
  acceptance reports the skip rather than pretending plain text is JSON.
