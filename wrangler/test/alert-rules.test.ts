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

test("fired packets carry the shared rule-id spelling (TOG-12100)", () => {
  const window = "2026-10-09T20-11-06Z";
  // Single shared spelling with the Rust canonical list (ALERT_RULE_IDS in
  // crates/core/src/evidence.rs); both sides pin all four here and there.
  assert.deepEqual(RULES.map((r) => r.id), [
    "job_stale",
    "job_consecutive_failures",
    "rest_429_rate",
    "db_pool_saturated",
  ]);
  assert.equal(packetFilename("job_stale:rank", window), `evidence-job_stale-${window}.json`);
  assert.equal(packetFilename("job_consecutive_failures:counter", window), `evidence-job_consecutive_failures-${window}.json`);
  assert.equal(packetFilename("rest_429_rate", window), `evidence-rest_429_rate-${window}.json`);
  assert.equal(packetFilename("db_pool_saturated", window), `evidence-db_pool_saturated-${window}.json`);
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
  const get = (e: Env, auth?: string) =>
    worker.fetch(new Request("https://w.invalid/ops/metrics", { headers: auth ? { authorization: auth } : {} }), e, ctx);
  assert.equal((await get(env(), "Bearer anything")).status, 404);
  assert.equal((await get(env("s3cret"))).status, 401);
  assert.equal((await get(env("s3cret"), "Bearer nope")).status, 401);
  assert.equal((await get(env("s3cret"), "Basic s3cret")).status, 401);
  assert.equal(calls.length, 0, "unauthenticated requests never reach the container");
  const ok = await get(env("s3cret"), "Bearer s3cret");
  assert.equal(ok.status, 200);
  assert.equal(calls.length, 1);
  const post = await worker.fetch(new Request("https://w.invalid/ops/metrics", { method: "POST", headers: { authorization: "Bearer s3cret" } }), env("s3cret"), ctx);
  assert.equal(post.status, 404);
});
