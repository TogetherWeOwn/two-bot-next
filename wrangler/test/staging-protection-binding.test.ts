/** Claim and plan binding checks for the migration protection Worker, run
 * against the Rust cutover fixtures parsed losslessly by `parseJson`. */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { parseJson } from "../src/staging-protection/encoding.ts";
import type { Json } from "../src/staging-protection/encoding.ts";
import { projection, same, validateClaim, validatePlan } from "../src/staging-protection/binding.ts";

const fixtures = new URL("../../crates/cutover/tests/fixtures/", import.meta.url);
const claimBytes = readFileSync(new URL("staging-migrate-apply-claim.json", fixtures));
const planBytes = readFileSync(new URL("staging-migrate-plan.json", fixtures));

const RUN = "37110947191";
const ATTEMPT = "2";
const HEAD = "b".repeat(40);
const PINS = { host: "staging.invalid", database: "two_bot", branch_id: "" };

// eslint-free loose mutation view over a freshly parsed fixture.
type Rec = Record<string, any>;
const freshClaim = (mutate?: (c: Rec) => void): Json => {
  const c = parseJson(new Uint8Array(claimBytes)) as Rec;
  mutate?.(c);
  return c as Json;
};
const freshPlan = (mutate?: (p: Rec) => void): Json => {
  const p = parseJson(new Uint8Array(planBytes)) as Rec;
  mutate?.(p);
  return p as Json;
};
const claimOf = (value: Json) => validateClaim(value, RUN, ATTEMPT, HEAD, PINS);
const evidenceRefused = (e: unknown) => e instanceof Error && e.message === "evidence_refused";
const claimRefused = (mutate: (c: Rec) => void, pins = PINS, head = HEAD) =>
  assert.throws(() => validateClaim(freshClaim(mutate), RUN, ATTEMPT, head, pins), evidenceRefused);
const planRefused = async (mutatePlan: (p: Rec) => void, mutateClaim?: (c: Rec) => void) => {
  const claim = claimOf(freshClaim());
  const adjusted = mutateClaim ? (freshClaim(mutateClaim) as Rec) : claim;
  await assert.rejects(validatePlan(freshPlan(mutatePlan), adjusted), evidenceRefused);
};

test("fixtures parse losslessly", () => {
  const plan = parseJson(new Uint8Array(planBytes)) as Rec;
  assert.equal(plan.pending_before[1], 9007199254740993n);
  assert.equal(plan.pending_store_before[1], 9007199254740992n);
  assert.equal(plan.runner_version, 1n);
  const claim = parseJson(new Uint8Array(claimBytes)) as Rec;
  assert.deepEqual(claim.expected_pending, ["1", "9007199254740993"]);
});

test("valid claim and plan bind, with both projections matching the fixtures", async () => {
  const claim = claimOf(freshClaim());
  const plan = freshPlan() as Rec;
  assert.equal(await projection(plan, false), plan.plan_manifest_sha256);
  assert.equal(await projection(plan, true), plan.store_plan_manifest_sha256);
  assert.equal(await projection(plan, false), claim.plan_manifest_sha256);
  assert.equal(await projection(plan, true), claim.store_plan_manifest_sha256);
  await validatePlan(plan, claim);
});

test("claim refuses non-objects, extra and missing fields", () => {
  for (const bad of [null, 1n, "x", [] as Json]) {
    assert.throws(() => claimOf(bad), evidenceRefused);
  }
  claimRefused(c => { c.extra = "x"; });
  for (const key of Object.keys(freshClaim() as Rec)) {
    claimRefused(c => { delete c[key]; });
  }
});

test("claim refuses identity and version drift", () => {
  claimRefused(c => { c.schema_version = 2n; });
  claimRefused(c => { c.schema_version = "1"; });
  claimRefused(c => { c.kind = "staging-migrate-plan"; });
  claimRefused(c => { c.repository = "Other/two-bot-next"; });
  claimRefused(c => { c.workflow_path = ".github/workflows/other.yml"; });
  claimRefused(c => { c.environment_name = "staging-migrate"; });
  claimRefused(c => { c.apply_run_id = "37110947192"; });
  claimRefused(c => { c.apply_run_id = 37110947191n; });
  claimRefused(c => { c.apply_run_attempt = "1"; });
  claimRefused(c => { c.workflow_head_sha = "c".repeat(40); });
  claimRefused(() => undefined, PINS, "B".repeat(40));
  claimRefused(c => { c.workflow_head_sha = "b".repeat(39); }, PINS, "b".repeat(39));
});

test("claim refuses a bad plan run id or source sha", () => {
  claimRefused(c => { c.plan_run_id = RUN; });
  claimRefused(c => { c.plan_run_id = "0123"; });
  claimRefused(c => { c.plan_run_id = "0"; });
  claimRefused(c => { c.plan_run_id = ""; });
  claimRefused(c => { c.plan_run_id = 37110947190n; });
  claimRefused(c => { c.plan_run_id = "1".repeat(21); });
  claimRefused(c => { c.source_sha = "a".repeat(39); });
  claimRefused(c => { c.source_sha = "A".repeat(40); });
  claimRefused(c => { c.source_sha = "g".repeat(40); });
  claimRefused(c => { c.source_sha = 1n; });
});

test("claim refuses malformed plan digests", () => {
  for (const key of ["plan_manifest_sha256", "store_plan_manifest_sha256"]) {
    claimRefused(c => { c[key] = "a".repeat(63); });
    claimRefused(c => { c[key] = "a".repeat(65); });
    claimRefused(c => { c[key] = "A".repeat(64); });
    claimRefused(c => { c[key] = "z".repeat(64); });
    claimRefused(c => { c[key] = 0n; });
  }
});

test("claim refuses target shape drift and pin mismatches", () => {
  claimRefused(c => { c.target.extra = "x"; });
  claimRefused(c => { delete c.target.branch_id; });
  claimRefused(c => { c.target = []; });
  claimRefused(c => { c.target = "staging.invalid"; });
  claimRefused(c => { c.target.host = 1n; });
  claimRefused(c => { c.target.host = "other.invalid"; });
  claimRefused(c => { c.target.database = "other_db"; });
  claimRefused(c => { c.target.branch_id = "main"; });
  claimRefused(c => { c.target.host = "bad host"; });
  claimRefused(c => { c.target.host = "-staging.invalid"; });
  claimRefused(c => { c.target.database = ""; });
  claimRefused(c => { c.target.database = "a".repeat(64); });
  claimRefused(c => { c.target.branch_id = "a b"; });
  // A matching pin does not excuse a production-looking target.
  claimRefused(c => { c.target.host = "prod.invalid"; }, { ...PINS, host: "prod.invalid" });
  claimRefused(c => { c.target.database = "Two_Prod"; }, { ...PINS, database: "Two_Prod" });
  // A PlanetScale host needs a branch id, even when pinned that way.
  claimRefused(c => { c.target.host = "db.psdb.cloud"; }, { ...PINS, host: "db.psdb.cloud" });
  assert.ok(validateClaim(
    freshClaim(c => { c.target.host = "db.psdb.cloud"; c.target.branch_id = "br-1"; }),
    RUN, ATTEMPT, HEAD, { host: "db.psdb.cloud", database: "two_bot", branch_id: "br-1" },
  ));
});

test("claim refuses malformed pending version lists", () => {
  for (const key of ["expected_pending", "expected_store_pending"]) {
    claimRefused(c => { c[key] = "1"; });
    claimRefused(c => { c[key] = [1n]; });
    claimRefused(c => { c[key] = ["0"]; });
    claimRefused(c => { c[key] = ["01"]; });
    claimRefused(c => { c[key] = ["2", "1"]; });
    claimRefused(c => { c[key] = ["1", "1"]; });
    claimRefused(c => { c[key] = ["-1"]; });
    claimRefused(c => { c[key] = ["9223372036854775808"]; });
    claimRefused(c => { c[key] = ["1".repeat(21)]; });
    assert.ok(claimOf(freshClaim(c => { c[key] = ["9223372036854775807"]; })));
    assert.ok(claimOf(freshClaim(c => { c[key] = []; })));
  }
});

test("claim refuses bad recovery and ACL references", () => {
  for (const key of ["recovery_evidence_ref", "acl_plan_ref"]) {
    claimRefused(c => { c[key] = ""; });
    claimRefused(c => { c[key] = 1n; });
    claimRefused(c => { c[key] = "has space"; });
    claimRefused(c => { c[key] = "tab\there"; });
    claimRefused(c => { c[key] = "user@host"; });
    claimRefused(c => { c[key] = "https://example.invalid/x"; });
    claimRefused(c => { c[key] = "a".repeat(201); });
    claimRefused(c => { c[key] = "é".repeat(101); });
    assert.ok(claimOf(freshClaim(c => { c[key] = "a".repeat(200); })));
  }
});

test("plan refuses mode, role, target and version drift", async () => {
  await planRefused(p => { p.runner_version = 2n; });
  await planRefused(p => { p.runner_version = 1; });
  await planRefused(p => { p.mode = "apply"; });
  await planRefused(p => { p.migration_target = "production"; });
  await planRefused(p => { p.role = "two_bot_migrator"; });
  await planRefused(p => { delete p.role; });
});

test("plan refuses applied counts, provenance and role-verification drift", async () => {
  await planRefused(p => { p.applied_count = 1n; });
  await planRefused(p => { p.applied_store_count = 1n; });
  await planRefused(p => { p.applied_count = 0; });
  await planRefused(p => { delete p.applied_store_count; });
  await planRefused(p => { p.plan_provenance_verified = true; });
  await planRefused(p => { p.plan_run_id = "37110947190"; });
  await planRefused(p => { p.role_verified_connections = 0n; });
  await planRefused(p => { p.role_verified_connections = 1; });
});

test("plan refuses a changed ledger in either set", async () => {
  await planRefused(p => { p.ledger_before = [1n]; });
  await planRefused(p => { p.ledger_after = [1n]; });
  await planRefused(p => { p.ledger_store_before = [400n]; });
  await planRefused(p => { p.ledger_store_after = [400n]; });
  await planRefused(p => { p.ledger_before = [1n]; p.ledger_after = [2n]; });
  await planRefused(p => { p.ledger_after = "x"; });
  await planRefused(p => { p.ledger_store_after = {}; });
});

test("plan refuses references that do not match the claim", async () => {
  await planRefused(p => { p.source_sha = "c".repeat(40); });
  await planRefused(p => { p.target.database = "other_db"; });
  await planRefused(p => { p.target.extra = "x"; });
  await planRefused(p => { p.recovery_evidence_ref = "recovery-review#other"; });
  await planRefused(p => { p.acl_plan_ref = "acl-review#other"; });
  await planRefused(p => { delete p.acl_plan_ref; });
});

test("plan refuses digests that do not match the claim or the projection", async () => {
  const other = "1".repeat(64);
  await planRefused(p => { p.plan_manifest_sha256 = other; });
  await planRefused(p => { p.store_plan_manifest_sha256 = other; });
  await planRefused(() => undefined, c => { c.plan_manifest_sha256 = other; });
  await planRefused(() => undefined, c => { c.store_plan_manifest_sha256 = other; });
  // Plan and claim agree with each other but not with the recomputed projection.
  await planRefused(p => { p.plan_manifest_sha256 = other; }, c => { c.plan_manifest_sha256 = other; });
  await planRefused(p => { p.store_plan_manifest_sha256 = other; }, c => { c.store_plan_manifest_sha256 = other; });
  // Swapping the two digests binds the wrong projection to each ledger.
  await planRefused(
    p => { [p.plan_manifest_sha256, p.store_plan_manifest_sha256] = [p.store_plan_manifest_sha256, p.plan_manifest_sha256]; },
    c => { [c.plan_manifest_sha256, c.store_plan_manifest_sha256] = [c.store_plan_manifest_sha256, c.plan_manifest_sha256]; },
  );
});

test("plan refuses projection input tampering", async () => {
  await planRefused(p => { p.source_migrations[0].description = "changed"; });
  await planRefused(p => { p.source_store_migrations[1].description = "changed"; });
  await planRefused(p => { p.source_migrations[1].version = 9007199254740992n; });
  await planRefused(p => { p.source_store_migrations[0].version = 401n; });
  await planRefused(p => { p.source_migrations[0].sha384 = "0".repeat(96); });
  await planRefused(p => { p.source_migrations[0].sha384_recomputed = "0".repeat(96); });
  await planRefused(p => { p.source_store_migrations[0].sha384_recomputed = "0".repeat(96); });
  await planRefused(p => { p.source_migrations[0].sha384 = "A".repeat(96); p.source_migrations[0].sha384_recomputed = "A".repeat(96); });
  await planRefused(p => { p.source_migrations[0].description = "a\nb"; });
  await planRefused(p => { p.source_migrations.reverse(); });
  await planRefused(p => { p.source_store_migrations = {}; });
  await planRefused(p => { p.source_sha = "a".repeat(39); }, c => { c.source_sha = "a".repeat(39); });
});

test("plan refuses pending lists that disagree with the claim or the source", async () => {
  await planRefused(p => { p.pending_before = [1n]; });
  await planRefused(p => { p.pending_before = [1n, 9007199254740992n]; });
  await planRefused(p => { p.pending_before = [9007199254740993n, 1n]; });
  await planRefused(p => { p.pending_before = [1n, 1n]; });
  await planRefused(p => { p.pending_before = [1, 9007199254740993]; });
  await planRefused(p => { p.pending_store_before = [400n]; });
  await planRefused(p => { p.pending_store_before = [400n, 9007199254740993n]; });
  await planRefused(p => { p.pending_store_before = [9007199254740992n, 400n]; });
  await planRefused(p => { p.pending_before = "x"; });
  await planRefused(() => undefined, c => { c.expected_pending = ["1"]; });
  await planRefused(() => undefined, c => { c.expected_store_pending = ["400"]; });
  await planRefused(() => undefined, c => { c.expected_pending = ["1", "9007199254740992"]; });
});

test("same compares bigint, array and object values structurally", () => {
  assert.equal(same(1n, 1n), true);
  assert.equal(same(1n, 2n), false);
  assert.equal(same([1n, "a"], [1n, "a"]), true);
  assert.equal(same([1n], [1n, 2n]), false);
  assert.equal(same({ a: 1n }, { a: 1n }), true);
  assert.equal(same({ a: 1n }, { b: 1n }), false);
  assert.equal(same([], {}), false);
  assert.equal(same(null, {}), false);
});
