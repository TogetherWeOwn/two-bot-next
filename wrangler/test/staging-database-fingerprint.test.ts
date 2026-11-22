import { test } from "node:test";
import assert from "node:assert/strict";
import { stagingDatabaseEvidence } from "../src/staging-database-fingerprint.ts";
import urls from "./fixtures/staging-database-fingerprint.json" with { type: "json" };

const QUERY = `?runtime_db_fingerprint=${urls.fingerprint}`;

test("runtime target evidence publishes only the matching approved digest", async () => {
  for (const url of urls.matching) {
    assert.deepEqual(await stagingDatabaseEvidence(QUERY, url), { runtimeDatabaseFingerprint: urls.fingerprint });
  }
  assert.deepEqual(await stagingDatabaseEvidence("", urls.matching[0]), {});
  assert.deepEqual(await stagingDatabaseEvidence(QUERY + "&extra=1", urls.matching[0]), {});
});

test("runtime target evidence omits unknown targets and refuses routing overrides", async () => {
  for (const url of [...urls.refused, urls.matching[0] + "x".repeat(4096)]) {
    assert.deepEqual(await stagingDatabaseEvidence(QUERY, url ?? undefined), { runtimeDatabaseFingerprint: null });
  }
});
