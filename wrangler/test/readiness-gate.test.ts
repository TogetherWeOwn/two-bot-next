import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { checkReadyz, READYZ_PARKED, READYZ_REQUIRED } from "../scripts/check-readyz.mjs";

const SCRIPT = fileURLToPath(new URL("../scripts/check-readyz.mjs", import.meta.url));
const SERVER_RS = new URL("../../crates/bot/src/server.rs", import.meta.url);

const body = (...components: string[][]) => ({
  components,
  jobs: {},
  build_revision: "unknown",
  build_id: "unknown",
});

const PROCESS = ["process", "ready"];
const GATEWAY = ["gateway", "ready"];
const DATABASE = ["database", "ready"];
const TOKEN = ["token_invalid", "ready"];

// The bot's real /readyz bodies (crates/bot/src/server.rs), one per state the gate must classify.
const FIXTURES = {
  ready: ["200", body(PROCESS, GATEWAY, DATABASE, TOKEN)],
  parked: ["503", body(PROCESS, ["gateway", "starting"], DATABASE, TOKEN)],
  "database down": ["503", body(PROCESS, GATEWAY, ["database", "down"], TOKEN)],
  "token invalid": ["503", body(PROCESS, GATEWAY, DATABASE, ["token_invalid", "down"])],
  "gateway and database down": ["503", body(PROCESS, ["gateway", "down"], ["database", "down"], TOKEN)],
};

function serverComponentNames() {
  const source = readFileSync(SERVER_RS, "utf8");
  const names = new Set();
  for (const fn of ["readiness_report", "with_token_state"]) {
    const match = new RegExp(`^fn ${fn}\\([\\s\\S]*?^\\}$`, "m").exec(source);
    assert.ok(match, `server.rs no longer defines fn ${fn}; update this drift guard`);
    for (const [, name] of match[0].matchAll(/"([a-z_]+)"\.to_owned\(\)/g)) names.add(name);
  }
  return [...names].sort();
}

test("the gate accepts each real server state with the status the server returns", () => {
  assert.equal(checkReadyz(...FIXTURES.ready), "all 4 components ready");
  assert.equal(checkReadyz(...FIXTURES.parked), "process ready, gateway starting (parked, not E2E approval)");
  assert.equal(checkReadyz(...FIXTURES["database down"]),
    "process ready, database down (parked, not E2E approval)");
  assert.equal(checkReadyz(...FIXTURES["token invalid"]),
    "process ready, token_invalid down (parked, not E2E approval)");
  assert.equal(checkReadyz(...FIXTURES["gateway and database down"]),
    "process ready, gateway down, database down (parked, not E2E approval)");
});

test("200 needs every present component ready; 503 needs process ready and only parked failures", () => {
  const accepted = [
    ["200", body(PROCESS, GATEWAY)],
    ["200", body(PROCESS, GATEWAY, DATABASE, TOKEN, ["voice", "ready"])],
    ["503", body(PROCESS, ["gateway", "starting"], ["token_invalid", "down"])],
    ["503", body(PROCESS, ["gateway", "down"], ["database", "down"], TOKEN)],
    ["503", body(PROCESS, GATEWAY, ["database", "down"], ["token_invalid", "down"])],
  ];
  for (const [code, report] of accepted) {
    assert.doesNotThrow(() => checkReadyz(code, report), JSON.stringify(report.components));
  }
  const rejected = [
    ["200", body(PROCESS, ["gateway", "starting"], DATABASE, TOKEN), "200 with gateway starting"],
    ["200", body(PROCESS, GATEWAY, ["database", "down"], TOKEN), "200 with database down"],
    ["200", body(PROCESS, GATEWAY, DATABASE, ["token_invalid", "down"]), "200 with token invalid down"],
    ["200", body(PROCESS, GATEWAY, ["voice", "down"]), "200 with an unknown component down"],
    ["500", body(PROCESS, GATEWAY, DATABASE, TOKEN), "500 with a complete body"],
    ["503", body(PROCESS, GATEWAY, DATABASE, TOKEN), "503 with every component ready"],
    ["503", body(["process", "down"], ["gateway", "down"]), "503 with process down"],
    ["503", body(PROCESS, GATEWAY, ["voice", "down"]), "503 with an unknown component down"],
    ["503", body(PROCESS, ["gateway", "ok"]), "status outside ready, starting, down"],
    ["200", body(GATEWAY, DATABASE), "missing process"],
    ["200", body(PROCESS, DATABASE), "missing gateway"],
    ["200", body(), "empty breakdown"],
    ["200", body(PROCESS, GATEWAY, GATEWAY), "duplicate component"],
    ["200", body(["process"]), "row of the wrong length"],
    ["200", body(PROCESS, [42, "ready"]), "non-string component name"],
    ["503", { error: "ownership_fenced", reason: "not_owner" }, "ownership refusal"],
    ["503", { error: "ownership_fenced", components: [PROCESS, GATEWAY] }, "ownership refusal with components"],
    ["503", null, "no body"],
    ["503", {}, "empty object"],
  ];
  for (const [code, report, label] of rejected) {
    assert.throws(() => checkReadyz(code, report), Error, label);
  }
});

test("the CLI prints the accepted state and refuses non-JSON", () => {
  const dir = mkdtempSync(join(tmpdir(), "readyz-gate-"));
  const file = join(dir, "readyz.json");
  const run = (text) => {
    writeFileSync(file, text);
    return spawnSync(process.execPath, [SCRIPT, "200", file], { encoding: "utf8" });
  };
  try {
    const accepted = run(JSON.stringify(FIXTURES.ready[1]));
    assert.equal(accepted.status, 0, accepted.stderr);
    assert.equal(accepted.stdout.trim(), "all 4 components ready");
    const refused = run("not json");
    assert.equal(refused.status, 1);
    assert.match(refused.stderr, /component breakdown/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the gate and fixtures name exactly the components the Rust server serves", () => {
  const served = serverComponentNames();
  const recognised = [...new Set([...READYZ_REQUIRED, ...READYZ_PARKED.keys()])].sort();
  const fixtures = [
    ...new Set(Object.values(FIXTURES).flatMap(([, report]) => report.components.map(([name]) => name))),
  ].sort();
  assert.deepEqual(recognised, served);
  assert.deepEqual(fixtures, served);
});
