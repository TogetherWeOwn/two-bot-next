import type { Json } from "./encoding.ts";

export const REPOSITORY = "TogetherWeOwn/two-bot-next";
export const WORKFLOW = ".github/workflows/staging-migrate.yml";
export const ENVIRONMENT = "staging-migrate-apply";
export const PLAN_MARKER = "Confirm read-only plan dispatch";
const I64_MAX = 9223372036854775807n;
const HEX40 = /^[0-9a-f]{40}$/;
const HEX64 = /^[0-9a-f]{64}$/;

export function requireEvidence(condition: unknown): asserts condition {
  if (!condition) throw new Error("evidence_refused");
}
export function object(value: Json | undefined): { [key: string]: Json } {
  requireEvidence(value !== null && typeof value === "object" && !Array.isArray(value));
  return value;
}
export function array(value: Json | undefined): Json[] {
  requireEvidence(Array.isArray(value));
  return value;
}
export function text(value: Json | undefined): string {
  requireEvidence(typeof value === "string");
  return value;
}
export function id(value: Json | undefined): string {
  requireEvidence(typeof value === "bigint" && value > 0n && value < 10n ** 20n);
  return String(value);
}
export function decimal(value: Json | undefined): string {
  const result = text(value);
  requireEvidence(/^[1-9][0-9]{0,19}$/.test(result));
  return result;
}
export function same(left: Json | undefined, right: Json | undefined): boolean {
  // Float metrics are permitted only outside binding evidence. In particular,
  // rounded ledger values must never establish unchanged lossless evidence.
  if (typeof left === "number" || typeof right === "number") return false;
  if (Array.isArray(left) && Array.isArray(right)) {
    return left.length === right.length && left.every((value, i) => same(value, right[i]));
  }
  if (left && right && typeof left === "object" && typeof right === "object"
      && !Array.isArray(left) && !Array.isArray(right)) {
    const keys = Object.keys(left);
    return keys.length === Object.keys(right).length
      && keys.every(key => Object.hasOwn(right, key) && same(left[key], right[key]));
  }
  return left === right;
}
export async function sha256(bytes: Uint8Array): Promise<string> {
  const hash = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(hash), value => value.toString(16).padStart(2, "0")).join("");
}
function versions(value: Json | undefined): bigint[] {
  const values = array(value);
  let last = 0n;
  return values.map(item => {
    requireEvidence(typeof item === "bigint" && item > last && item <= I64_MAX);
    last = item;
    return item;
  });
}
export async function projection(manifest: { [key: string]: Json }, store: boolean): Promise<string> {
  const source = text(manifest.source_sha);
  requireEvidence(HEX40.test(source));
  const pending = versions(manifest[store ? "pending_store_before" : "pending_before"]);
  const rows = [source + "\n", ...pending.map(value => `${value}\n`)];
  const sourceVersions: bigint[] = [];
  let last = 0n;
  for (const value of array(manifest[store ? "source_store_migrations" : "source_migrations"])) {
    const row = object(value);
    const version = row.version;
    requireEvidence(typeof version === "bigint" && version > last && version <= I64_MAX);
    last = version;
    sourceVersions.push(version);
    const description = text(row.description);
    const checksum = text(row.sha384);
    requireEvidence(!/[\r\n]/.test(description) && /^[0-9a-f]{96}$/.test(checksum));
    requireEvidence(row.sha384_recomputed === checksum);
    rows.push(`${version}:${description}:${checksum}\n`);
  }
  requireEvidence(pending.every(value => sourceVersions.includes(value)));
  return sha256(new TextEncoder().encode(rows.join("")));
}
export interface Target {
  host: string;
  database: string;
  branch_id: string;
}
export function target(value: Json | undefined): Target {
  const result = object(value);
  requireEvidence(Object.keys(result).sort().join(",") === "branch_id,database,host");
  const host = text(result.host), database = text(result.database), branch_id = text(result.branch_id);
  requireEvidence(/^[a-zA-Z0-9][a-zA-Z0-9.-]{0,252}$/.test(host)
    && /^[a-zA-Z0-9_.-]{1,63}$/.test(database) && /^[a-zA-Z0-9_-]{0,63}$/.test(branch_id)
    && !/prod/i.test(host + database)
    && (!host.toLowerCase().endsWith(".psdb.cloud") || branch_id !== ""));
  return { host, database, branch_id };
}
function reference(value: Json | undefined): string {
  const result = text(value);
  requireEvidence(new TextEncoder().encode(result).length <= 200 && result.length > 0
    && !/\s|@|:\/\//u.test(result));
  return result;
}
export function validateClaim(value: Json, runId: string, attempt: string, head: string, pins: Target) {
  const claim = object(value);
  const fields = ["schema_version", "kind", "repository", "workflow_path", "environment_name",
    "apply_run_id", "apply_run_attempt", "workflow_head_sha", "plan_run_id", "source_sha",
    "plan_manifest_sha256", "store_plan_manifest_sha256", "target", "expected_pending",
    "expected_store_pending", "recovery_evidence_ref", "acl_plan_ref"];
  requireEvidence(Object.keys(claim).length === fields.length && fields.every(key => Object.hasOwn(claim, key)));
  requireEvidence(claim.schema_version === 1n && claim.kind === "staging-migrate-apply-claim"
    && claim.repository === REPOSITORY && claim.workflow_path === WORKFLOW
    && claim.environment_name === ENVIRONMENT && claim.apply_run_id === runId
    && claim.apply_run_attempt === attempt && claim.workflow_head_sha === head && HEX40.test(head));
  requireEvidence(decimal(claim.plan_run_id) !== runId && HEX40.test(text(claim.source_sha)));
  requireEvidence(HEX64.test(text(claim.plan_manifest_sha256)) && HEX64.test(text(claim.store_plan_manifest_sha256)));
  const actual = target(claim.target);
  requireEvidence(actual.host === pins.host && actual.database === pins.database && actual.branch_id === pins.branch_id);
  for (const key of ["expected_pending", "expected_store_pending"]) {
    const values = array(claim[key]).map(value => {
      const version = decimal(value);
      requireEvidence(BigInt(version) <= I64_MAX);
      return BigInt(version);
    });
    versions(values);
  }
  reference(claim.recovery_evidence_ref);
  reference(claim.acl_plan_ref);
  return claim;
}
export async function validatePlan(value: Json, claim: { [key: string]: Json }): Promise<void> {
  const plan = object(value);
  requireEvidence(plan.runner_version === 1n && plan.mode === "plan" && plan.migration_target === "staging"
    && plan.role === "two_bot_migrator_ro" && plan.applied_count === 0n && plan.applied_store_count === 0n
    && plan.plan_provenance_verified === false && plan.plan_run_id === ""
    && typeof plan.role_verified_connections === "bigint" && plan.role_verified_connections > 0n);
  requireEvidence(same(array(plan.ledger_before), array(plan.ledger_after))
    && same(array(plan.ledger_store_before), array(plan.ledger_store_after)));
  for (const key of ["source_sha", "target", "recovery_evidence_ref", "acl_plan_ref"]) {
    requireEvidence(same(plan[key], claim[key]));
  }
  for (const store of [false, true]) {
    const key = store ? "store_plan_manifest_sha256" : "plan_manifest_sha256";
    requireEvidence(await projection(plan, store) === plan[key] && plan[key] === claim[key]);
    const pending = versions(plan[store ? "pending_store_before" : "pending_before"]);
    requireEvidence(same(pending.map(String), claim[store ? "expected_store_pending" : "expected_pending"]));
  }
}
