/**
 * Acceptance tests (protected): per-credential budgets on the
 * `GET /ops/summary` dual gate.
 *
 * Contract under test: the summary route accepts either the scrape bearer
 * or the dashboard service-token pair, and each credential has its own
 * per-caller attempt budget (10 burst at the time of writing). Guessing at
 * one credential must not throttle the other: a bucket is spent only when
 * that credential type is actually presented and evaluated.
 *
 * - A request presenting only a scrape bearer (valid or not) spends scrape
 *   budget only. Failed bearer guesses never spend dashboard budget, and
 *   a dashboard-only request is not refused by the scrape gate.
 * - A request presenting only the dashboard pair (valid or not) spends
 *   dashboard budget only, symmetrically.
 * - A request presenting a bearer is evaluated by the scrape gate first:
 *   while that caller is scrape-throttled (429), adding a valid dashboard
 *   pair to a bearer-presenting request does not bypass the fence.
 *   Symmetrically, a dashboard-throttled caller cannot bypass its 429 by
 *   adding a wrong bearer to its dashboard request.
 * - Preserved semantics: either valid credential forwards, a wrong bearer
 *   plus a valid dashboard pair still falls through and forwards, each
 *   gate still throttles its own guessing and its own legitimate burst,
 *   unconfigured gates 404, missing/wrong credentials against a configured
 *   gate 401, non-GET 404, and refused requests never reach the container.
 *
 * These tests were written test-first against a baseline where every
 * fall-through request spent a token from BOTH buckets (verified
 * empirically: 10 wrong bearers from one caller make a following valid
 * dashboard request 429, and 10 wrong dashboard attempts make a following
 * valid scrape request 429). The budget-isolation tests FAIL there and
 * pass once each bucket is taken only for the credential presented; the
 * remaining tests are positive controls that stay green throughout. Do
 * not edit this file to make an implementation pass; fix the
 * implementation.
 *
 * Synthetic env only; the container is an in-memory stub answering the
 * parked 503 readiness report. No deployed Worker, Access tenant,
 * database or real secret is contacted. Bursts run well inside the
 * buckets' one-token-per-second refill, matching the neighboring
 * per-caller throttle tests.
 *
 * Run: node --import ./test/cloudflare-loader.mjs --test test/ops-summary-dual-gate-budget-acceptance.test.ts
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";
import {
  DASHBOARD_CLIENT_ID_HEADER,
  DASHBOARD_CLIENT_SECRET_HEADER,
  OPS_SUMMARY_PATH,
} from "../src/ops-dashboard.ts";

const DEPLOYMENT = "synthetic-dual-gate-deployment";
const CLIENT_ID = "synthetic-dual-gate-client-id-0123456789";
const CLIENT_SECRET = "synthetic-dual-gate-client-secret-0123456789abcdef";
const SCRAPE_TOKEN = "synthetic-dual-gate-scrape-token-0123456789abcdef";
const WRONG_BEARER = "Bearer wrong-bearer-not-a-secret-0123456789";
const SUMMARY_BODY =
  '{"components":[["process","ready"],["gateway","down"]],"jobs":{},"build_revision":"synthetic","build_id":"synthetic"}';
/** Per-caller burst both gates allow before throttling (pinned behavior). */
const BURST = 10;

const dashCreds = {
  [DASHBOARD_CLIENT_ID_HEADER]: CLIENT_ID,
  [DASHBOARD_CLIENT_SECRET_HEADER]: CLIENT_SECRET,
};
const wrongDashCreds = {
  [DASHBOARD_CLIENT_ID_HEADER]: "wrong-client-id-not-a-secret-0123456789",
  [DASHBOARD_CLIENT_SECRET_HEADER]: "wrong-client-secret-not-a-secret-0123456789",
};
const bearer = { authorization: `Bearer ${SCRAPE_TOKEN}` };
const wrongBearer = { authorization: WRONG_BEARER };

function workerEnv(secrets?: { id?: string; secret?: string; scrape?: string }) {
  const calls: Request[] = [];
  const env = {
    CF_VERSION_METADATA: { id: DEPLOYMENT },
    OPS_DASHBOARD_CLIENT_ID: secrets?.id,
    OPS_DASHBOARD_CLIENT_SECRET: secrets?.secret,
    METRICS_SCRAPE_TOKEN: secrets?.scrape,
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_BOT: {
      getByName: () => ({
        fetch: async (r: Request) => {
          calls.push(r);
          return new Response(SUMMARY_BODY, {
            status: 503,
            headers: { "content-type": "application/json" },
          });
        },
      }),
    },
  } as unknown as Env;
  return { env, calls };
}

const bothConfigured = () =>
  workerEnv({ id: CLIENT_ID, secret: CLIENT_SECRET, scrape: SCRAPE_TOKEN });

const ctx = {} as ExecutionContext;
// Attempt buckets are module-level per caller, so every scenario gets a
// caller id nobody else in this isolate has spent from.
let callerSeq = 0;
const freshCaller = (label: string) => `dual-gate-${label}-${++callerSeq}`;

function summary(
  env: Env,
  ip: string,
  headers: Record<string, string>,
  init?: RequestInit,
) {
  return worker.fetch(
    new Request(`https://w.invalid${OPS_SUMMARY_PATH}`, {
      ...init,
      headers: { ...headers, "cf-connecting-ip": ip },
    }),
    env,
    ctx,
  );
}

/** Send `n` identical requests from one caller, asserting each status. */
async function burst(
  env: Env,
  ip: string,
  headers: Record<string, string>,
  n: number,
  expected: number,
  label: string,
) {
  for (let i = 0; i < n; i++) {
    const res = await summary(env, ip, headers);
    assert.equal(res.status, expected, `${label} ${i + 1}/${n} must be ${expected}`);
    await res.text();
  }
}

/** A forwarded summary read: the stub's parked 503 report, untouched. */
async function assertForwards(res: Response, label: string) {
  assert.equal(res.status, 503, `${label} must forward to the container`);
  assert.equal(await res.text(), SUMMARY_BODY, `${label} must proxy the summary body`);
}

async function assertThrottled(res: Response, label: string) {
  assert.equal(res.status, 429, `${label} must be throttled`);
  assert.ok(Number(res.headers.get("retry-after")) >= 1, `${label} must carry retry-after`);
  await res.text();
}

// ---------------------------------------------------------------------------
// Failing-first: budget isolation between the two gates
// ---------------------------------------------------------------------------

test("wrong scrape-bearer guesses do not spend the dashboard budget", async () => {
  const { env, calls } = bothConfigured();
  const ip = freshCaller("bearer-guesses");
  // A full burst of bearer guesses from one caller (shared NAT, stale
  // scraper): each is a scrape 401 and none reaches the container.
  await burst(env, ip, wrongBearer, BURST, 401, "wrong bearer");
  assert.equal(calls.length, 0, "refused guesses never reach the container");
  // The legitimate dashboard user behind the same caller id is not locked
  // out: a valid dashboard pair still forwards.
  await assertForwards(await summary(env, ip, dashCreds), "valid dashboard after bearer guesses");
  assert.equal(calls.length, 1, "only the dashboard forward reaches the container");
});

test("wrong dashboard guesses do not spend the scrape budget (reverse isolation)", async () => {
  const { env, calls } = bothConfigured();
  const ip = freshCaller("dashboard-guesses");
  await burst(env, ip, wrongDashCreds, BURST, 401, "wrong dashboard pair");
  assert.equal(calls.length, 0, "refused guesses never reach the container");
  // The legitimate scraper behind the same caller id still forwards.
  await assertForwards(await summary(env, ip, bearer), "valid scrape after dashboard guesses");
  assert.equal(calls.length, 1, "only the scrape forward reaches the container");
});

test("legitimate dashboard forwards do not spend the scrape budget", async () => {
  const { env, calls } = bothConfigured();
  const ip = freshCaller("dashboard-burst");
  await burst(env, ip, dashCreds, BURST, 503, "valid dashboard forward");
  assert.equal(calls.length, BURST);
  // Dashboard successes evaluated no scrape credential: the scrape budget
  // for this caller is untouched.
  await assertForwards(await summary(env, ip, bearer), "valid scrape after dashboard burst");
  assert.equal(calls.length, BURST + 1);
});

test("legitimate scrape forwards do not lock out a dashboard-only request", async () => {
  const { env, calls } = bothConfigured();
  const ip = freshCaller("scrape-burst");
  await burst(env, ip, bearer, BURST, 503, "valid scrape forward");
  assert.equal(calls.length, BURST);
  // A dashboard-only request presents no bearer, so it is not refused by
  // the spent scrape gate; its own budget is full.
  await assertForwards(await summary(env, ip, dashCreds), "valid dashboard after scrape burst");
  assert.equal(calls.length, BURST + 1);
});

// ---------------------------------------------------------------------------
// Controls: fences, own-gate budgets and preserved semantics (green before
// and after the fix)
// ---------------------------------------------------------------------------

test("scrape 429 is a hard fence for bearer-presenting requests, valid dashboard pair included", async () => {
  // Exhaust the scrape budget with legitimate forwards, then present both
  // credentials at once: the scrape gate runs first for a request that
  // carries a bearer, and its 429 cannot be bypassed via the dashboard.
  {
    const { env, calls } = bothConfigured();
    const ip = freshCaller("fence-valid-bearer");
    await burst(env, ip, bearer, BURST, 503, "valid scrape forward");
    assert.equal(calls.length, BURST);
    await assertThrottled(
      await summary(env, ip, { ...bearer, ...dashCreds }),
      "both credentials while scrape-throttled",
    );
    assert.equal(calls.length, BURST, "fenced attempts never reach the container");
  }
  // Same fence after the bucket was spent on wrong-bearer guesses: adding
  // a valid dashboard pair to a (still bearer-presenting) retry stays 429.
  {
    const { env, calls } = bothConfigured();
    const ip = freshCaller("fence-wrong-bearer");
    await burst(env, ip, wrongBearer, BURST, 401, "wrong bearer");
    await assertThrottled(
      await summary(env, ip, { ...wrongBearer, ...dashCreds }),
      "wrong bearer plus valid dashboard while scrape-throttled",
    );
    assert.equal(calls.length, 0, "fenced attempts never reach the container");
  }
});

test("dashboard 429 cannot be bypassed by adding a wrong bearer", async () => {
  const { env, calls } = bothConfigured();
  const ip = freshCaller("dashboard-fence");
  await burst(env, ip, dashCreds, BURST, 503, "valid dashboard forward");
  assert.equal(calls.length, BURST);
  await assertThrottled(
    await summary(env, ip, { ...wrongBearer, ...dashCreds }),
    "wrong bearer plus valid dashboard while dashboard-throttled",
  );
  assert.equal(calls.length, BURST, "throttled attempts never reach the container");
});

test("each gate still throttles its own legitimate burst", async () => {
  {
    const { env, calls } = bothConfigured();
    const ip = freshCaller("own-scrape");
    await burst(env, ip, bearer, BURST, 503, "valid scrape forward");
    await assertThrottled(await summary(env, ip, bearer), "scrape burst + 1");
    assert.equal(calls.length, BURST, "throttled attempts never reach the container");
  }
  {
    const { env, calls } = bothConfigured();
    const ip = freshCaller("own-dashboard");
    await burst(env, ip, dashCreds, BURST, 503, "valid dashboard forward");
    await assertThrottled(await summary(env, ip, dashCreds), "dashboard burst + 1");
    assert.equal(calls.length, BURST, "throttled attempts never reach the container");
  }
});

test("each gate still throttles its own guessing", async () => {
  {
    const { env, calls } = bothConfigured();
    const ip = freshCaller("guess-scrape");
    await burst(env, ip, wrongBearer, BURST, 401, "wrong bearer");
    await assertThrottled(await summary(env, ip, wrongBearer), "wrong bearer burst + 1");
    assert.equal(calls.length, 0);
  }
  {
    const { env, calls } = bothConfigured();
    const ip = freshCaller("guess-dashboard");
    await burst(env, ip, wrongDashCreds, BURST, 401, "wrong dashboard pair");
    await assertThrottled(await summary(env, ip, wrongDashCreds), "wrong dashboard burst + 1");
    assert.equal(calls.length, 0);
  }
});

test("either valid credential forwards, alone or combined with a wrong peer", async () => {
  const { env, calls } = bothConfigured();
  await assertForwards(
    await summary(env, freshCaller("valid-scrape"), bearer),
    "valid scrape alone",
  );
  await assertForwards(
    await summary(env, freshCaller("valid-dashboard"), dashCreds),
    "valid dashboard alone",
  );
  // Wrong bearer + valid dashboard pair falls through to the dashboard.
  await assertForwards(
    await summary(env, freshCaller("fallthrough"), { ...wrongBearer, ...dashCreds }),
    "wrong bearer plus valid dashboard",
  );
  // Valid bearer opens the gate regardless of dashboard headers.
  await assertForwards(
    await summary(env, freshCaller("scrape-first"), { ...bearer, ...wrongDashCreds }),
    "valid bearer plus wrong dashboard",
  );
  assert.equal(calls.length, 4, "every authorized read reaches the container once");
});

test("unconfigured and missing-credential semantics are preserved", async () => {
  // Neither credential configured: the route does not exist, whatever is
  // presented.
  {
    const { env, calls } = workerEnv(undefined);
    for (const [label, headers] of [
      ["no credentials", {}],
      ["scrape bearer", bearer],
      ["dashboard pair", dashCreds],
      ["both", { ...bearer, ...dashCreds }],
    ] as const) {
      const res = await summary(env, freshCaller(`unconfigured-${label}`), { ...headers });
      assert.equal(res.status, 404, `unconfigured route with ${label} must 404`);
      await res.text();
    }
    assert.equal(calls.length, 0, "unconfigured routes never reach the container");
  }
  // Only the scrape gate configured: dashboard credentials are a 401, not
  // a forward and not a 404.
  {
    const { env, calls } = workerEnv({ scrape: SCRAPE_TOKEN });
    const res = await summary(env, freshCaller("scrape-only"), dashCreds);
    assert.equal(res.status, 401);
    await res.text();
    assert.equal(calls.length, 0);
  }
  // Only the dashboard gate configured: a scrape bearer is a 401.
  {
    const { env, calls } = workerEnv({ id: CLIENT_ID, secret: CLIENT_SECRET });
    const res = await summary(env, freshCaller("dashboard-only"), bearer);
    assert.equal(res.status, 401);
    await res.text();
    assert.equal(calls.length, 0);
  }
  // Both configured, nothing (or only half a dashboard pair) presented.
  {
    const { env, calls } = bothConfigured();
    for (const [label, headers] of [
      ["no credentials", {}],
      ["client id only", { [DASHBOARD_CLIENT_ID_HEADER]: CLIENT_ID }],
      ["client secret only", { [DASHBOARD_CLIENT_SECRET_HEADER]: CLIENT_SECRET }],
    ] as const) {
      const res = await summary(env, freshCaller(`missing-${label}`), { ...headers });
      assert.equal(res.status, 401, `${label} must 401 while a gate is configured`);
      await res.text();
    }
    assert.equal(calls.length, 0, "unauthenticated requests never reach the container");
  }
});

test("throttling stays per-caller: a spent caller does not throttle another", async () => {
  const { env, calls } = bothConfigured();
  const spent = freshCaller("spent");
  await burst(env, spent, wrongBearer, BURST, 401, "wrong bearer");
  await assertForwards(
    await summary(env, freshCaller("bystander"), dashCreds),
    "valid dashboard from a different caller",
  );
  assert.equal(calls.length, 1);
});

test("non-GET methods 404 and never reach the container", async () => {
  const { env, calls } = bothConfigured();
  for (const [method, headers] of [
    ["POST", dashCreds],
    ["POST", bearer],
    ["PUT", { ...bearer, ...dashCreds }],
  ] as const) {
    const res = await summary(env, freshCaller(`method-${method}`), { ...headers }, { method });
    assert.equal(res.status, 404, `${method} must 404`);
    await res.text();
  }
  assert.equal(calls.length, 0, "mutations must never reach the container");
});
