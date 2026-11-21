// Translate the caller's immutable staging grant into receipt metadata.
import { writeFileSync } from 'node:fs';
import { isDirectInvocation } from './promotion-error-gate.mjs';
import { validateMetadata } from './voice-rest-receipt.mjs';

export function metadataFromGrant({ runId, runAttempt, sha, deploymentId, expiresAt }) {
  if (typeof expiresAt !== 'string' || !/^[1-9][0-9]*$/.test(expiresAt)) {
    throw new Error('grant_expiry_invalid');
  }
  const milliseconds = Number(expiresAt);
  if (!Number.isSafeInteger(milliseconds) || !Number.isFinite(new Date(milliseconds).getTime())) {
    throw new Error('grant_expiry_invalid');
  }
  return validateMetadata({ runId, runAttempt, sha, deploymentId, expiresAt: new Date(milliseconds).toISOString() });
}

export function writeGrantMetadata(path, env) {
  const metadata = metadataFromGrant({
    runId: env.GITHUB_RUN_ID, runAttempt: env.GITHUB_RUN_ATTEMPT,
    sha: env.STAGING_USE_SHA, deploymentId: env.STAGING_USE_DEPLOYMENT,
    expiresAt: env.STAGING_USE_EXPIRES_AT,
  });
  writeFileSync(path, `${JSON.stringify(metadata)}\n`, { flag: 'wx', mode: 0o600 });
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  try {
    if (process.argv.length !== 3 || !process.argv[2]) throw new Error();
    writeGrantMetadata(process.argv[2], process.env);
  } catch {
    console.error('voice measurement metadata refused');
    process.exitCode = 1;
  }
}
