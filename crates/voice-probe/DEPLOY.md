# voice-probe deploy note (operator handoff)

Proves or disproves one undocumented claim: a Cloudflare Container can do
**outbound UDP plus replies** (what Discord voice needs for IP discovery
and RTP). Docs only say Containers cannot *receive* non-HTTP UDP. The
probe exchanges Discord-shaped UDP with an echo the operator controls —
no bot token, no secrets, no Discord traffic.

## Run order

1. Build from the repo root:
   `docker build -f crates/voice-probe/Dockerfile -t voice-probe .`
2. Start the echo where UDP ingress works (any host with a public UDP
   port — **not** a Cloudflare Container, which cannot receive UDP):
   `docker run --rm -p 9001:9001/udp voice-probe --mode echo --bind 0.0.0.0:9001`
3. Run the probe **inside the Container under test**, pointing at the echo:
   `ECHO_HOST=<echo-host>:9001 DURATION_S=300 RATE_S=10 voice-probe`
   (flags `--echo`, `--duration-s`, `--rate-s` override the environment).
4. Capture stdout: exactly one JSON evidence object. Then tear the echo
   down — short-lived probe runs only, never public in production.

Fast smoke first: `DURATION_S=10 RATE_S=5 voice-probe --echo <host>:9001`
should exit 0 with `discovery_ok: true` and `loss_pct: 0.0` on a healthy
path before the 5-minute run.

## Environment

| Variable     | Default | Meaning                                     |
|--------------|---------|---------------------------------------------|
| `ECHO_HOST`  | —       | Echo target `host:port` (required in probe mode) |
| `DURATION_S` | `300`   | RTT send window, seconds (min 1)            |
| `RATE_S`     | `10`    | RTT packets per second (1-1000)             |

## Evidence object

```json
{
  "discovery_ok": true,
  "observed_egress": "203.0.113.7:52130",
  "sent": 3000,
  "received": 2991,
  "loss_pct": 0.3,
  "rtt_ms": {"min": 1.125, "p50": 2.5, "p95": 9.75, "max": 41.0},
  "duration_s": 300.25,
  "echo_host": "echo.example:9001",
  "started_at": "2026-10-22T10:00:00Z"
}
```

- `discovery_ok` / `observed_egress`: the Discord-shaped exchange
  succeeded, and the echo saw the probe from this `ip:port` — the egress
  mapping, not just reply arrival. `null` egress on failure.
- `loss_pct` / `rtt_ms`: receive shortfall and nearest-rank latency over
  the run. `rtt_ms` fields are `null` when nothing came back.
- Numbers are evidence, not gates: exit 0 means the run completed, even
  at 100% loss. Exit 1 is fatal only (unresolvable echo, socket or
  stdout failure); exit 2 is a usage error.
