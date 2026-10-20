import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

// Protected acceptance: the four operational safety CLIs in scripts/ must run
// their existing gate under every invocation form an operator or workflow can
// plausibly use — absolute path, cwd-relative path, a symlink to the file, a
// path containing spaces (whose URL form percent-escapes them), and a symlink
// inside a spaced directory. None may silently exit 0 without executing.
// Every case runs a real `node` subprocess against the real script files; no
// token used here is real and no case reaches the network (refusals fire
// before any fetch, and the one success case for the status CLI is served by
// an in-process fetch stub injected with --import).

const WRANGLER_DIR = fileURLToPath(new URL("..", import.meta.url));
const SCRIPTS_DIR = join(WRANGLER_DIR, "scripts");

const ENTRYPOINTS = [
  "check-readyz.mjs",
  "ownership-control.mjs",
  "production-ownership-control.mjs",
  "staging-ownership-status.mjs",
] as const;
type Entrypoint = (typeof ENTRYPOINTS)[number];

// Run-owned scratch layout: plain file symlinks, a symlinked directory whose
// own name contains spaces, and real file symlinks inside a spaced directory.
const ROOT = mkdtempSync(join(tmpdir(), "safety-cli-invocation-"));
const LINKS_DIR = join(ROOT, "links");
const SPACED_LINK_DIR = join(ROOT, "dir with spaces");
const SPACED_REAL_DIR = join(ROOT, "real spaced dir");
mkdirSync(LINKS_DIR);
mkdirSync(SPACED_REAL_DIR);
symlinkSync(SCRIPTS_DIR, SPACED_LINK_DIR, "dir");
for (const file of ENTRYPOINTS) {
  symlinkSync(join(SCRIPTS_DIR, file), join(LINKS_DIR, file), "file");
  symlinkSync(join(SCRIPTS_DIR, file), join(SPACED_REAL_DIR, file), "file");
}

interface InvocationForm {
  name: string;
  scriptPath: (file: Entrypoint) => string;
}

const FORMS: InvocationForm[] = [
  { name: "absolute path", scriptPath: (file) => join(SCRIPTS_DIR, file) },
  { name: "relative path", scriptPath: (file) => join("scripts", file) },
  { name: "symlink", scriptPath: (file) => join(LINKS_DIR, file) },
  { name: "spaced path", scriptPath: (file) => join(SPACED_LINK_DIR, file) },
  { name: "symlink in spaced dir", scriptPath: (file) => join(SPACED_REAL_DIR, file) },
];

// Readiness fixtures: the bot's real /readyz component shapes.
const readyBody = {
  components: [
    ["process", "ready"],
    ["gateway", "ready"],
    ["database", "ready"],
    ["token_invalid", "ready"],
  ],
};
const parkedBody = {
  components: [
    ["process", "ready"],
    ["gateway", "starting"],
    ["database", "ready"],
    ["token_invalid", "ready"],
  ],
};
const databaseDownBody = {
  components: [
    ["process", "ready"],
    ["gateway", "ready"],
    ["database", "down"],
    ["token_invalid", "ready"],
  ],
};
const READY_FILE = join(ROOT, "ready.json");
const PARKED_FILE = join(ROOT, "parked.json");
const DATABASE_DOWN_FILE = join(ROOT, "database-down.json");
const MALFORMED_FILE = join(ROOT, "malformed.json");
const MISSING_FILE = join(ROOT, "no-such-readyz.json");
writeFileSync(READY_FILE, JSON.stringify(readyBody));
writeFileSync(PARKED_FILE, JSON.stringify(parkedBody));
writeFileSync(DATABASE_DOWN_FILE, JSON.stringify(databaseDownBody));
writeFileSync(MALFORMED_FILE, JSON.stringify({ error: "ownership_refused" }));

// Offline fetch stub: one well-formed ownership status answer, injected into
// the child with --import so the status CLI's success path runs with no
// service call. The deployment/owner ids below are fixtures, not real ids.
const FETCH_STUB = join(ROOT, "fetch-stub.mjs");
writeFileSync(
  FETCH_STUB,
  "globalThis.fetch = async () => new Response(JSON.stringify({" +
    'deploymentId: "dep-1",' +
    'owner: { deploymentId: "dep-1", epoch: 2, phase: "active", actor: "ci",' +
    ' timestamp: "2026-01-01T00:00:00.000Z", oldEpoch: 1, oldDeploymentId: null },' +
    'running: false }), { status: 200, headers: { "content-type": "application/json" } });\n',
);

// Importer harness: importing a CLI module must stay side-effect-free.
const IMPORTER = join(ROOT, "importer.mjs");
writeFileSync(IMPORTER, 'await import(process.argv[2]);\nconsole.log("import-ok");\n');

const CONFIG_KEYS = [
  "STAGING_WORKER_URL",
  "PRODUCTION_WORKER_URL",
  "OWNERSHIP_CONTROL_TOKEN",
  "OWNERSHIP_ACTOR",
  "OWNERSHIP_EXPECTED_DEPLOYMENT",
  "OWNERSHIP_RELEASE_FENCE",
] as const;

// A syntactically valid stand-in, built at runtime so no credential-shaped
// literal lands in the repo. It only ever reaches offline validation paths
// (preflight / refusals / the fetch stub) and must never be printed.
const DUMMY_TOKEN = "x".repeat(32);
const STAGING_URL = "https://two-bot-next-staging.testaccount.workers.dev";
const PRODUCTION_URL = "https://two-bot-next.testaccount.workers.dev";

function scrubbedEnv(overrides: Record<string, string> = {}): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  for (const key of CONFIG_KEYS) delete env[key];
  return { ...env, ...overrides };
}

interface RunResult {
  status: number | null;
  stdout: string;
  stderr: string;
}

function runScript(
  scriptPath: string,
  args: string[],
  env: NodeJS.ProcessEnv,
  nodeArgs: string[] = [],
): RunResult {
  const result = spawnSync(process.execPath, [...nodeArgs, scriptPath, ...args], {
    cwd: WRANGLER_DIR,
    env,
    encoding: "utf8",
    timeout: 15_000,
  });
  assert.equal(result.error, undefined, `subprocess failed to start: ${result.error}`);
  assert.equal(result.signal, null, `subprocess killed by signal ${result.signal}`);
  return { status: result.status, stdout: result.stdout, stderr: result.stderr };
}

interface Scenario {
  name: string;
  args: string[];
  env?: Record<string, string>;
  nodeArgs?: string[];
  expectStatus: number;
  expectStdout?: string;
  expectStdoutMatch?: RegExp;
  expectStderr?: string;
}

const READYZ_REFUSAL =
  "Readiness gate failed: expected the bot's component breakdown, never an ownership refusal\n";
const TOKEN_REFUSAL = "OWNERSHIP_CONTROL_TOKEN is missing or invalid; stop before deployment\n";
const STAGING_ORIGIN_REFUSAL = "Expected the approved two-bot-next-staging workers.dev origin\n";

const SCENARIOS: Record<Entrypoint, Scenario[]> = {
  "check-readyz.mjs": [
    {
      name: "missing readiness input refuses",
      args: [],
      expectStatus: 1,
      expectStdout: "",
      expectStderr: READYZ_REFUSAL,
    },
    {
      name: "missing readiness file refuses",
      args: ["200", MISSING_FILE],
      expectStatus: 1,
      expectStdout: "",
      expectStderr: READYZ_REFUSAL,
    },
    {
      name: "malformed readiness input refuses",
      args: ["200", MALFORMED_FILE],
      expectStatus: 1,
      expectStdout: "",
      expectStderr: READYZ_REFUSAL,
    },
    {
      name: "database fault is never parked",
      args: ["503", DATABASE_DOWN_FILE],
      expectStatus: 1,
      expectStdout: "",
      expectStderr: READYZ_REFUSAL,
    },
    {
      name: "valid ready report passes",
      args: ["200", READY_FILE],
      expectStatus: 0,
      expectStdout: "all 4 components ready\n",
      expectStderr: "",
    },
    {
      name: "valid parked report passes",
      args: ["503", PARKED_FILE],
      expectStatus: 0,
      expectStdout: "process ready, gateway starting (parked, not E2E approval)\n",
      expectStderr: "",
    },
  ],
  "ownership-control.mjs": [
    {
      name: "missing configuration refuses",
      args: [],
      expectStatus: 1,
      expectStdout: "",
      expectStderr: "Ownership control failed; stop and report without substituting credentials\n",
    },
    {
      name: "missing token refuses",
      args: ["preflight"],
      env: { STAGING_WORKER_URL: STAGING_URL },
      expectStatus: 1,
      expectStdout: "",
      expectStderr: TOKEN_REFUSAL,
    },
    {
      name: "unapproved origin refuses",
      args: ["preflight"],
      env: { STAGING_WORKER_URL: "https://example.com", OWNERSHIP_CONTROL_TOKEN: DUMMY_TOKEN },
      expectStatus: 1,
      expectStdout: "",
      expectStderr: STAGING_ORIGIN_REFUSAL,
    },
    {
      name: "unsupported action refuses",
      args: ["bogus"],
      env: { STAGING_WORKER_URL: STAGING_URL, OWNERSHIP_CONTROL_TOKEN: DUMMY_TOKEN },
      expectStatus: 1,
      expectStdout: "",
      expectStderr: "Unknown action\n",
    },
    {
      name: "valid preflight passes",
      args: ["preflight"],
      env: { STAGING_WORKER_URL: STAGING_URL, OWNERSHIP_CONTROL_TOKEN: DUMMY_TOKEN },
      expectStatus: 0,
      expectStdout: '{"configured":true}\n',
      expectStderr: "",
    },
  ],
  "production-ownership-control.mjs": [
    {
      name: "missing configuration refuses",
      args: [],
      expectStatus: 1,
      expectStdout: "",
      expectStderr: "PRODUCTION_WORKER_URL is not a valid URL; stop before deployment\n",
    },
    {
      name: "missing token refuses",
      args: ["preflight"],
      env: { PRODUCTION_WORKER_URL: PRODUCTION_URL },
      expectStatus: 1,
      expectStdout: "",
      expectStderr: TOKEN_REFUSAL,
    },
    {
      name: "staging origin refuses on the production path",
      args: ["preflight"],
      env: { PRODUCTION_WORKER_URL: STAGING_URL, OWNERSHIP_CONTROL_TOKEN: DUMMY_TOKEN },
      expectStatus: 1,
      expectStdout: "",
      expectStderr: "Refusing a staging origin on the production control path; stop before deployment\n",
    },
    {
      name: "unsupported action refuses",
      args: ["bogus"],
      env: { PRODUCTION_WORKER_URL: PRODUCTION_URL, OWNERSHIP_CONTROL_TOKEN: DUMMY_TOKEN },
      expectStatus: 1,
      expectStdout: "",
      expectStderr: "Unknown action\n",
    },
    {
      name: "valid preflight passes",
      args: ["preflight"],
      env: { PRODUCTION_WORKER_URL: PRODUCTION_URL, OWNERSHIP_CONTROL_TOKEN: DUMMY_TOKEN },
      expectStatus: 0,
      expectStdout: '{"configured":true}\n',
      expectStderr: "",
    },
  ],
  "staging-ownership-status.mjs": [
    {
      name: "unsupported argument refuses",
      args: ["extra"],
      expectStatus: 1,
      expectStdout: "",
      expectStderr: "Ownership status takes no arguments; stop without substituting credentials\n",
    },
    {
      name: "missing configuration refuses",
      args: [],
      expectStatus: 1,
      expectStdout: "",
      expectStderr: "Ownership status is not configured; stop without substituting credentials\n",
    },
    {
      name: "unapproved origin refuses",
      args: [],
      env: { STAGING_WORKER_URL: "https://example.com", OWNERSHIP_CONTROL_TOKEN: DUMMY_TOKEN },
      expectStatus: 1,
      expectStdout: "",
      expectStderr: STAGING_ORIGIN_REFUSAL,
    },
    {
      name: "valid status read passes offline",
      args: [],
      env: { STAGING_WORKER_URL: STAGING_URL, OWNERSHIP_CONTROL_TOKEN: DUMMY_TOKEN },
      nodeArgs: ["--import", pathToFileURL(FETCH_STUB).href],
      expectStatus: 0,
      expectStdoutMatch: /^\{"owner_id":"dep-1","observed_at":"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z"\}\n$/,
      expectStderr: "",
    },
  ],
};

for (const file of ENTRYPOINTS) {
  for (const form of FORMS) {
    for (const scenario of SCENARIOS[file]) {
      test(`${file}: ${scenario.name} via ${form.name}`, () => {
        const result = runScript(
          form.scriptPath(file),
          scenario.args,
          scrubbedEnv(scenario.env),
          scenario.nodeArgs,
        );
        assert.equal(
          result.status,
          scenario.expectStatus,
          `exit code (stdout=${JSON.stringify(result.stdout)} stderr=${JSON.stringify(result.stderr)})`,
        );
        if (scenario.expectStdout !== undefined) assert.equal(result.stdout, scenario.expectStdout);
        if (scenario.expectStdoutMatch !== undefined) assert.match(result.stdout, scenario.expectStdoutMatch);
        if (scenario.expectStderr !== undefined) assert.equal(result.stderr, scenario.expectStderr);
        // No invocation form may leak the configured token, real or dummy.
        assert.ok(!result.stdout.includes(DUMMY_TOKEN), "stdout must not contain the token");
        assert.ok(!result.stderr.includes(DUMMY_TOKEN), "stderr must not contain the token");
      });
    }
  }
}

// Importing any of the four modules — directly, through a symlink, or through
// a spaced path — must stay side-effect-free even with no configuration: the
// gate runs only on direct execution, never on import.
const IMPORT_FORMS: InvocationForm[] = [
  { name: "absolute path", scriptPath: (file) => join(SCRIPTS_DIR, file) },
  { name: "symlink", scriptPath: (file) => join(LINKS_DIR, file) },
  { name: "spaced path", scriptPath: (file) => join(SPACED_LINK_DIR, file) },
];

for (const file of ENTRYPOINTS) {
  for (const form of IMPORT_FORMS) {
    test(`${file}: import via ${form.name} is side-effect-free`, () => {
      const result = runScript(IMPORTER, [pathToFileURL(form.scriptPath(file)).href], scrubbedEnv());
      assert.equal(result.status, 0, `exit code (stderr=${JSON.stringify(result.stderr)})`);
      assert.equal(result.stdout, "import-ok\n");
      assert.equal(result.stderr, "");
    });
  }
}
