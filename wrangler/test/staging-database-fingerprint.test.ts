import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { stagingDatabaseEvidence } from "../src/staging-database-fingerprint.ts";
import urls from "./fixtures/staging-database-fingerprint.json" with { type: "json" };

const FINGERPRINT = createHash("sha256")
  .update(JSON.stringify({ branch_id: "fixture_branch", database: "postgres", host: "fixture.pg.psdb.cloud" }))
  .digest("hex");
const QUERY = `?runtime_db_fingerprint=${FINGERPRINT}`;

test("runtime target evidence publishes only the matching approved digest", async () => {
  for (const url of urls.matching) {
    assert.deepEqual(await stagingDatabaseEvidence(QUERY, url), { runtimeDatabaseFingerprint: FINGERPRINT });
  }
  assert.deepEqual(await stagingDatabaseEvidence("", urls.matching[0]), {});
  assert.deepEqual(await stagingDatabaseEvidence(QUERY + "&extra=1", urls.matching[0]), {});
});

test("runtime target evidence omits unknown targets and refuses routing overrides", async () => {
  for (const url of [...urls.refused, urls.matching[0] + "x".repeat(4096)]) {
    assert.deepEqual(await stagingDatabaseEvidence(QUERY, url ?? undefined), { runtimeDatabaseFingerprint: null });
  }
});
