// Verify the public wire vectors independently with the legacy node:crypto API.
// Offline only: no environment keys, Discord client, database or network.
import { strict as assert } from 'node:assert';
import { createHash, createHmac } from 'node:crypto';
import { readFileSync } from 'node:fs';

const { vectors } = JSON.parse(readFileSync(new URL('./internal-action-signing.json', import.meta.url), 'utf8'));
assert.equal(vectors.length, 2);
for (const vector of vectors) {
  const hash = createHash('sha256').update(vector.body).digest('hex');
  assert.equal(hash, vector.body_hash);
  const canonical = `POST\n/internal/actions\n${vector.timestamp}\n${vector.nonce}\n${hash}`;
  const signature = `sha256=${createHmac('sha256', vector.secret).update(canonical).digest('hex')}`;
  assert.equal(signature, vector.signature);
}
console.log(`PASS: ${vectors.length} unchanged legacy node:crypto wire vectors`);
