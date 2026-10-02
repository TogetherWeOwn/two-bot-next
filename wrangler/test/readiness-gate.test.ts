import { test } from "node:test";
import assert from "node:assert/strict";
import { checkReadyz } from "../scripts/check-readyz.mjs";

const report = (gateway: string) => ({ components: [["process", "ready"], ["gateway", gateway]], jobs: {} });

test("staging scaffold gate requires a real, status-consistent bot report", () => {
  assert.equal(checkReadyz("200", report("ready")), "gateway ready");
  for (const status of ["down", "starting"]) {
    assert.match(checkReadyz("503", report(status)), /not ready/);
  }
  for (const [code, body] of [
    ["503", { error: "ownership_fenced", reason: "not_owner" }],
    ["503", {}], ["503", null], ["200", report("down")],
    ["503", report("ready")], ["500", report("ready")],
    ["503", { components: [["process", "ready"], ["process", "down"]] }],
    ["503", { components: [["process", "ready"], ["gateway"]] }],
  ]) {
    assert.throws(() => checkReadyz(code, body));
  }
});
