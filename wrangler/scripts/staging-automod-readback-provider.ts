import { boundRead, readReceiverBytes, releaseUpstream } from "../src/internal-actions.ts";
import {
  parseStagingAutomodOperation, STAGING_AUTOMOD_AUDIENCE, STAGING_AUTOMOD_BODY_BYTES,
  STAGING_AUTOMOD_OIDC_HEADER, STAGING_AUTOMOD_PATH,
} from "../src/staging-automod-bridge.ts";
import { sameLease, STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { STAGING_USE_PATH, type StagingUse } from "../src/staging-lifecycle.ts";
import { parseUniqueJson } from "../src/unique-json.ts";
import type { runStagingAutomodProtectedHost } from "./staging-automod-protected-host.ts";

type Host = NonNullable<Parameters<typeof runStagingAutomodProtectedHost>[1]>;
type Provider = Pick<Host, "readRun" | "authority">;
type Pins = Readonly<{
  deploymentId: string; sourceSha: string; ownerEpoch: number; runId: string; runAttempt: 1;
}>;
type Credentials = Readonly<{ stagingControlBearer: string; githubReadToken: string }>;
type Transport = Readonly<{
  fetch: (url: string, init: RequestInit) => Promise<Response>;
  mintOidcToken: (audience: typeof STAGING_AUTOMOD_AUDIENCE, signal: AbortSignal) => Promise<string>;
}>;

const STAGING = "https://two-bot-next-staging.5150.workers.dev";
const GITHUB_RUNS = "https://api.github.com/repos/TogetherWeOwn/two-bot-next/actions/runs/";
const REPOSITORY_ID = 1396224242;
const WORKFLOW = ".github/workflows/staging-automod-preview.yml";
const WORKFLOW_PATHS = [WORKFLOW, `${WORKFLOW}@main`, `${WORKFLOW}@refs/heads/main`];
const MAX_RUN_BYTES = 1_048_576;
const READ_TIMEOUT_MS = 2_000;

function record(value: unknown): Record<string, unknown> | undefined {
  if (value !== null && typeof value === "object" && !Array.isArray(value)) return value as Record<string, unknown>;
}

function token(value: unknown, limit: number): value is string {
  return typeof value === "string" && value.length <= limit && /^[A-Za-z0-9._~+/-]+=*$/.test(value);
}

function targets(value: unknown, requireName = true): boolean {
  const target = record(value);
  return target?.applicationId === STAGING_AUTOMOD_TARGET.applicationId
    && target.guildId === STAGING_AUTOMOD_TARGET.guildId
    && ((!requireName && !Object.hasOwn(target, "name")) || target.name === STAGING_AUTOMOD_TARGET.name);
}

async function withinDeadline<T>(parent: AbortSignal, read: (signal: AbortSignal) => Promise<T>) {
  if (parent.aborted) return;
  const deadline = new AbortController();
  const signal = AbortSignal.any([parent, deadline.signal]);
  const cutoff = performance.now() + READ_TIMEOUT_MS;
  const timer = setTimeout(() => deadline.abort(), READ_TIMEOUT_MS);
  try {
    const value = await boundRead(read(signal), signal);
    if (!signal.aborted && performance.now() < cutoff) return value;
  } catch {
    // Transport and JSON errors can contain credentials or response contents.
  } finally {
    clearTimeout(timer);
  }
}

/** Dormant runner readbacks. The trusted OIDC seam mints for the Worker
 * audience and receives neither bearer. The fetch seam honors redirect refusal.
 * Each authority read confirms the
 * pinned run, the protected bridge reply and the Worker lease/owner together.
 */
export function bindStagingAutomodReadbackProvider(pins: Pins, credentials: Credentials,
  transport: Transport): Provider | undefined {
  const { deploymentId, sourceSha, ownerEpoch, runId, runAttempt } = pins;
  const { stagingControlBearer, githubReadToken } = credentials;
  const { fetch, mintOidcToken } = transport;
  if (typeof deploymentId !== "string" || !/^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/.test(deploymentId)
    || typeof sourceSha !== "string" || !/^[0-9a-f]{40}$/.test(sourceSha)
    || typeof runId !== "string" || !/^[1-9][0-9]{0,15}$/.test(runId) || !Number.isSafeInteger(Number(runId))
    || runAttempt !== 1 || !Number.isSafeInteger(ownerEpoch) || ownerEpoch <= 0
    || !token(stagingControlBearer, 4_089) || stagingControlBearer.length < 32
    || !token(githubReadToken, 4_089) || githubReadToken === stagingControlBearer) return;

  async function json(url: string, init: RequestInit, limit: number, signal: AbortSignal) {
    signal.throwIfAborted();
    const response = await boundRead(fetch(url, { ...init, redirect: "error", signal }).then(reply => {
      // A transport can ignore abort and deliver a body after our caller left.
      if (signal.aborted) releaseUpstream(reply);
      return reply;
    }), signal);
    if (signal.aborted || response.status !== 200 || response.redirected || (response.url && response.url !== url)) {
      releaseUpstream(response);
      return;
    }
    const bytes = await readReceiverBytes(response, limit, signal);
    if (bytes && !signal.aborted) {
      return record(parseUniqueJson(new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes)));
    }
  }

  async function readRun(signal: AbortSignal) {
    const body = await json(`${GITHUB_RUNS}${runId}`, { method: "GET", headers: {
      authorization: `Bearer ${githubReadToken}`, accept: "application/vnd.github+json",
      "x-github-api-version": "2022-11-28",
    } }, MAX_RUN_BYTES, signal);
    if (body?.id !== Number(runId) || body.run_attempt !== 1 || body.head_sha !== sourceSha
      || body.event !== "workflow_dispatch" || body.status !== "in_progress" || body.head_branch !== "main"
      || typeof body.path !== "string" || !WORKFLOW_PATHS.includes(body.path)
      || record(body.repository)?.id !== REPOSITORY_ID || record(body.head_repository)?.id !== REPOSITORY_ID) return;
    return Object.freeze({ id: body.id, run_attempt: 1, head_sha: sourceSha, event: body.event, status: body.status,
      head_branch: body.head_branch, path: body.path,
      repository: Object.freeze({ id: REPOSITORY_ID }), head_repository: Object.freeze({ id: REPOSITORY_ID }) });
  }

  function matchesLease(lease: Readonly<StagingUse>) {
    return lease.deploymentId === deploymentId && lease.sha === sourceSha && lease.epoch === ownerEpoch
      && lease.runId === runId && lease.runAttempt === 1 && lease.acquiredAt <= Date.now();
  }

  async function confirmOwner(lease: Readonly<StagingUse>, signal: AbortSignal) {
    const status = await json(`${STAGING}${STAGING_USE_PATH}`, { method: "GET",
      headers: { authorization: `Bearer ${stagingControlBearer}` } }, STAGING_AUTOMOD_BODY_BYTES, signal);
    const owner = record(status?.owner), use = record(status?.use);
    return status?.deploymentId === deploymentId && status.sha === sourceSha && owner?.phase === "active"
      && owner.deploymentId === deploymentId && owner.epoch === ownerEpoch
      && use?.purpose === lease.purpose && sameLease(lease, use);
  }

  async function bridge(operation: unknown, signal: AbortSignal) {
    if (!await readRun(signal)) return;
    signal.throwIfAborted();
    const oidc = await boundRead(mintOidcToken(STAGING_AUTOMOD_AUDIENCE, signal), signal);
    if (typeof oidc !== "string" || oidc.length > 8_192 || !/^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/.test(oidc)
      || oidc === githubReadToken || oidc === stagingControlBearer) return;
    const reply = await json(`${STAGING}${STAGING_AUTOMOD_PATH}`, { method: "POST", body: JSON.stringify(operation),
      headers: { authorization: `Bearer ${stagingControlBearer}`, [STAGING_AUTOMOD_OIDC_HEADER]: oidc,
        "content-type": "application/json" } }, STAGING_AUTOMOD_BODY_BYTES, signal);
    if (reply && Object.keys(reply).length === 1 && Object.hasOwn(reply, "value")) return reply.value;
  }

  return {
    readRun: (requestedRun, signal) => requestedRun === runId
      ? withinDeadline(signal, readRun) : Promise.resolve(undefined),
    authority: {
      readIssuedLease: async (context, parent) => {
        if (context?.environment !== "staging" || context.deploymentId !== deploymentId || context.sha !== sourceSha
          || context.runId !== runId || context.runAttempt !== 1 || !targets(context.target, false)) return;
        const scope = { environment: "staging", deploymentId, sha: sourceSha, runId, runAttempt: 1,
          target: STAGING_AUTOMOD_TARGET };
        return withinDeadline(parent, async signal => {
          const issuedLease = await bridge({ op: "read-lease", context: scope }, signal);
          const lease = parseStagingAutomodOperation({ op: "read-approval", action: "recover", issuedLease });
          if (lease && lease !== "unknown" && lease.op === "read-approval" && matchesLease(lease.issuedLease)
            && await confirmOwner(lease.issuedLease, signal)) return lease.issuedLease;
        });
      },
      readApproval: async (action, issuedLease, parent) => {
        const request = parseStagingAutomodOperation({ op: "read-approval", action, issuedLease });
        if (!request || request === "unknown" || request.op !== "read-approval" || !matchesLease(request.issuedLease)) return;
        const lease = request.issuedLease;
        return withinDeadline(parent, async signal => {
          const approval = record(await bridge(request, signal)), returned = record(approval?.issuedLease);
          if (approval?.action === action && targets(approval.target) && returned?.purpose === lease.purpose
            && sameLease(lease, returned) && await confirmOwner(lease, signal)
            && (action === "recover" || Date.now() < lease.expiresAt)) {
            return Object.freeze({ action, issuedLease: lease, target: STAGING_AUTOMOD_TARGET });
          }
        });
      },
    },
  };
}
