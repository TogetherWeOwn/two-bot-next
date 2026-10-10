import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import worker, { type Env } from "../src/index.ts";
import {
  EMPTY_STATE, RULES, RUNBOOK_BASE_URL, evaluateMetrics, packetFilename, parseExposition, ruleFor, runbookUrl, transitionMessages,
} from "../src/alert-rules.ts";

const NOW = 1_000_000;
function body(extra: string[]): string {
  return ["# HELP x y", "# TYPE x gauge", ...extra, ""].join("\n");
}
const ev = (lines: string[], prev = EMPTY_STATE) => evaluateMetrics(parseExposition(body(lines)), prev, NOW);

/** Heading-slug index for one checked-in doc (single definition reused below). */
function docAnchors(doc: string): Set<string> {
  const text = readFileSync(new URL(`../../docs/${doc}`, import.meta.url), "utf8");
  return new Set([...text.matchAll(/^#+ (.+)$/gm)].map((m) =>
    m[1]!.toLowerCase().replace(/[^a-z0-9 -]/g, "").replace(/ /g, "-")));
}

test("job stale fires only past two intervals and ignores never-succeeded", () => {
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="rank"} ${NOW - 1200}`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="rank"} ${NOW - 1201}`]).firing, ["job_stale:rank"]);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="rank"} 0`]).firing, []);
});

test("consecutive failures fire at 3", () => {
  assert.deepEqual(ev([`two_bot_job_consecutive_failures{job="counter"} 2`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_consecutive_failures{job="counter"} 3`]).firing, ["job_consecutive_failures:counter"]);
});

test("429 rate uses the delta, ignores resets and tiny windows", () => {
  const first = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 100`, `two_bot_rest_requests_total{route="other",result="429"} 0`]);
  const hot = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 180`, `two_bot_rest_requests_total{route="other",result="429"} 20`], first.state);
  assert.deepEqual(hot.firing, ["rest_429_rate"]);
  const small = ev([`two_bot_rest_requests_total{route="other",result="2xx"} 100`, `two_bot_rest_requests_total{route="other",result="429"} 5`], first.state);
  assert.deepEqual(small.firing, []);
  const restart = ev([`two_bot_rest_requests_total{route="other",result="429"} 1`], hot.state);
  assert.deepEqual(restart.firing, []);
});

test("db pool saturation needs three consecutive samples", () => {
  const lines = ["two_bot_db_pool_configured 1", "two_bot_db_pool_connections 5", "two_bot_db_pool_max_connections 5", "two_bot_db_pool_idle_connections 0"];
  let state = EMPTY_STATE;
  for (const expected of [[], [], ["db_pool_saturated"]]) {
    const r = ev(lines, state);
    assert.deepEqual(r.firing, expected);
    state = r.state;
  }
  assert.equal(ev(["two_bot_db_pool_configured 1", "two_bot_db_pool_connections 5", "two_bot_db_pool_max_connections 5", "two_bot_db_pool_idle_connections 1"], state).state.poolStreak, 0);
});

test("transitions notify once on fire and once on resolve", () => {
  assert.equal(transitionMessages([], ["job_stale:rank"]).length, 1);
  assert.equal(transitionMessages(["job_stale:rank"], ["job_stale:rank"]).length, 0);
  assert.match(transitionMessages(["job_stale:rank"], [])[0]!, /RESOLVED/);
});

test("db errors fire on a burst, ignore trickles and restarts", () => {
  const base = ev([`two_bot_db_errors_total{op="admission"} 0`, `two_bot_db_errors_total{op="other"} 0`]);
  assert.deepEqual(base.firing, []);
  const trickle = ev([`two_bot_db_errors_total{op="admission"} 2`, `two_bot_db_errors_total{op="other"} 0`], base.state);
  assert.deepEqual(trickle.firing, []);
  const burst = ev([`two_bot_db_errors_total{op="admission"} 5`, `two_bot_db_errors_total{op="other"} 0`], base.state);
  assert.deepEqual(burst.firing, ["db_errors"]);
  const restart = ev([`two_bot_db_errors_total{op="admission"} 1`], burst.state);
  assert.deepEqual(restart.firing, []);
});

test("send admission blocked needs three consecutive windows with new refusals", () => {
  const lines = (n: number) => [`two_bot_send_admissions_total{outcome="blocked"} ${n}`];
  let state = EMPTY_STATE;
  for (const expected of [[], [], ["send_admission_blocked"]]) {
    const r = ev(lines(state.sendBlocked + 1), state);
    assert.deepEqual(r.firing, expected);
    state = r.state;
  }
  // A quiet window clears the streak; a counter reset (restart) resets it.
  assert.equal(ev(lines(state.sendBlocked), state).state.sendBlockedStreak, 0);
  assert.equal(ev([`two_bot_send_admissions_total{outcome="blocked"} 0`], state).state.sendBlockedStreak, 0);
});

test("voice failures fire on the lifecycle-drill failure exposition, silent on healthy", () => {
  // Synthetic exposition mirroring docs/voice-lifecycle-alert-drill.md Q4:
  // 42 successful creates, then injected create/discord x7,
  // move/persistence x4 and dead-letter create x3.
  const failing = [
    `two_bot_voice_operations_total{op="create",outcome="success"} 42`,
    `two_bot_voice_operations_total{op="create",outcome="discord"} 7`,
    `two_bot_voice_operations_total{op="move",outcome="persistence"} 4`,
    `two_bot_voice_dead_letters_total{action="create"} 3`,
  ];
  assert.deepEqual(ev(failing).firing, ["voice_failures"]);
  const healthy = [`two_bot_voice_operations_total{op="create",outcome="success"} 42`];
  assert.deepEqual(ev(healthy).firing, []);
});

test("voice failures ignore trickles and restarts, fire on lone dead-letters/orphans", () => {
  const base = ev([`two_bot_voice_operations_total{op="create",outcome="success"} 42`]);
  // One failure in 43 ops (2.3%) is a trickle, not a storm.
  const trickle = ev(
    [`two_bot_voice_operations_total{op="create",outcome="success"} 42`, `two_bot_voice_operations_total{op="create",outcome="discord"} 1`],
    base.state,
  );
  assert.deepEqual(trickle.firing, []);
  // A counter that went backwards means the process restarted: no window.
  const restart = ev([`two_bot_voice_operations_total{op="create",outcome="success"} 1`], ev([
    `two_bot_voice_operations_total{op="create",outcome="success"} 42`,
    `two_bot_voice_operations_total{op="create",outcome="discord"} 7`,
    `two_bot_voice_operations_total{op="move",outcome="persistence"} 4`,
  ]).state);
  assert.deepEqual(restart.firing, []);
  // A single new dead-letter or orphan pages even below the ratio window.
  const quiet = ev([]);
  assert.deepEqual(ev([`two_bot_voice_dead_letters_total{action="create"} 1`], quiet.state).firing, ["voice_failures"]);
  assert.deepEqual(ev([`two_bot_voice_orphans_total 1`], quiet.state).firing, ["voice_failures"]);
});

test("gateway missed events fire on any increase, never on the first sample or a reset", () => {
  const base = ev([`two_bot_gateway_missed_events_total 0`]);
  assert.deepEqual(base.firing, []);
  // First sample with a nonzero counter only stores the baseline.
  const first = ev([`two_bot_gateway_missed_events_total 5`]);
  assert.deepEqual(first.firing, []);
  // An increase between two samples fires.
  const fire = ev([`two_bot_gateway_missed_events_total 6`], first.state);
  assert.deepEqual(fire.firing, ["gateway_missed_events"]);
  // A flat window recovers (no increase, no fire).
  assert.deepEqual(ev([`two_bot_gateway_missed_events_total 6`], fire.state).firing, []);
  // A counter that went backwards means the process restarted: no window.
  const restart = ev([`two_bot_gateway_missed_events_total 1`], fire.state);
  assert.deepEqual(restart.firing, []);
  // The post-restart baseline fires again on the next increase.
  assert.deepEqual(ev([`two_bot_gateway_missed_events_total 2`], restart.state).firing, ["gateway_missed_events"]);
});

test("receiver refusals need three consecutive windows with new refusals, never the first sample or a reset", () => {
  const refused = (family: string, outcome: string, n: number) =>
    `two_bot_internal_actions_total{family="${family}",outcome="${outcome}"} ${n}`;
  const executed = (family: string, n: number) =>
    `two_bot_internal_actions_total{family="${family}",outcome="executed"} ${n}`;
  const mod = (auth: number, rate = 2, exec = 10) => [
    refused("moderation", "auth_failure", auth),
    refused("moderation", "rate_limit", rate),
    executed("moderation", exec),
  ];
  // First sample only stores the baseline, even with nonzero refusals.
  const first = ev(mod(5));
  assert.deepEqual(first.firing, []);
  // One or two rising windows stay silent: a single forged pre-auth probe
  // (always family `other`) must not page.
  const second = ev(mod(6, 2, 100), first.state);
  assert.deepEqual(second.firing, []);
  const singleOther = ev(
    [`two_bot_internal_actions_total{family="other",outcome="auth_failure"} 1`],
    ev([`two_bot_internal_actions_total{family="other",outcome="auth_failure"} 0`]).state,
  );
  assert.deepEqual(singleOther.firing, []);
  // Two rising windows stay silent; the third consecutive rise fires with
  // the family subject; executed growth alone stays silent.
  const third = ev(mod(7, 2, 101), second.state);
  assert.deepEqual(third.firing, []);
  const fire = ev(mod(8, 2, 102), third.state);
  assert.deepEqual(fire.firing, ["receiver_refusals:moderation"]);
  // A flat window clears the streak and recovers (no increase, no fire).
  const flat = ev(mod(8, 2, 103), fire.state);
  assert.deepEqual(flat.firing, []);
  assert.equal(flat.state.receiverRefusalStreaks["moderation"], 0);
  // Other families track their own streak: one `membership` refusal stays silent.
  const mixedOne = ev([
    refused("moderation", "auth_failure", 8),
    refused("moderation", "rate_limit", 2),
    refused("membership", "unknown_key", 1),
    executed("membership", 3),
  ], flat.state);
  assert.deepEqual(mixedOne.firing, []);
  // A counter that went backwards means the process restarted: no window.
  const restart = ev([refused("moderation", "auth_failure", 1)], fire.state);
  assert.deepEqual(restart.firing, []);
  assert.equal(restart.state.receiverRefusalStreaks["moderation"] ?? 0, 0);
  // The post-restart baseline needs three fresh rises before firing again.
  const r1 = ev([refused("moderation", "auth_failure", 2)], restart.state);
  const r2 = ev([refused("moderation", "auth_failure", 3)], r1.state);
  assert.deepEqual(r1.firing, []);
  assert.deepEqual(r2.firing, []);
  assert.deepEqual(ev([refused("moderation", "auth_failure", 4)], r2.state).firing, ["receiver_refusals:moderation"]);
});

test("ticker stale fires past 10 minutes, ignores boot, parked and fresh tickers", () => {
  // Boot (never succeeded) and parked (never registered) stay zero: silent.
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="scheduled_messages"} 0`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="settings"} 0`]).firing, []);
  // Fresh ticks are silent; a 10-minute-old success is stale.
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="scheduled_messages"} ${NOW - 600}`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="scheduled_messages"} ${NOW - 601}`]).firing, ["ticker_stale:scheduled_messages"]);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="settings"} ${NOW - 601}`]).firing, ["ticker_stale:settings"]);
  // Recovery: a fresh success after a stale window stops firing.
  const stale = ev([`two_bot_job_last_success_timestamp_seconds{job="settings"} ${NOW - 601}`]);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="settings"} ${NOW - 1}`], stale.state).firing, []);
  // Jobs outside TICKER_STALE_JOBS never fire this rule, however stale.
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="audit_retry"} ${NOW - 3600}`]).firing, []);
  assert.deepEqual(ev([`two_bot_job_last_success_timestamp_seconds{job="rank"} ${NOW - 3600}`]).firing.filter((k) => k.startsWith("ticker_stale")), []);
});

test("fired packets carry the shared rule-id spelling (TOG-12100)", () => {
  const window = "2026-10-09T20-11-06Z";
  // Single shared spelling with the Rust canonical list (ALERT_RULE_IDS in
  // crates/core/src/evidence.rs); both sides pin all ten here and there.
  assert.deepEqual(RULES.map((r) => r.id), [
    "job_stale",
    "job_consecutive_failures",
    "rest_429_rate",
    "db_pool_saturated",
    "db_errors",
    "send_admission_blocked",
    "voice_failures",
    "gateway_missed_events",
    "ticker_stale",
    "receiver_refusals",
  ]);
  assert.equal(packetFilename("job_stale:rank", window), `evidence-job_stale-${window}.json`);
  assert.equal(packetFilename("job_consecutive_failures:counter", window), `evidence-job_consecutive_failures-${window}.json`);
  assert.equal(packetFilename("rest_429_rate", window), `evidence-rest_429_rate-${window}.json`);
  assert.equal(packetFilename("db_pool_saturated", window), `evidence-db_pool_saturated-${window}.json`);
  assert.equal(packetFilename("db_errors", window), `evidence-db_errors-${window}.json`);
  assert.equal(packetFilename("send_admission_blocked", window), `evidence-send_admission_blocked-${window}.json`);
  assert.equal(packetFilename("voice_failures", window), `evidence-voice_failures-${window}.json`);
  assert.equal(packetFilename("gateway_missed_events", window), `evidence-gateway_missed_events-${window}.json`);
  assert.equal(packetFilename("ticker_stale:scheduled_messages", window), `evidence-ticker_stale-${window}.json`);
  assert.equal(packetFilename("receiver_refusals:moderation", window), `evidence-receiver_refusals-${window}.json`);
  // Unknown keys get no filename rather than a misleading one; hostile
  // window stamps stay filename-safe.
  assert.equal(packetFilename("no_such_rule", window), undefined);
  assert.equal(packetFilename("rest_429_rate", "2026-10-09T20:11:06Z"), "evidence-rest_429_rate-2026-10-09T20-11-06Z.json");
});

test("every rule links to an existing runbook heading", () => {
  const slugs = docAnchors("runbook.md");
  for (const rule of RULES) {
    const anchor = rule.runbook.split("#")[1]!;
    assert.ok(slugs.has(anchor), `${rule.id}: missing runbook heading for #${anchor}`);
  }
});

test("every fired packet carries a runbook deep link that resolves in checked-in docs", () => {
  const slugs = docAnchors("runbook.md");
  // Fire one of each rule kind so every RULES entry is covered.
  const firing = [
    ...ev([`two_bot_job_last_success_timestamp_seconds{job="rank"} ${NOW - 1201}`]).firing,
    ...ev([`two_bot_job_consecutive_failures{job="counter"} 3`]).firing,
    ...ev(
      [`two_bot_rest_requests_total{route="other",result="2xx"} 180`, `two_bot_rest_requests_total{route="other",result="429"} 20`],
      ev([`two_bot_rest_requests_total{route="other",result="2xx"} 100`, `two_bot_rest_requests_total{route="other",result="429"} 0`]).state,
    ).firing,
  ];
  const poolLines = ["two_bot_db_pool_configured 1", "two_bot_db_pool_connections 5", "two_bot_db_pool_max_connections 5", "two_bot_db_pool_idle_connections 0"];
  let pool = EMPTY_STATE;
  for (let i = 0; i < 3; i++) pool = ev(poolLines, pool).state;
  firing.push(...pool.firing);
  firing.push(
    ...ev(
      [`two_bot_db_errors_total{op="admission"} 5`, `two_bot_db_errors_total{op="other"} 0`],
      ev([`two_bot_db_errors_total{op="admission"} 0`, `two_bot_db_errors_total{op="other"} 0`]).state,
    ).firing,
  );
  let blocked = EMPTY_STATE;
  for (let i = 1; i <= 3; i++) {
    blocked = ev([`two_bot_send_admissions_total{outcome="blocked"} ${i}`], blocked).state;
  }
  firing.push(...blocked.firing);
  firing.push(
    ...ev([
      `two_bot_voice_operations_total{op="create",outcome="success"} 42`,
      `two_bot_voice_operations_total{op="create",outcome="discord"} 7`,
      `two_bot_voice_operations_total{op="move",outcome="persistence"} 4`,
      `two_bot_voice_dead_letters_total{action="create"} 3`,
    ]).firing,
  );
  firing.push(
    ...ev(
      [`two_bot_gateway_missed_events_total 1`],
      ev([`two_bot_gateway_missed_events_total 0`]).state,
    ).firing,
  );
  firing.push(
    ...ev([`two_bot_job_last_success_timestamp_seconds{job="scheduled_messages"} ${NOW - 601}`]).firing,
  );
  let refused = ev([`two_bot_internal_actions_total{family="moderation",outcome="auth_failure"} 0`]);
  for (const n of [1, 2, 3]) {
    refused = ev([`two_bot_internal_actions_total{family="moderation",outcome="auth_failure"} ${n}`], refused.state);
  }
  firing.push(...refused.firing);
  assert.equal(firing.length, RULES.length, `expected one firing key per rule, got: ${firing.join(", ")}`);
  const packets = transitionMessages([], firing);
  assert.equal(packets.length, RULES.length);
  for (const rule of RULES) {
    const packet = packets.find((p) => p.includes(`: ${rule.summary}. Runbook: `));
    assert.ok(packet, `${rule.id}: no fired packet carries its summary`);
    assert.ok(packet!.includes(runbookUrl(rule)), `${rule.id}: packet lacks full deep link ${runbookUrl(rule)}`);
    const url = packet!.slice(packet!.indexOf("Runbook: ") + "Runbook: ".length);
    assert.ok(url.startsWith(RUNBOOK_BASE_URL), `${rule.id}: runbook is not a full docs URL: ${url}`);
    const anchor = url.slice(`${RUNBOOK_BASE_URL}runbook.md#`.length);
    assert.ok(slugs.has(anchor), `${rule.id}: packet URL anchor #${anchor} missing from docs/runbook.md`);
    assert.equal(ruleFor(firing.find((k) => k.startsWith(rule.id))!)?.id, rule.id);
  }
});

test("/ops/metrics: 404 without a configured token, 401 without/with a wrong bearer, proxied with the right one", async () => {
  const calls: Request[] = [];
  const env = (token?: string) => ({
    // The fence stamps the deployment header on every DO forward, so the
    // fixture must carry the version-metadata binding like production.
    CF_VERSION_METADATA: { id: "synthetic-metrics-deployment" },
    METRICS_SCRAPE_TOKEN: token,
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_BOT: { getByName: () => ({ fetch: async (r: Request) => { calls.push(r); return new Response("ok"); } }) },
  }) as unknown as Env;
  const ctx = {} as ExecutionContext;
  const token = "synthetic-alert-rules-scrape-token-0123456789";
  const get = (e: Env, auth?: string) =>
    worker.fetch(new Request("https://w.invalid/ops/metrics", { headers: auth ? { authorization: auth } : {} }), e, ctx);
  assert.equal((await get(env(), "Bearer anything")).status, 404);
  assert.equal((await get(env("short"))).status, 404, "short token is not configured");
  assert.equal((await get(env(token))).status, 401);
  assert.equal((await get(env(token), "Bearer nope")).status, 401);
  assert.equal((await get(env(token), "Basic nope")).status, 401);
  assert.equal(calls.length, 0, "unauthenticated requests never reach the container");
  const ok = await get(env(token), `Bearer ${token}`);
  assert.equal(ok.status, 200);
  assert.equal(calls.length, 1);
  const post = await worker.fetch(new Request("https://w.invalid/ops/metrics", { method: "POST", headers: { authorization: `Bearer ${token}` } }), env(token), ctx);
  assert.equal(post.status, 404);
});
