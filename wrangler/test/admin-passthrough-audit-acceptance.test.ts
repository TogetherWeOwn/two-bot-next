/**
 * TOG-36199: failing-first acceptance for Admin API slice 7 (TOG-35921 §8.7):
 * allowlisted Discord passthrough + audit query. Test files only — no src
 * changes. The Builder starts from this head and must not edit this file.
 *
 * Surfaces under test (all Worker edge + docs; the container behind the
 * binding is stubbed, following wrangler/test/mcp.test.ts):
 * - POST /admin/v1/discord-passthrough {verb, path} (+ confirm_id for
 *   mutations): fixed discord.com base, verb+path allowlist in code, admin
 *   scope, audit per call.
 * - GET /admin/v1/audit: rows scoped to the caller's token guilds; operators
 *   see the full cross-guild view.
 * - MCP tools/call get_admin_audit {}: same guild/token scoping over POST /mcp.
 * - Docs + staging prove-out runbook merged; the allowlist-addition cadence
 *   names its owner.
 *
 * Test-token convention (SHARED with the slice 0/1 acceptance suites, which
 * pin the gateway side): when the production registry binding is unbound the
 * gateway validates admin bearers against the ADMIN_API_TOKENS_JSON env
 * snapshot — the redirect-store dual-mode precedent (bound Hyperdrive in
 * production, JSON snapshot otherwise; see wrangler/src/redirect-store.ts).
 * Each record is {token_id ("adm_"+16 hex), secret_hash (sha256 hex of the
 * bearer), scopes (subset of read/moderate/admin), guild_ids (string
 * snowflakes), expires_at (RFC3339), revoked_at (null when live)}. The
 * gateway hashes the presented bearer and compares hashes, byte-identically
 * in both modes. Synthetic bearers below are >= 32 chars (spec §4.1 floor).
 *
 * Status contract pinned here (spec §5.2 fixed 401/403 + REST conventions):
 * - missing/unknown bearer -> 401, zero container forwards, zero DO resolve.
 * - well-formed but non-allowlisted (verb, path) -> 403 JSON {error}, refused
 *   before the container is touched (zero forwards, zero DO resolve).
 * - malformed body (bad JSON, missing/mistyped verb/path) -> 400 JSON {error}.
 * - non-admin scope on passthrough -> 403 (admin scope required, decision A4).
 * - mutating passthrough without confirm_id -> 400 or 403, never forwarded
 *   (set-pin on purpose: allowlist-first and confirm-first orders both refuse).
 * - admitted passthrough -> 200 JSON relaying the Discord result.
 * - audit reads -> 200 JSON {rows: [...]}; every row carries at least the
 *   spec §4.4 columns token_id, guild_id, verb, decision.
 *
 * Fixed probe: GET /api/v10/users/@me is the ONE allowlist entry this suite
 * requires. Rationale: it is the most harmless Discord read (bot self-read),
 * so it doubles as the safe staging connectivity probe the runbook needs.
 * The rest of the allowlist is the builder's to choose; the refusal battery
 * below uses inputs no sane v1 allowlist contains (absolute URLs incl. an
 * absolute discord.com form, traversal incl. encoded variants, unknown verbs,
 * credential-adjacent and destructive Discord paths). If the owner wants a
 * different probe, this file returns to TOG-36199.
 *
 * Resolved ambiguities (flagged in the handoff for owner visibility):
 * - A3 says passthrough-MUTATING needs confirm while A4 says confirm per
 *   call: reads execute directly with audit (reads never need confirm, A3);
 *   only mutations require confirm_id. This suite never sends confirm_id for
 *   the GET probe and asserts unconfirmed mutations are refused.
 * - The legacy MCP_TOKEN bearer is neither an admin-API token nor an
 *   operator token, so get_admin_audit with MCP_TOKEN is refused. The test
 *   accepts HTTP 401 (slices 4-6 retire MCP_TOKEN from /mcp) or a JSON-RPC
 *   error other than -32601 (known tool, refused by scope).
 * - "Full view operators" = holders of the existing OPS_CONTROL_TOKEN
 *   operator bearer (wrangler/src/ops-control.ts). GET /admin/v1/audit honors
 *   both credentials: admin-API tokens get the guild-filtered view, the
 *   operator bearer gets every row.
 * - Audit scoping is enforced at BOTH layers (spec §4.3/§5.2 philosophy):
 *   the Worker forwards the token's guild scope to the container AND filters
 *   returned rows to the token's guilds. The stub container below always
 *   answers with mixed-guild rows, so a Worker that relays verbatim fails.
 * - The caller bearer is never forwarded to the container (mcp.test.ts
 *   precedent); scope claims travel in MAC-covered headers instead.
 * - Audit-row WRITES (every authenticated call writes one, decision A5) are
 *   container-side and owned by the slice-2 suite plus the builder's unit
 *   tests; this Worker-level suite cannot observe the container's Postgres.
 *
 * Docs contract (content search across docs/*.md, runbook.test.ts pattern —
 * filenames are the builder's choice, one coherent doc must hold each set):
 * - allowlist doc: "discord-passthrough" + "allowlist".
 * - staging prove-out runbook: staging + prove-out + discord-passthrough +
 *   audit + a refusal step + a guild-scoping step.
 * - cadence owner: the discord-passthrough doc states each allowlist addition
 *   is reviewed and names the owning CISO/owner/operator (per-addition
 *   review, decision A4).
 * - the runbook documents the users/@me prove-out probe.
 * - guards (pass vacuously while docs are absent): no secret values and no
 *   private tracker IDs in the passthrough docs (public repo).
 * NOTE for the builder: if the runbook lands in docs/runbook.md, its shell
 * fences must stay one-line curl/npm/two-bot commands (runbook.test.ts).
 *
 * Rate-budget isolation: buckets are per-isolate module singletons, so every
 * request here carries a fresh TEST-NET-1 caller IP AND a freshly issued
 * token — per-IP and per-token gateway budgets never bleed across cases.
 *
 * RED expectation (today): every P/A/D test fails — POST /admin/* falls
 * through to the redirect lookup (405: POST is not a campaign method), GET
 * /admin/* misses the empty snapshot (404), tools/call get_admin_audit is
 * -32601, and no doc mentions the passthrough. The C* controls pass now and
 * at green, proving the harness itself is sound.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import worker, { type Env } from "../src/index.ts";

const sha256hex = (value: string): string =>
  createHash("sha256").update(value, "utf8").digest("hex");

// The stub container always answers with mixed-guild rows; the Worker must
// filter to the caller's token guilds (defense in depth, see header).
const GUILD_A = "100000000000000011";
const GUILD_B = "100000000000000022";

const MCP_TOKEN = "synthetic-admin-slice7-mcp-token-0123456789";
const OPS_TOKEN = "synthetic-admin-slice7-operator-token-0123";

// The one allowlist entry this suite requires (see header for rationale).
const PROBE_VERB = "GET";
const PROBE_PATH = "/api/v10/users/@me";

interface SnapshotRecord {
  token_id: string;
  secret_hash: string;
  scopes: string[];
  guild_ids: string[];
  expires_at: string;
  revoked_at: null;
}

interface IssuedToken {
  bearer: string;
  record: SnapshotRecord;
}

let tokenSeq = 0;
function issueToken(scopes: string[], guildIds: string[]): IssuedToken {
  tokenSeq += 1;
  const bearer = `synthetic-admin-bearer-${String(tokenSeq).padStart(4, "0")}-0123456789abcdef`;
  assert.ok(bearer.length >= 32, "synthetic bearers meet the 32-byte spec floor");
  return {
    bearer,
    record: {
      token_id: `adm_${tokenSeq.toString(16).padStart(16, "0")}`,
      secret_hash: sha256hex(bearer),
      scopes,
      guild_ids: guildIds,
      expires_at: "2030-01-01T00:00:00.000Z",
      revoked_at: null,
    },
  };
}

// Direct outbound fetches from the Worker edge: passthrough goes through the
// container binding, never a Worker-side fetch — and never to a caller URL.
// Record-only (never throws): assertions below prove the list stays empty.
const directFetches: string[] = [];
globalThis.fetch = (async (input: unknown) => {
  const url = typeof input === "string" ? input : String((input as { url?: unknown })?.url ?? input);
  directFetches.push(url);
  return Response.json({ synthetic: "direct-fetch-called", url }, { status: 500 });
}) as typeof fetch;

interface Forwarded {
  method: string;
  url: string;
  headers: Record<string, string>;
  body: string;
}

function harness(
  respond: (req: Request) => Response | Promise<Response>,
  tokens: IssuedToken[] = [],
) {
  const forwarded: Forwarded[] = [];
  const names: string[] = [];
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    // The ownership fence refuses forwards without a deployment id.
    CF_VERSION_METADATA: { id: "synthetic-admin-slice7-deployment" },
    MCP_TOKEN,
    OPS_CONTROL_TOKEN: OPS_TOKEN,
    ADMIN_API_TOKENS_JSON: JSON.stringify(tokens.map((t) => t.record)),
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

let callerSeq = 0;
const callerIp = (): string => {
  callerSeq += 1;
  return `192.0.2.${(callerSeq % 200) + 1}`;
};

const authHeaders = (auth: string | null): Record<string, string> => ({
  ...(auth ? { authorization: auth } : {}),
  "content-type": "application/json",
  "cf-connecting-ip": callerIp(),
});

const passthroughRequest = (body: unknown, auth: string | null): Request =>
  new Request("https://probe.invalid/admin/v1/discord-passthrough", {
    method: "POST",
    headers: authHeaders(auth),
    body: typeof body === "string" ? body : JSON.stringify(body),
  });

const auditRequest = (auth: string | null): Request =>
  new Request("https://probe.invalid/admin/v1/audit", {
    method: "GET",
    headers: authHeaders(auth),
  });

const rpc = (method: string, params: unknown, auth: string | null): Request =>
  new Request("https://probe.invalid/mcp", {
    method: "POST",
    headers: authHeaders(auth),
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });

async function jsonBody(res: Response): Promise<any> {
  assert.match(res.headers.get("content-type") ?? "", /application\/json/);
  return (await res.json()) as any;
}

function auditRow(tokenId: string, guildId: string, verb: string, receipt: string): Record<string, unknown> {
  return {
    token_id: tokenId,
    scope: "admin",
    guild_id: guildId,
    verb,
    params_redacted: { verb: "GET" },
    dry_run: false,
    confirm_id: null,
    decision: "allowed",
    receipt,
    created_at: "2026-10-21T00:00:01.000Z",
  };
}

/** Mixed-guild container answer in the get_audit excerpt shape. */
const mixedAuditExcerpt = () => Response.json({
  available: true,
  rows: [
    auditRow("adm_00000000000000a1", GUILD_A, "discord.passthrough", "synthetic-receipt-a"),
    auditRow("adm_00000000000000b2", GUILD_B, "channel.create", "synthetic-receipt-b"),
  ],
  truncated: false,
});

function assertNoBearerLeak(forwarded: Forwarded[], bearer: string): void {
  for (const call of forwarded) {
    assert.equal(
      call.headers["authorization"],
      undefined,
      "the caller bearer must never be forwarded to the container",
    );
    assert.ok(
      !call.body.includes(bearer),
      "the caller bearer secret must not appear in forwarded bodies",
    );
  }
}

// ---------------------------------------------------------------------------
// Passthrough
// ---------------------------------------------------------------------------

test("P1: unauthenticated passthrough -> 401, zero forwards", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  for (const auth of [null, "Bearer wrong-bearer-value-0123456789abcdef"] as const) {
    const res = await worker.fetch(
      passthroughRequest({ verb: PROBE_VERB, path: PROBE_PATH }, auth),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 401, "the route exists and is gated (today: redirect 405)");
    await res.text();
  }
  assert.deepEqual(h.names, [], "unauthenticated calls must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "unauthenticated calls must never reach the container");
});

test("P2: admin token + GET users/@me -> 200, one forward carrying verb+path, never a URL", async () => {
  const marker = "synthetic-probe-user-marker";
  const token = issueToken(["admin"], [GUILD_A]);
  const h = harness(
    () => Response.json({ id: GUILD_A, username: marker, discriminator: "0" }),
    [token],
  );
  const directBefore = directFetches.length;
  const res = await worker.fetch(
    passthroughRequest({ verb: PROBE_VERB, path: PROBE_PATH }, `Bearer ${token.bearer}`),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await jsonBody(res);
  assert.ok(
    JSON.stringify(body).includes(marker),
    "the admitted call relays the Discord result",
  );
  assert.equal(h.forwarded.length, 1, "one admitted call forwards exactly once, via the container binding");
  assert.deepEqual(h.names, ["two-bot"], "passthrough forwards through the fenced singleton");
  const forward = h.forwarded[0]!;
  assert.ok(forward.body.includes(PROBE_VERB), "the forward carries the verb");
  assert.ok(forward.body.includes(PROBE_PATH), "the forward carries the path");
  assert.ok(
    !forward.body.includes("https://") && !forward.body.includes("http://"),
    "the edge forwards verb+path; only the container builds the fixed discord.com URL",
  );
  assertNoBearerLeak(h.forwarded, token.bearer);
  assert.equal(
    directFetches.length,
    directBefore,
    "even admitted calls never fetch directly from the Worker edge",
  );
});

test("P3: non-allowlisted destructive/credential paths -> 403, zero forwards", async () => {
  const cases: [string, string][] = [
    ["DELETE", `/api/v10/guilds/${GUILD_A}`],
    ["POST", "/api/v10/oauth2/token"],
  ];
  for (const [verb, path] of cases) {
    const token = issueToken(["admin"], [GUILD_A]);
    const h = harness(() => Response.json({ synthetic: true }), [token]);
    const res = await worker.fetch(
      passthroughRequest({ verb, path }, `Bearer ${token.bearer}`),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 403, `${verb} ${path} must be refused as non-allowlisted`);
    const body = await jsonBody(res);
    assert.equal(typeof body.error, "string", "refusals are fixed-shape JSON errors");
    assert.deepEqual(h.names, [], "refused paths must not resolve the DO");
    assert.deepEqual(h.forwarded, [], "refused paths must never reach the container");
  }
});

test("P4: caller-supplied URLs are impossible", async () => {
  const evilHost = "evil.example";
  // Well-formed strings no allowlist contains: absolute URLs (even an
  // absolute discord.com form — only path form is accepted), traversal incl.
  // encoded variants, unknown verbs, internal/wrong-version paths.
  const pathCases: [string, string][] = [
    ["GET", `https://${evilHost}/collect`],
    ["GET", "https://discord.com/api/v10/users/@me"],
    ["GET", `//${evilHost}/x`],
    ["GET", `/api/v10/channels/123/../../guilds/${GUILD_A}`],
    ["GET", "/api/v10/channels/%2e%2e/%2e%2e/guilds"],
    ["GET", "/api/v10/channels/123\\..\\guilds"],
    ["GET", "not-a-path"],
    ["GET", ""],
    ["GET", "/admin/v1/audit"],
    ["GET", "/api/v11/users/@me"],
    ["TRACE", PROBE_PATH],
    ["CONNECT", PROBE_PATH],
    ["FOO", PROBE_PATH],
  ];
  for (const [verb, path] of pathCases) {
    const token = issueToken(["admin"], [GUILD_A]);
    const h = harness(() => Response.json({ synthetic: true }), [token]);
    const res = await worker.fetch(
      passthroughRequest({ verb, path }, `Bearer ${token.bearer}`),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 403, `${verb} ${JSON.stringify(path)} must be refused`);
    const text = await res.text();
    assert.ok(!text.includes(evilHost), "refusals must not reflect the hostile host");
    assert.deepEqual(h.forwarded, [], "hostile inputs must never reach the container");
  }
  // A url/host/base field in the body is never honored: strict schemas 400,
  // lenient ones ignore it and judge verb+path — either way nothing evil is
  // forwarded and nothing is fetched.
  const fieldCases: Record<string, unknown>[] = [
    { verb: PROBE_VERB, path: PROBE_PATH, url: `https://${evilHost}/collect` },
    { verb: PROBE_VERB, path: PROBE_PATH, host: evilHost },
    { verb: PROBE_VERB, path: PROBE_PATH, base: `https://${evilHost}` },
  ];
  for (const body of fieldCases) {
    const token = issueToken(["admin"], [GUILD_A]);
    const h = harness(() => Response.json({ synthetic: true }), [token]);
    const directBefore = directFetches.length;
    const res = await worker.fetch(
      passthroughRequest(body, `Bearer ${token.bearer}`),
      h.env,
      h.ctx,
    );
    assert.ok(
      res.status === 400 || res.status === 403,
      `url-bearing body must be refused, got ${res.status}`,
    );
    const text = await res.text();
    assert.ok(!text.includes(evilHost), "refusals must not reflect the hostile host");
    for (const call of h.forwarded) {
      assert.ok(!call.body.includes(evilHost), "forwards must never carry the hostile host");
    }
    assert.equal(directFetches.length, directBefore, "no direct fetch of the hostile URL");
  }
  assert.ok(
    !directFetches.join("\n").includes(evilHost),
    "the hostile host is never fetched directly",
  );
});

test("P5: non-admin scope on passthrough -> 403, zero forwards", async () => {
  for (const scopes of [["moderate"], ["read"]] as const) {
    const token = issueToken([...scopes], [GUILD_A]);
    const h = harness(() => Response.json({ synthetic: true }), [token]);
    const res = await worker.fetch(
      passthroughRequest({ verb: PROBE_VERB, path: PROBE_PATH }, `Bearer ${token.bearer}`),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 403, `${scopes} scope must not open the passthrough (admin only)`);
    await res.text();
    assert.deepEqual(h.forwarded, [], "scope-refused calls must never reach the container");
  }
});

test("P6: mutating passthrough without confirm_id is refused and never forwarded", async () => {
  const token = issueToken(["admin"], [GUILD_A]);
  const h = harness(() => Response.json({ synthetic: true }), [token]);
  const res = await worker.fetch(
    passthroughRequest(
      { verb: "POST", path: `/api/v10/channels/${GUILD_A}/messages` },
      `Bearer ${token.bearer}`,
    ),
    h.env,
    h.ctx,
  );
  assert.ok(
    res.status === 400 || res.status === 403,
    `unconfirmed mutation must be refused, got ${res.status}`,
  );
  await res.text();
  assert.deepEqual(h.forwarded, [], "unconfirmed mutations must never reach the container");
});

test("P7: malformed passthrough body -> 400, zero forwards", async () => {
  const bodies: unknown[] = [
    "{not-json",
    {},
    { verb: PROBE_VERB },
    { path: PROBE_PATH },
    { verb: 42, path: PROBE_PATH },
    { verb: PROBE_VERB, path: null },
  ];
  for (const body of bodies) {
    const token = issueToken(["admin"], [GUILD_A]);
    const h = harness(() => Response.json({ synthetic: true }), [token]);
    const res = await worker.fetch(
      passthroughRequest(body, `Bearer ${token.bearer}`),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 400, `malformed body ${JSON.stringify(body)} must be a 400`);
    await res.text();
    assert.deepEqual(h.forwarded, [], "malformed bodies must never reach the container");
  }
});

// ---------------------------------------------------------------------------
// Audit query (REST + MCP)
// ---------------------------------------------------------------------------

test("A1: unauthenticated audit query -> 401, zero forwards", async () => {
  const h = harness(() => mixedAuditExcerpt());
  for (const auth of [null, "Bearer wrong-bearer-value-0123456789abcdef"] as const) {
    const res = await worker.fetch(auditRequest(auth), h.env, h.ctx);
    assert.equal(res.status, 401, "the route exists and is gated (today: redirect 404)");
    await res.text();
  }
  assert.deepEqual(h.names, [], "unauthenticated calls must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "unauthenticated calls must never reach the container");
});

function assertAuditRows(body: any, guildIds: string[]): void {
  assert.ok(Array.isArray(body.rows), "audit answers carry a rows array");
  assert.deepEqual(
    body.rows.map((row: any) => row.guild_id).sort(),
    [...guildIds].sort(),
    "rows are scoped to exactly the token guilds",
  );
  for (const row of body.rows) {
    for (const column of ["token_id", "guild_id", "verb", "decision"]) {
      assert.ok(row[column] !== undefined, `audit rows carry the spec column ${column}`);
    }
  }
}

test("A2: read token sees only its own guild rows", async () => {
  const token = issueToken(["read"], [GUILD_A]);
  const h = harness(() => mixedAuditExcerpt(), [token]);
  const res = await worker.fetch(auditRequest(`Bearer ${token.bearer}`), h.env, h.ctx);
  assert.equal(res.status, 200);
  assertAuditRows(await jsonBody(res), [GUILD_A]);
  assert.equal(h.forwarded.length, 1, "one audit query forwards exactly once");
  assert.ok(
    h.forwarded[0]!.body.includes(GUILD_A),
    "the token guild scope is forwarded to the container",
  );
  assert.ok(
    !h.forwarded[0]!.body.includes(GUILD_B),
    "out-of-scope guilds are never forwarded",
  );
  assertNoBearerLeak(h.forwarded, token.bearer);
});

test("A2b: guild-B token sees only guild B (symmetric scoping)", async () => {
  const token = issueToken(["read"], [GUILD_B]);
  const h = harness(() => mixedAuditExcerpt(), [token]);
  const res = await worker.fetch(auditRequest(`Bearer ${token.bearer}`), h.env, h.ctx);
  assert.equal(res.status, 200);
  assertAuditRows(await jsonBody(res), [GUILD_B]);
});

test("A2c: multi-guild token sees both guilds", async () => {
  const token = issueToken(["moderate"], [GUILD_A, GUILD_B]);
  const h = harness(() => mixedAuditExcerpt(), [token]);
  const res = await worker.fetch(auditRequest(`Bearer ${token.bearer}`), h.env, h.ctx);
  assert.equal(res.status, 200);
  assertAuditRows(await jsonBody(res), [GUILD_A, GUILD_B]);
});

test("A3: operator bearer gets the full cross-guild view", async () => {
  const h = harness(() => mixedAuditExcerpt());
  const res = await worker.fetch(auditRequest(`Bearer ${OPS_TOKEN}`), h.env, h.ctx);
  assert.equal(res.status, 200);
  assertAuditRows(await jsonBody(res), [GUILD_A, GUILD_B]);
});

test("A4: MCP get_admin_audit with an admin-API token returns scoped rows", async () => {
  const token = issueToken(["read"], [GUILD_A]);
  const h = harness(() => mixedAuditExcerpt(), [token]);
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_admin_audit", arguments: {} }, `Bearer ${token.bearer}`),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await jsonBody(res);
  assert.equal(body.error, undefined, "scoped admin audit reads succeed over MCP");
  assertAuditRows(body.result?.structuredContent, [GUILD_A]);
  assertNoBearerLeak(h.forwarded, token.bearer);
});

test("A5: MCP get_admin_audit with the read-only MCP bearer is refused", async () => {
  const h = harness(() => mixedAuditExcerpt());
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_admin_audit", arguments: {} }, `Bearer ${MCP_TOKEN}`),
    h.env,
    h.ctx,
  );
  if (res.status === 401) {
    // Slices 4-6 retired the legacy bearer from /mcp: still refused.
    await res.text();
  } else {
    assert.equal(res.status, 200);
    const body = await jsonBody(res);
    assert.ok(body.error, "the read-only MCP bearer must be refused admin audit rows");
    assert.notEqual(
      body.error.code,
      -32601,
      "get_admin_audit is a known tool: refused by scope, not unknown",
    );
  }
  assert.deepEqual(h.forwarded, [], "refused callers must never reach the container");
});

test("A6 (control): anonymous MCP get_admin_audit -> 401", async () => {
  const h = harness(() => mixedAuditExcerpt());
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_admin_audit", arguments: {} }, null),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 401);
  await res.text();
  assert.deepEqual(h.forwarded, [], "unauthenticated calls must never reach the container");
});

// ---------------------------------------------------------------------------
// Docs + runbook (criterion 3)
// ---------------------------------------------------------------------------

const repoRoot = new URL("../../", import.meta.url);

function docsMatching(...patterns: RegExp[]): string[] {
  const names = readdirSync(new URL("docs/", repoRoot)).filter((name) => name.endsWith(".md"));
  return names.filter((name) => {
    const text = readFileSync(new URL(`docs/${name}`, repoRoot), "utf8");
    return patterns.every((pattern) => pattern.test(text));
  });
}

test("D1: the passthrough allowlist is documented", async () => {
  assert.ok(
    docsMatching(/discord-passthrough/, /allowlist/i).length > 0,
    "some doc under docs/ documents the discord-passthrough allowlist",
  );
});

test("D2: the staging prove-out runbook covers passthrough refusal + audit scoping", async () => {
  assert.ok(
    docsMatching(
      /staging/i,
      /prove[\s-]?out/i,
      /discord-passthrough/,
      /audit/i,
      /refus/i,
      /guild/i,
    ).length > 0,
    "one runbook doc covers the staging prove-out: passthrough refusal + guild-scoped audit",
  );
});

test("D3: the allowlist-addition cadence names its owner", async () => {
  assert.ok(
    docsMatching(
      /discord-passthrough/,
      /allowlist/i,
      /addition|adding|change/i,
      /review|approv/i,
      /ciso|owner|operator/i,
      /\beach\b|\bevery\b/i,
    ).length > 0,
    "the discord-passthrough doc states each allowlist addition is reviewed and names the owning CISO/owner/operator",
  );
});

test("D4: the runbook documents the users/@me prove-out probe", async () => {
  assert.ok(
    docsMatching(/discord-passthrough/, /users\/@me/).length > 0,
    "the runbook documents the GET users/@me probe this suite admits",
  );
});

test("D5 (guard): passthrough docs carry no secret values", async () => {
  for (const name of docsMatching(/discord-passthrough/)) {
    const text = readFileSync(new URL(`docs/${name}`, repoRoot), "utf8");
    assert.doesNotMatch(
      text,
      /(?:token|secret)\s*[:=]\s*[A-Za-z0-9_-]{16,}/i,
      `${name} must not embed token/secret values`,
    );
  }
});

test("D6 (guard): passthrough docs carry no private tracker references", async () => {
  for (const name of docsMatching(/discord-passthrough/)) {
    const text = readFileSync(new URL(`docs/${name}`, repoRoot), "utf8");
    assert.doesNotMatch(
      text,
      /\b(?:TOG|PAP)-\d+\b|\/(?:TOG|PAP)\/(?:issues|agents|projects|approvals|runs)\//,
      `${name} is public: no private tracker references`,
    );
  }
});

// ---------------------------------------------------------------------------
// Harness controls (pass now and at green)
// ---------------------------------------------------------------------------

test("C1 (control): GET /mcp -> 405 with Allow: POST", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(
    new Request("https://probe.invalid/mcp", {
      headers: { authorization: `Bearer ${MCP_TOKEN}` },
    }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 405);
  assert.equal(res.headers.get("allow"), "POST");
  await res.text();
  assert.deepEqual(h.forwarded, [], "refused methods must never reach the container");
});

test("C2 (control): MCP tools/list still includes get_status", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(
    rpc("tools/list", {}, `Bearer ${MCP_TOKEN}`),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await jsonBody(res);
  const names = (body.result?.tools as { name: string }[] | undefined)?.map((t) => t.name);
  assert.ok(names?.includes("get_status"), "the read toolset stays advertised");
});

test("C3 (control): valid token + unknown /admin/v1 route -> 404", async () => {
  const token = issueToken(["admin"], [GUILD_A]);
  const h = harness(() => Response.json({ synthetic: true }), [token]);
  const res = await worker.fetch(
    new Request("https://probe.invalid/admin/v1/no-such-route", {
      headers: authHeaders(`Bearer ${token.bearer}`),
    }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 404, "unknown admin routes 404 (redirect miss today, exact routing at green)");
  await res.text();
});

test("C4 (control): MCP garbage bearer -> 401, zero forwards", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(
    rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: {} }, "Bearer wrong-bearer-value-0123456789abcdef"),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 401);
  assert.match(res.headers.get("www-authenticate") ?? "", /Bearer/);
  await res.text();
  assert.deepEqual(h.names, [], "unauthenticated calls must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "unauthenticated calls must never reach the container");
});

test("C5 (control): get_status proxies one container GET (stub sanity)", async () => {
  const marker = "synthetic-admin-slice7-stub-marker";
  const h = harness(() => Response.json({ status: "ok", marker }));
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_status", arguments: {} }, `Bearer ${MCP_TOKEN}`),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await jsonBody(res);
  assert.ok(JSON.stringify(body).includes(marker), "tool result must carry the container JSON");
  assert.equal(h.forwarded.length, 1, "the stub records the proxied container fetch");
  assert.equal(h.forwarded[0]!.method, "GET");
});
