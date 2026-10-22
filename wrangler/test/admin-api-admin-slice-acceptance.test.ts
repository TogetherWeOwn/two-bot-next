/**
 * TOG-36198: failing-first acceptance for the Admin MCP + REST API admin slice
 * (TOG-35921 spec section 8 slice 6; owner decisions A1-A7, 2026-10-21).
 * Test file only -- no src changes. The Builder builds on this card's branch
 * and never edits this file; corrections go to the Test Author on this card.
 *
 * Slice 6 under test: new verbs + executors for channel/role/permission CRUD;
 * announcement.post / event.* / settings.set mapping; /admin/v1 + mcp:admin
 * tools; confirm required. Acceptance criteria (mirrored from this card):
 *   AC1. channel/role/permission CRUD behind admin scope + confirm
 *   AC2. per-effect fences mirror the existing verb pattern; Discord-429-terminal honored
 *   AC3. staging: channel create via dry-run then confirm
 *
 * ---------------------------------------------------------------------------
 * Slice-6-owned contract pinned here (Builder: honor or renegotiate on card)
 * ---------------------------------------------------------------------------
 * Verbs (dot convention per moderation.purge/settings.set/event.upsert/role.assign;
 * permission overwrites mirror the audit vocabulary channel_overwrite_* in mcp.ts):
 *   channel.create/update/delete, role.create/update/delete,
 *   channel.overwrite_upsert/delete. Reads stay in slice 4 (no channel.read here).
 * Mapped verbs exposed by this slice: announcement.post, event.upsert,
 * event.cancel, settings.set (event.read/settings.get stay in slice 4).
 * REST (resource style; every call carries a JSON body with guild_id -- IDs stay
 * out of URLs per the section 4.4 redaction posture, so DELETEs take bodies):
 *   POST   /admin/v1/channels                     channel.create
 *   PATCH  /admin/v1/channels/:id                  channel.update
 *   DELETE /admin/v1/channels/:id                  channel.delete
 *   POST   /admin/v1/roles                        role.create
 *   PATCH  /admin/v1/roles/:id                     role.update
 *   DELETE /admin/v1/roles/:id                     role.delete
 *   PUT    /admin/v1/channels/:id/overwrites/:trg  channel.overwrite_upsert
 *   DELETE /admin/v1/channels/:id/overwrites/:trg  channel.overwrite_delete
 *   POST   /admin/v1/announcements                announcement.post (channel_key, never a raw snowflake)
 *   POST   /admin/v1/events                       event.upsert (event_key)
 *   POST   /admin/v1/events/:key/cancel           event.cancel
 *   PUT    /admin/v1/settings/:key                settings.set
 * MCP (mcp:admin; snake_case per the existing get_* tools; control tools reuse
 * the existing toolResult envelope; admin tools are hidden from lesser scopes
 * in tools/list and refused on tools/call):
 *   admin_channel_create/update/delete, admin_role_create/update/delete,
 *   admin_overwrite_upsert/delete, admin_announcement_post, admin_event_upsert,
 *   admin_event_cancel, admin_settings_set.
 * Policy (spec sections 4.2/5.2/5.3, A2/A3): every verb above requires admin
 * scope; read/moderate callers get 403; destructive verbs require a confirm_id
 * minted by a prior dry-run for the same token+guild+verb+plan; dry_run and
 * confirm_id are the spec-literal field names.
 * Edge refusal properties (spec section 5.2 "fixed 401/403", ops_backfill one-401
 * precedent): unknown/expired/revoked bearer -> 401; valid token with wrong
 * scope or cross-guild target -> 403; malformed params -> 400. Refusals are
 * fixed-shape (byte-identical across calls), never echo secrets or params, and
 * reach the container with ZERO execute-forwards. Exact refusal code strings
 * are NOT pinned here (asserted as non-empty strings); only the spec-fixed
 * statuses and the zero-forward property are.
 *
 * ---------------------------------------------------------------------------
 * Prerequisite assumptions (slices 0-3; Builder reconciles with landed code)
 * ---------------------------------------------------------------------------
 * [A-ENV] Service-key / enable config names are slice-1-owned. This harness
 *   shotguns candidate env names (list below); collapse to the landed name on
 *   reconcile. If the landed name is outside the list, every Tier-2 test stays
 *   404-dark and the fix is one added line, no logic change.
 * [A-VERIFY] The Worker verifies scoped bearers through the stubbed container
 *   boundary (the Worker has no Postgres, so the section 5.2 edge lookup must
 *   cross it). The oracle answers EVERY non-execute forward as a verify call
 *   with exactly the section 4.1 registry columns; guild_ids are JSON numbers
 *   (serde default; fixtures use 16-digit IDs under 2^53). Bearer sought in the
 *   Authorization header first, then anywhere in the body.
 * [A-EXEC] Execute-forwards are POSTs whose JSON body carries a string `action`
 *   (the existing envelope shape) plus the existing x-two-* HMAC headers over
 *   the slice-1 extended canonical string. Tests identify them path-agnostically
 *   and scan values (never field placement) for scope/guild attribution.
 * [A-DRYRUN] dry_run:true travels top-level in the forwarded envelope body
 *   (slice 3: "dry_run flag in dispatch"); the dry-run answer is a receiver
 *   envelope whose result carries confirm_id + plan_hash (tests deep-search, so
 *   nesting may vary, but top-level result.confirm_id/result.plan_hash is the
 *   assumed shape -- one stub-only correction if slice 3 nests otherwise).
 * [A-RELAY] Container answers relay through the existing receiver-envelope
 *   projection (relayReceiverResponse semantics): successes keep their result,
 *   refusals keep their table status/code/retryability with the fixed message.
 *   Success assertions therefore check status + marker-presence + no-echo, and
 *   only spec-fixed refusal statuses are asserted exactly.
 * Deliberately NOT covered here (sibling/Builder-owned): token issuance and
 * rotation UX (slice 0), scope-claim tamper tests (slice 1), confirm
 * atomicity/TTL wall-clock and audit-row assertions (slices 2-3),
 * executor single-attempt counts and admission-hold internals (Builder unit
 * tests beside the fences, internal_action_http/tests.rs pattern), the
 * ADMIN_WRITE_BUCKET shape (container unit tests), and the true staging
 * prove-out of AC3 (manual, per the slice-7 runbook; the test named
 * "staging proxy" is its automatable end-to-end mirror).
 *
 * ---------------------------------------------------------------------------
 * RED expectations (all Tier-2 + scope tests FAIL now, for these reasons)
 * ---------------------------------------------------------------------------
 * /admin/v1 does not exist, so mutating methods fall through to the redirect
 * handler, which answers 405 (non-GET/HEAD on unknown paths, redirect.ts) --
 * every REST test below fails with "405 !== <401|403|400|200>" until the admin
 * router owns /admin/v1/*. /mcp knows only MCP_TOKEN (mcp:read): registry
 * bearers -> 401, never a list. tools/call of any admin_* tool -> -32601.
 * Controls (read-scope list stays exactly-five, unknown tool -32601) pass now
 * AND after the build: they lock durable properties and prove the harness.
 * The unknown-admin-path test fails now (405, unrouted) and passes once the
 * admin router 404s unknown subpaths with zero forwards.
 *
 * Synthetic harness only, following wrangler/test/mcp.test.ts: the stub TWO_BOT
 * records what the Worker forwards and plays registry + executor oracle; no
 * endpoint, database, Discord API or secret is contacted.
 * Run: node --import ./test/cloudflare-loader.mjs --test test/admin-api-admin-slice-acceptance.test.ts
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";

/** Section 8 slice 6 verbs ( channel/role/permission CUD; reads are slice 4). */
const SLICE6_VERBS = [
  "channel.create",
  "channel.update",
  "channel.delete",
  "role.create",
  "role.update",
  "role.delete",
  "channel.overwrite_upsert",
  "channel.overwrite_delete",
] as const;

/** Slice-6-mapped executors (event.read/settings.get stay in slice 4). */
const MAPPED_VERBS = [
  "announcement.post",
  "event.upsert",
  "event.cancel",
  "settings.set",
] as const;

const ADMIN_VERBS: readonly string[] = [...SLICE6_VERBS, ...MAPPED_VERBS];

/** mcp:admin tools pinned by this slice. */
const ADMIN_TOOLS = [
  "admin_channel_create",
  "admin_channel_update",
  "admin_channel_delete",
  "admin_role_create",
  "admin_role_update",
  "admin_role_delete",
  "admin_overwrite_upsert",
  "admin_overwrite_delete",
  "admin_announcement_post",
  "admin_event_upsert",
  "admin_event_cancel",
  "admin_settings_set",
] as const;

const READ_TOOLS = [
  "get_status",
  "get_gateway",
  "get_backfill_progress",
  "get_config",
  "get_audit",
] as const;

// 16-digit snowflake-shaped fixtures: pass digit-shape checks, stay exactly
// representable as JSON numbers (under 2^53) for the verify-row assumption.
const GUILD = "1000000000000001";
const OTHER_GUILD = "1000000000000002";
const CHANNEL_ID = "1000000000000042";
const ROLE_ID = "1000000000000043";
const TARGET_ID = "1000000000000044";

const MCP_TOKEN = "synthetic-mcp-read-token-0123456789abcdef";
const ADMIN_BEARER = "synthetic-admin-bearer-0123456789abcdef-gh";
const MOD_BEARER = "synthetic-moderate-bearer-0123456789abc-def";
const READ_BEARER = "synthetic-read-bearer-0123456789abcdef-ghij";
const EXPIRED_BEARER = "synthetic-expired-bearer-0123456789abcdef-kl";
const REVOKED_BEARER = "synthetic-revoked-bearer-0123456789abcdef-mn";
const FOREIGN_BEARER = "synthetic-foreign-bearer-0123456789abcdef-op";
const UNKNOWN_BEARER = "synthetic-unknown-bearer-0123456789abcdef-qr";
const SERVICE_KEY = "synthetic-service-key-0123456789abcdef-01234567";

interface RegistryEntry {
  tokenId: string;
  scopes: string[];
  guilds: string[];
  status: "live" | "expired" | "revoked";
}

const REGISTRY = new Map<string, RegistryEntry>([
  [ADMIN_BEARER, { tokenId: "adm_test_admin_01", scopes: ["admin"], guilds: [GUILD], status: "live" }],
  [MOD_BEARER, { tokenId: "adm_test_mod_01", scopes: ["moderate"], guilds: [GUILD], status: "live" }],
  [READ_BEARER, { tokenId: "adm_test_read_01", scopes: ["read"], guilds: [GUILD], status: "live" }],
  [EXPIRED_BEARER, { tokenId: "adm_test_exp_01", scopes: ["admin"], guilds: [GUILD], status: "expired" }],
  [REVOKED_BEARER, { tokenId: "adm_test_rev_01", scopes: ["admin"], guilds: [GUILD], status: "revoked" }],
  [FOREIGN_BEARER, { tokenId: "adm_test_frn_01", scopes: ["admin"], guilds: [OTHER_GUILD], status: "live" }],
]);

const TOKEN_IDS = new Set([...REGISTRY.values()].map((e) => e.tokenId));
const KNOWN_BEARERS = new Set(REGISTRY.keys());

interface Forwarded {
  method: string;
  url: string;
  headers: Record<string, string>;
  body: string;
}

interface Plan {
  confirmId: string;
  planHash: string;
  tokenId: string;
  guild: string;
  verb: string;
  used: boolean;
}

/** Recursively collect every value stored under `key` in a JSON tree. */
function deepFind(root: unknown, key: string): unknown[] {
  const out: unknown[] = [];
  const walk = (node: unknown): void => {
    if (Array.isArray(node)) {
      for (const item of node) walk(item);
      return;
    }
    if (typeof node === "object" && node !== null) {
      for (const [k, v] of Object.entries(node)) {
        if (k === key) out.push(v);
        walk(v);
      }
    }
  };
  walk(root);
  return out;
}

/** True when any string in the JSON tree contains `needle`. */
function deepContains(root: unknown, needle: string): boolean {
  if (typeof root === "string") return root.includes(needle);
  if (Array.isArray(root)) return root.some((item) => deepContains(item, needle));
  if (typeof root === "object" && root !== null) {
    return Object.entries(root).some(([k, v]) => k.includes(needle) || deepContains(v, needle));
  }
  return false;
}

function tryParse(body: string): unknown {
  try {
    return JSON.parse(body) as unknown;
  } catch {
    return null;
  }
}

/** Execute-forwards: POSTs whose JSON body carries a string `action` (any path). */
function isExecuteForward(f: Forwarded): boolean {
  if (f.method !== "POST") return false;
  const parsed = tryParse(f.body);
  return (
    typeof parsed === "object" &&
    parsed !== null &&
    !Array.isArray(parsed) &&
    typeof (parsed as Record<string, unknown>)["action"] === "string"
  );
}

function forwardAction(f: Forwarded): string | null {
  const parsed = tryParse(f.body) as Record<string, unknown> | null;
  const action = parsed?.["action"];
  return typeof action === "string" ? action : null;
}

function executeForwards(forwarded: Forwarded[]): Forwarded[] {
  return forwarded.filter(isExecuteForward);
}

function bearerOf(headers: Record<string, string>): string | null {
  const m = /^Bearer (.+)$/.exec(headers["authorization"] ?? "");
  return m ? m[1]! : null;
}

interface Harness {
  env: Env;
  ctx: ExecutionContext;
  forwarded: Forwarded[];
  plans: Map<string, Plan>;
}

function harness(): Harness {
  const forwarded: Forwarded[] = [];
  const plans = new Map<string, Plan>();
  let planSeq = 0;

  const envelope = (status: number, payload: Record<string, unknown>): Response =>
    Response.json(payload, { status, headers: { "content-type": "application/json" } });

  /** Attribute a forward to a registry entry via bearer or token_id scan. */
  const attribute = (req: Request, bodyText: string): RegistryEntry | null => {
    const direct = /^Bearer (.+)$/.exec(req.headers.get("authorization") ?? "")?.[1];
    if (direct && KNOWN_BEARERS.has(direct)) return REGISTRY.get(direct)!;
    const parsed = tryParse(bodyText);
    for (const bearer of KNOWN_BEARERS) {
      if (deepContains(parsed, bearer)) return REGISTRY.get(bearer)!;
    }
    for (const id of TOKEN_IDS) {
      if (deepContains(parsed, id)) {
        for (const entry of REGISTRY.values()) {
          if (entry.tokenId === id) return entry;
        }
      }
    }
    const headerScan = [...req.headers.values()].join("\n");
    for (const bearer of KNOWN_BEARERS) {
      if (headerScan.includes(bearer)) return REGISTRY.get(bearer)!;
    }
    return null;
  };

  const refusal = (status: number, code: string, retryable: boolean, requestId: string): Response =>
    envelope(status, {
      ok: false,
      request_id: requestId,
      error: { code, message: "request refused", retryable },
    });

  const success = (requestId: string, result: Record<string, unknown>): Response =>
    envelope(200, { ok: true, request_id: requestId, result });

  const requestIdOf = (req: Request): string =>
    req.headers.get("x-request-id") ?? "00000000000000000000000000";

  const oracle = async (req: Request): Promise<Response> => {
    const headers: Record<string, string> = {};
    req.headers.forEach((value, key) => {
      headers[key] = value;
    });
    const body = await req.text();
    forwarded.push({ method: req.method, url: req.url, headers, body });
    const requestId = requestIdOf(req);
    const parsed = tryParse(body);
    const action =
      typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)
        ? (parsed as Record<string, unknown>)["action"]
        : null;

    // Non-execute traffic is answered as a verify/registry call [A-VERIFY]:
    // exactly the section 4.1 columns, bearer from header or body.
    if (!(req.method === "POST" && typeof action === "string")) {
      const headerBearer = /^Bearer (.+)$/.exec(req.headers.get("authorization") ?? "")?.[1] ?? null;
      let bearer = headerBearer && KNOWN_BEARERS.has(headerBearer) ? headerBearer : null;
      if (!bearer) {
        for (const candidate of KNOWN_BEARERS) {
          if (deepContains(parsed, candidate) || req.url.includes(candidate)) {
            bearer = candidate;
            break;
          }
        }
      }
      const entry = bearer ? REGISTRY.get(bearer)! : null;
      if (!entry) {
        return envelope(401, { known: false, error: "unknown bearer" });
      }
      return envelope(200, {
        token_id: entry.tokenId,
        secret_hash: "withheld",
        scopes: entry.scopes,
        guild_ids: entry.guilds.map(Number),
        label: "synthetic",
        issued_by: "synthetic-operator",
        issued_at: "2026-10-01T00:00:00.000Z",
        expires_at: entry.status === "expired" ? "2020-01-01T00:00:00.000Z" : "2027-10-01T00:00:00.000Z",
        revoked_at: entry.status === "revoked" ? "2026-10-02T00:00:00.000Z" : null,
        last_used_at: null,
      });
    }

    // Execute path: emulate the container authorize + dispatch for admin verbs.
    const verb = action;
    const entry = attribute(req, body);
    if (!entry || entry.status !== "live") {
      return refusal(401, "unauthorized", false, requestId);
    }
    // Per-effect fence mirror: verbs without an adapter stay refused even when
    // the envelope verifies -- shipping a verb must never widen this list.
    if (!ADMIN_VERBS.includes(verb)) {
      return refusal(403, "action_not_allowed", false, requestId);
    }
    if (!entry.scopes.includes("admin")) {
      return refusal(403, "action_not_allowed", false, requestId);
    }
    const guilds = deepFind(parsed, "guild_id").filter((g): g is string => typeof g === "string");
    if (!guilds.some((g) => entry.guilds.includes(g))) {
      return refusal(403, "action_not_allowed", false, requestId);
    }
    const dryRun =
      typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)
        ? (parsed as Record<string, unknown>)["dry_run"]
        : null;
    if (dryRun === true) {
      planSeq += 1;
      const confirmId = `cfm_test_${planSeq}_${planSeq * 7919}`;
      const planHash = `ph_test_${planSeq}`;
      plans.set(confirmId, {
        confirmId,
        planHash,
        tokenId: entry.tokenId,
        guild: guilds.find((g) => entry.guilds.includes(g))!,
        verb,
        used: false,
      });
      return success(requestId, {
        dry_run: true,
        verb,
        guild_id: guilds.find((g) => entry.guilds.includes(g))!,
        confirm_id: confirmId,
        plan_hash: planHash,
        summary: `would execute ${verb}`,
      });
    }
    const confirmIds = deepFind(parsed, "confirm_id").filter(
      (c): c is string => typeof c === "string",
    );
    const plan = confirmIds.map((c) => plans.get(c)).find((p) => p !== undefined) ?? null;
    if (
      !plan ||
      plan.used ||
      plan.tokenId !== entry.tokenId ||
      plan.verb !== verb ||
      !guilds.includes(plan.guild)
    ) {
      // Missing, spent, or cross-plan/cross-guild confirm: refused. The Worker
      // must forward validity checks here (never invent its own verdict).
      return refusal(403, "action_not_allowed", false, requestId);
    }
    plan.used = true;
    return success(requestId, {
      executed: true,
      verb,
      guild_id: plan.guild,
      plan_hash: plan.planHash,
      target_id: verb.startsWith("channel.") ? CHANNEL_ID : ROLE_ID,
    });
  };

  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    CF_VERSION_METADATA: { id: "synthetic-admin-slice-deployment" },
    MCP_TOKEN,
    // [A-ENV] Slice-1-owned service-key / enable names, shotgunned: exactly one
    // is canonical after reconcile. Presence lights the surface (MCP_TOKEN
    // precedent); absence keeps it 404-dark.
    ADMIN_SERVICE_KEY: SERVICE_KEY,
    ADMIN_API_SERVICE_KEY: SERVICE_KEY,
    TWO_ADMIN_SERVICE_KEY: SERVICE_KEY,
    INTERNAL_ADMIN_SERVICE_KEY: SERVICE_KEY,
    ADMIN_API_KEYS: `admin-svc:${SERVICE_KEY}`,
    TWO_INTERNAL_ADMIN_KEYS: `admin-svc:${SERVICE_KEY}`,
    TWO_ADMIN_KEYS: `admin-svc:${SERVICE_KEY}`,
    TWO_INTERNAL_KEYS: `admin-svc:${SERVICE_KEY}`,
    ADMIN_SERVICE_KEY_ID: "admin-svc",
    TWO_ADMIN_SERVICE_KEY_ID: "admin-svc",
    ADMIN_API_KEY_ID: "admin-svc",
    ADMIN_API_ENABLED: "1",
    TWO_ADMIN_API: "1",
    ADMIN_API_INGRESS: "1",
    TWO_INTERNAL_ADMIN: "1",
    TWO_BOT: {
      getByName: (_name: string) => ({ fetch: oracle }),
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  return { env, ctx, forwarded, plans };
}

let callerSeq = 0;
function callerIp(): string {
  callerSeq += 1;
  return `198.51.100.${(callerSeq % 200) + 1}`;
}

function adminRequest(
  method: string,
  path: string,
  bearer: string | null,
  body?: unknown,
): Request {
  return new Request(`https://admin.invalid${path}`, {
    method,
    headers: {
      ...(bearer ? { authorization: `Bearer ${bearer}` } : {}),
      "content-type": "application/json",
      "cf-connecting-ip": callerIp(),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

function mcpRequest(bearer: string | null, method: string, params: unknown): Request {
  return new Request("https://probe.invalid/mcp", {
    method: "POST",
    headers: {
      ...(bearer ? { authorization: `Bearer ${bearer}` } : {}),
      "content-type": "application/json",
      "cf-connecting-ip": callerIp(),
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
}

async function jsonBody(res: Response): Promise<any> {
  assert.match(res.headers.get("content-type") ?? "", /application\/json/);
  return (await res.json()) as any;
}

/** Fail when any secret-adjacent value is reflected in response text. */
function assertNoEcho(text: string, bearers: string[], label: string): void {
  for (const bearer of bearers) {
    assert.ok(!text.includes(bearer), `${label}: response must not echo bearer material`);
  }
  assert.ok(!text.includes(SERVICE_KEY), `${label}: response must not echo service-key material`);
}

/** First string value stored under `key`, or null. */
function firstString(root: unknown, key: string): string | null {
  const found = deepFind(root, key).find((v): v is string => typeof v === "string");
  return found ?? null;
}

// ---------------------------------------------------------------------------
// Tier 1: controls (green now and after the build) + edge-auth RED pins
// ---------------------------------------------------------------------------

test("control: read-scope tools/list stays exactly the five read tools", async () => {
  const h = harness();
  const res = await worker.fetch(mcpRequest(MCP_TOKEN, "tools/list", {}), h.env, h.ctx);
  assert.equal(res.status, 200);
  const body = await jsonBody(res);
  const names = (body.result?.tools as { name: string }[] | undefined)?.map((t) => t.name).sort();
  assert.deepEqual(names, [...READ_TOOLS].sort());
  assert.equal(executeForwards(h.forwarded).length, 0);
});

test("control: unknown MCP tool stays -32601 with zero forwards", async () => {
  const h = harness();
  const res = await worker.fetch(
    mcpRequest(MCP_TOKEN, "tools/call", { name: "no_such_tool", arguments: {} }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await jsonBody(res);
  assert.equal(body.result, undefined);
  assert.equal(body.error?.code, -32601);
  assert.equal(executeForwards(h.forwarded).length, 0);
});

test("unknown admin path 404s with zero forwards once routed", async () => {
  const h = harness();
  const res = await worker.fetch(
    adminRequest("POST", "/admin/v1/nonexistent-resource", ADMIN_BEARER, { guild_id: GUILD }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 404);
  assert.equal(executeForwards(h.forwarded).length, 0);
});

test("unknown bearer on channel create -> 401 fixed-shape, zero forwards", async () => {
  const h = harness();
  const first = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", UNKNOWN_BEARER, { guild_id: GUILD, name: "general" }),
    h.env,
    h.ctx,
  );
  assert.equal(first.status, 401);
  const firstText = await first.text();
  assertNoEcho(firstText, [UNKNOWN_BEARER], "unknown-bearer 401");
  const second = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", "synthetic-other-unknown-bearer-zz", {
      guild_id: GUILD,
      name: "general",
    }),
    h.env,
    h.ctx,
  );
  assert.equal(second.status, 401);
  // Fixed-shape failures are indistinguishable across callers and secrets.
  assert.equal(await second.text(), firstText);
  assert.equal(executeForwards(h.forwarded).length, 0);
});

test("missing bearer on channel create -> 401, zero forwards", async () => {
  const h = harness();
  const res = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", null, { guild_id: GUILD, name: "general" }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 401);
  assert.equal(executeForwards(h.forwarded).length, 0);
});

test("expired and revoked bearers -> 401, zero forwards", async () => {
  for (const bearer of [EXPIRED_BEARER, REVOKED_BEARER]) {
    const h = harness();
    const res = await worker.fetch(
      adminRequest("POST", "/admin/v1/channels", bearer, { guild_id: GUILD, name: "general" }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 401);
    assertNoEcho(await res.text(), [bearer], "stale-bearer 401");
    assert.equal(executeForwards(h.forwarded).length, 0);
  }
});

test("read and moderate bearers on channel create -> 403 fixed-shape, zero forwards", async () => {
  const h = harness();
  const bodies: string[] = [];
  for (const bearer of [READ_BEARER, MOD_BEARER]) {
    const res = await worker.fetch(
      adminRequest("POST", "/admin/v1/channels", bearer, { guild_id: GUILD, name: "general" }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 403);
    const text = await res.text();
    assert.ok(text.length > 0 && text.length < 4096, "403 body is fixed-shape, never a reflection");
    assertNoEcho(text, [bearer], "wrong-scope 403");
    bodies.push(text);
  }
  assert.equal(bodies[0], bodies[1], "wrong-scope 403s are indistinguishable across scopes");
  assert.equal(executeForwards(h.forwarded).length, 0);
});

test("admin bearer on cross-guild channel create -> 403, zero forwards", async () => {
  const h = harness();
  const res = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", FOREIGN_BEARER, { guild_id: GUILD, name: "general" }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 403);
  assertNoEcho(await res.text(), [FOREIGN_BEARER], "cross-guild 403");
  assert.equal(executeForwards(h.forwarded).length, 0);
});

// ---------------------------------------------------------------------------
// Tier 2: staging proxy -- channel create via dry-run then confirm (AC3)
// ---------------------------------------------------------------------------

test("staging proxy: channel create via dry-run then confirm executes once", async () => {
  const h = harness();
  const dry = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "announcements",
      dry_run: true,
    }),
    h.env,
    h.ctx,
  );
  assert.equal(dry.status, 200);
  const dryText = await dry.text();
  assertNoEcho(dryText, [ADMIN_BEARER], "dry-run plan");
  const plan = JSON.parse(dryText) as unknown;
  const confirmId = firstString(plan, "confirm_id");
  const planHash = firstString(plan, "plan_hash");
  assert.ok(confirmId, "dry-run plan must carry a confirm_id");
  assert.ok(planHash, "dry-run plan must carry a plan_hash");
  // The dry-run forward is non-mutating: exactly one forward, flagged dry_run.
  const dryForwards = executeForwards(h.forwarded);
  assert.equal(dryForwards.length, 1);
  assert.equal(forwardAction(dryForwards[0]!), "channel.create");
  assert.equal((tryParse(dryForwards[0]!.body) as Record<string, unknown>)["dry_run"], true);

  const done = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "announcements",
      confirm_id: confirmId,
    }),
    h.env,
    h.ctx,
  );
  assert.equal(done.status, 200);
  const doneText = await done.text();
  assertNoEcho(doneText, [ADMIN_BEARER], "execute receipt");
  const receipt = JSON.parse(doneText) as unknown;
  assert.ok(
    deepContains(receipt, CHANNEL_ID) || deepContains(receipt, "channel.create"),
    "execute receipt must attribute the created channel",
  );
  // Exactly one mutating forward for the whole flow: dry-run + execute, no retry.
  const execForwards = executeForwards(h.forwarded).filter(
    (f) => (tryParse(f.body) as Record<string, unknown>)["dry_run"] !== true,
  );
  assert.equal(execForwards.length, 1);
  assert.equal(forwardAction(execForwards[0]!), "channel.create");
  assert.ok(
    deepContains(tryParse(execForwards[0]!.body), confirmId!),
    "execute forward must carry the confirm binding",
  );
});

test("channel create without confirm_id -> 400 at the edge, zero forwards", async () => {
  const h = harness();
  const res = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, { guild_id: GUILD, name: "general" }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 400);
  assertNoEcho(await res.text(), [ADMIN_BEARER], "missing-confirm 400");
  assert.equal(executeForwards(h.forwarded).length, 0);
});

test("channel create with bogus confirm_id -> refused after one validity forward", async () => {
  const h = harness();
  const res = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "general",
      confirm_id: "cfm_bogus_never_issued",
    }),
    h.env,
    h.ctx,
  );
  assert.ok(res.status !== 200, "bogus confirm must not execute");
  assertNoEcho(await res.text(), [ADMIN_BEARER], "bogus-confirm refusal");
  // Validity is the container's verdict: the Worker forwards exactly once, relays.
  assert.equal(executeForwards(h.forwarded).length, 1);
});

test("confirm from a channel plan cannot execute a role create (cross-plan refused)", async () => {
  const h = harness();
  const dry = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "general",
      dry_run: true,
    }),
    h.env,
    h.ctx,
  );
  assert.equal(dry.status, 200);
  const confirmId = firstString(JSON.parse(await dry.text()) as unknown, "confirm_id");
  assert.ok(confirmId);
  const res = await worker.fetch(
    adminRequest("POST", "/admin/v1/roles", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "member",
      confirm_id: confirmId,
    }),
    h.env,
    h.ctx,
  );
  assert.ok(res.status !== 200, "cross-plan confirm must not execute");
  const execCount = executeForwards(h.forwarded).filter(
    (f) => (tryParse(f.body) as Record<string, unknown>)["dry_run"] !== true,
  ).length;
  assert.equal(execCount, 1);
});

test("confirm is single-use: second execute with the same confirm_id is refused", async () => {
  const h = harness();
  const dry = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "general",
      dry_run: true,
    }),
    h.env,
    h.ctx,
  );
  assert.equal(dry.status, 200);
  const confirmId = firstString(JSON.parse(await dry.text()) as unknown, "confirm_id");
  assert.ok(confirmId);
  const first = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "general",
      confirm_id: confirmId,
    }),
    h.env,
    h.ctx,
  );
  assert.equal(first.status, 200);
  const second = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "general",
      confirm_id: confirmId,
    }),
    h.env,
    h.ctx,
  );
  assert.ok(second.status !== 200, "replayed confirm must not execute twice");
});

// ---------------------------------------------------------------------------
// Tier 2: role + permission-overwrite CRUD behind admin scope + confirm (AC1)
// ---------------------------------------------------------------------------

test("role create via dry-run then confirm executes with verb role.create", async () => {
  const h = harness();
  const dry = await worker.fetch(
    adminRequest("POST", "/admin/v1/roles", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "raid-leader",
      dry_run: true,
    }),
    h.env,
    h.ctx,
  );
  assert.equal(dry.status, 200);
  const confirmId = firstString(JSON.parse(await dry.text()) as unknown, "confirm_id");
  assert.ok(confirmId);
  const done = await worker.fetch(
    adminRequest("POST", "/admin/v1/roles", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "raid-leader",
      confirm_id: confirmId,
    }),
    h.env,
    h.ctx,
  );
  assert.equal(done.status, 200);
  const actions = executeForwards(h.forwarded).map(forwardAction);
  assert.deepEqual(actions, ["role.create", "role.create"]);
});

test("role update and delete require admin scope + confirm", async () => {
  // Moderate bearer: both refused at the edge with zero forwards.
  for (const [method, path] of [
    ["PATCH", `/admin/v1/roles/${ROLE_ID}`],
    ["DELETE", `/admin/v1/roles/${ROLE_ID}`],
  ] as const) {
    const h = harness();
    const res = await worker.fetch(
      adminRequest(method, path, MOD_BEARER, { guild_id: GUILD, name: "x" }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 403);
    assert.equal(executeForwards(h.forwarded).length, 0);
  }
  // Admin bearer without confirm: 400 at the edge, zero forwards.
  for (const [method, path, verb] of [
    ["PATCH", `/admin/v1/roles/${ROLE_ID}`, "role.update"],
    ["DELETE", `/admin/v1/roles/${ROLE_ID}`, "role.delete"],
  ] as const) {
    const h = harness();
    const res = await worker.fetch(
      adminRequest(method, path, ADMIN_BEARER, { guild_id: GUILD, name: "x" }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 400);
    assert.equal(executeForwards(h.forwarded).length, 0);
    void verb;
  }
  // Admin dry-run maps to the right verb.
  for (const [method, path, verb] of [
    ["PATCH", `/admin/v1/roles/${ROLE_ID}`, "role.update"],
    ["DELETE", `/admin/v1/roles/${ROLE_ID}`, "role.delete"],
  ] as const) {
    const h = harness();
    const res = await worker.fetch(
      adminRequest(method, path, ADMIN_BEARER, { guild_id: GUILD, name: "x", dry_run: true }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 200);
    assert.ok(firstString(JSON.parse(await res.text()) as unknown, "confirm_id"));
    assert.deepEqual(executeForwards(h.forwarded).map(forwardAction), [verb]);
  }
});

test("channel update and delete map to channel.update/delete behind confirm", async () => {
  for (const [method, path, verb] of [
    ["PATCH", `/admin/v1/channels/${CHANNEL_ID}`, "channel.update"],
    ["DELETE", `/admin/v1/channels/${CHANNEL_ID}`, "channel.delete"],
  ] as const) {
    const h = harness();
    const dry = await worker.fetch(
      adminRequest(method, path, ADMIN_BEARER, { guild_id: GUILD, name: "renamed", dry_run: true }),
      h.env,
      h.ctx,
    );
    assert.equal(dry.status, 200);
    const confirmId = firstString(JSON.parse(await dry.text()) as unknown, "confirm_id");
    assert.ok(confirmId);
    const done = await worker.fetch(
      adminRequest(method, path, ADMIN_BEARER, { guild_id: GUILD, name: "renamed", confirm_id: confirmId }),
      h.env,
      h.ctx,
    );
    assert.equal(done.status, 200);
    assert.deepEqual(executeForwards(h.forwarded).map(forwardAction), [verb, verb]);
  }
});

test("permission overwrite upsert and delete map behind admin scope + confirm", async () => {
  const base = `/admin/v1/channels/${CHANNEL_ID}/overwrites/${TARGET_ID}`;
  // Moderate bearer refused at the edge.
  for (const method of ["PUT", "DELETE"] as const) {
    const h = harness();
    const res = await worker.fetch(
      adminRequest(method, base, MOD_BEARER, { guild_id: GUILD, allow: "1024", deny: "0" }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 403);
    assert.equal(executeForwards(h.forwarded).length, 0);
  }
  // Admin dry-run then confirm per verb.
  for (const [method, verb] of [
    ["PUT", "channel.overwrite_upsert"],
    ["DELETE", "channel.overwrite_delete"],
  ] as const) {
    const h = harness();
    const dry = await worker.fetch(
      adminRequest(method, base, ADMIN_BEARER, {
        guild_id: GUILD,
        allow: "1024",
        deny: "0",
        dry_run: true,
      }),
      h.env,
      h.ctx,
    );
    assert.equal(dry.status, 200);
    const confirmId = firstString(JSON.parse(await dry.text()) as unknown, "confirm_id");
    assert.ok(confirmId);
    const done = await worker.fetch(
      adminRequest(method, base, ADMIN_BEARER, {
        guild_id: GUILD,
        allow: "1024",
        deny: "0",
        confirm_id: confirmId,
      }),
      h.env,
      h.ctx,
    );
    assert.equal(done.status, 200);
    assert.deepEqual(executeForwards(h.forwarded).map(forwardAction), [verb, verb]);
  }
});

// ---------------------------------------------------------------------------
// Tier 2: announcement / event / settings mapping (AC1)
// ---------------------------------------------------------------------------

test("announcement.post, event.* and settings.set map through /admin/v1", async () => {
  const cases = [
    {
      path: "/admin/v1/announcements",
      method: "POST",
      body: { guild_id: GUILD, channel_key: "ann", text: "hello" },
      verb: "announcement.post",
    },
    {
      path: "/admin/v1/events",
      method: "POST",
      body: { guild_id: GUILD, event_key: "raid-night" },
      verb: "event.upsert",
    },
    {
      path: "/admin/v1/events/raid-night/cancel",
      method: "POST",
      body: { guild_id: GUILD },
      verb: "event.cancel",
    },
    {
      path: "/admin/v1/settings/welcome_mode",
      method: "PUT",
      body: { guild_id: GUILD, value: "quiet" },
      verb: "settings.set",
    },
  ] as const;
  for (const c of cases) {
    const h = harness();
    const dry = await worker.fetch(
      adminRequest(c.method, c.path, ADMIN_BEARER, { ...c.body, dry_run: true }),
      h.env,
      h.ctx,
    );
    assert.equal(dry.status, 200);
    const confirmId = firstString(JSON.parse(await dry.text()) as unknown, "confirm_id");
    assert.ok(confirmId, `${c.verb}: dry-run must mint a confirm`);
    assert.deepEqual(executeForwards(h.forwarded).map(forwardAction), [c.verb]);
    const done = await worker.fetch(
      adminRequest(c.method, c.path, ADMIN_BEARER, { ...c.body, confirm_id: confirmId }),
      h.env,
      h.ctx,
    );
    assert.equal(done.status, 200);
    assert.deepEqual(executeForwards(h.forwarded).map(forwardAction), [c.verb, c.verb]);
  }
});

// ---------------------------------------------------------------------------
// Tier 2: fences mirror the existing verb pattern (AC2)
// ---------------------------------------------------------------------------

test("malformed admin params are refused before any container forward", async () => {
  const badBodies: unknown[] = [
    { guild_id: GUILD }, // missing name
    { guild_id: "not-a-snowflake", name: "general" },
    { guild_id: GUILD, name: 123 },
    { guild_id: GUILD, name: "" },
    { guild_id: GUILD, name: "x".repeat(5000) },
    { name: "general" }, // missing guild_id
    [],
  ];
  for (const body of badBodies) {
    const h = harness();
    const res = await worker.fetch(
      adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, body),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 400);
    const text = await res.text();
    assert.ok(text.length < 4096, "400 body is fixed-shape, never a reflection");
    assertNoEcho(text, [ADMIN_BEARER], "validation 400");
    assert.equal(executeForwards(h.forwarded).length, 0);
  }
});

test("malformed target IDs in admin paths are refused with zero forwards", async () => {
  for (const path of [
    "/admin/v1/channels/not-a-snowflake",
    `/admin/v1/channels/${CHANNEL_ID}/overwrites/..`,
    "/admin/v1/roles/%20",
  ]) {
    const h = harness();
    const res = await worker.fetch(
      adminRequest("DELETE", path, ADMIN_BEARER, { guild_id: GUILD }),
      h.env,
      h.ctx,
    );
    assert.ok(res.status === 400 || res.status === 404, `DELETE ${path}: refused, got ${res.status}`);
    assert.equal(executeForwards(h.forwarded).length, 0);
  }
});

test("execute-forward carries HMAC binding plus scope/guild attribution", async () => {
  const h = harness();
  const dry = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "general",
      dry_run: true,
    }),
    h.env,
    h.ctx,
  );
  assert.equal(dry.status, 200);
  const forwards = executeForwards(h.forwarded);
  assert.equal(forwards.length, 1);
  const fwd = forwards[0]!;
  // Existing HMAC internal-action path: the x-two-* headers travel.
  assert.ok(fwd.headers["x-two-signature"], "forward must carry the HMAC signature");
  assert.ok(fwd.headers["x-two-key-id"], "forward must carry the service key id");
  // Scope/guild attribution must be on the wire (container re-checks it), in
  // headers or body -- values are scanned, placement is slice-1-owned.
  const wire = `${JSON.stringify(fwd.headers)}\n${fwd.body}`;
  assert.ok(wire.includes(GUILD), "forward must bind the target guild");
  assert.ok(
    wire.includes("adm_test_admin_01") || wire.includes("admin"),
    "forward must bind the caller scope/token",
  );
  // The caller's bearer itself must never travel verbatim past the edge.
  assert.ok(!wire.includes(ADMIN_BEARER), "caller bearer must not be forwarded verbatim");
});

test("container 429 on admin execute relays 429 with exactly one forward (terminal)", async () => {
  const h = harness();
  // Program the oracle: dry-run plans normally, executes answer rate_limited.
  const realFetch = h.env.TWO_BOT.getByName("two-bot").fetch;
  void realFetch;
  // Re-drive through a dedicated harness whose oracle 429s executes.
  const h2 = harness();
  const origOracle = h2.env.TWO_BOT.getByName("two-bot").fetch;
  (h2.env as unknown as Record<string, unknown>)["TWO_BOT"] = {
    getByName: (_name: string) => ({
      fetch: async (req: Request): Promise<Response> => {
        const clone = req.clone();
        const text = await clone.text();
        const parsed = tryParse(text) as Record<string, unknown> | null;
        if (req.method === "POST" && typeof parsed?.["action"] === "string" && parsed["dry_run"] !== true) {
          // Record the execute forward, then answer terminal rate_limited.
          const res = await origOracle(req);
          void res;
          const last = h2.forwarded[h2.forwarded.length - 1]!;
          void last;
          return Response.json(
            {
              ok: false,
              request_id: req.headers.get("x-request-id") ?? "00000000000000000000000000",
              error: { code: "rate_limited", message: "request refused", retryable: true },
            },
            { status: 429, headers: { "content-type": "application/json" } },
          );
        }
        return origOracle(req);
      },
    }),
  };
  const dry = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "general",
      dry_run: true,
    }),
    h2.env,
    h2.ctx,
  );
  assert.equal(dry.status, 200);
  const confirmId = firstString(JSON.parse(await dry.text()) as unknown, "confirm_id");
  assert.ok(confirmId);
  const res = await worker.fetch(
    adminRequest("POST", "/admin/v1/channels", ADMIN_BEARER, {
      guild_id: GUILD,
      name: "general",
      confirm_id: confirmId,
    }),
    h2.env,
    h2.ctx,
  );
  assert.equal(res.status, 429);
  const text = await res.text();
  assert.ok(text.includes("rate_limited"), "relayed 429 keeps the receiver code");
  assertNoEcho(text, [ADMIN_BEARER], "429 relay");
  const execCount = executeForwards(h2.forwarded).filter(
    (f) => (tryParse(f.body) as Record<string, unknown>)["dry_run"] !== true,
  ).length;
  assert.equal(execCount, 1, "terminal 429: the Worker never retries the forward");
  await new Promise((r) => setTimeout(r, 20));
  const execAfter = executeForwards(h2.forwarded).filter(
    (f) => (tryParse(f.body) as Record<string, unknown>)["dry_run"] !== true,
  ).length;
  assert.equal(execAfter, 1, "no late retry after the 429 response settles");
});

// ---------------------------------------------------------------------------
// Tier 2: mcp:admin tools (AC1)
// ---------------------------------------------------------------------------

test("admin tools/list carries the admin set plus the read set", async () => {
  const h = harness();
  const res = await worker.fetch(mcpRequest(ADMIN_BEARER, "tools/list", {}), h.env, h.ctx);
  assert.equal(res.status, 200);
  const body = await jsonBody(res);
  const names = new Set(
    ((body.result?.tools as { name: string }[] | undefined) ?? []).map((t) => t.name),
  );
  for (const tool of ADMIN_TOOLS) {
    assert.ok(names.has(tool), `admin list must include ${tool}`);
  }
  // Higher scope implies lower for reads (A2).
  for (const tool of READ_TOOLS) {
    assert.ok(names.has(tool), `admin list must keep read tool ${tool}`);
  }
});

test("read bearer calling an admin tool is refused (recognized, not unknown)", async () => {
  const h = harness();
  const res = await worker.fetch(
    mcpRequest(MCP_TOKEN, "tools/call", {
      name: "admin_channel_create",
      arguments: { guild_id: GUILD, name: "general", dry_run: true },
    }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await jsonBody(res);
  assert.equal(body.result, undefined, "read bearer must not receive an admin result");
  assert.ok(body.error, "read bearer must receive an error");
  assert.equal(typeof body.error.code, "number");
  assert.notEqual(
    body.error.code,
    -32601,
    "admin tools exist: refusal must differ from unknown-tool",
  );
  assert.equal(executeForwards(h.forwarded).length, 0);
});

test("every admin tool is recognized: bad args -> -32602, never -32601", async () => {
  // Recognition-via-validation: with an admin bearer past the scope gate, a
  // malformed argument proves the tool is registered (-32602) rather than
  // absent (-32601), without running a full flow per tool.
  for (const tool of ADMIN_TOOLS) {
    const h = harness();
    const res = await worker.fetch(
      mcpRequest(ADMIN_BEARER, "tools/call", {
        name: tool,
        arguments: { guild_id: "not-a-snowflake", dry_run: true },
      }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 200);
    const body = await jsonBody(res);
    assert.equal(
      body.error?.code,
      -32602,
      `${tool}: malformed args must be invalid-params, not unknown-tool`,
    );
    assert.equal(executeForwards(h.forwarded).length, 0);
  }
});

test("mcp admin_channel_create via dry-run then confirm executes once", async () => {
  const h = harness();
  const dry = await worker.fetch(
    mcpRequest(ADMIN_BEARER, "tools/call", {
      name: "admin_channel_create",
      arguments: { guild_id: GUILD, name: "raid-lfg", dry_run: true },
    }),
    h.env,
    h.ctx,
  );
  assert.equal(dry.status, 200);
  const dryBody = await jsonBody(dry);
  assert.equal(dryBody.error, undefined);
  const confirmId = firstString(dryBody, "confirm_id");
  assert.ok(confirmId, "mcp dry-run must carry a confirm_id");
  assert.deepEqual(executeForwards(h.forwarded).map(forwardAction), ["channel.create"]);
  const done = await worker.fetch(
    mcpRequest(ADMIN_BEARER, "tools/call", {
      name: "admin_channel_create",
      arguments: { guild_id: GUILD, name: "raid-lfg", confirm_id: confirmId },
    }),
    h.env,
    h.ctx,
  );
  assert.equal(done.status, 200);
  const doneBody = await jsonBody(done);
  assert.equal(doneBody.error, undefined);
  assert.deepEqual(executeForwards(h.forwarded).map(forwardAction), [
    "channel.create",
    "channel.create",
  ]);
});

test("mcp admin_settings_set dry-run forwards verb settings.set", async () => {
  const h = harness();
  const dry = await worker.fetch(
    mcpRequest(ADMIN_BEARER, "tools/call", {
      name: "admin_settings_set",
      arguments: { guild_id: GUILD, key: "welcome_mode", value: "quiet", dry_run: true },
    }),
    h.env,
    h.ctx,
  );
  assert.equal(dry.status, 200);
  const body = await jsonBody(dry);
  assert.equal(body.error, undefined);
  assert.ok(firstString(body, "confirm_id"));
  assert.deepEqual(executeForwards(h.forwarded).map(forwardAction), ["settings.set"]);
});
