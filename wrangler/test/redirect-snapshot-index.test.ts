/**
 * RedirectStore snapshot index: first-row-wins on duplicate slugs.
 *
 * `parseMappingsSnapshot` rejects duplicate slugs, so no parse-path test can
 * pin the index's duplicate semantics. This test constructs the store
 * directly: the first row must win, matching the previous Array.find scan.
 * Synthetic fixtures only — no endpoint, database, or deployment is
 * contacted.
 *
 * Run: node --import ./test/cloudflare-loader.mjs --test test/redirect-snapshot-index.test.ts
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { RedirectStore } from "../src/redirect-store.ts";

test("snapshot index keeps the first row for duplicate slugs", async () => {
  const store = new RedirectStore(undefined, undefined, [
    { slug: "reddit", inviteCode: "firstCode" },
    { slug: "reddit", inviteCode: "secondCode" },
  ]);
  assert.equal((await store.lookup("reddit"))?.inviteCode, "firstCode");
});
