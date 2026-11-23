import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { test } from "node:test";

// CR-C14N-1 Appendix A only. No production consumers, token handling or I/O
// beyond reading the pinned synthetic fixture bytes as test expectations.
const DM = "two.bot.staging-reads.manifest.v1\n";
const DR = "two.bot.staging-reads.request.v1\n";
const sha256 = (text: string) => createHash("sha256").update(text, "utf8").digest("hex");
const fixture = (name: string) => readFileSync(new URL(
  `../../crates/core/tests/fixtures/counted_read_${name}.json`, import.meta.url,
));

const candidate = "b".repeat(40);
const challenge = "d".repeat(64);
const operations = ["identity", "guild_context", "bot_membership", "event_list"];

function encodeFixtureManifest() {
  // Build the single Appendix A manifest from synthetic requirements and the
  // fixed plan, not from parsed expected bytes or a caller's route inventory.
  return JSON.stringify({
    v: "1", candidate_sha: candidate,
    slots: Array.from({ length: 40 }, (_, k) => ({
      index: String(k), operation: operations[k % 4], offset_ms: String(15000 + 10000 * k),
    })),
    routes: [{ id: "fixture_route", evidence_contract_sha256: sha256("fixture route requirement\n"),
      read_operation: "identity" }],
    journeys: [{ id: "fixture_journey", evidence_contract_sha256: sha256("fixture journey requirement\n"),
      route_ids: ["fixture_route"] }],
  });
}

const d20 = /^[1-9][0-9]{0,19}$/;
const safeDecimal = /^(0|[1-9][0-9]{0,15})$/;
const g40 = /^[0-9a-f]{40}$/;
const h64 = /^[0-9a-f]{64}$/;
const deployment = /^[A-Za-z0-9_-]{1,128}$/;
const requestRules = {
  v: "1", verb: /^(challenge|begin|read|status|close)$/, environment: "staging",
  repository_id: "1396224242", repository_owner_id: "319943574",
  workflow_ref: "TogetherWeOwn/two-bot-next/.github/workflows/deploy-production.yml@refs/heads/main",
  tooling_sha: g40, event_name: /^(workflow_dispatch|workflow_run|schedule)$/,
  caller_run_id: d20, caller_run_attempt: safeDecimal, candidate_run_id: d20,
  candidate_run_attempt: safeDecimal, candidate_sha: g40, build_id: /^[A-Za-z0-9._:-]{1,128}$/,
  worker_version_id: deployment, image_digest: /^sha256:[0-9a-f]{64}$/,
  owner_deployment_id: deployment, owner_epoch: safeDecimal, lifecycle_run_id: d20,
  lifecycle_run_attempt: safeDecimal, acquired_at_ms: safeDecimal, expires_at_ms: safeDecimal,
  staging_application_id: "1469137636663758888", staging_guild_id: "1545644954272137297",
  challenge: null, incarnation: null, manifest_sha256: h64, index: null, query: null,
};
const matches = (value: unknown, rule: RegExp): value is string =>
  typeof value === "string" && rule.exec(value)?.[0] === value;
const nonzeroHex = (value: unknown, rule: RegExp) => matches(value, rule) && /[^0]/.test(value);

function encodeRequest(q: Record<string, unknown>): string {
  const entries = Object.entries(requestRules);
  if (Object.keys(q).length !== entries.length || entries.some(([key, rule]) =>
    !Object.hasOwn(q, key) || (rule instanceof RegExp ? !matches(q[key], rule) : rule !== null && q[key] !== rule),
  )) throw new Error("invalid Q fields");

  for (const [key, rule] of entries) {
    if (rule === safeDecimal && (BigInt(q[key] as string) > 9007199254740991n ||
        (q[key] === "0" && !key.endsWith("_at_ms")))) throw new Error("invalid Q decimal");
  }
  if (q.build_id === "unknown" || q.owner_deployment_id !== q.worker_version_id ||
      q.lifecycle_run_id !== q.caller_run_id || q.lifecycle_run_attempt !== q.caller_run_attempt ||
      BigInt(q.acquired_at_ms as string) >= BigInt(q.expires_at_ms as string)) throw new Error("invalid Q tuple");
  if (q.verb === "challenge" ? q.challenge !== null || q.incarnation !== null :
      !nonzeroHex(q.challenge, h64) || !nonzeroHex(q.incarnation, /^[0-9a-f]{32}$/)) {
    throw new Error("invalid Q bootstrap");
  }
  if (q.verb === "read" ? !matches(q.index, /^(0|[1-9]|[1-3][0-9])$/) : q.index !== null) {
    throw new Error("invalid Q index");
  }
  if (q.query !== (q.verb === "status" ? "measurement" : null)) throw new Error("invalid Q query");
  return JSON.stringify(Object.fromEntries(entries.map(([key]) => [key, q[key]])));
}

function decodeRequest(bytes: string) {
  // Consume only the next schema key and a restricted scalar. Duplicates
  // (even escaped spellings) cannot enter the map through this grammar.
  const scalar = /"([A-Za-z0-9_./:@-]+)"|null/y;
  const q: Record<string, unknown> = {};
  let cursor = 0;
  for (const [i, key] of Object.keys(requestRules).entries()) {
    const prefix = `${i === 0 ? "{" : ","}"${key}":`;
    if (!bytes.startsWith(prefix, cursor)) throw new Error("noncanonical Q");
    scalar.lastIndex = cursor + prefix.length;
    const parsed = scalar.exec(bytes);
    if (!parsed) throw new Error("noncanonical Q");
    q[key] = parsed[1] ?? null;
    cursor = scalar.lastIndex;
  }
  if (bytes[cursor] !== "}" || cursor + 1 !== bytes.length || encodeRequest(q) !== bytes) {
    throw new Error("noncanonical Q");
  }
  return q;
}

function syntheticRequest(verb: string) {
  const incarnation = [1, 2, 3, 4294967295].map((limb) => limb.toString(16).padStart(8, "0")).join("");
  return {
    // Deliberately not in wire order; encodeRequest owns the order.
    verb, v: "1", environment: "staging", repository_id: "1396224242", repository_owner_id: "319943574",
    workflow_ref: "TogetherWeOwn/two-bot-next/.github/workflows/deploy-production.yml@refs/heads/main",
    tooling_sha: "a".repeat(40), event_name: "workflow_dispatch", caller_run_id: "100", caller_run_attempt: "1",
    candidate_run_id: "99", candidate_run_attempt: "2", candidate_sha: candidate, build_id: "99-2",
    worker_version_id: "fixture_deployment", image_digest: `sha256:${"c".repeat(64)}`,
    owner_deployment_id: "fixture_deployment", owner_epoch: "1", lifecycle_run_id: "100", lifecycle_run_attempt: "1",
    acquired_at_ms: "1794855600000", expires_at_ms: "1794858000000", staging_application_id: "1469137636663758888",
    staging_guild_id: "1545644954272137297", challenge: verb === "challenge" ? null : challenge,
    incarnation: verb === "challenge" ? null : incarnation, manifest_sha256: sha256(DM + encodeFixtureManifest()),
    index: verb === "read" ? "0" : null, query: verb === "status" ? "measurement" : null,
  };
}

function audience(q: ReturnType<typeof syntheticRequest>) {
  const digest = sha256(DR + encodeRequest(q));
  return `urn:two:bot:staging-read:v1:${q.verb === "challenge" ? "challenge" : `${q.challenge}:${digest}`}`;
}

const requests = [
  ["challenge", "ebb43f5d18e6bdc4dbe2ba88f7718e91f9a3511744ef783e362acf21c1c4204d"],
  ["begin", "7382de091efcf9cadbc53e0a5a579ef7a203a3d31424ef8bb79bb1444a86cbf9"],
  ["read", "26eb806bdc541bdb431e37bdf373a7c56fb5cb1ab62d004674094ab413aa429f"],
  ["status", "cd08bf49580dd2f80df6b53c28e517abf5073ab654cf001e517677c9c3d0ae1e"],
  ["close", "35f99ad1084416feb0a915d20ee1675cdb88acfa2bbf26bd132daaa8f91936b3"],
];

test("counted-read Appendix A: independently encode M and all five Q vectors", () => {
  const manifest = encodeFixtureManifest();
  assert.deepEqual(Buffer.from(manifest), fixture("manifest"));
  assert.equal(sha256(DM + manifest), "0f1f9821354531dc6aedd4ee10c3ddc365e0032ac557aace69a1a76d7fbd6e1c");
  for (const [verb, digest] of requests) {
    const q = syntheticRequest(verb);
    const bytes = encodeRequest(q);
    assert.deepEqual(Buffer.from(bytes), fixture(verb), verb);
    assert.deepEqual(decodeRequest(bytes), q, verb);
    assert.equal(sha256(DR + bytes), digest, verb);
    assert.equal(audience(q), verb === "challenge" ? "urn:two:bot:staging-read:v1:challenge" :
      `urn:two:bot:staging-read:v1:${"d".repeat(64)}:${digest}`, verb);
  }
  const target = `/internal/staging-reads/v1/status?q=${Buffer.from(encodeRequest(syntheticRequest("status"))).toString("base64url")}`;
  // Length and plain SHA-256 pinned from the exact Appendix A status target.
  assert.equal(target.length, 1534);
  assert.equal(sha256(target), "bd2e931331d0992e58633caba701cb6bd34ee68fd1095a258e599886a427c688");
});

test("counted-read Appendix A: refuse N1–N5 and verify their invalid-byte digests", () => {
  const begin = encodeRequest(syntheticRequest("begin"));
  for (const [name, mutated, length, digest] of [
    ["N1", begin + "\n", 1114, "f6d9714b2b91ddcffb6cb78da4b56fae860ca57791450667a6eb9ab82044d478"],
    ["N2", begin.replace('"v":"1"', '"v":"\\u0031"'), 1118, "58cfe5a37b206022e1b61b5cf943e7c05e81b4009ecb74a21eb38c0aa1cad4e0"],
    ["N3", begin.replace('"owner_epoch":"1"', '"owner_epoch":1'), 1111, "680f1dceb6686cd1d2e52bb18f72b4debd43e5214e5b3a80f65ee686038fc623"],
    ["N4", '{"v":"1",' + begin.slice(1), 1121, "0722d1252ae714fa811e50781597338430c27c26fcd79e8d009e3aca3644440f"],
    ["N5", "{ " + begin.slice(1), 1114, "3b9bb442eebf088e5884d52d90125ea71c71756669cfcb313ed389ef52ef5d62"],
  ] as const) {
    assert.equal(mutated.length, length, name);
    assert.equal(sha256(DR + mutated), digest, name);
    assert.throws(() => decodeRequest(mutated), /noncanonical Q/, name);
  }
});

test("counted-read Q: refuse invalid field values on encode and decode", () => {
  for (const [verb, key, value] of [
    ["challenge", "challenge", challenge],
    ["begin", "incarnation", "0".repeat(32)],
    ["begin", "candidate_sha", "B".repeat(40)],
    ["read", "index", "40"],
    ["read", "index", null],
    ["status", "query", null],
    ["close", "index", "0"],
  ] as const) {
    // Use canonical field order with just the selected value replaced.
    const q = decodeRequest(encodeRequest(syntheticRequest(verb)));
    q[key] = value;
    assert.throws(() => encodeRequest(q), /invalid Q/, `${verb}.${key}`);
    assert.throws(() => decodeRequest(JSON.stringify(q)), /invalid Q/, `${verb}.${key}`);
  }
});

test("counted-read Appendix A: refuse wrong-domain and non-string audiences", () => {
  const q = syntheticRequest("begin");
  const bytes = encodeRequest(q);
  const raw = sha256(bytes);
  assert.equal(raw, "ca42a52aa0bce2edd1750b03192ebf73008ed7b101eb75db9adfe9e06a847975");
  const prefix = `urn:two:bot:staging-read:v1:${challenge}:`;
  const accepts = (claim: unknown) => claim === audience(q);
  assert.equal(accepts(audience(q)), true);
  for (const claim of [prefix + raw, prefix + sha256(DM + bytes), [audience(q)], audience(syntheticRequest("challenge"))]) {
    assert.equal(accepts(claim), false);
  }
});
