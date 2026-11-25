import { readSync } from "node:fs";
import { sameLease } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";
import type { runStagingAutomodBoundJob } from "./staging-automod-private-job.ts";
import { isDirectInvocation } from "./is-direct-invocation.mjs";

type Lease = NonNullable<Parameters<typeof runStagingAutomodBoundJob>[1]>;
type JobResult = Awaited<ReturnType<typeof runStagingAutomodBoundJob>>;

const LEASE_KEYS = ["environment", "purpose", "phase", "runId", "runAttempt", "deploymentId",
  "sha", "epoch", "acquiredAt", "expiresAt"] as const;
const MAX_INPUT_BYTES = 4096;
const UNCONFIRMED_LEASE = Object.freeze({ verdict: "failed", code: "lease_unconfirmed" } as const);
const UNCONFIRMED_ACTIONS = Object.freeze({ verdict: "failed", start: false, recover: false } as const);

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function issuedLease(value: unknown): value is Lease {
  return record(value) && Object.keys(value).length === LEASE_KEYS.length
    && LEASE_KEYS.every(key => Object.hasOwn(value, key))
    && value.purpose === AUTOMOD_STAGING_PURPOSE && value.runAttempt === 1
    && sameLease(value as Lease, value);
}

/**
 * Run with `node wrangler/scripts/staging-automod-evidence.ts < evidence.json`.
 * Supply the independently retained original lease as expectedLease. Supply
 * the protected job's original lease and typed runner result as attempt.lease
 * and attempt.result. The caller supplies their provenance. This offline
 * comparison only summarizes the confirmations for that exact lease.
 */
export function assembleStagingAutomodEvidence(input: unknown): JobResult {
  if (!record(input) || !Object.hasOwn(input, "expectedLease")
    || !Object.hasOwn(input, "attempt") || !record(input.attempt)) return UNCONFIRMED_LEASE;
  const { expectedLease, attempt } = input;
  if (!Object.hasOwn(attempt, "lease") || !issuedLease(expectedLease) || !issuedLease(attempt.lease)
    || !sameLease(expectedLease, attempt.lease)) return UNCONFIRMED_LEASE;

  const { result } = attempt;
  if (!Object.hasOwn(attempt, "result") || !record(result)
    || !Object.hasOwn(result, "verdict")) return UNCONFIRMED_ACTIONS;
  const { verdict, start, recover, code } = result;
  if (Object.hasOwn(result, "code") && code === "lease_unconfirmed" && verdict === "failed"
    && !Object.hasOwn(result, "start") && !Object.hasOwn(result, "recover")) return UNCONFIRMED_LEASE;
  if (Object.hasOwn(result, "code") || !Object.hasOwn(result, "start") || !Object.hasOwn(result, "recover")
    || typeof start !== "boolean" || typeof recover !== "boolean") return UNCONFIRMED_ACTIONS;

  const confirmed = start && recover ? "passed" : "failed";
  return verdict === confirmed ? { verdict: confirmed, start, recover } : UNCONFIRMED_ACTIONS;
}

function readInput(): unknown {
  const bytes = Buffer.alloc(MAX_INPUT_BYTES + 1);
  let length = 0;
  while (length < bytes.length) {
    const count = readSync(0, bytes, length, bytes.length - length, null);
    if (count === 0) break;
    length += count;
  }
  if (length > MAX_INPUT_BYTES) return;
  const text = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes.subarray(0, length));
  const input: unknown = JSON.parse(text);
  // After syntax validation, each quoted token followed by a colon is a key in
  // the innermost object. Reject duplicate keys before using JSON.parse's result.
  const objects: Set<string>[] = [];
  for (const [token, key, colon] of text.matchAll(/("(?:[^"\\]|\\[\s\S])*")(\s*:)?|[{}]/g)) {
    if (token === "{") objects.push(new Set());
    else if (token === "}") objects.pop();
    else if (colon && key !== undefined) {
      const name: string = JSON.parse(key);
      const names = objects.at(-1);
      if (!names || names.has(name)) return;
      names.add(name);
    }
  }
  return input;
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  let evidence: JobResult;
  try {
    evidence = assembleStagingAutomodEvidence(readInput());
  } catch {
    evidence = UNCONFIRMED_LEASE;
  }
  process.stdout.write(JSON.stringify(evidence) + "\n");
  process.exitCode = evidence.verdict === "passed" ? 0 : 1;
}
