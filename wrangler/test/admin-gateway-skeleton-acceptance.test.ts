/**
 * Failing acceptance for the slice-1 Worker admin gateway skeleton
 * (approved admin API spec sections 3.2 and 5.2).
 *
 * The RED side for the edge half: gateway behavior fails against the current
 * Worker (no /admin/v1 route exists, so requests fall through to the
 * redirect handler's 404) and passes once the Builder lands the gate plus
 * the service-key signing. Test files only; no `src` changes. The Builder
 * builds on this branch and never edits this file.
 *
 * What the tests prove, through the real Worker with a recording DO stub
 * (never staging or production):
 *
 * 1. unauthenticated POST/GET /admin/v1 is refused AT THE EDGE (401 with
 *    `WWW-Authenticate`), before the container is touched: missing, empty,
 *    wrong-scheme, and over-long bearers are one indistinguishable refusal;
 * 2. the closed prove-out: no verb executes. With a well-formed bearer the
 *    gateway signs exactly one service-key request with MAC-covered
 *    scope/guild claims and bearer proof, the container refuses the verb, and the
 *    gateway relays that refusal without retrying or executing anything;
 * 3. the skeleton stays dark unless the service key is configured, answers
 *    405 with `Allow` for other methods, never routes query strings, and
 *    brute-force caps guessing (429 with `retry-after`).
 *
 * Pinned choices the Builder (and the CISO review) must honor or
 * renegotiate on the acceptance card: the `ADMIN_GATEWAY_SERVICE_KEY_ID` +
 * `ADMIN_GATEWAY_SERVICE_KEY` Worker secrets (route dark unless both are
 * set and the secret is >= 32 chars); 401 body `unauthorized` with
 * `www-authenticate: Bearer` (the /mcp shape); 405 body `method not
 * allowed` with `allow: GET, POST`; unknown verbs forward once for the
 * container's authoritative refusal (the edge never invents verbs, and the
 * forward is what proves the signing path closed); relayed refusals keep
 * the container status/code with the fixed `request refused` message (the
 * ingress projection rule: container text never leaves); bearer shape
 * `Bearer adm_<16 lowercase hex>.<32+ base64url chars>` (<=4096 chars).
 * Only syntax is checked at the edge; VALUE validation belongs to the
 * authoritative container. POST takes body.guild_id; GET takes the final
 * path segment. No caller scope, identity or guild is invented.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash, createHmac } from "node:crypto";
import worker, { type Env } from "../src/index.ts";

const SERVICE_KEY_ID = "fixture-admin-service";
const SERVICE_SECRET = "s".repeat(40);
// Public registry row identifier, not a bearer; the secret suffix is separate.
const PUBLIC_ADMIN_ID = "adm_0123456789abcdef";
const ADMIN_BEARER = `${PUBLIC_ADMIN_ID}.${"a".repeat(40)}`;
const INVALID_BEARER = `${PUBLIC_ADMIN_ID}.${"z".repeat(40)}`;
const TARGET_GUILD = "123456789012345678";
const PROBE_VERB = "probe_no_such_verb";
const SENTINEL = "synthetic-admin-gateway-do-not-echo";
const SCOPE_TAG = "two-internal-action/scope-claim/v1";

interface Forwarded {
  method: string;
  url: string;
  headers: Record<string, string>;
  bytes: Uint8Array;
}

function harness(
  bindings: { serviceSecret?: string; serviceId?: string } = {
    serviceSecret: SERVICE_SECRET, serviceId: SERVICE_KEY_ID,
  },
) {
  const { serviceSecret, serviceId } = bindings;
  const names: string[] = [];
  const forwarded: Forwarded[] = [];
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    CF_VERSION_METADATA: { id: "synthetic-admin-deployment" },
    ...(serviceId === undefined ? {} : { ADMIN_GATEWAY_SERVICE_KEY_ID: serviceId }),
    ...(serviceSecret === undefined ? {} : { ADMIN_GATEWAY_SERVICE_KEY: serviceSecret }),
    TWO_BOT: {
      getByName(name: string) {
        names.push(name);
        return {
          async fetch(req: Request) {
            const headers: Record<string, string> = {};
            req.headers.forEach((value, key) => {
              headers[key] = value;
            });
            const bytes = new Uint8Array(await req.arrayBuffer());
            forwarded.push({
              method: req.method,
              url: req.url,
              headers,
              bytes,
            });
            // The stub is the authority seam, NOT an edge token registry.
            // A missing/wrong proof is refused even under a valid service key.
            // Every authenticated verb stays closed until gateway audit exists.
            const body = JSON.parse(new TextDecoder().decode(bytes)) as Record<string, unknown>;
            const authenticated = body["admin_bearer"] === ADMIN_BEARER;
            const requestId = req.headers.get("x-request-id") ?? "unknown";
            return Response.json(
              {
                ok: false,
                error: {
                  code: authenticated ? "action_not_allowed" : "unauthorized",
                  message: SENTINEL,
                  retryable: false,
                },
                request_id: requestId,
              },
              { status: authenticated ? 403 : 401, headers: {
                "content-type": "application/json",
                // Registry-origin refusals carry the receiver's fixed challenge;
                // service-MAC failures deliberately do not. Assertions unchanged.
                ...(authenticated ? {} : { "www-authenticate": "Bearer" }),
              } },
            );
          },
        };
      },
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  return { env, ctx, names, forwarded };
}

let callerSeq = 0;
function adminRequest(
  method: string,
  path: string,
  auth: string | null,
  body?: unknown,
): Request {
  // Each case gets its own TEST-NET-1 caller, never a shared rate bucket
  // (the brute-force test owns its separate TEST-NET-3 address).
  const headers: Record<string, string> = {
    "content-type": "application/json",
    "cf-connecting-ip": `192.0.2.${(callerSeq++ % 200) + 1}`,
    "x-request-id": "01H0000000000000000000000A",
  };
  if (auth !== null) headers["authorization"] = auth;
  const hasBody = body !== undefined && method !== "GET" && method !== "HEAD";
  return new Request(`https://probe.invalid${path}`, {
    method,
    headers,
    body: hasBody ? JSON.stringify(body) : undefined,
  });
}

/** The pinned eight-line canonical over the forwarded bytes (byte-exact: the
 * gateway must sign what it sends, never a re-serialization). */
function scopedTag(
  secret: string,
  timestamp: string,
  nonce: string,
  raw: Uint8Array,
  scope: string,
  guild: string,
): string {
  const bodyHash = createHash("sha256").update(raw).digest("hex");
  const canonical = [
    SCOPE_TAG,
    "POST",
    "/internal/actions",
    timestamp,
    nonce,
    bodyHash,
    scope,
    guild,
  ].join("\n");
  return "sha256=" + createHmac("sha256", secret).update(canonical, "utf8").digest("hex");
}

async function assertEdge401(res: Response, label: string): Promise<void> {
  assert.equal(res.status, 401, `${label}: unauthenticated must be 401`);
  assert.equal(
    res.headers.get("www-authenticate"),
    "Bearer",
    `${label}: 401 carries the bearer challenge`,
  );
  const text = await res.text();
  assert.equal(text, "unauthorized", `${label}: fixed refusal body`);
  assert.ok(!text.includes(SENTINEL), `${label}: refusal echoes nothing`);
}

const BAD_BEARERS: [string, string | null][] = [
  ["missing header", null],
  ["bare scheme", "Bearer"],
  ["empty token", "Bearer "],
  ["wrong scheme", "Token synthetic-admin-bearer"],
  ["over-long token", `Bearer ${"b".repeat(4097)}`],
  ["missing secret", `Bearer ${PUBLIC_ADMIN_ID}.`],
  ["short secret", `Bearer ${PUBLIC_ADMIN_ID}.short`],
  ["bad token id", `Bearer adm_NOT_HEX_00000000.${"a".repeat(40)}`],
  ["embedded whitespace", `Bearer ${PUBLIC_ADMIN_ID}.${"a".repeat(32)} bad`],
];

test("unauthenticated POST /admin/v1 is refused at the edge", async () => {
  // (1) RED until slice 1: every malformed credential shape draws the one
  // indistinguishable 401 without resolving the DO or reading the container.
  for (const [label, auth] of BAD_BEARERS) {
    const h = harness();
    const res = await worker.fetch(
      adminRequest("POST", `/admin/v1/${PROBE_VERB}`, auth, { guild_id: TARGET_GUILD, params: { probe: "acceptance" } }),
      h.env,
      h.ctx,
    );
    await assertEdge401(res, `POST ${label}`);
    assert.deepEqual(h.names, [], `${label}: no DO lookup before auth`);
    assert.deepEqual(h.forwarded, [], `${label}: no container contact before auth`);
  }
});

test("unauthenticated GET /admin/v1 is refused at the edge", async () => {
  // (1) Same gate for reads: GET without a bearer never reaches the skeleton.
  for (const [label, auth] of BAD_BEARERS) {
    const h = harness();
    const res = await worker.fetch(adminRequest("GET", `/admin/v1/${PROBE_VERB}/${TARGET_GUILD}`, auth), h.env, h.ctx);
    await assertEdge401(res, `GET ${label}`);
    assert.deepEqual(h.names, [], `${label}: no DO lookup before auth`);
    assert.deepEqual(h.forwarded, [], `${label}: no container contact before auth`);
  }
});

test("other methods answer 405 with Allow", async () => {
  // RED until slice 1, method-first like /mcp: only GET and POST exist on
  // the namespace. (The fallthrough also 405s, but with an empty body and
  // `Allow: GET, HEAD`; the namespace shape below is what differs.)
  for (const method of ["PUT", "DELETE", "PATCH"]) {
    const h = harness();
    const res = await worker.fetch(
      adminRequest(method, `/admin/v1/${PROBE_VERB}`, `Bearer ${ADMIN_BEARER}`, { guild_id: TARGET_GUILD, params: { probe: 1 } }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 405, `${method} must be refused`);
    assert.equal(res.headers.get("allow"), "GET, POST", `${method} names the namespace`);
    assert.equal(await res.text(), "method not allowed", `${method} fixed body`);
    assert.deepEqual(h.forwarded, [], `${method}: refused before any forward`);
  }
});

test("unconfigured gateway stays dark", async () => {
  // Control, green before and after: without the service key the route does
  // not exist. Dark means fallthrough-identical (compared against a
  // definitely-unrouted path, so this never couples to the redirect
  // handler's exact shapes): never a gateway refusal, never a forward.
  const misconfigured = [
    { serviceId: SERVICE_KEY_ID },
    { serviceSecret: "short-secret", serviceId: SERVICE_KEY_ID },
    { serviceSecret: SERVICE_SECRET },
    {},
  ];
  for (const bindings of misconfigured) {
    for (const method of ["GET", "POST"]) {
      const h = harness(bindings);
      assert.equal(Object.hasOwn(h.env, "ADMIN_GATEWAY_SERVICE_KEY"), "serviceSecret" in bindings);
      assert.equal(Object.hasOwn(h.env, "ADMIN_GATEWAY_SERVICE_KEY_ID"), "serviceId" in bindings);
      const res = await worker.fetch(
        adminRequest(method, `/admin/v1/${PROBE_VERB}`, `Bearer ${ADMIN_BEARER}`, { guild_id: TARGET_GUILD, params: { probe: 1 } }),
        h.env,
        h.ctx,
      );
      const plain = await worker.fetch(
        adminRequest(method, "/synthetic-unrouted-zz9-probe", `Bearer ${ADMIN_BEARER}`, { guild_id: TARGET_GUILD, params: { probe: 1 } }),
        h.env,
        h.ctx,
      );
      assert.equal(res.status, plain.status, `${method} dark equals fallthrough status`);
      assert.equal(await res.text(), await plain.text(), `${method} dark equals fallthrough body`);
      assert.equal(
        res.headers.get("allow"),
        plain.headers.get("allow"),
        `${method} dark equals fallthrough Allow`,
      );
      assert.equal(res.headers.get("www-authenticate"), null, `${method} dark never challenges`);
      assert.deepEqual(h.names, [], "dark route resolves nothing");
      assert.deepEqual(h.forwarded, [], "dark route forwards nothing");
    }
  }
});

test("non-dedicated and off-grammar service ids keep the gateway dark", async () => {
  // Removing the dedicated-id check would let an ordinary website key expose
  // this route. All requests have valid bearer/target/secret inputs, so an
  // unrelated input refusal cannot masquerade as reserved-suffix isolation.
  const invalidIds = [
    "website-staging",
    "fixture-scope-key",
    "fixture-admin-service-extra",
    "fixture-ADMIN-SERVICE",
    "-admin-service", // zero prefix characters
    ".-admin-service",
    "fixture/key-admin-service",
    "fixture key-admin-service",
    "é-admin-service",
    `${"a".repeat(115)}-admin-service`, // 129 total bytes
  ];
  for (const serviceId of invalidIds) {
    for (const method of ["GET", "POST"]) {
      const h = harness({ serviceId, serviceSecret: SERVICE_SECRET });
      const path = method === "GET"
        ? `/admin/v1/event.read/${TARGET_GUILD}` : "/admin/v1/event.read";
      const res = await worker.fetch(adminRequest(method, path, `Bearer ${ADMIN_BEARER}`, {
        guild_id: TARGET_GUILD,
      }), h.env, h.ctx);
      const plain = await worker.fetch(adminRequest(method, "/synthetic-unrouted-zz9-probe",
        `Bearer ${ADMIN_BEARER}`, { guild_id: TARGET_GUILD }), h.env, h.ctx);
      assert.equal(res.status, plain.status, `${serviceId}: ${method} dark fallthrough status`);
      assert.equal(await res.text(), await plain.text(), `${serviceId}: ${method} dark body`);
      assert.equal(res.headers.get("allow"), plain.headers.get("allow"), `${serviceId}: dark Allow`);
      assert.equal(res.headers.get("www-authenticate"), null, `${serviceId}: never challenges`);
      assert.deepEqual(h.names, [], `${serviceId}: no DO lookup`);
      assert.deepEqual(h.forwarded, [], `${serviceId}: no container contact`);
    }
  }
});

test("dedicated service ids at grammar bounds enable signed gateway forwarding", async () => {
  // Prevent a blanket-dark workaround: 1..114 ASCII [A-Za-z0-9_-] prefix
  // characters followed by the exact suffix, and >=32-byte secrets, are live.
  for (const serviceId of ["A-admin-service", "Z_9--admin-service", SERVICE_KEY_ID,
    `${"a".repeat(114)}-admin-service`]) {
    for (const method of ["GET", "POST"]) {
      const serviceSecret = "t".repeat(32);
      const h = harness({ serviceId, serviceSecret });
      const path = method === "GET"
        ? `/admin/v1/event.read/${TARGET_GUILD}` : "/admin/v1/event.read";
      const res = await worker.fetch(adminRequest(method, path, `Bearer ${ADMIN_BEARER}`, {
        guild_id: TARGET_GUILD,
      }), h.env, h.ctx);
      assert.equal(res.status, 403, `${serviceId}: configured route relays closed dispatch`);
      assert.deepEqual(await res.json(), {
        ok: false, request_id: "01H0000000000000000000000A",
        error: { code: "action_not_allowed", message: "request refused", retryable: false },
      });
      assert.equal(h.names.length, 1, `${serviceId}: exactly one DO lookup`);
      assert.equal(h.forwarded.length, 1, `${serviceId}: exactly one signed forward`);
      const forward = h.forwarded[0]!;
      assert.equal(forward.headers["x-two-key-id"], serviceId);
      assert.equal(forward.headers["x-two-scope"], "read");
      assert.equal(forward.headers["x-two-guild"], TARGET_GUILD);
      assert.equal(forward.headers["x-two-signature"], scopedTag(serviceSecret,
        forward.headers["x-two-timestamp"]!, forward.headers["x-two-nonce"]!, forward.bytes,
        "read", TARGET_GUILD), `${serviceId}: MAC covers exact forwarded bytes and claims`);
      assert.equal(JSON.parse(new TextDecoder().decode(forward.bytes)).admin_bearer, ADMIN_BEARER);
    }
  }
});

test("query strings never route to the gateway", async () => {
  // RED until slice 1: the query is never signed, so it must never route.
  // Explicit 404 (the ingress idiom), never the fallthrough and never the
  // gate, with nothing forwarded either way.
  for (const method of ["GET", "POST"]) {
    const h = harness();
    const res = await worker.fetch(
      adminRequest(method, `/admin/v1/${PROBE_VERB}?signed=no`, `Bearer ${ADMIN_BEARER}`, { guild_id: TARGET_GUILD, params: { probe: 1 } }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 404, `${method} query string must not route`);
    assert.equal(await res.text(), "not found", `${method} explicit refusal body`);
    assert.deepEqual(h.forwarded, [], "unrouted request forwards nothing");
  }
});

test("valid bearer proves the closed path: one signed forward, refusal relayed", async () => {
  // (2) RED until slice 1: the centerpiece. A well-formed bearer signs
  // exactly one service-key request with MAC-covered claims; the container
  // refuses the unknown verb; the gateway relays that refusal with the
  // correlated request id. Nothing executes, nothing retries.
  const h = harness();
  const res = await worker.fetch(
    adminRequest("POST", `/admin/v1/${PROBE_VERB}`, `Bearer ${ADMIN_BEARER}`, { guild_id: TARGET_GUILD, params: { probe: "acceptance" } }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 403, "unknown admin verb relays the container refusal");
  assert.deepEqual(await res.json(), {
    ok: false,
    request_id: "01H0000000000000000000000A",
    error: { code: "action_not_allowed", message: "request refused", retryable: false },
  });
  assert.equal(h.forwarded.length, 1, "exactly one forward: no retry, no fan-out");
  const forward = h.forwarded[0]!;
  assert.equal(forward.method, "POST", "forward is the receiver POST");
  assert.equal(new URL(forward.url).pathname, "/internal/actions", "forward hits the receiver");
  assert.equal(forward.headers["x-two-key-id"], SERVICE_KEY_ID, "forward uses the service key id");
  const scope = forward.headers["x-two-scope"];
  const guild = forward.headers["x-two-guild"];
  assert.equal(scope, "read", "unknown probe uses the closed read claim");
  assert.equal(guild, TARGET_GUILD, "signed guild equals the explicit request target");
  assert.match(forward.headers["x-two-timestamp"] ?? "", /^\d{1,15}$/, "forward timestamp shape");
  assert.match(
    forward.headers["x-two-nonce"] ?? "",
    /^[0-9a-fA-F]{32}$/,
    "forward nonce shape",
  );
  const expected = scopedTag(
    SERVICE_SECRET,
    forward.headers["x-two-timestamp"]!,
    forward.headers["x-two-nonce"]!,
    forward.bytes,
    scope!,
    guild!,
  );
  assert.equal(
    forward.headers["x-two-signature"],
    expected,
    "forwarded MAC covers the exact forwarded bytes and claims",
  );
  const flipped = scopedTag(
    SERVICE_SECRET,
    forward.headers["x-two-timestamp"]!,
    forward.headers["x-two-nonce"]!,
    forward.bytes,
    `${scope}x`,
    guild!,
  );
  assert.notEqual(
    forward.headers["x-two-signature"],
    flipped,
    "forwarded MAC binds the claim bytes (a flipped claim must not verify)",
  );
  const body = JSON.parse(new TextDecoder().decode(forward.bytes)) as Record<string, unknown>;
  assert.equal(body["action"], PROBE_VERB, "forwarded action is not invented");
  assert.equal(body["guild_id"], TARGET_GUILD, "body and signed guild agree");
  assert.equal(body["admin_bearer"], ADMIN_BEARER, "MAC-covered body carries transient bearer proof");
  assert.equal(
    forward.headers["x-request-id"],
    "01H0000000000000000000000A",
    "forward correlates the edge request id",
  );

  // A second call mints a fresh envelope: nonces never repeat across
  // requests, so a static-nonce signer fails here, not in production.
  const res2 = await worker.fetch(
    adminRequest("POST", `/admin/v1/${PROBE_VERB}`, `Bearer ${ADMIN_BEARER}`, { guild_id: TARGET_GUILD, params: { probe: "acceptance" } }),
    h.env,
    h.ctx,
  );
  assert.equal(res2.status, 403, "second call relays the same refusal");
  assert.equal(h.forwarded.length, 2, "second call forwards once more");
  assert.notEqual(
    h.forwarded[0]!.headers["x-two-nonce"],
    h.forwarded[1]!.headers["x-two-nonce"],
    "each forward mints a fresh nonce",
  );
});

test("valid-shaped invalid bearer reaches authority and returns the fixed 401", async () => {
  for (const method of ["POST", "GET"]) {
    const h = harness();
    const path = method === "GET"
      ? `/admin/v1/${PROBE_VERB}/${TARGET_GUILD}` : `/admin/v1/${PROBE_VERB}`;
    const res = await worker.fetch(adminRequest(method, path, `Bearer ${INVALID_BEARER}`, {
      guild_id: TARGET_GUILD,
    }), h.env, h.ctx);
    await assertEdge401(res, `${method} invalid VALUE`);
    assert.equal(h.forwarded.length, 1, "registry proof requires exactly one authority contact");
    const body = JSON.parse(new TextDecoder().decode(h.forwarded[0]!.bytes));
    assert.equal(body.admin_bearer, INVALID_BEARER, "authority receives the actual presented proof");
  }
});

test("GET target guild comes from the validated path, never an invented default", async () => {
  const h = harness();
  const res = await worker.fetch(adminRequest("GET", `/admin/v1/${PROBE_VERB}/${TARGET_GUILD}`,
    `Bearer ${ADMIN_BEARER}`), h.env, h.ctx);
  assert.equal(res.status, 403);
  assert.equal(h.forwarded.length, 1);
  const forward = h.forwarded[0]!;
  assert.equal(forward.headers["x-two-guild"], TARGET_GUILD);
  assert.equal(forward.headers["x-two-scope"], "read");
  assert.equal(forward.headers["x-two-signature"], scopedTag(SERVICE_SECRET,
    forward.headers["x-two-timestamp"]!, forward.headers["x-two-nonce"]!, forward.bytes,
    "read", TARGET_GUILD));
  assert.equal(JSON.parse(new TextDecoder().decode(forward.bytes)).guild_id, TARGET_GUILD);
});

test("missing and malformed target guilds never contact the container", async () => {
  for (const guild of [undefined, "", "0", "-1", "abc", "1e3", "1".repeat(21), 123]) {
    const h = harness();
    const res = await worker.fetch(adminRequest("POST", `/admin/v1/${PROBE_VERB}`,
      `Bearer ${ADMIN_BEARER}`, { guild_id: guild }), h.env, h.ctx);
    assert.equal(res.status, 400, `invalid guild ${String(guild)} must not be invented or coerced`);
    assert.deepEqual(h.forwarded, []);
  }
  const h = harness();
  const res = await worker.fetch(adminRequest("GET", `/admin/v1/${PROBE_VERB}`,
    `Bearer ${ADMIN_BEARER}`), h.env, h.ctx);
  assert.equal(res.status, 400, "GET without target guild is invalid");
  assert.deepEqual(h.forwarded, []);
});

test("caller identity and scope overrides cannot replace request bearer binding", async () => {
  const h = harness();
  const req = adminRequest("POST", "/admin/v1/event.read", `Bearer ${INVALID_BEARER}`, {
    guild_id: TARGET_GUILD, admin_bearer: ADMIN_BEARER, token_id: PUBLIC_ADMIN_ID,
    scope: "admin", holder_kind: "human",
    params: { token_id: PUBLIC_ADMIN_ID, scope: "admin" },
  });
  req.headers.set("x-two-scope", "admin");
  req.headers.set("x-two-guild", "987654321098765432");
  const res = await worker.fetch(req, h.env, h.ctx);
  await assertEdge401(res, "unbound argument override");
  assert.equal(h.forwarded.length, 1);
  const forward = h.forwarded[0]!;
  const body = JSON.parse(new TextDecoder().decode(forward.bytes));
  assert.equal(body.admin_bearer, INVALID_BEARER, "proof comes only from request identity");
  assert.equal(forward.headers["x-two-scope"], "read", "scope comes from code, not caller headers");
  assert.equal(forward.headers["x-two-guild"], TARGET_GUILD);
});

test("known verbs relay authoritative refusal and never become edge control tools", async () => {
  for (const verb of ["event.read", "moderation.warn", "role.assign"]) {
    const h = harness();
    const res = await worker.fetch(adminRequest("POST", `/admin/v1/${verb}`,
      `Bearer ${ADMIN_BEARER}`, { guild_id: TARGET_GUILD }), h.env, h.ctx);
    assert.equal(res.status, 403, `${verb}: closed until audit exists`);
    assert.deepEqual(await res.json(), {
      ok: false, request_id: "01H0000000000000000000000A",
      error: { code: "action_not_allowed", message: "request refused", retryable: false },
    });
    assert.equal(h.forwarded.length, 1, "no retry or local executor");
  }
});

test("brute-force guessing trips the cap", async () => {
  // (3) RED until slice 1: rapid bad-bearer attempts from one caller trip a
  // 429 with retry-after instead of guessing forever. Param-agnostic (any
  // sane bucket trips within fifty); nothing is ever forwarded.
  const h = harness();
  let saw429 = false;
  for (let i = 0; i < 50; i++) {
    const res = await worker.fetch(
      new Request(`https://probe.invalid/admin/v1/${PROBE_VERB}`, {
        method: "POST",
        headers: {
          authorization: `Bearer malformed-${i}`,
          "content-type": "application/json",
          "cf-connecting-ip": "203.0.113.77",
        },
        body: JSON.stringify({ guild_id: TARGET_GUILD, params: { probe: "acceptance" } }),
      }),
      h.env,
      h.ctx,
    );
    assert.ok(
      res.status === 401 || res.status === 429,
      `attempt ${i}: only 401 or 429, got ${res.status}`,
    );
    if (res.status === 429) {
      saw429 = true;
      assert.match(res.headers.get("retry-after") ?? "", /^\d+$/, "429 names a backoff");
      assert.equal(await res.text(), "slow down\n", "429 fixed body");
    } else {
      await res.text();
    }
  }
  assert.ok(saw429, "fifty rapid guesses must trip the brute-force cap");
  assert.deepEqual(h.forwarded, [], "guessing never reaches the container");
});
