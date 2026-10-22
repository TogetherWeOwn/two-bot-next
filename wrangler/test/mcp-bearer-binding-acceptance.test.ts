/**
 * Failing acceptance pinning the MCP control-tools bearer binding
 * (approved admin API spec section 3.2 landing zone). The binding MUST
 * be pinned before slice 4 registers the
 * first control tool: this file is that pin.
 *
 * Two halves. The closed half (green before and after) proves no control
 * tool is registered: `tools/list` advertises exactly the five read tools,
 * and probe-named control calls answer `-32601` without touching the
 * container. The seam half is RED until slice 1: it loads the pure binding
 * module and pins how control tools hold, scope, and step up the token.
 * Test files only; no `src` changes. The Builder builds on this branch and
 * never edits this file.
 *
 * Pinned seam contract (`wrangler/src/mcp-control-binding.ts`, which the
 * Builder creates; the Builder honors or renegotiates on the card):
 *
 * ```ts
 * export type McpControlScope = "read" | "moderate" | "admin";
 * export interface McpControlBinding {
 *   readonly bearer: string;
 *   readonly scopes: readonly McpControlScope[];
 *   readonly steppedUp: boolean;
 * }
 * export class ControlBindingError extends Error { readonly code: string; }
 * export function bindControlBearer(
 *   bearer: string | null | undefined,
 *   scopes: readonly McpControlScope[],
 *   steppedUp?: boolean,
 * ): McpControlBinding;
 * export function requireControlScope(
 *   binding: McpControlBinding | null | undefined,
 *   scope: McpControlScope,
 * ): void;
 * ```
 *
 * Behavior: the binding holds the bearer per request (two bindings never
 * cross-talk, and an absent bearer never resolves to an ambient token);
 * scope is exact membership in the binding's grants; step-up is an explicit
 * per-binding elevation, checked after scope. Refusal codes, in check
 * order: `control_binding_required`, `control_scope_insufficient`,
 * `control_step_up_required`. Refusals never echo the bearer, and the seam
 * is pure and synchronous (local checks only, no fetch). Scope ORDER and
 * the scope-to-verb map stay slice 0's; the step-up ceremony and the audit
 * write stay slices 4 and 2's — this file pins the binding they all share.
 * This seam is NOT registry authority: caller-supplied scopes and step-up
 * cannot authorize a container action. The HTTP bearer-proof suite pins
 * that security boundary, including forged claims and legacy v1 attempts.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";

const MCP_TOKEN = "synthetic-binding-mcp-token-0123456789ab";
const SEAM_PATH = "../src/mcp-control-binding.ts";
const PROBE_TOOLS = ["control.probe_no_such_tool_acceptance", "moderation.probe_no_such_tool"];

function mcpHarness() {
  const names: string[] = [];
  const forwarded: string[] = [];
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    CF_VERSION_METADATA: { id: "synthetic-binding-deployment" },
    MCP_TOKEN,
    TWO_BOT: {
      getByName(name: string) {
        names.push(name);
        return {
          async fetch(req: Request) {
            forwarded.push(req.url);
            return Response.json({ synthetic: true });
          },
        };
      },
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  return { env, ctx, names, forwarded };
}

let callerSeq = 0;
function mcpCall(body: unknown): Request {
  return new Request("https://probe.invalid/mcp", {
    method: "POST",
    headers: {
      authorization: `Bearer ${MCP_TOKEN}`,
      "content-type": "application/json",
      "cf-connecting-ip": `192.0.2.${(callerSeq++ % 200) + 1}`,
    },
    body: JSON.stringify(body),
  });
}

/** Load the pinned seam module. The path is a variable (never a literal)
 * so the missing module fails each test by assertion with its reason,
 * instead of failing the file to load. */
async function loadSeam(): Promise<Record<string, any>> {
  try {
    return (await import(SEAM_PATH)) as Record<string, any>;
  } catch (error) {
    assert.fail(
      `control-binding seam absent at ${SEAM_PATH} (slice 1 must add it): ` +
        `${error instanceof Error ? error.message : String(error)}`,
    );
  }
}

async function requireCode(
  fn: () => unknown,
  code: string,
  errorClass: new (...args: Array<any>) => Error,
  label: string,
): Promise<Error> {
  try {
    await fn();
  } catch (error) {
    assert.ok(error instanceof errorClass, `${label}: throws the binding error`);
    assert.equal(
      (error as { code?: unknown }).code,
      code,
      `${label}: refusal code is ${code}`,
    );
    return error as Error;
  }
  assert.fail(`${label}: must refuse with ${code}`);
}

test("tools/list still advertises exactly the five read tools", async () => {
  // Closed half, green before and after: slice 1 registers no control tool.
  const h = mcpHarness();
  const res = await worker.fetch(
    mcpCall({ jsonrpc: "2.0", id: "binding-list", method: "tools/list", params: {} }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200, "requests use the JSON-RPC channel");
  const body = (await res.json()) as any;
  assert.deepEqual(
    body.result.tools.map((tool: { name: string }) => tool.name).sort(),
    ["get_audit", "get_backfill_progress", "get_config", "get_gateway", "get_status"],
    "exactly the five read tools, no control tool",
  );
});

test("probe-named control calls are unknown tools", async () => {
  // Closed half: probe names (which no slice will ever register) answer
  // -32601 without resolving the DO or reading the container.
  for (const name of PROBE_TOOLS) {
    const h = mcpHarness();
    const res = await worker.fetch(
      mcpCall({
        jsonrpc: "2.0",
        id: "binding-probe",
        method: "tools/call",
        params: { name, arguments: {} },
      }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 200, "requests use the JSON-RPC channel");
    assert.deepEqual(await res.json(), {
      jsonrpc: "2.0",
      id: "binding-probe",
      error: { code: -32601, message: "unknown tool" },
    });
    assert.deepEqual(h.names, [], `${name}: unknown tools resolve nothing`);
    assert.deepEqual(h.forwarded, [], `${name}: unknown tools read nothing`);
  }
});

test("seam exposes the binding contract", async () => {
  // RED until slice 1: the module and its four exports exist.
  const seam = await loadSeam();
  assert.equal(typeof seam["bindControlBearer"], "function", "binds a request bearer");
  assert.equal(typeof seam["requireControlScope"], "function", "checks scope then step-up");
  assert.equal(typeof seam["ControlBindingError"], "function", "names the refusal class");
});

test("binding holds the bearer per request, never ambient", async () => {
  // RED until slice 1: hold semantics. Each binding carries exactly the
  // bearer it was given (no cross-talk), and an absent bearer never falls
  // back to a module-global or ambient token.
  const seam = await loadSeam();
  const first = seam["bindControlBearer"]("synthetic-control-a-001", ["read"], false);
  const second = seam["bindControlBearer"]("synthetic-control-b-002", ["moderate"], true);
  assert.equal(first.bearer, "synthetic-control-a-001", "first binding keeps its bearer");
  assert.equal(second.bearer, "synthetic-control-b-002", "second binding keeps its own");
  assert.deepEqual(first.scopes, ["read"], "first binding keeps its scopes");
  assert.equal(first.steppedUp, false, "step-up defaults off");
  assert.equal(second.steppedUp, true, "explicit step-up sticks");
  for (const absent of [undefined, null, ""]) {
    await requireCode(
      () => seam["bindControlBearer"](absent, ["read"], false),
      "control_binding_required",
      seam["ControlBindingError"],
      `absent bearer (${String(absent)})`,
    );
  }
});

test("scope check is exact membership in the binding grants", async () => {
  // RED until slice 1: scope semantics. A granted scope passes; anything
  // else refuses before step-up is even considered. Scope order and the
  // verb map stay slice 0's: this pins membership only.
  const seam = await loadSeam();
  const binding = seam["bindControlBearer"]("synthetic-control-c-003", ["read", "moderate"], true);
  assert.equal(
    seam["requireControlScope"](binding, "read"),
    undefined,
    "granted scope passes and returns nothing",
  );
  assert.equal(
    seam["requireControlScope"](binding, "moderate"),
    undefined,
    "second grant passes too",
  );
  await requireCode(
    () => seam["requireControlScope"](binding, "admin"),
    "control_scope_insufficient",
    seam["ControlBindingError"],
    "ungranted scope",
  );
  for (const absent of [undefined, null]) {
    await requireCode(
      () => seam["requireControlScope"](absent, "read"),
      "control_binding_required",
      seam["ControlBindingError"],
      `absent binding (${String(absent)})`,
    );
  }
});

test("step-up is explicit and checked after scope", async () => {
  // RED until slice 1: step-up semantics. A granted scope without elevation
  // refuses; with elevation it passes. Grant-before-freshness order: a miss
  // on scope reports scope even when step-up is also missing.
  const seam = await loadSeam();
  const flat = seam["bindControlBearer"]("synthetic-control-d-004", ["moderate"], false);
  await requireCode(
    () => seam["requireControlScope"](flat, "moderate"),
    "control_step_up_required",
    seam["ControlBindingError"],
    "granted scope without step-up",
  );
  const elevated = seam["bindControlBearer"]("synthetic-control-e-005", ["moderate"], true);
  assert.equal(
    seam["requireControlScope"](elevated, "moderate"),
    undefined,
    "granted scope with step-up passes",
  );
  await requireCode(
    () => seam["requireControlScope"](flat, "admin"),
    "control_scope_insufficient",
    seam["ControlBindingError"],
    "scope miss wins over step-up miss",
  );
});

test("refusals never echo the bearer and the seam stays pure sync", async () => {
  // RED until slice 1: refusal hygiene plus the purity pin. Binding checks
  // are local: no promise, no fetch, and no bearer bytes in any error.
  const seam = await loadSeam();
  const bearer = "synthetic-control-secret-006-must-not-echo";
  const binding = seam["bindControlBearer"](bearer, ["read"], false);
  const errors: Error[] = [];
  errors.push(
    await requireCode(
      () => seam["requireControlScope"](binding, "admin"),
      "control_scope_insufficient",
      seam["ControlBindingError"],
      "scope refusal",
    ),
  );
  errors.push(
    await requireCode(
      () => seam["requireControlScope"](binding, "read"),
      "control_step_up_required",
      seam["ControlBindingError"],
      "step-up refusal",
    ),
  );
  errors.push(
    await requireCode(
      () => seam["bindControlBearer"]("", ["read"], false),
      "control_binding_required",
      seam["ControlBindingError"],
      "binding refusal",
    ),
  );
  for (const error of errors) {
    const text = `${error.name}: ${error.message} ${(error as { code?: unknown }).code ?? ""}`;
    assert.ok(!text.includes(bearer), `refusal echoes nothing: ${text}`);
    assert.ok(!text.includes("synthetic-control"), `refusal echoes nothing: ${text}`);
  }
  const elevated = seam["bindControlBearer"](bearer, ["read"], true);
  const result = seam["requireControlScope"](elevated, "read");
  assert.equal(result, undefined, "passing check returns nothing");
  assert.ok(
    !(result instanceof Promise),
    "the seam is synchronous: local checks, never I/O",
  );
});
