export const AUTOMOD_STAGING_PURPOSE = "automod-preview";

// Allows an explicitly authorized four-ACTIVE-hour soak plus startup/cleanup;
// routine synthetics and deployment callers must request much shorter windows.
export const MAX_STAGING_USE_MS = 6 * 60 * 60 * 1000;
