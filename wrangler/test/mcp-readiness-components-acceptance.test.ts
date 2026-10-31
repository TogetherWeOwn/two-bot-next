/**
 * Failing-first acceptance: refuse malformed readiness component rows.
 * Test files only — no src changes.
 *
 * Synthetic harness only, following wrangler/test/mcp.test.ts: the stub
 * TWO_BOT records what the Worker forwards; no endpoint, database or
 * secret is contacted.
 *
 * Contract under test (builder: implement exactly this for the /readyz
 * reports excerpted by get_gateway and get_backfill_progress):
 * - The real schema is Vec<(String, ComponentStatus)> serialized as
 *   [name, status] pairs, with status one of "ready", "starting", "down"
 *   (crates/core/src/health.rs). Before excerpting, every component entry
 *   must be an exact two-element pair with a string name and one of those
 *   exact string status values.
 * - Any malformed entry — null/non-array entry, wrong tuple length,
 *   non-string name or status, status outside the vocabulary, hostile
 *   object/array status — takes the existing bounded JSON-RPC error path
 *   (HTTP 200 JSON-RPC error) on both accepted upstream statuses (200 and
 *   JSON 503), for both tools, returning no result/structuredContent and
 *   echoing no upstream content. Every row is validated, not only the
 *   row a tool excerpts.
 * - Valid ready/starting/down values, arbitrary (including empty)
 *   component names, unrelated extra report fields and existing
 *   empty-list semantics are preserved. No gateway component is required
 *   and readiness itself is not redefined.
 * - Authentication, read-only tool scope, time/body bounds and
 *   ownership-fence behaviour are unchanged (pinned in mcp.test.ts).
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";

const MCP_TOKEN = "synthetic-mcp-bearer-token-0123456789abcd";
const SENTINEL = "synthetic-upstream-sentinel-must-not-echo";

interface Forwarded {
  method: string;
  url: string;
  headers: Record<string, string>;
  body: string;
}

function harness(respond: (req: Request) => Response | Promise<Response>) {
  const forwarded: Forwarded[] = [];
  const names: string[] = [];
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    // The ownership fence refuses forwards without a deployment id.
    CF_VERSION_METADATA: { id: "synthetic-mcp-deployment" },
    MCP_TOKEN,
    TWO_BOT: {
      getByName: (name: string) => {
        names.push(name);
        return {
          fetch: async (r: Request) => {
            const headers: Record<string, string> = {};
            r.headers.forEach((value, key) => {
              headers[key] = value;
            });
            forwarded.push({ method: r.method, url: r.url, headers, body: await r.text() });
            return respond(r);
          },
        };
      },
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  return { env, ctx, forwarded, names };
}

let rpcCallerSeq = 0;
const rpc = (method: string, params: unknown) => {
  // Same per-caller budget isolation as mcp.test.ts.
  rpcCallerSeq += 1;
  return new Request("https://probe.invalid/mcp", {
    method: "POST",
    headers: {
      authorization: `Bearer ${MCP_TOKEN}`,
      "content-type": "application/json",
      "cf-connecting-ip": `198.51.100.${(rpcCallerSeq % 200) + 1}`,
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
};

const JOBS_FIXTURE = {
  backfill: {
    parked: false,
    running: false,
    last_start: null,
    last_success: null,
    last_error_class: null,
    consecutive_failures: 0,
  },
};

const TOOLS = ["get_gateway", "get_backfill_progress"] as const;
type Tool = (typeof TOOLS)[number];

/** One real tools/call against a stub /readyz report. Returns raw text. */
async function callWithReport(
  tool: Tool,
  report: unknown,
  status: 200 | 503,
): Promise<{ resStatus: number; text: string; body: any; forwarded: Forwarded[] }> {
  const h = harness(() => Response.json(report, { status }));
  const res = await worker.fetch(
    rpc("tools/call", { name: tool, arguments: {} }),
    h.env,
    h.ctx,
  );
  const text = await res.text();
  return { resStatus: res.status, text, body: JSON.parse(text), forwarded: h.forwarded };
}

// ---------------------------------------------------------------- refusals

interface MalformedCase {
  name: string;
  components: unknown;
  /** Extra forbidden substrings specific to this case, besides SENTINEL. */
  forbidden?: string[];
}

const MALFORMED: MalformedCase[] = [
  { name: "null entry", components: [null] },
  { name: "string entry (not a pair)", components: ["gateway"] },
  { name: "number entry", components: [42] },
  { name: "boolean entry", components: [true] },
  { name: "object entry (not a pair)", components: [{ gateway: "ready" }] },
  { name: "zero-element entry", components: [[]] },
  { name: "one-element entry", components: [["gateway"]] },
  {
    name: "three-element entry",
    components: [["gateway", "ready", SENTINEL]],
  },
  { name: "non-string (number) name", components: [[42, "ready"]] },
  { name: "non-string (null) name", components: [[null, "ready"]] },
  { name: "non-string (array) name", components: [[["gateway"], "ready"]] },
  { name: "non-string (object) name", components: [[{ name: SENTINEL }, "ready"]] },
  { name: "non-string (number) status", components: [["gateway", 42]] },
  { name: "non-string (null) status", components: [["gateway", null]] },
  { name: "non-string (boolean) status", components: [["gateway", true]] },
  {
    name: "hostile object status",
    components: [["gateway", { unexpected: SENTINEL }]],
  },
  { name: "array status", components: [["gateway", ["ready"]]] },
  {
    name: "status outside the vocabulary",
    components: [["gateway", `${SENTINEL}-status`]],
    forbidden: [`${SENTINEL}-status`],
  },
  { name: "capitalised status is not the vocabulary", components: [["gateway", "Ready"]] },
  { name: "upper-case status is not the vocabulary", components: [["gateway", "READY"]] },
  { name: "whitespace-padded status", components: [["gateway", " ready "]] },
  { name: "empty status", components: [["gateway", ""]] },
  {
    name: "malformed row after valid rows (every row is validated)",
    components: [
      ["process", "ready"],
      ["gateway", "ready"],
      [null],
    ],
  },
  {
    name: "malformed non-gateway row beside a valid gateway row",
    components: [
      ["gateway", "ready"],
      ["database", "bogus"],
    ],
  },
  {
    name: "malformed row before a valid gateway row",
    components: [
      ["database", 503],
      ["gateway", "ready"],
    ],
  },
];

for (const c of MALFORMED) {
  for (const tool of TOOLS) {
    for (const status of [200, 503] as const) {
      test(`tools/call ${tool} refuses ${c.name} on /readyz ${status}`, async () => {
        // jobs / gateway_failure carry the sentinel in fields each tool
        // would excerpt on success: a refusal must echo none of it.
        const report = {
          components: c.components,
          jobs: { marker: SENTINEL },
          gateway_failure: { marker: SENTINEL },
        };
        const { resStatus, text, body, forwarded } = await callWithReport(tool, report, status);
        assert.equal(resStatus, 200, "refusal stays on the JSON-RPC error channel");
        assert.equal(body.jsonrpc, "2.0");
        assert.equal(body.id, 1);
        assert.ok(body.error, "malformed rows must surface a JSON-RPC error");
        assert.equal(
          body.error.code,
          -32000,
          "malformed rows take the existing bounded container-failure error path",
        );
        assert.equal(typeof body.error.message, "string");
        assert.equal(body.result, undefined, "refusal must carry no result");
        assert.ok(!text.includes("structuredContent"), "refusal must carry no structuredContent");
        assert.ok(!text.includes(SENTINEL), "refusal must echo no upstream sentinel/content");
        for (const forbidden of c.forbidden ?? []) {
          assert.ok(!text.includes(forbidden), `refusal must not echo ${forbidden}`);
        }
        assert.equal(forwarded.length, 1, "one tool call proxies exactly one container fetch");
        assert.equal(forwarded[0]!.method, "GET");
        assert.equal(
          forwarded[0]!.headers["authorization"],
          undefined,
          "caller bearer must not be forwarded",
        );
      });
    }
  }
}

// ----------------------------------------------------------------- controls

for (const statusValue of ["ready", "starting", "down"] as const) {
  for (const status of [200, 503] as const) {
    test(`tools/call get_gateway preserves a valid gateway=${statusValue} row on /readyz ${status}`, async () => {
      const report = {
        components: [
          ["process", "ready"],
          ["gateway", statusValue],
          ["database", "starting"],
        ],
        jobs: JOBS_FIXTURE,
      };
      const { resStatus, body } = await callWithReport("get_gateway", report, status);
      assert.equal(resStatus, 200);
      assert.equal(body.error, undefined);
      assert.deepEqual(body.result?.structuredContent, { gateway: statusValue });
    });

    test(`tools/call get_backfill_progress preserves jobs beside gateway=${statusValue} on /readyz ${status}`, async () => {
      const report = {
        components: [
          ["process", "ready"],
          ["gateway", statusValue],
        ],
        jobs: JOBS_FIXTURE,
      };
      const { resStatus, body } = await callWithReport("get_backfill_progress", report, status);
      assert.equal(resStatus, 200);
      assert.equal(body.error, undefined);
      assert.deepEqual(body.result?.structuredContent, {
        implemented: false,
        source: "readyz.jobs",
        jobs: JOBS_FIXTURE,
      });
    });
  }
}

for (const status of [200, 503] as const) {
  test(`tools/call get_gateway preserves arbitrary names and extra report fields on /readyz ${status}`, async () => {
    const failure = { phase: "durable_gateway", class: "timeout" };
    const report = {
      components: [
        ["", "ready"],
        ["weird-name-☃", "starting"],
        ["gateway", "down"],
      ],
      jobs: JOBS_FIXTURE,
      gateway_failure: failure,
      unrelated_extra_field: { nested: ["anything"] },
    };
    const gateway = await callWithReport("get_gateway", report, status);
    assert.equal(gateway.body.error, undefined);
    assert.deepEqual(gateway.body.result?.structuredContent, {
      gateway: "down",
      gateway_failure: failure,
    });
    const backfill = await callWithReport("get_backfill_progress", report, status);
    assert.equal(backfill.body.error, undefined);
    assert.deepEqual(backfill.body.result?.structuredContent, {
      implemented: false,
      source: "readyz.jobs",
      jobs: JOBS_FIXTURE,
    });
  });

  test(`empty components list keeps existing semantics on /readyz ${status}`, async () => {
    const withJobs = { components: [], jobs: JOBS_FIXTURE };
    const gateway = await callWithReport("get_gateway", withJobs, status);
    assert.equal(gateway.body.error, undefined, "empty list is a valid report, not a refusal");
    assert.deepEqual(gateway.body.result?.structuredContent, {});
    const backfill = await callWithReport("get_backfill_progress", withJobs, status);
    assert.equal(backfill.body.error, undefined);
    assert.deepEqual(backfill.body.result?.structuredContent, {
      implemented: false,
      source: "readyz.jobs",
      jobs: JOBS_FIXTURE,
    });
    const bare = await callWithReport("get_backfill_progress", { components: [] }, status);
    assert.equal(bare.body.error, undefined);
    assert.deepEqual(bare.body.result?.structuredContent, {
      implemented: false,
      source: "readyz.jobs",
      jobs: {},
    });
  });

  test(`no gateway component is required on /readyz ${status}`, async () => {
    const report = {
      components: [
        ["process", "ready"],
        ["database", "down"],
      ],
      jobs: JOBS_FIXTURE,
    };
    const gateway = await callWithReport("get_gateway", report, status);
    assert.equal(gateway.body.error, undefined, "a report without gateway must not be refused");
    assert.deepEqual(gateway.body.result?.structuredContent, {});
    const backfill = await callWithReport("get_backfill_progress", report, status);
    assert.equal(backfill.body.error, undefined);
    assert.deepEqual(backfill.body.result?.structuredContent, {
      implemented: false,
      source: "readyz.jobs",
      jobs: JOBS_FIXTURE,
    });
  });
}
