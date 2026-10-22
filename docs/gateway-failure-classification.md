# Gateway failure classification against existing rules

Design doc. No code or threshold change is proposed or authorized here.
Each gateway failure mode maps to exactly one alerting rule or observer,
except the intentional overlaps recorded in §4 with rationale.

Source revisions: `wrangler/src/alert-rules.ts` rules table and pairing
logic, `crates/bot/src/gateway_failure.rs` failure vocabulary,
`crates/bot/src/server.rs` readiness report, `crates/bot/src/gateway.rs`
transport-loss funnel, `docs/runbook.md` alert sections,
`docs/container-readiness.md` unready monitor, `docs/metrics.md` catalog.

## 1. The existing rules (fixed set)

Metrics rules (`RULES` in `wrangler/src/alert-rules.ts`); the four this
doc classifies against:

- `gateway_unpaired_disconnect` — `two_bot_gateway_disconnects_total` grew
  and recoveries (`two_bot_gateway_resumes_total` plus fresh READY from
  `two_bot_gateway_events_total{event="READY"}`) have not caught up for 3
  consecutive samples. Any recovery growth clears at once. Presence, not
  arithmetic, pairs loss with recovery: one recovered outage routinely adds
  N disconnects against a single recovery, so subtracting growths would page
  forever (runbook §Alert: gateway unpaired disconnect).
- `gateway_missed_events` — any increase of
  `two_bot_gateway_missed_events_total` between two samples. First sample is
  baseline-only; a backwards counter (restart) skips the window.
- `gateway_checkpoint_failures` — any increase of
  `two_bot_gateway_checkpoint_failures_total` summed over every `stage`
  between two samples. Every checkpoint failure stops the dispatch worker,
  so there is no benign singleton. The failing process lingers (15 s
  startup, 75 s running) so a keepalive scrape lands before the restart
  resets the counter; the post-restart reset resolves rather than firing.
- `dispatch_drops:<lane>` — **ticket, never a page**, and explicitly **not**
  proof of gateway packet loss or a sequence gap. Local dispatch-admission
  refusals only (the `reactions` lane also counts per-member fairness
  refusals while slots remain free).

Readiness observers (not metrics rules):

- `container_unready_alert` / `container_unready_recovery` — the keepalive
  probes `/readyz` about every 60 s; ~10 consecutive failed probes (≈10 min
  at default cadence) emit one alert and one recovery per incident
  (`docs/container-readiness.md`). This is the liveness observer for
  everything that keeps `/readyz` non-2xx, including states no counter
  rule can baseline.
- `/readyz` itself — components `process` (always ready when answering),
  `gateway` (`Unconfigured`→down, `Armed`→starting, `Connected`→ready,
  `Draining`→down), `database` (bounded 2 s live ping), `token_invalid`
  (fatal latch until restart), plus the informational `jobs` map and the
  diagnostic `gateway_failure` field (`{"phase":"durable_gateway",
  "class":"…"}`), which is an overlay, never a rule input.

## 2. Failure-mode classification

`/readyz` is 503 whenever the gateway is not `Connected`, the database ping
fails, or `token_invalid` is latched (`server.rs`, `readiness_report` and
`with_token_state`). So `container_unready_alert` observes every sustained
outage of those three components, whatever else also fires. The classifier
column names the rule that owns the cause. Where another rule owns the cause,
the unready streak is a registered companion in §4 (O2, O6, O7, O8); where nothing
else can see the mode, it is the classifier (F5, F6, F10).

| # | Failure mode | Exactly one classifier | Why this one |
|---|--------------|------------------------|--------------|
| F1 | Transport loss, gateway never comes back (failed Twilight retries, close frames, op 9 invalid session, cold-resume IDENTIFY with no later RESUMED/fresh READY) | `gateway_unpaired_disconnect` | The disconnect series funnels every transport loss through `transport_disconnected`; the 3-sample presence pairing is the only rule that asks "did it come back". The same loss leaves the gateway `Armed`, so `/readyz` is 503 and `container_unready_alert` joins about 10 samples in (O6). |
| F2 | Sequence gap inside one session (Discord-assigned dispatches never received; session continues, RESUME does not replay) | `gateway_missed_events` | The missed-events counter is the only series that measures stream completeness rather than liveness. |
| F3 | Checkpoint commit failure, `stage="pre_commit"` (commit skipped after a funnel/leveling/acknowledgement failure) | `gateway_checkpoint_failures` | Both stages sum into this one rule; the stage is only visible while the failing process still lives. |
| F4 | Checkpoint commit failure, `stage="commit"` (durable store write failed) | `gateway_checkpoint_failures` | Same rule as F3 by construction (summed counters). First response differs (correlate DB-error/pool alerts — a `commit` failure next to them points at the database), but the classifier is one. |
| F5 | Gateway parked: prerequisites missing, shard never runs (`Unconfigured`→down) | `container_unready_alert` (readiness observer) | No counter ever baselines, so no metrics rule can fire; the unready streak is the only signal. First sample is baseline-only by design. |
| F6 | Gateway `Armed` since boot with the post-connect handshake stalled and no error on record (no HELLO/READY yet, no disconnect growth; rare) | `container_unready_alert` | Same as F5: no disconnect growth, so F1 stays silent and no counter rule has a window to baseline. A first-boot connect that fails is F1, not F6: it goes through `transport_disconnected` and grows the disconnect counter like any reconnect. A `checkpoint_io` wait is not persistent either: it is bounded to `CHECKPOINT_IO_MAX` (5 s) and its timeout is fatal (F3/F4/F10/F11). |
| F7 | Bot-token refusal latched (`token_invalid`→down, fatal until restart) | `rest_token_invalid` (metrics rule; `container_unready_alert` follows) | The latch forces `/readyz` to 503 until restart (`with_token_state`), and the metrics rule pages on the first valid latched sample with the credential-death attribution; the unready streak still pages about 10 samples in as the liveness companion, as for F5. |
| F8 | Database unreachable/down (`database`→down while gateway stays up) | `db_errors` / `db_pool_saturated` (DB rules, not gateway rules) | The `database` component is independent of the `gateway` component by contract. A `commit`-stage checkpoint failure coinciding with DB alerts is triaged as a database incident. A sustained outage also fails the 2 s ping in `/readyz` with the gateway still `Connected`, so `container_unready_alert` follows the DB rules about 10 samples in (O7). |
| F9 | Dispatch-lane saturation / per-member fairness refusal | `dispatch_drops:<lane>` (ticket) | Local admission pressure only. Must never be read as gateway loss; the rule text says so. |
| F10 | Startup init failures before the shard runs (`store_unavailable`, `gateway_pool_connect_failed`, `checkpoint_load_failed`, `onboarding_gates_invalid`, `onboarding_init_failed`, `custom_commands_init_failed`, `milestones_load_failed`, `automod_config_invalid`, `automod_executor_failed`, `raid_executor_failed`) | `container_unready_alert` (readiness observer) | All ten are fatal before any session exists: the process serves `gateway_failure` for 15 s, then exits. No metrics baseline exists, so no counter rule fires. The `gateway_failure` class names the cause; the unready streak pages the liveness. DB-adjacent members of this set (`gateway_pool_connect_failed`, `checkpoint_load_failed`) triage against DB alerts but do not fire gateway rules. |
| F11 | Running-gateway failure caused by a checkpoint failure (`gateway_runtime_failed` with a checkpoint origin) | `gateway_checkpoint_failures` | The checkpoint rule is the cause alert; the `gateway_failure` overlay (`gateway_runtime_failed`, both stages) is diagnostic, not a second alert. |
| F12 | Running-gateway failure NOT caused by a checkpoint failure (`gateway_runtime_failed` other origins, `gateway_task_panicked`) | Manifestation decides: F1/F2/unready (§4, overlap O3) | No dedicated rule exists for panics or non-checkpoint runtime exits. They surface through whatever they break: transport counters (F1/F2) if a session was live, otherwise the unready streak across the restart (a backwards counter re-baselines and holds firing alerts rather than clearing). |

Notes:

- `session_checkpoint` job success is event-driven (one commit per durable
  dispatch) and is excluded from the `job_unknown_stale` fallback precisely
  because idle guilds go stale while healthy; gateway liveness for that
  label is covered by readiness plus F2, not by job staleness.
- `gateway_failure` (all 12 classes) never fires anything itself. It is a
  fixed-vocabulary diagnostic served on `/readyz` and as the Worker-side
  `container_gateway_failure` event. It lingers 75 s for
  `gateway_runtime_failed` and 15 s for every other class, including
  `gateway_task_panicked` (`failure_linger` in `shutdown.rs`). Classification
  counts it as evidence, not as a classifier — this is what keeps every row
  at exactly one.
- A RESUMED or fresh READY clears F1 even when the same window added
  disconnects. A RESUME that replays every missed dispatch counts zero under
  F2: `missed_gap` counts only sequence numbers that never arrive
  (`crates/core/src/gateway_session.rs`), and replayed dispatches arrive with
  contiguous sequence numbers. The runbook says the same: RESUME does not
  replay what the counter counts.

## 3. Disconnect-page vs missed-events double-fire analysis

The two rules answer different questions on purpose: F1 asks "did the
gateway come back" (liveness), F2 asks "is the stream whole"
(completeness). Their firing shapes differ accordingly: F2 fires on the
first window with an increase (immediate, no streak — any gap fails the
zero-missed-events acceptance), while F1 needs 3 consecutive outstanding
samples (~2–3 min, so a reconnect that resumes within two samples never
pages).

Three outcomes for one transport incident:

1. **Clean disconnect, no gaps** (all events replayed or no traffic in the
   window): F1 pages if still down after 3 samples, and
   `container_unready_alert` follows at about 10 samples if it is still
   down (O6); F2 stays silent.
2. **Gap with fast recovery** (RESUMED within two samples): F2 fires once;
   F1 never reaches its bound, and the unready streak never reaches its
   threshold. The incident reads as "lossy but back".
3. **Lossy outage still down**: F2 fires first, then F1 joins it, then
   `container_unready_alert` at about 10 samples. F2 and F1 firing together
   means "transport loss with sequence gaps and no recovery yet" — the most
   urgent combination, and each half still directs a different response
   (F2: correlate disconnect growth, re-baseline after restarts, never
   restart to "clear" the counter; F1: confirm `gateway ready` in container
   logs, escalate through the watch path). The unready alert adds no new
   diagnosis; it confirms the outage outlasted the paging window.

Neither rule subsumes the other: F2 fires with no disconnect at all when
the gap predates instrumentation or the process restarted mid-window
(re-baseline both scrapes), and F1 fires with no gap on a clean loss.
Merging or mutual-suppressing them would destroy one axis: suppressing F2
under F1 hides data loss inside every long outage, and suppressing F1
under F2 hides "still down" inside every gappy one. Double-fire is the
intended signal, not a defect. No threshold or logic change follows.

## 4. Intentional overlap register

| # | Overlap | Rationale for keeping both |
|---|---------|----------------------------|
| O1 | F1 + F2 on one lossy outage (§3, outcome 3) | Liveness vs completeness; different latencies (immediate vs 3-sample), different responses, neither subsumes the other. |
| O2 | F3/F4 cause-page + `container_unready_alert` on the fatal restart | The checkpoint rule names the cause immediately (pre-exit scrape inside the linger); the unready streak observes the resulting liveness gap (~10 min) across the supervisor restart. After the restart the checkpoint alert resolves (counter reset) while a firing unpaired alert holds — cause recorded and cleared, liveness state retained. Exactly one cause alert plus one liveness observer. |
| O3 | F12 (panic / non-checkpoint runtime exit) surfacing via F1/F2 and the unready streak | A runtime exit has no dedicated rule; its manifestations already have owners. Adding a "runtime exited" page would double-count every F1/F2/F3 incident that ends in an exit. The `gateway_failure` class (`gateway_runtime_failed` / `gateway_task_panicked`) plus the `container_gateway_failure` Worker event give the cause without another alert, when a scrape or probe lands inside the linger. `gateway_runtime_failed` lingers 75 s; `gateway_task_panicked` lingers 15 s, so a panic's cause is often gone before the next 60 s keepalive check. |
| O4 | `pre_commit` + `commit` stages under one rule (F3/F4) | The stage is recoverable only while the failing process lives; post-restart only `gateway_runtime_failed` remains. Two rules would flap on which half survives the restart. One rule, stage-triaged in first response. |
| O5 | F4 (`commit`) + DB-error/pool alerts on a database outage | Both fire, but ownership is explicit: the combination points at the database, and the fix belongs to the DB path, not the gateway. No suppression either way — the coincidence is itself the diagnosis. |
| O6 | F1 + `container_unready_alert` on a transport loss that never recovers | Every loss sets the gateway `Armed` (`transport_disconnected`), so `/readyz` is 503 until a RESUMED or fresh READY restores `Connected`. F1 pages at about 3 samples with "did it come back"; the unready streak confirms the outage at about 10 samples. The same streak is separately the classifier for the handshake-stalled `Armed` case with no disconnect growth (F6). Different latencies, and the unready alert needs no counter baseline. No suppression either way. |
| O7 | F8 DB rules + `container_unready_alert` on a sustained database outage | The 2 s database ping is part of `/readyz`, so a database outage turns readiness 503 even while the gateway stays `Connected`. The DB rules name the cause; the unready streak observes the lost readiness about 10 samples in, the same split as O2. |
| O8 | F7 `rest_token_invalid` + `container_unready_alert` on a latched bot token | The metrics rule pages on the first valid latched sample with the credential-death attribution; the unready streak confirms the liveness loss about 10 samples in, the same cause-plus-observer split as O2/O6. |

Rejected (not an overlap): `dispatch_drops` never joins any combination
above as gateway evidence; startup classes (F10) stay metrics-silent because
there is no baseline to compare against.

## 5. Reviewer approval

Docs-only; no code or threshold change. Approval is recorded on the PR
that carries this file.
