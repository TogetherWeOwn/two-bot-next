# TOG-9807 — S2 measured twilight prototype (gateway + one slash command)

Date: 2026-09-29. Method: real `twilight-gateway` 0.17.1 `Shard` +
`twilight-http` 0.17.1 `Client` against a dev-only mock Discord double
(`crates/discord/tests/common/mod.rs`; plain-`ws://` gateway, raw-tokio HTTP),
same host, release profile (`opt-level=z`, lto, strip). No production
services touched; no real Discord traffic.

## What the prototype proves

1. **Gateway connect:** the shard dials the mock via
   `ConfigBuilder::proxy_url("ws://…")` (the documented redirect seam; the
   URL builder only substitutes the base, keeping `?v=10&encoding=json` and
   the zlib query), answers `Hello` with `Identify`, and yields `READY`
   through `next_event(READY | INTERACTION_CREATE)`. Text frames pass through
   uncompressed — the mock needs no zlib encoding.
2. **Command register:** `create_guild_command("ping")` posts
   `POST /api/v10/applications/{app}/guilds/{guild}/commands` via
   `ClientBuilder::proxy(host, use_http = true)` (plain HTTP against the
   mock, same seam production uses for its HTTP proxy) and parses the
   returned `Command` body.
3. **Slash-command answer:** the mock fires `INTERACTION_CREATE` for `/ping`
   (twilight-model's custom `Interaction` deserializer requires
   `application_id`, `authorizing_integration_owners`, `id`, `token`, `type`
   plus command `data`); the test answers with a `ChannelMessageWithSource`
   callback and asserts the mock observed a 204 and the `pong` body.

Driver: `cargo test -p two-bot-discord --test s2_prototype` (release binary
run directly, `--test-threads 1`, 3/3 green). CI runs the same test in debug
for behaviour; numbers below are release-only.

## RSS / CPU (test process, sampled from /proc)

Conservative bound: the measured process hosts the shard, the HTTP client,
**and** the mock double (both listeners, both protocol tasks). The shipped
bot binary contains neither the mock nor the test harness.

| run | idle RSS (5 s post-READY) | post-command peak | CPU %1-core (whole run) |
|-----|---------------------------|-------------------|-------------------------|
| 1   | 6.0 MiB                   | 6.3 MiB           | 0.0%                    |
| 2   | 5.9 MiB                   | 6.2 MiB           | 0.0%                    |
| 3   | 5.9 MiB                   | 6.2 MiB           | 0.0%                    |

(Debug profile, same test: 17.9 idle / 18.8 peak — same order, larger as
expected. CI asserts behaviour only, not numbers.)

## Gate verdict: `lite` CONFIRMED

Peak 6.3 MiB « 200 MiB gate — two orders of magnitude of headroom. The real
bot will add the axum surface, `twilight-cache-inmemory` subset, sqlx pool,
and a real-guild cache (B1 showed Node's cache dominating at 107 members),
but even 10–20× this prototype stays far under the ceiling. ADR 0001's
`lite` placement stands; S3's staging soak should re-measure with a real
guild cache before it becomes load-bearing.

## Notes for S3+

- **S3 MUST promote `rustls`+`ring` to a runtime dependency.**
  `Client::builder().build()` and `Shard` construction build their TLS
  connectors eagerly and rustls panics with no provider feature enabled —
  found here because the mock is plain-HTTP/ws. Today S1's release binary
  would panic the same way on first connect; S3's real-dial work must carry
  the provider (`ring` needs only perl+cc, present on CI runners).
- New transitive crates (`tokio-websockets` server-side, `futures-util`
  `sink`, `rustls`/`ring`): all MIT/Apache-2.0/BSD-3-Clause, already in
  `deny.toml`'s allow-list — no change needed. All are dev-only; the release
  binary gains zero new dependencies.
- Mock payload shapes were verified field-by-field against the twilight
  0.17.1 sources (not docs.rs): `Command` requires `version: "1"` as a
  string-serialized `Id`; `Interaction` rejects payloads missing the five
  required fields above.
