import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { checkArtifact, checkInstalledTree, checkProvenance } from './qualification-baseline.mjs';

const directory = mkdtempSync(join(process.env.RUNNER_TEMP, 'wrangler-provenance-'));
const bundlePath = join(directory, 'bundle.json');
const artifactPath = join(directory, 'wrangler.tgz');
const lock = JSON.parse(readFileSync('package-lock.json', 'utf8')).packages['node_modules/wrangler'];

async function download(url) {
  const response = await fetch(url, { signal: AbortSignal.timeout(30000) });
  assert(response.ok, `download failed: ${response.status}`);
  return Buffer.from(await response.arrayBuffer());
}

const registry = JSON.parse(await download('https://registry.npmjs.org/-/npm/v1/attestations/wrangler@4.156.0'));
const candidates = registry.attestations.filter(item => item.predicateType === 'https://slsa.dev/provenance/v1');
assert.equal(candidates.length, 1, 'expected exactly one provenance bundle');
const bundle = Buffer.from(JSON.stringify(candidates[0].bundle));
const artifact = await download('https://registry.npmjs.org/wrangler/-/wrangler-4.156.0.tgz');
checkArtifact(artifact, lock.integrity);
writeFileSync(bundlePath, bundle, { flag: 'wx' });
writeFileSync(artifactPath, artifact, { flag: 'wx' });
execFileSync('cosign', [
  'verify-blob-attestation', '--bundle', bundlePath,
  '--certificate-identity', 'https://github.com/cloudflare/workers-sdk/.github/workflows/changesets.yml@refs/heads/main',
  '--certificate-oidc-issuer', 'https://token.actions.githubusercontent.com',
  '--type', 'https://slsa.dev/provenance/v1', '--digestAlg', 'sha512', artifactPath,
], { stdio: 'inherit', timeout: 60000 });
assert(readFileSync(bundlePath).equals(bundle), 'bundle changed during verification');
assert(readFileSync(artifactPath).equals(artifact), 'artifact changed during verification');
const statement = JSON.parse(Buffer.from(candidates[0].bundle.dsseEnvelope.payload, 'base64'));
checkProvenance(statement, lock.integrity);
// Compare the installed package to the authenticated archive, excluding only
// its separately installed dependency tree. Extraction stays in runner scratch.
const extracted = join(directory, 'extracted');
mkdirSync(extracted);
execFileSync('tar', ['-xzf', artifactPath, '-C', extracted, '--no-same-owner', '--no-same-permissions'], { timeout: 30000 });
checkInstalledTree(join(extracted, 'package'), 'node_modules/wrangler');
console.log('Installed Wrangler matches authenticated artifact and source provenance');
