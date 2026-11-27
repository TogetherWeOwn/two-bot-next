import assert from "node:assert/strict";
import test from "node:test";
import { bindStagingAutomodInventoryVerifier } from "../scripts/staging-automod-inventory-verifier.ts";
import { syntheticVerifier } from "./fixtures/staging-automod-verifier.ts";

const workflowSha = "a".repeat(40);
const runId = "1234";
const githubReadToken = "synthetic_inventory_read_token";
const readEnvironment = syntheticVerifier(`
  process.stdout.write(JSON.stringify({
    // Python may initialize its own locale in an isolated environment.
    keys: Object.keys(process.env).filter(key => key !== "LC_CTYPE").sort(),
    hasExpectedToken: process.env.GH_TOKEN === ${JSON.stringify(githubReadToken)},
  }));
`);

test("the inventory verifier only forwards the explicitly supplied GitHub read token", async t => {
  const ambient = {
    OWNERSHIP_CONTROL_TOKEN: "parent-ownership-token",
    STAGING_OWNERSHIP_CONTROL_TOKEN: "parent-staging-ownership-token",
    ACTIONS_ID_TOKEN_REQUEST_URL: "https://example.com/parent-oidc-url",
    ACTIONS_ID_TOKEN_REQUEST_TOKEN: "parent-oidc-token",
    GH_TOKEN: "parent-gh-token",
    GITHUB_TOKEN: "parent-github-token",
    GH_HOST: "example.com",
    INVENTORY_VERIFIER_AMBIENT: "parent-unrelated-value",
    NODE_V8_COVERAGE: "/dev/null",
  };
  const previous = Object.fromEntries(Object.keys(ambient).map(key => [key, process.env[key]]));
  t.after(() => {
    for (const [key, value] of Object.entries(previous)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  });
  Object.assign(process.env, ambient);

  const signal = new AbortController().signal;
  for (const options of [undefined, { githubReadToken }]) {
    const keys = options === undefined ? [] : ["GH_TOKEN"];
    const run = bindStagingAutomodInventoryVerifier((file, args, launchOptions) => {
      assert.deepEqual(Object.keys(launchOptions.env), keys);
      return readEnvironment(file, args, launchOptions);
    }, options);
    assert.deepEqual(JSON.parse(await run(workflowSha, runId, signal)), {
      keys, hasExpectedToken: options !== undefined,
    });
  }
});

test("the inventory verifier rejects invalid GitHub read tokens before launching", async () => {
  const signal = new AbortController().signal;
  let launches = 0;
  for (const invalidToken of [
    "", " ", " leading", "trailing ", "embedded space", "tab\tinside", "newline\n",
    "carriage\rreturn", "null\0byte", "delete\x7fbyte", "unicode\u00a0space", "x".repeat(4_097),
  ]) {
    const run = bindStagingAutomodInventoryVerifier(() => {
      launches += 1;
      throw new Error("Unexpected verifier launch");
    }, { githubReadToken: invalidToken });
    await assert.rejects(run(workflowSha, runId, signal), { message: "Staging inventory verifier failed" });
  }
  assert.equal(launches, 0);
});
