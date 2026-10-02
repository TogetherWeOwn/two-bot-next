import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import worker, { type Env } from "../src/index.ts";
import {
  EMPTY_STATE, RULES, evaluateMetrics, parseExposition, transitionMessages,
} from "../src/alert-rules.ts";

const NOW = 1_000_000;
function body(extra: string[]): string {
  return ["# HELP x y", "# TYPE x gauge", ...extra, ""].join("\n");
}
const ev = (lines: string[], prev = EMPTY_STATE) => evaluateMetrics(parseExposition(body(lines)), prev, NOW);

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

test("every rule links to an existing runbook heading", () => {
  const runbook = readFileSync(new URL("../../docs/runbook.md", import.meta.url), "utf8");
  const slugs = new Set([...runbook.matchAll(/^#+ (.+)$/gm)].map((m) =>
    m[1]!.toLowerCase().replace(/[^a-z0-9 -]/g, "").replace(/ /g, "-")));
  for (const rule of RULES) {
    const anchor = rule.runbook.split("#")[1]!;
    assert.ok(slugs.has(anchor), `${rule.id}: missing runbook heading for #${anchor}`);
  }
});

test("/ops/metrics: 404 without a configured token, 401 without/with a wrong bearer, proxied with the right one", async () => {
  const calls: Request[] = [];
  const env = (token?: string) => ({
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
