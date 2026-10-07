/** Command-drift guard: source grep for the binary, installed Wrangler help for
 * npm scripts. Never execute an operational command, connect to a database,
 * authenticate to Cloudflare, or read a token. Included in `npm test` / worker check.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

const root = new URL("../../", import.meta.url);
const read = (path: string) => readFileSync(new URL(path, root), "utf8");
const runbook = read("docs/runbook.md");
const main = read("crates/bot/src/main.rs");
const backup = read("crates/bot/src/backup_cli.rs");
const drill = read("crates/bot/src/restore_drill.rs");
const scripts: Record<string, string> = JSON.parse(read("wrangler/package.json")).scripts;

function shellCommands(markdown: string): string[] {
  return [...markdown.matchAll(/```bash\n([\s\S]*?)```/g)].flatMap((block) =>
    block[1].split("\n").map((line) => line.trim()).filter((line) => line && !line.startsWith("#")),
  );
}

function rustFunction(name: string, source = backup): string {
  const start = source.search(new RegExp(`^(?:pub )?(?:async )?fn ${name}\\(`, "m"));
  assert.ok(start >= 0, `missing binary parser: ${name}`);
  const rest = source.slice(start);
  const next = rest.slice(1).search(/^(?:(?:pub )?(?:async )?fn |mod |#\[cfg\(test\)\])/m);
  return next < 0 ? rest : rest.slice(0, next + 1);
}

function checkBinaryCommands(markdown: string): string[] {
  const dispatch = new Map([...backup.matchAll(/^\s*"([a-z-]+)" => (cmd_[a-z_]+|crate::restore_drill::dispatch)\(([^)]*)\)/gm)]
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
        let parser = arm.handler === "crate::restore_drill::dispatch"
          ? rustFunction("dispatch", drill)
          : rustFunction(arm.handler);
        if (arm.handler === "cmd_restore") {
          assert.ok(parser.includes("parse_restore_args(args)"));
          parser = rustFunction("parse_restore_args");
        }
        for (const option of parser.matchAll(/(?:==|!=)\s*"(--[a-z-]+)"|"(--[a-z-]+)"\s*=>/g)) {
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

test("operations runbook links readiness guidance without case-colliding filenames", () => {
  const names = readdirSync(new URL("docs/", root));
  assert.equal(new Set(names.map((name) => name.toLowerCase())).size, names.length,
    "docs must be safe to check out on case-insensitive filesystems");
  assert.ok(runbook.includes("[Container readiness monitoring](container-readiness.md)"));
  const readiness = read("docs/container-readiness.md");
  assert.ok(readiness.includes("[operations runbook](runbook.md#sustained-unready-alerts)"));
  for (const event of ["container_unready_alert", "container_unready_recovery", "container_keepalive_arm_failed"]) {
    assert.ok(readiness.includes(event), `missing readiness guidance: ${event}`);
  }
});

// Intentional typo fixtures prove this test does not silently skip new commands.
test("binary runbook examples grep the selected dispatcher/parser, not help prose", () => {
  const commands = checkBinaryCommands(runbook);
  for (const command of ["gateway", "--help", "--healthcheck", "backup", "restore", "restore-drill", "backup-upload", "guild-config-snapshot", "guild-config-restore"]) {
    assert.ok(commands.includes(command), `missing operator example: ${command}`);
  }
  for (const command of ["restart", "restore file --dryrun", "backup --dry-run", "guild-config-snapshot --apply", "backup-upload file --force", "restore-drill file --force", "restore-drill file --dry-run", "restore file --apply", "guild-config-restore --snapshot file --dry-run"]) {
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
  assert.ok(checkLocalLinks(read("docs/incident-tabletop-2026-10-06.md")) > 0);
  assert.throws(() => checkLocalLinks("[missing](does-not-exist.md)"), /missing local/);
  assert.throws(() => checkLocalLinks("[missing](#does-not-exist)"), /missing runbook heading/);
});

test("public tabletop evidence contains no private tracker references", () => {
  for (const doc of ["docs/incident-tabletop-2026-10-01.md", "docs/incident-tabletop-2026-10-06.md"]) {
    assert.doesNotMatch(read(doc),
      /\b(?:TOG|PAP)-\d+\b|\/(?:TOG|PAP)\/(?:issues|agents|projects|approvals|runs)\//);
  }
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
    ["crates/discord/src/ratelimit_guard.rs", "Discord refused the bot token; REST disabled until restart"],
  ];
  for (const [path, message] of logs) {
    assert.ok(incidents.includes(message), `missing incident signal: ${message}`);
    assert.ok(read(path).includes(`"${message}"`), `log no longer emitted in ${path}: ${message}`);
  }
});

// Documentation regression guards, not proof of runtime behavior, a deployed
// control or recovery. Fixtures also reject stale claims beside corrected prose.
const incidentContracts: [string, string, string, string][] = [
  ["persistent containment", "persistent ownership fence", "no authenticated HTTP stop route or persistent incident-pause control", "wrangler/src/ownership.ts"],
  ["activated writers", "Moderation and automod are implemented and gated", "these moderation action slices and Worker flag forwarding are absent", "wrangler/src/container-env.ts"],
  ["conditional live redirects", "With `REDIRECT_DB`, the Worker supplies `connectPostgres`", "the Worker constructs its RedirectStore with an undefined connector", "wrangler/src/redirect-store.ts"],
  ["DML-only jobs", "lazy jobs also use `skip_migrations=true`", "the lazy jobs connection currently requests migrations and the web contract DDL", "crates/bot/src/website_jobs.rs"],
  ["durable send admission", "token-wide durable `PgSendAdmission`", "Current send admission is per executor, not shared per token", "crates/core/src/send_admission/postgres.rs"],
  ["shared process guard", "process-wide `process_guard`", "There is no wired token-wide cooldown/queue/breaker", "crates/discord/src/ratelimit_guard.rs"],
  ["database readiness", "`database` component performs a bounded live ping", "There is no DB-ready component or DB-error metric", "crates/bot/src/server.rs"],
  ["authorized metrics proxy", "authenticated `GET /ops/metrics`", "the Worker and DO do not proxy it", "wrangler/src/index.ts"],
  ["job success coverage", "Every supervised job success updates", "the six periodic jobs do not populate that success metric", "crates/bot/src/jobs.rs"],
  ["current archive coverage", "current writer is v4", "does not include gateway_sessions or website tables", "crates/core/src/backup/dump_file.rs"],
  ["durable scorecard retries", "three durable attempt slots, five minutes apart", "the scorecard consumes its weekly attempt before DB work", "crates/bot/src/community_scorecard_retry.rs"],
  ["default-dark action ingress", "`POST /internal/actions` is implemented but staging-only and default-dark", "Internal-action endpoints and /voice/ownership/health are not wired bot endpoints", "wrangler/src/index.ts"],
];

function guidanceText(markdown: string): string {
  return markdown.replace(/[`*]/g, "").replace(/\s+/g, " ").toLowerCase();
}

function checkIncidentContract(markdown: string, contract: (typeof incidentContracts)[number]): void {
  const [name, claim, stale, source] = contract;
  const text = guidanceText(markdown);
  assert.ok(text.includes(guidanceText(claim)), `missing current incident contract: ${name}`);
  assert.ok(!text.includes(guidanceText(stale)), `stale incident claim: ${name}`);
  assert.ok(markdown.includes(source), `missing source reference: ${source}`);
}

for (const contract of incidentContracts) {
  const [name, claim, stale] = contract;
  test(`incident guidance preserves ${name} rather than historical absence claims`, () => {
    checkIncidentContract(runbook, contract);
    // Replacing all wrapped/repeated corrective prose must fail, and adding
    // the old claim beside the correction must not silently pass either.
    assert.throws(() => checkIncidentContract(runbook.replace(/\s+/g, " ").replaceAll(claim, stale), contract),
      /missing current incident contract/);
    assert.throws(() => checkIncidentContract(`${runbook}\n${stale}`, contract), /stale incident claim/);
  });
}

test("historical tabletop cannot substitute for current wiring or staging acceptance", () => {
  const history = read("docs/incident-tabletop-2026-10-01.md");
  assert.match(history, /historical findings at the October-1 source baseline/);
  assert.match(history, /not current\s+wiring guidance/);
  assert.match(history, /Local source walkthrough completed; staging walkthrough blocked/);
  assert.match(history, /successful source tests do not fill this gate/);
});

test("staging tabletop record stays a dry run with explicit open gaps", () => {
  const record = read("docs/incident-tabletop-2026-10-06.md");
  assert.match(record, /no outage was injected/);
  assert.match(record, /It is not cutover acceptance/);
  assert.match(record, /## Gaps found/);
  assert.match(record, /Image provenance for this head is unproven/);
  assert.ok(runbook.includes("(incident-tabletop-2026-10-06.md)"), "runbook must link the staging record");
  assert.doesNotMatch(record, /(?:token|secret)\s*[:=]\s*[A-Za-z0-9_-]{16,}/i);
});

test("ownership runbook examples use only the covered staging control client", () => {
  const commands = shellCommands(runbook).filter((line) => line.startsWith("node "));
  assert.ok(commands.length >= 3, "must cover status, takeover and fence");
  for (const line of commands) {
    assert.match(line, /^node wrangler\/scripts\/ownership-control\.mjs (?:status|(?:takeover|fence) "\$\{CURRENT_OWNER_EPOCH\}")$/);
  }
  assert.ok(read("wrangler/scripts/ownership-control.mjs").includes("export async function control("));
});

test("shell fences contain only covered tools and one-line examples", () => {
  for (const line of shellCommands(runbook)) {
    assert.match(line, /^(?:npm |curl |node wrangler\/scripts\/ownership-control\.mjs |env -u TWO_RESTORE_URL two-bot |(?:TWO_DATABASE_URL=\S+ |TWO_RESTORE_URL=\S+ |TWO_RESTORE_DRILL_BOOTSTRAP_URL=\S+ TWO_RESTORE_DRILL_EVIDENCE_DIR=\S+ )?two-bot(?: |$))/);
    assert.doesNotMatch(line, /[|;]|&&|\\$/);
  }
});
