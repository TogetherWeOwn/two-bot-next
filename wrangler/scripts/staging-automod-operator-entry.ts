import { boundRead } from "../src/internal-actions.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { runStagingAutomodRehearsal } from "./staging-automod-rehearsal-caller.ts";

type Caller = Parameters<typeof runStagingAutomodRehearsal>;
type ServingSelection = Pick<NonNullable<Caller[0]["context"]>, "deploymentId" | "sha">;
type Runner = Readonly<{
  GITHUB_REPOSITORY_ID?: string;
  GITHUB_WORKFLOW_REF?: string;
  GITHUB_REF?: string;
  GITHUB_REF_PROTECTED?: string;
  GITHUB_EVENT_NAME?: string;
  GITHUB_RUN_ID?: string;
  GITHUB_RUN_ATTEMPT?: string;
  GITHUB_SHA?: string;
  GITHUB_WORKFLOW_SHA?: string;
}>;
type GitHubRun = Readonly<{
  id?: unknown;
  event?: unknown;
  status?: unknown;
  run_attempt?: unknown;
  path?: unknown;
  head_branch?: unknown;
  head_sha?: unknown;
  repository?: { id?: unknown };
  head_repository?: { id?: unknown };
}>;

const REPOSITORY_ID = 1396224242;
const WORKFLOW = ".github/workflows/staging-automod-preview.yml";
const WORKFLOW_REF = `TogetherWeOwn/two-bot-next/${WORKFLOW}@refs/heads/main`;
const MAIN_PATHS = [WORKFLOW, `${WORKFLOW}@main`, `${WORKFLOW}@refs/heads/main`];
const UNCONFIRMED = Object.freeze({ verdict: "failed", code: "context_unconfirmed" } as const);

/** Bind the dormant caller to this protected staging job. The host supplies
 * GitHub-owned runner metadata and an authenticated read of its Actions run.
 * The selected serving identity comes from the approved rehearsal plan.
 * Authority, inventory and invocation still come from the protected providers.
 */
export async function runStagingAutomodOperatorEntry(
  options: Readonly<{ enabled?: boolean; runner?: Runner; serving?: ServingSelection }>,
  readRun: (runId: string, signal: AbortSignal) => Promise<unknown>,
  providers: Caller[1],
) {
  if (options.enabled !== true) return { verdict: "disabled" } as const;
  const { runner, serving } = options;
  const runId = runner?.GITHUB_RUN_ID;
  const workflowSha = runner?.GITHUB_WORKFLOW_SHA;
  if (!serving || !runner || runner.GITHUB_REPOSITORY_ID !== String(REPOSITORY_ID)
    || runner.GITHUB_WORKFLOW_REF !== WORKFLOW_REF || runner.GITHUB_REF !== "refs/heads/main"
    || runner.GITHUB_REF_PROTECTED !== "true" || runner.GITHUB_EVENT_NAME !== "workflow_dispatch"
    || runner.GITHUB_RUN_ATTEMPT !== "1" || typeof runId !== "string" || !/^[1-9][0-9]*$/.test(runId)
    || !Number.isSafeInteger(Number(runId)) || typeof workflowSha !== "string"
    || !/^[0-9a-f]{40}$/.test(workflowSha) || runner.GITHUB_SHA !== workflowSha) return UNCONFIRMED;

  const { deploymentId, sha } = serving;
  const signal = AbortSignal.timeout(2_000);
  let run: GitHubRun | undefined;
  try {
    run = await boundRead(readRun(runId, signal), signal) as GitHubRun | undefined;
  } catch {
    return UNCONFIRMED;
  }
  if (signal.aborted || run?.id !== Number(runId) || run.run_attempt !== 1 || run.event !== "workflow_dispatch"
    || run.status !== "in_progress" || typeof run.path !== "string" || !MAIN_PATHS.includes(run.path)
    || run.head_branch !== "main" || run.head_sha !== workflowSha || run.repository?.id !== REPOSITORY_ID
    || run.head_repository?.id !== REPOSITORY_ID) return UNCONFIRMED;

  return runStagingAutomodRehearsal({ enabled: true, context: {
    environment: "staging", target: STAGING_AUTOMOD_TARGET, deploymentId, sha,
    runId, runAttempt: 1, workflowRef: WORKFLOW_REF, eventName: "workflow_dispatch", refProtected: true,
  } }, providers);
}
