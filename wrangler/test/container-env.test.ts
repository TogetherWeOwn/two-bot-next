/**
 * TOG-12020: the Worker forwards an explicit, reviewed TWO_* flag allowlist
 * into the Container. All env values are synthetic; nothing is contacted.
 * The drift test reads the Rust sources in this checkout (crates/, src/).
 * Run: node --import ./test/cloudflare-loader.mjs --test test/container-env.test.ts
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { FORWARDED_FLAGS, NOT_FORWARDED, forwardedFlagVars } from "../src/container-env.ts";
import { TwoBotContainer, type Env } from "../src/index.ts";

const REPO_ROOT = fileURLToPath(new URL("../../", import.meta.url));
const RUST_ROOTS = ["crates", "src"];
const BASE_VARS = ["DISCORD_TOKEN", "DATABASE_URL", "GUILD_ID", "LISTEN_ADDR"];
// A `TWO_*` string literal; a trailing `_` is a prefix (`"TWO_INTERNAL_"`), not a name.
const TWO_LITERAL = /"(TWO_[A-Z0-9_]*[A-Z0-9])"/g;

function rustSources(): string[] {
  const files: string[] = [];
  for (const root of RUST_ROOTS) {
    const dir = join(REPO_ROOT, root);
    for (const entry of readdirSync(dir, { recursive: true, encoding: "utf8" })) {
      if (entry.endsWith(".rs")) files.push(readFileSync(join(dir, entry), "utf8"));
    }
  }
  return files;
}

function twoNames(sources: string[]): Set<string> {
  const names = new Set<string>();
  for (const source of sources) {
    for (const match of source.matchAll(TWO_LITERAL)) names.add(match[1]!);
  }
  return names;
}

function unclassified(names: Iterable<string>): string[] {
  const forwarded = new Set<string>(FORWARDED_FLAGS);
  return [...names].filter((name) => !forwarded.has(name) && !Object.hasOwn(NOT_FORWARDED, name)).sort();
}

/** Construct the real DO class far enough to read its startup envVars. */
function containerEnv(env: Partial<Env>): Record<string, string> {
  const database = new DatabaseSync(":memory:");
  try {
    const ctx = {
      container: { running: false },
      storage: {
        kv: { get: () => undefined },
        sql: { exec: (query: string) => database.prepare(query).all() },
      },
      // Alarm scheduling is out of scope here; container.test.ts covers start().
      blockConcurrencyWhile: () => Promise.resolve(),
    };
    const bot = new TwoBotContainer(ctx as unknown as DurableObjectState, env as Env);
    return bot.envVars as Record<string, string>;
  } finally {
    database.close();
  }
}

test("allowlisted names are forwarded verbatim", () => {
  const env: Record<string, string> = {};
  FORWARDED_FLAGS.forEach((name, i) => {
    env[name] = i % 3 === 0 ? ` value ${i}, with "quotes" ` : i % 3 === 1 ? "1" : "";
  });
  assert.deepEqual(forwardedFlagVars(env), env);
});

test("unlisted names, secrets and non-string values are dropped", () => {
  const env = {
    TWO_BOT: {},
    DISCORD_TOKEN: "synthetic-discord-token",
    DATABASE_URL: "synthetic-database-value",
    GUILD_ID: "111222333444555666",
    OPS_ALERT_WEBHOOK_URL: "https://ops.invalid/synthetic-secret",
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_DATABASE_URL: "synthetic-legacy-database-value",
    TWO_MODERATION_AUDIT_SECRET: "synthetic-mac-secret",
    TWO_BACKUP_S3_SECRET_ACCESS_KEY: "synthetic-s3-secret",
    TWO_INTERNAL_ALLOW_MODERATION: "1",
    TWO_INTERNAL_ALLOW_ANYTHING: "1",
    TWO_AUTOMOD_NOT_A_REAL_FLAG: "1",
    two_automod: "1",
    TWO_AUTOMOD: 1,
    TWO_MODERATION: true,
    TWO_TEMP_VOICE: undefined,
    TWO_ANTI_NUKE: null,
    TWO_RAID_JOIN_THRESHOLD: { toString: () => "5" },
    TWO_ANNOUNCEMENTS: "1",
  };
  assert.deepEqual(forwardedFlagVars(env), { TWO_ANNOUNCEMENTS: "1" });
});

test("the existing 4 Container vars are unchanged and cannot be overridden", () => {
  const base = {
    DISCORD_TOKEN: "synthetic-discord-token",
    DATABASE_URL: "synthetic-database-value",
    GUILD_ID: "111222333444555666",
    BOT_PORT: "8080",
  };
  const expected = {
    DISCORD_TOKEN: base.DISCORD_TOKEN,
    DATABASE_URL: base.DATABASE_URL,
    GUILD_ID: base.GUILD_ID,
    LISTEN_ADDR: "0.0.0.0:8080",
  };
  assert.deepEqual(containerEnv(base), expected);
  assert.deepEqual(
    containerEnv({ ...base, TWO_MODERATION: "1", TWO_FEED_POLL_SECONDS: "600" }),
    { ...expected, TWO_MODERATION: "1", TWO_FEED_POLL_SECONDS: "600" },
  );
  for (const name of BASE_VARS) {
    assert.ok(!(FORWARDED_FLAGS as readonly string[]).includes(name), name);
  }
});

test("configured voice infrastructure IDs reach the Container unchanged", () => {
  const vars = {
    DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID: "600",
    TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID: "201",
    TWO_TEMP_VOICE_CATEGORY_ID: "401",
    TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS: "700, 701",
  };
  assert.deepEqual(containerEnv(vars), { ...vars, LISTEN_ADDR: "0.0.0.0:8080" });
  assert.equal(containerEnv({}).DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID, undefined);
});

test("the allowlist is explicit, non-secret and disjoint from NOT_FORWARDED", () => {
  assert.equal(new Set(FORWARDED_FLAGS).size, FORWARDED_FLAGS.length, "duplicate allowlist name");
  for (const name of FORWARDED_FLAGS) {
    assert.match(name, /^TWO_[A-Z0-9_]*[A-Z0-9]$/, name);
    assert.ok(!name.startsWith("TWO_INTERNAL_"), `${name}: capability gates are not flags`);
    assert.doesNotMatch(name, /TOKEN|SECRET|PASSWORD|DATABASE_URL|_URL$|_KEY$|ACCESS_KEY/, `${name} looks secret`);
    assert.ok(!Object.hasOwn(NOT_FORWARDED, name), `${name} is in both lists`);
  }
  for (const [name, reason] of Object.entries(NOT_FORWARDED)) {
    assert.match(name, /^TWO_[A-Z0-9_]*[A-Z0-9]$/, name);
    assert.ok(reason.trim().length > 0, `${name} needs a reason`);
  }
});

test("drift: every TWO_* name in the Rust sources is forwarded or NOT_FORWARDED", () => {
  const names = twoNames(rustSources());
  // Guard the scanner itself so an empty or broken walk cannot pass silently.
  for (const known of ["TWO_MODERATION", "TWO_AUTOMOD_REPEAT_COUNT", "TWO_DATABASE_URL"]) {
    assert.ok(names.has(known), `scanner missed ${known}`);
  }
  assert.ok(!names.has("TWO_INTERNAL"), "prefix literals are not names");
  assert.deepEqual(
    unclassified(names),
    [],
    "classify these in wrangler/src/container-env.ts: FORWARDED_FLAGS for a non-secret " +
      "runtime flag, NOT_FORWARDED (with a reason) for everything else",
  );
  const stale = FORWARDED_FLAGS.filter((name) => !names.has(name));
  assert.deepEqual(stale, [], "allowlisted names no Rust source mentions");
});

test("drift: a newly read flag fails until it is classified", () => {
  const source = 'let on = std::env::var("TWO_BRAND_NEW_GATE").is_ok(); let p = "TWO_INTERNAL_";';
  assert.deepEqual(unclassified(twoNames([source])), ["TWO_BRAND_NEW_GATE"]);
  assert.deepEqual(unclassified(twoNames(['env("TWO_MODERATION")'])), []);
});
