import type { TwoBotContainer } from "../src/index.ts";
import { readReceiverBytes, releaseUpstream } from "../src/internal-actions.ts";
import {
  STAGING_AUTOMOD_AUDIENCE, STAGING_AUTOMOD_BODY_BYTES, STAGING_AUTOMOD_OIDC_HEADER, STAGING_AUTOMOD_PATH,
} from "../src/staging-automod-bridge.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { parseUniqueJson } from "../src/unique-json.ts";
import { bindStagingAutomodInventoryReader } from "./staging-automod-inventory-reader.ts";
import { bindStagingAutomodInventoryVerifier } from "./staging-automod-inventory-verifier.ts";
import type { runStagingAutomodProtectedHost } from "./staging-automod-protected-host.ts";

type Host = NonNullable<Parameters<typeof runStagingAutomodProtectedHost>[1]>;
type Instance = ReturnType<typeof TwoBotContainer.bindProtectedAutomodInstance>;
type Serving = Readonly<{ deploymentId: string; sha: string }>;
type Fetch = (input: string | URL, init: RequestInit) => Promise<Response>;
type ClientEnv = Readonly<{
  ACTIONS_ID_TOKEN_REQUEST_URL?: string;
  ACTIONS_ID_TOKEN_REQUEST_TOKEN?: string;
  OWNERSHIP_CONTROL_TOKEN?: string;
}>;

/** Fixed staging Worker. No input, environment variable or argument selects it. */
export const STAGING_AUTOMOD_BRIDGE_URL = `https://two-bot-next-staging.5150.workers.dev${STAGING_AUTOMOD_PATH}`;
const RUNS_URL = "https://api.github.com/repos/TogetherWeOwn/two-bot-next/actions/runs/";
const MAX_TOKEN_LENGTH = 8_192;
const MAX_RUN_BYTES = 1_048_576;

async function boundedJson(response: Response, limit: number, signal: AbortSignal): Promise<unknown> {
  const bytes = await readReceiverBytes(response, limit, signal);
  if (!bytes) throw new Error("AutoMod bridge response unavailable");
  return parseUniqueJson(new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes));
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Bind the protected job to the staging Worker's AutoMod bridge. Every call
 * mints a fresh Actions OIDC token for the fixed audience. The Worker and DO
 * authenticate the bearer and token, and the DO applies its compiled plan.
 * Returns undefined unless the OIDC request variables and bearer exist.
 */
export function bindStagingAutomodInstanceClient(env: ClientEnv, serving: Serving, fetcher: Fetch = fetch,
  readAuthenticatedInventory: Host["readAuthenticatedInventory"] =
  bindStagingAutomodInventoryReader(bindStagingAutomodInventoryVerifier())) {
  const requestUrl = env.ACTIONS_ID_TOKEN_REQUEST_URL, requestToken = env.ACTIONS_ID_TOKEN_REQUEST_TOKEN;
  const controlBearer = env.OWNERSHIP_CONTROL_TOKEN;
  if (!requestUrl || !requestToken || !controlBearer || controlBearer.length < 32) return;
  let idTokenUrl: URL;
  try { idTokenUrl = new URL(requestUrl); } catch { return; }
  if (idTokenUrl.protocol !== "https:" || !idTokenUrl.hostname.endsWith(".actions.githubusercontent.com")) return;
  idTokenUrl.searchParams.set("audience", STAGING_AUTOMOD_AUDIENCE);

  async function mint(signal: AbortSignal): Promise<string> {
    const response = await fetcher(idTokenUrl, { method: "GET", redirect: "error", signal,
      headers: { authorization: `Bearer ${requestToken}`, accept: "application/json" } });
    if (response.status !== 200) {
      releaseUpstream(response);
      throw new Error("Actions OIDC token unavailable");
    }
    const body = await boundedJson(response, MAX_TOKEN_LENGTH * 2, signal);
    const value = record(body) ? body.value : undefined;
    if (typeof value !== "string" || value.length > MAX_TOKEN_LENGTH) throw new Error("Actions OIDC token unavailable");
    return value;
  }

  async function bridge(operation: Record<string, unknown>, signal: AbortSignal): Promise<unknown> {
    signal.throwIfAborted();
    const body = JSON.stringify(operation);
    if (new TextEncoder().encode(body).byteLength > STAGING_AUTOMOD_BODY_BYTES) throw new Error("AutoMod bridge body too large");
    const token = await mint(signal);
    const response = await fetcher(STAGING_AUTOMOD_BRIDGE_URL, { method: "POST", redirect: "error", signal, body,
      headers: { authorization: `Bearer ${controlBearer}`, [STAGING_AUTOMOD_OIDC_HEADER]: token,
        "content-type": "application/json" } });
    if (response.status !== 200) {
      releaseUpstream(response);
      throw new Error("AutoMod bridge refused");
    }
    const reply = await boundedJson(response, STAGING_AUTOMOD_BODY_BYTES, signal);
    if (!record(reply) || Object.keys(reply).length !== 1 || !Object.hasOwn(reply, "value")) {
      throw new Error("AutoMod bridge reply invalid");
    }
    return reply.value;
  }

  // Records returned here are revalidated by the protected job inputs.
  const optional = <T>(value: unknown): T | undefined => {
    if (value === null) return undefined;
    if (!record(value)) throw new Error("AutoMod bridge reply invalid");
    return value as T;
  };
  type Authority = Instance["authority"];
  const authority: Authority = {
    readIssuedLease: async ({ environment, deploymentId, sha, runId, runAttempt }, signal) => optional(await bridge(
      { op: "read-lease", context: { environment, deploymentId, sha, runId, runAttempt, target: STAGING_AUTOMOD_TARGET } },
      signal)),
    readApproval: async (action, issuedLease, signal) => optional(await bridge(
      { op: "read-approval", action, issuedLease }, signal)),
  };
  const instance: Instance = {
    authority,
    acquireLease: async (pin, change, signal) => {
      const lease = optional<Awaited<ReturnType<Instance["acquireLease"]>>>(await bridge({ op: "acquire", pin, change }, signal));
      if (!lease) throw new Error("AutoMod bridge reply invalid");
      return lease;
    },
    invokePrivate: async (pin, call, signal) => {
      if (await bridge({ op: "invoke", pin, call }, signal) !== null) throw new Error("AutoMod bridge reply invalid");
    },
  };

  const host: Host = {
    authority,
    readRun: async (runId, signal) => {
      if (!/^[1-9][0-9]{0,19}$/.test(runId)) return;
      const response = await fetcher(`${RUNS_URL}${runId}`, { method: "GET", redirect: "error", signal,
        headers: { accept: "application/vnd.github+json", "x-github-api-version": "2022-11-28" } });
      if (response.status !== 200) {
        releaseUpstream(response);
        return;
      }
      return boundedJson(response, MAX_RUN_BYTES, signal);
    },
    // Local mirror for the job-side admission checks. The DO enforces the
    // authoritative environment, bearer, build and ownership again.
    readWorker: () => ({ controlBearer, env: { TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded",
      OWNERSHIP_CONTROL_TOKEN: controlBearer, DISCORD_APPLICATION_ID: STAGING_AUTOMOD_TARGET.applicationId,
      GUILD_ID: STAGING_AUTOMOD_TARGET.guildId, CF_VERSION_METADATA: { id: serving.deploymentId },
      CONTAINER_RELEASE: serving.sha } }),
    readAuthenticatedInventory,
    invokePrivate: instance.invokePrivate,
  };
  return Object.freeze({ instance, host });
}
