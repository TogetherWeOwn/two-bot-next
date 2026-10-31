/**
 * End-to-end correlation id: the Worker stamps `x-request-id`, the Durable
 * Object forwards it, and the container honors it when it has a generated
 * shape. All fixtures are synthetic; nothing is contacted.
 * Run: node --import ./test/cloudflare-loader.mjs --test test/request-id.test.ts
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import {
  REQUEST_ID_HEADER,
  REQUEST_ID_LEN,
  ensureRequestId,
  isRequestId,
  newRequestId,
} from "../src/request-id.ts";

// Hand-built 26-char Crockford fixture (no I/L/O/U, no lowercase).
const FIXTURE = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

test("header name and length match the container contract", () => {
  assert.equal(REQUEST_ID_HEADER, "x-request-id");
  assert.equal(REQUEST_ID_LEN, 26);
  assert.equal(FIXTURE.length, 26);
  assert.ok(isRequestId(FIXTURE));
});

test("generated ids are valid, deterministic and time-ordered", () => {
  const rand = new Uint8Array(16).fill(7);
  const id = newRequestId(1_720_000_000_000, rand);
  assert.equal(id, newRequestId(1_720_000_000_000, rand));
  assert.ok(isRequestId(id));
  assert.ok(newRequestId(1_720_000_000_001, rand) > id);
  const minted = newRequestId();
  assert.ok(isRequestId(minted));
  assert.notEqual(minted, newRequestId());
});

test("only generated shapes are honored", () => {
  assert.equal(ensureRequestId(FIXTURE), FIXTURE);
  for (const bad of [
    null,
    "",
    "short",
    "01ARZ3NDEKTSV4RRFFQ69G5FAV!", // 26 chars but `!` is outside the alphabet
    "01arz3ndektsv4rrffq69g5fav", // lowercase is never generated
    "01ARZ3NDEKTSV4RRFFQ69G5FAI", // `I` is excluded from Crockford
    `${FIXTURE}X`, // 27 chars
    "Bearer synthetic-token-value",
    "https://user:synthetic-secret@example.invalid/",
  ]) {
    const minted = ensureRequestId(bad);
    assert.ok(isRequestId(minted), `${bad} must mint a fresh id`);
    assert.notEqual(minted, bad);
  }
});

test("contract matches the Rust core (drift guard)", () => {
  const core = readFileSync(
    fileURLToPath(new URL("../../crates/core/src/internal_actions.rs", import.meta.url)),
    "utf8",
  );
  assert.ok(
    core.includes('pub const REQUEST_ID_HEADER: &str = "x-request-id"'),
    "Rust header name drifted",
  );
  assert.ok(
    core.includes("b\"0123456789ABCDEFGHJKMNPQRSTVWXYZ\""),
    "Rust alphabet drifted",
  );
  assert.ok(
    core.includes("pub const REQUEST_ID_LEN: usize = 26"),
    "Rust length drifted",
  );
});
