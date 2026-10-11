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
import {
  FORWARDED_DISCORD_IDS,
  FORWARDED_FLAGS,
  NOT_FORWARDED,
  forwardedDiscordIdVars,
  forwardedFlagVars,
  isSnowflake,
  isSnowflakeList,
} from "../src/container-env.ts";
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

// TOG-19025 (M3.6): the non-secret DISCORD_* IDs the Rust loaders read.
const DISCORD_ID_FIXTURES: Record<string, string> = {
  DISCORD_AUDIT_LOG_CHANNEL_ID: "111111111111111111",
  DISCORD_VOICE_LOG_CHANNEL_ID: "222222222222222222",
  DISCORD_MODERATION_LOG_CHANNEL_ID: "333333333333333333",
  DISCORD_STAFF_ALERT_CHANNEL_ID: "444444444444444444",
  DISCORD_TICKET_CATEGORY_ID: "555555555555555555",
  DISCORD_TICKET_PANEL_CHANNEL_ID: "666666666666666666",
  DISCORD_TICKET_STAFF_ROLE_ID: "777777777777777777",
  DISCORD_LANDING_CHANNEL_IDS: "888888888888888888, 999999999999999999",
  DISCORD_GOODBYE_CHANNEL_IDS: "101010101010101010",
  DISCORD_ANCHOR_WELCOME_CHANNEL_ID: "121212121212121212",
  DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID: "600",
  DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID: "131313131313131313",
};

test("the Discord ID allowlist holds exactly the M3.6 loader inputs", () => {
  assert.deepEqual(
    [...FORWARDED_DISCORD_IDS].sort(),
    Object.keys(DISCORD_ID_FIXTURES).sort(),
    "allowlist and fixtures must stay in step",
  );
});

test("snowflake validation matches the Rust loaders", () => {
  for (const valid of ["1", "600", "2222", "111222333444555666", "18446744073709551615", " 123 ", "00123"]) {
    assert.equal(isSnowflake(valid), true, valid);
  }
  for (const invalid of ["", "   ", "0", "00", "-1", "abc", "voice", "secret-not-an-id", "12.5", "18446744073709551616", "99999999999999999999"]) {
    assert.equal(isSnowflake(invalid), false, invalid);
  }
  assert.equal(isSnowflakeList("111, 222"), true);
  assert.equal(isSnowflakeList(" 12, ,13 "), true);
  assert.equal(isSnowflakeList("secret-not-an-id"), false);
  assert.equal(isSnowflakeList("111, abc"), false);
  assert.equal(isSnowflakeList("0"), false);
});

test("each forwarded Discord ID reaches the Container verbatim when valid", () => {
  assert.deepEqual(forwardedDiscordIdVars(DISCORD_ID_FIXTURES), DISCORD_ID_FIXTURES);
  assert.deepEqual(
    containerEnv(DISCORD_ID_FIXTURES),
    { ...DISCORD_ID_FIXTURES, LISTEN_ADDR: "0.0.0.0:8080" },
  );
});

test("invalid, empty and non-string Discord IDs are dropped, never forwarded", () => {
  const env = {
    ...DISCORD_ID_FIXTURES,
    DISCORD_AUDIT_LOG_CHANNEL_ID: "not-a-snowflake",
    DISCORD_VOICE_LOG_CHANNEL_ID: "   ",
    DISCORD_MODERATION_LOG_CHANNEL_ID: "",
    DISCORD_STAFF_ALERT_CHANNEL_ID: "secret-not-an-id",
    DISCORD_TICKET_CATEGORY_ID: "0",
    DISCORD_TICKET_PANEL_CHANNEL_ID: 123,
    DISCORD_TICKET_STAFF_ROLE_ID: null,
    DISCORD_LANDING_CHANNEL_IDS: "111, abc",
    DISCORD_GOODBYE_CHANNEL_IDS: ", ,",
    DISCORD_ANCHOR_WELCOME_CHANNEL_ID: undefined,
    DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID: "18446744073709551616",
    DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID: "-5",
  };
  // Drops are loud (see next test); keep this test's output clean.
  const warnings = captureWarnings(() => {
    assert.deepEqual(forwardedDiscordIdVars(env), {});
  });
  assert.ok(warnings.length > 0, "expected drop warnings");
  let forwarded: Record<string, string> = {};
  captureWarnings(() => {
    forwarded = containerEnv(env);
  });
  for (const name of FORWARDED_DISCORD_IDS) {
    assert.equal(forwarded[name], undefined, name);
  }
});

/** Run `fn` with console.warn captured; returns the joined messages. */
function captureWarnings(fn: () => void): string[] {
  const messages: string[] = [];
  const original = console.warn;
  console.warn = (...args: unknown[]) => {
    messages.push(args.map(String).join(" "));
  };
  try {
    fn();
  } finally {
    console.warn = original;
  }
  return messages;
}

test("dropped Discord IDs warn with the variable name, never the value", () => {
  const warnings = captureWarnings(() => {
    assert.deepEqual(
      forwardedDiscordIdVars({
        DISCORD_AUDIT_LOG_CHANNEL_ID: "not-a-snowflake",
        DISCORD_LANDING_CHANNEL_IDS: "111, abc",
        DISCORD_VOICE_LOG_CHANNEL_ID: "   ",
        DISCORD_GOODBYE_CHANNEL_IDS: ", ,",
        DISCORD_MODERATION_LOG_CHANNEL_ID: "",
      }),
      {},
    );
  });
  // Loud drops: the two invalid values. Silent drops: unset-equivalents
  // (empty, whitespace-only, all-empty list) and absent names.
  assert.equal(warnings.length, 2, warnings.join("\n"));
  assert.ok(warnings[0]!.includes("DISCORD_AUDIT_LOG_CHANNEL_ID"), warnings[0]);
  assert.ok(warnings[1]!.includes("DISCORD_LANDING_CHANNEL_IDS"), warnings[1]);
  for (const message of warnings) {
    assert.doesNotMatch(message, /not-a-snowflake|111, abc/, `value leaked: ${message}`);
  }
});

test("no secret rides the Discord ID allowlist", () => {
  const env = {
    DISCORD_TOKEN: "synthetic-discord-token",
    DISCORD_BOT_TOKEN: "synthetic-bot-token",
    DISCORD_STAGING_BOT_TOKEN: "synthetic-staging-token",
    DISCORD_API_BASE: "https://not-forwarded.invalid",
    DISCORD_GATEWAY_URL: "ws://not-forwarded.invalid",
    DISCORD_PREFLIGHT_API_BASE: "http://127.0.0.1:9",
    ...DISCORD_ID_FIXTURES,
  };
  const forwarded = forwardedDiscordIdVars(env);
  assert.deepEqual(forwarded, DISCORD_ID_FIXTURES);
  const container = containerEnv(env);
  for (const secret of ["DISCORD_BOT_TOKEN", "DISCORD_STAGING_BOT_TOKEN", "DISCORD_API_BASE", "DISCORD_GATEWAY_URL", "DISCORD_PREFLIGHT_API_BASE"]) {
    assert.equal(container[secret], undefined, secret);
  }
  // DISCORD_TOKEN stays on its own explicit secret line, never the allowlist.
  assert.ok(!(FORWARDED_DISCORD_IDS as readonly string[]).includes("DISCORD_TOKEN"));
});

test("the allowlist is explicit, non-secret and disjoint from NOT_FORWARDED", () => {
  assert.equal(new Set(FORWARDED_FLAGS).size, FORWARDED_FLAGS.length, "duplicate allowlist name");
  for (const name of FORWARDED_FLAGS) {
    assert.match(name, /^TWO_[A-Z0-9_]*[A-Z0-9]$/, name);
    assert.ok(!name.startsWith("TWO_INTERNAL_"), `${name}: capability gates are not flags`);
    assert.doesNotMatch(name, /TOKEN|SECRET|PASSWORD|DATABASE_URL|_URL$|_KEY$|ACCESS_KEY/, `${name} looks secret`);
    assert.ok(!Object.hasOwn(NOT_FORWARDED, name), `${name} is in both lists`);
  }
  assert.equal(new Set(FORWARDED_DISCORD_IDS).size, FORWARDED_DISCORD_IDS.length, "duplicate Discord ID name");
  for (const name of FORWARDED_DISCORD_IDS) {
    assert.match(name, /^DISCORD_[A-Z0-9_]*[A-Z0-9]$/, name);
    assert.doesNotMatch(name, /TOKEN|SECRET|PASSWORD|DATABASE_URL|API_BASE|GATEWAY_URL|PREFLIGHT|STAGING|_KEY$/, `${name} looks secret`);
    assert.ok(!Object.hasOwn(NOT_FORWARDED, name), `${name} is in both lists`);
    assert.ok(!(FORWARDED_FLAGS as readonly string[]).includes(name), `${name} is in both allowlists`);
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
