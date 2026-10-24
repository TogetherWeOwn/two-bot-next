import assert from "node:assert/strict";
import test from "node:test";
import { parseConfigPut } from "../src/ops-control.ts";

test("ops cannot retire a catalogue outside the database drain lock", () => {
  assert.equal(parseConfigPut({
    overrides: { TWO_SELF_ROLE_PANELS: "[]" },
    expectedEpoch: 0,
    actor: "fixture",
  }), null);
  assert.ok(parseConfigPut({
    overrides: { TWO_TICKET_COOLDOWN_SECONDS: "60" },
    expectedEpoch: 0,
    actor: "fixture",
  }));
});
