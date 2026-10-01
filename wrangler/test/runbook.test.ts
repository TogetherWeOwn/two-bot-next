/** Command-drift guard: source grep for the binary, installed Wrangler help for
 * npm scripts. Never execute an operational command, connect to a database,
 * authenticate to Cloudflare, or read a token. Included in `npm test` / worker check.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, mkdtempSync, rmSync, existsSync } from "node:fs";
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

function rustFunction(name: string): string {
  const start = backup.search(new RegExp(`^(?:pub )?(?:async )?fn ${name}\\(`, "m"));
  assert.ok(start >= 0, `missing binary parser: ${name}`);
  const rest = backup.slice(start);
  const next = rest.slice(1).search(/^(?:(?:pub )?(?:async )?fn |mod |#\[cfg\(test\)\])/m);
  return next < 0 ? rest : rest.slice(0, next + 1);
}

function checkBinaryCommands(markdown: string): string[] {
  const dispatch = new Map([...backup.matchAll(/^\s*"([a-z-]+)" => (cmd_[a-z_]+)\(([^)]*)\)/gm)]
    .map((m) => [m[1], { handler: m[2], args: m[3] }]));
  assert.ok(main.includes('backup_cli::dispatch(&cli_args)'), "backup dispatch must remain wired");
  const seen: string[] = [];
  for (const line of shellCommands(markdown)) {
    const match = line.match(/(?:^|\s)two-bot(?:\s+(\S+))?/);
    if (!match) continue;
    const command = match[1];
    const options = new Set<string>();
    if (command === "--help") {
      assert.ok(main.includes("print_backup_help_and_exit().await"));
      assert.ok(backup.includes('args[0] == "--help"'));
    } else if (command === "--healthcheck") {
      assert.ok(main.includes('arg == "--healthcheck"'));
    } else if (command) {
      const arm = dispatch.get(command);
      assert.ok(arm, `unimplemented binary subcommand: ${command}`);
      // No-argument dispatch arms ignore trailing flags: never advertise those
      // as controls. For restore, follow the handler into its actual parser.
      if (arm.args) {
        let parser = rustFunction(arm.handler);
        if (arm.handler === "cmd_restore") {
          assert.ok(parser.includes("parse_restore_args(args)"));
          parser = rustFunction("parse_restore_args");
        }
        for (const option of parser.matchAll(/==\s*"(--[a-z-]+)"|"(--[a-z-]+)"\s*=>/g)) {
          options.add(option[1] ?? option[2]);
        }
      }
    }
    for (const token of line.slice(match.index! + match[0].length).trim().split(/\s+/)) {
      if (!token.startsWith("-")) continue;
      const option = token.split("=")[0];
      assert.ok(options.has(option), `unimplemented ${command ?? "gateway"} option: ${option}`);
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

function scriptCommand(script: string, aliases: Record<string, string>): string[] {
  assert.ok(Object.hasOwn(aliases, script), `missing npm script: ${script}`);
  const command = aliases[script].split(/\s+/);
  assert.equal(command[0], "wrangler", `operations script must invoke pinned Wrangler: ${script}`);
  return command.slice(1);
}

function checkWranglerCommands(markdown: string, aliases: Record<string, string>, helpFor: (prefix: string[]) => string): Set<string> {
  const checked = new Set<string>();
  for (const { script, args } of npmCommands(markdown)) {
    if (script === "ci") continue; // Built-in npm installer, not an ops script.
    assert.ok(Object.hasOwn(aliases, script), `missing npm script: ${script}`);
    if (script === "test") continue; // The only intentional non-Wrangler script.
    const command = scriptCommand(script, aliases);
    const prefix = [...command, ...(["versions", "deployments", "containers"].includes(command[0]) ? args.slice(0, 1) : [])];
    assert.ok(prefix.length > 0 && prefix.every((part) => /^[a-z][a-z-]*$/.test(part)), `invalid Wrangler command: ${prefix.join(" ")}`);
    const help = helpFor(prefix);
    assert.ok(help.includes(`wrangler ${prefix.join(" ")}`), `missing help for ${prefix.join(" ")}`);
    assert.doesNotMatch(help, /Unknown argument|Unknown command/i);
    // --help bypasses Wrangler's argument validation. Read advertised option
    // names from the selected command's flag sections, not its exit status.
    const options = new Set<string>();
    let flagSection = false;
    for (const line of help.split("\n")) {
      if (/^[A-Z][A-Z ]+$/.test(line)) flagSection = line === "GLOBAL FLAGS" || line === "OPTIONS";
      if (!flagSection) continue;
      const option = line.match(/^\s+(?:(-[A-Za-z]),\s+)?(--[a-z][a-z-]*)\s+/);
      if (option) {
        if (option[1]) options.add(option[1]);
        options.add(option[2]);
      }
    }
    assert.ok(options.has("--help"), `missing flag help for ${prefix.join(" ")}`);
    for (const token of [...command, ...args]) {
      if (!token.startsWith("-")) continue;
      const option = token.split("=")[0];
      assert.ok(options.has(option), `unimplemented Wrangler option for ${prefix.join(" ")}: ${option}`);
    }
    checked.add([...command, ...args].join(" "));
  }
  return checked;
}

// Intentional typo fixtures prove this test does not silently skip new commands.
test("binary runbook examples grep the selected dispatcher/parser, not help prose", () => {
  const commands = checkBinaryCommands(runbook);
  for (const command of ["gateway", "--help", "--healthcheck", "backup", "restore", "backup-upload", "guild-config-snapshot", "guild-config-restore"]) {
    assert.ok(commands.includes(command), `missing operator example: ${command}`);
  }
  for (const command of ["restart", "restore file --dryrun", "backup --dry-run", "guild-config-snapshot --apply", "backup-upload file --force", "restore file --apply", "guild-config-restore --snapshot file --dry-run"]) {
    assert.throws(() => checkBinaryCommands(`\`\`\`bash\ntwo-bot ${command}\n\`\`\``), /unimplemented/);
  }
});

test("Wrangler runbook examples exist in npm scripts and pinned CLI help", () => {
  const scratch = mkdtempSync(join(process.env.PAPERCLIP_RUN_SCRATCH_DIR ?? process.env.PAPERCLIP_SCRATCH_DIR ?? tmpdir(), "runbook-help-"));
  try {
    const helpCache = new Map<string, string>();
    const helpFor = (prefix: string[]) => {
      const key = prefix.join(" ");
      const cached = helpCache.get(key);
      if (cached !== undefined) return cached;
      // Only a validated command path + --help: no operational flags/values.
      // Fresh HOME and minimal env prevent inherited auth profiles or secrets.
      const result = spawnSync(process.execPath, [new URL("wrangler/node_modules/wrangler/bin/wrangler.js", root).pathname, ...prefix, "--help"], {
        cwd: scratch,
        env: { PATH: process.env.PATH, HOME: scratch, XDG_CONFIG_HOME: scratch, WRANGLER_SEND_METRICS: "false", CI: "true" },
        encoding: "utf8",
        timeout: 30_000,
      });
      assert.equal(result.status, 0, `${key}: ${result.error ?? ""}\n${result.stdout}\n${result.stderr}`);
      const help = result.stdout + result.stderr;
      helpCache.set(key, help);
      return help;
    };
    const checked = checkWranglerCommands(runbook, scripts, helpFor);
    assert.ok(checked.size >= 7, "must cover deploy, versions, deployments, rollback, logs and containers");
    assert.throws(() => scriptCommand("restart", scripts), /missing npm script/);
    assert.throws(() => npmCommands("```bash\nnpm run missing\n```"), /unrecognized/);
    for (const option of ["--evn", "--envv=staging", "--dryrun", "-z"]) {
      assert.throws(() => checkWranglerCommands(`\`\`\`bash\nnpm --prefix wrangler run rollback -- VERSION_ID ${option} staging\n\`\`\``, scripts, helpFor), /unimplemented Wrangler option/);
    }
    assert.throws(() => checkWranglerCommands(runbook, { ...scripts, rollback: "wranglr rollback" }, helpFor), /must invoke pinned Wrangler/);
    assert.throws(() => checkWranglerCommands(runbook, { ...scripts, logs: "node logger.js" }, helpFor), /must invoke pinned Wrangler/);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});

function checkLocalLinks(markdown: string): number {
  let checked = 0;
  for (const match of markdown.matchAll(/\[[^\]]*\]\(([^)\s]+)\)/g)) {
    const href = match[1];
    if (/^(?:https?:\/\/|\/TOG\/)/.test(href)) continue; // Offline check; not remote availability.
    const [path, anchor] = href.split("#");
    const target = new URL(path || "runbook.md", new URL("docs/", root));
    assert.ok(existsSync(target), `missing local runbook link: ${href}`);
    if (anchor && target.pathname.endsWith(".md")) {
      const headings = [...readFileSync(target, "utf8").matchAll(/^#{1,6} (.+)$/gm)]
        .map((heading) => heading[1].toLowerCase().replace(/[^\w\s-]/g, "").replace(/\s/g, "-"));
      assert.ok(headings.includes(anchor), `missing runbook heading: ${href}`);
    }
    checked++;
  }
  return checked;
}

test("runbook local file links and Markdown heading anchors resolve", () => {
  assert.ok(checkLocalLinks(runbook) > 20);
  assert.ok(checkLocalLinks(read("docs/incident-tabletop-2026-10-01.md")) > 0);
  assert.throws(() => checkLocalLinks("[missing](does-not-exist.md)"), /missing local/);
  assert.throws(() => checkLocalLinks("[missing](#does-not-exist)"), /missing runbook heading/);
});

test("incident playbooks cite emitted metrics and selected literal log messages", () => {
  const incidents = runbook.split("## Incident playbooks\n")[1]?.split("## Secret inventory:")[0];
  assert.ok(incidents, "missing incident playbooks");
  const metrics = read("crates/core/src/metrics.rs");
  const names = new Set([...incidents.matchAll(/\btwo_bot_[a-z_]+\b/g)].map((match) => match[0]));
  assert.ok(names.size >= 8, "incident signals must name their existing metric families");
  for (const name of names) {
    assert.ok(metrics.includes(`"${name}"`), `metric not emitted by the registry: ${name}`);
  }
  const logs: [string, string][] = [
    ["crates/bot/src/gateway.rs", "gateway reconnect failed; Twilight will retry"],
    ["crates/bot/src/gateway.rs", "gateway ready; checkpoint committed"],
    ["crates/bot/src/main.rs", "durable gateway initialized; shard connecting"],
    ["crates/bot/src/main.rs", "durable gateway failed; checkpoint unchanged, readiness unavailable"],
    ["crates/bot/src/jobs.rs", "periodic job failed"],
    ["crates/bot/src/command_runtime.rs", "sticky lookup failed; skipping activity"],
    ["crates/bot/src/command_runtime.rs", "sticky claim failed; skipping activity"],
    ["wrangler/src/index.ts", "two-bot container stopped"],
    ["crates/bot/src/server.rs", "SIGTERM received; draining"],
  ];
  for (const [path, message] of logs) {
    assert.ok(incidents.includes(message), `missing incident signal: ${message}`);
    assert.ok(read(path).includes(`"${message}"`), `log no longer emitted in ${path}: ${message}`);
  }
});

test("shell fences contain only covered tools and one-line examples", () => {
  for (const line of shellCommands(runbook)) {
    assert.match(line, /^(?:npm |curl |env -u TWO_RESTORE_URL two-bot |(?:TWO_DATABASE_URL=\S+ |TWO_RESTORE_URL=\S+ )?two-bot(?: |$))/);
    assert.doesNotMatch(line, /[|;]|&&|\\$/);
  }
});
