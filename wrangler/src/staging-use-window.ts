export const AUTOMOD_STAGING_PURPOSE = "automod-preview";

// Allows an explicitly authorized four-ACTIVE-hour soak plus startup/cleanup;
// routine synthetics and deployment callers must request much shorter windows.
export const MAX_STAGING_USE_MS = 6 * 60 * 60 * 1000;

export interface StagingWindow {
  environment: "staging";
  runId: string;
  runAttempt: number;
  expiresAt: number;
}

export interface StagingUse extends StagingWindow {
  deploymentId: string;
  sha: string;
  epoch: number;
  acquiredAt: number;
  phase: "preparing" | "active" | "released";
  voiceMove?: true;
  purpose?: typeof AUTOMOD_STAGING_PURPOSE;
}
