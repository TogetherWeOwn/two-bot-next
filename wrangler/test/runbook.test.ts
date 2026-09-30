/** Command-drift guard: source grep for the binary, installed Wrangler help for
 * npm scripts. Never execute an operational command, connect to a database,
 * authenticate to Cloudflare, or read a token. Included in `npm test` / worker check.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

const root = new URL("../../", import.meta.url);
const read = (path: string) => readFileSync(new URL(path, root), "utf8");
const runbook = read("docs/runbook.md");
const main = read("crates/bot/src/main.rs");
const backup = read("crates/bot/src/backup_cli.rs");
const scripts: Record<string, string> = JSON.parse(read("wrangler/package.json")).scripts;

function shellCommands(markdown: string): string[] {
  return [...markdown.matchAll(/```bash\n([\s\S]*?)```/g)].flatMap((block) =>
    block[1].split("\n").map((line) => line.trim()).filter((line) => line && !line.startsWith("#")),
  );
}

function checkBinaryCommands(markdown: string): string[] {
  const dispatch = new Set([...backup.matchAll(/^\s*"([a-z-]+)" => cmd_/gm)].map((m) => m[1]));
  assert.ok(main.includes('backup_cli::dispatch(&cli_args)'), "backup dispatch must remain wired");
  const seen: string[] = [];
  for (const line of shellCommands(markdown)) {
    const match = line.match(/(?:^|\s)two-bot(?:\s+(\S+))?/);
    if (!match) continue;
    const command = match[1];
    if (command === "--help") {
      assert.ok(main.includes("print_backup_help_and_exit().await"));
      assert.ok(backup.includes('args[0] == "--help"'));
    } else if (command === "--healthcheck") {
      assert.ok(main.includes('arg == "--healthcheck"'));
    } else if (command) {
      assert.ok(dispatch.has(command), `unimplemented binary subcommand: ${command}`);
    }
    for (const option of line.slice(match.index! + match[0].length).matchAll(/--[a-z-]+/g)) {
      assert.ok(backup.includes(`"${option[0]}"`), `unimplemented backup option: ${option[0]}`);
    }
    seen.push(command ?? "gateway");
  }
  return seen;
}

function npmCommands(markdown: string): { script: string; args: string[] }[] {
  return shellCommands(markdown).filter((line) => line.startsWith("npm ")).map((line) => {
    const match = line.match(/^npm --prefix wrangler (?:run (\S+)(?: -- (.*))?|(ci|test)(?: --include=dev)?)$/);
    assert.ok(match, `unrecognized npm command form: ${line}`);
    return { script: match[1] ?? match[3], args: match[2]?.split(/\s+/) ?? [] };
  });
}

function scriptCommand(script: string): string[] {
  assert.ok(Object.hasOwn(scripts, script), `missing npm script: ${script}`);
  const command = scripts[script].split(/\s+/);
  assert.equal(command[0], "wrangler", `operations script must invoke pinned Wrangler: ${script}`);
  return command.slice(1);
}

// Intentional typo fixtures prove this test does not silently skip new commands.
test("binary runbook examples grep the real dispatcher, not just help prose", () => {
  const commands = checkBinaryCommands(runbook);
  for (const command of ["gateway", "--help", "--healthcheck", "backup", "restore", "backup-upload", "guild-config-snapshot", "guild-config-restore"]) {
    assert.ok(commands.includes(command), `missing operator example: ${command}`);
  }
  assert.throws(() => checkBinaryCommands("```bash\ntwo-bot restart\n```"), /unimplemented/);
  assert.throws(() => checkBinaryCommands("```bash\ntwo-bot restore file --dryrun\n```"), /unimplemented/);
});

test("Wrangler runbook examples exist in npm scripts and pinned CLI help", () => {
  const scratch = mkdtempSync(join(process.env.PAPERCLIP_RUN_SCRATCH_DIR ?? process.env.PAPERCLIP_SCRATCH_DIR ?? tmpdir(), "runbook-help-"));
  try {
    const checked = new Set<string>();
    for (const { script, args } of npmCommands(runbook)) {
      if (script === "ci") continue; // Built-in npm installer, not an ops script.
      assert.ok(Object.hasOwn(scripts, script), `missing npm script: ${script}`);
      if (!scripts[script].startsWith("wrangler ")) continue;
      const argv = [...scriptCommand(script), ...args];
      const key = argv.join(" ");
      if (checked.has(key)) continue;
      checked.add(key);
      // --help short-circuits Wrangler before auth/deploy. Pass only a fresh HOME
      // and OS paths, never inherited secrets or a real Wrangler auth profile.
      const result = spawnSync(process.execPath, [new URL("wrangler/node_modules/wrangler/bin/wrangler.js", root).pathname, ...argv, "--help"], {
        cwd: scratch,
        env: { PATH: process.env.PATH, HOME: scratch, XDG_CONFIG_HOME: scratch, WRANGLER_SEND_METRICS: "false", CI: "true" },
        encoding: "utf8",
        timeout: 30_000,
      });
      assert.equal(result.status, 0, `${key}: ${result.error ?? ""}\n${result.stdout}\n${result.stderr}`);
      const help = result.stdout + result.stderr;
      const prefix = [...scriptCommand(script), ...(["versions", "deployments", "containers"].includes(script) ? args.slice(0, 1) : [])];
      assert.ok(help.includes(`wrangler ${prefix.join(" ")}`), `missing help for ${key}`);
      assert.doesNotMatch(help, /Unknown argument|Unknown command/i);
    }
    assert.ok(checked.size >= 7, "must cover deploy, versions, deployments, rollback, logs and containers");
    assert.throws(() => scriptCommand("restart"), /missing npm script/);
    assert.throws(() => npmCommands("```bash\nnpm run missing\n```"), /unrecognized/);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});

test("shell fences contain only covered tools and one-line examples", () => {
  for (const line of shellCommands(runbook)) {
    assert.match(line, /^(?:npm |curl |env -u TWO_RESTORE_URL two-bot |(?:TWO_DATABASE_URL=\S+ |TWO_RESTORE_URL=\S+ )?two-bot(?: |$))/);
    assert.doesNotMatch(line, /[|;]|&&|\\$/);
  }
});
