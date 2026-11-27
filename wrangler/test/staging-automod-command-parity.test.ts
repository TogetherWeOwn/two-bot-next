import assert from "node:assert/strict";
import { test } from "node:test";
import { verifyStagingAutomodCommandParity } from "../scripts/staging-automod-command-parity.ts";
import fixture from "./fixtures/staging-automod-publication.json" with { type: "json" };

const target = {
  applicationId: "1469137636663758888", guildId: "1545644954272137297", name: "automod-preview", type: 1,
};
const pin = { id: "123456789012345678", version: "223456789012345678" };
const command = fixture.replies.preview;

function verify(readback: unknown) {
  return verifyStagingAutomodCommandParity({ enabled: true, pin, readGuildCommands: async () => readback });
}

test("defaults to a frozen disabled verdict without inspecting evidence", async () => {
  assert.deepEqual(await verifyStagingAutomodCommandParity(), { verdict: "disabled" });
  for (const enabled of [false, "true" as unknown as boolean]) {
    const result = await verifyStagingAutomodCommandParity({
      enabled,
      get pin(): never { return assert.fail("inspected pin while disabled"); },
      get readGuildCommands(): never { return assert.fail("inspected reader while disabled"); },
    });
    assert.deepEqual(result, { verdict: "disabled" });
    assert.ok(Object.isFrozen(result));
  }
});

test("passes one exact pinned command in the complete guild list", async () => {
  let reads = 0;
  const result = await verifyStagingAutomodCommandParity({ enabled: true, pin, readGuildCommands: async scope => {
    assert.deepEqual(scope, target);
    assert.ok(Object.isFrozen(scope));
    reads++;
    return [fixture.replies.help, command, fixture.replies.ping];
  } });
  assert.equal(reads, 1);
  assert.deepEqual(result, { verdict: "pass" });
  assert.ok(Object.isFrozen(result));
});

test("uses the authenticated guild scope for effective guild-only contexts", async () => {
  for (const globalFields of [
    { dm_permission: undefined, contexts: undefined },
    { dm_permission: true, contexts: null },
    { dm_permission: false, contexts: [0] },
  ]) {
    assert.deepEqual(await verify([{ ...command, ...globalFields }]), { verdict: "pass" });
    assert.deepEqual(await verify([{ ...command, ...globalFields, guild_id: undefined }]), { verdict: "fail" });
    assert.deepEqual(await verify([{ ...command, ...globalFields, guild_id: pin.id }]), { verdict: "fail" });
  }
});

test("normalizes optional metadata on unrelated guild rows", async () => {
  const { help, ping } = fixture.replies;
  const unrelated = { ...help, guild_id: undefined, type: undefined };
  assert.deepEqual(await verify([command, unrelated, ping]), { verdict: "pass" });
  assert.deepEqual(await verify([command, unrelated, { ...help, id: ping.id }]), { verdict: "fail" });
  assert.deepEqual(await verify([command, { ...unrelated, type: 4 }]), { verdict: "fail" });
  assert.deepEqual(await verify([{ ...command, type: undefined }, unrelated]), { verdict: "fail" });
});

test("refuses malformed, absent and ambiguous command sets", async () => {
  const cases: [string, unknown][] = [
    ["non-list", command],
    ["empty list", []],
    ["unrelated command only", [fixture.replies.help]],
    ["malformed row beside valid command", [command, null]],
    ["duplicate exact command", [command, command]],
    ["second registration under another ID", [command, { ...command, id: fixture.replies.help.id }]],
    ["same name under another type", [command, { ...command, id: fixture.replies.help.id, type: 2 }]],
    ["pinned ID under another name", [command, { ...command, name: "other" }]],
    ["unrelated command from another guild", [command, { ...fixture.replies.help, guild_id: pin.id }]],
  ];
  for (const [label, readback] of cases) {
    assert.deepEqual(await verify(readback), { verdict: "fail" }, label);
  }
});

test("bounds verification to the maximum guild command list", async () => {
  const others = Array.from({ length: 110 }, (_, i) => ({ ...fixture.replies.help,
    id: String(1000000000000000100n + BigInt(i)), name: `command-${i}` }));
  assert.deepEqual(await verify([command, ...others.slice(1)]), { verdict: "pass" });
  assert.deepEqual(await verify([command, ...others]), { verdict: "fail" });
});

test("refuses target, revision, description and permission drift", async () => {
  const cases: [string, Record<string, unknown>][] = [
    ["wrong application", { application_id: pin.id }],
    ["wrong guild", { guild_id: pin.id }],
    ["missing guild identity", { guild_id: undefined }],
    ["wrong type", { type: 2 }],
    ["wrong ID", { id: fixture.replies.help.id }],
    ["stale version", { version: fixture.replies.help.version }],
    ["stale description", { description: "Preview automod" }],
    ["missing member permission restriction", { default_member_permissions: undefined }],
    ["unrestricted member permissions", { default_member_permissions: null }],
    ["ordinary moderator default", { default_member_permissions: "8192" }],
    ["administrator-only default", { default_member_permissions: "0" }],
    ["legacy default overridden", { default_permission: false }],
    ["user installation", { integration_types: [0, 1] }],
    ["changed localization", { description_localizations: { en: "Preview automod" } }],
  ];
  for (const [label, patch] of cases) {
    assert.deepEqual(await verify([{ ...command, ...patch }]), { verdict: "fail" }, label);
  }
});

test("refuses option set and wire schema drift", async () => {
  const [text, mentions, attachments] = command.options;
  const cases: [string, unknown[]][] = [
    ["missing option", [text, mentions]],
    ["extra option", [...command.options, { name: "member", type: 6 }]],
    ["duplicate option", [text, mentions, mentions]],
    ["reordered options", [text, attachments, mentions]],
    ["renamed option", [text, { ...mentions, name: "member" }, attachments]],
    ["changed option description", [{ ...text, description: "Sample" }, mentions, attachments]],
    ["wrong option type", [{ ...text, type: 4 }, mentions, attachments]],
    ["required flag omitted", [{ ...text, required: undefined }, mentions, attachments]],
    ["optional flag made required", [text, { ...mentions, required: true }, attachments]],
    ["null optional flag", [text, { ...mentions, required: null }, attachments]],
    ["lower text minimum", [{ ...text, min_length: 0 }, mentions, attachments]],
    ["higher text maximum", [{ ...text, max_length: 2001 }, mentions, attachments]],
    ["missing mention minimum", [text, { ...mentions, min_value: undefined }, attachments]],
    ["higher mention maximum", [text, { ...mentions, max_value: 101 }, attachments]],
    ["higher attachment maximum", [text, mentions, { ...attachments, max_length: 1001 }]],
    ["unexpected attachment minimum", [text, mentions, { ...attachments, min_length: 1 }]],
    ["autocomplete enabled", [{ ...text, autocomplete: true }, mentions, attachments]],
    ["nonempty choices", [{ ...text, choices: [{ name: "sample", value: "sample" }] }, mentions, attachments]],
    ["nested options", [{ ...text, options: [mentions] }, mentions, attachments]],
  ];
  for (const [label, options] of cases) {
    assert.deepEqual(await verify([{ ...command, options }]), { verdict: "fail" }, label);
  }
});

test("refuses invalid revision pins and sanitizes reader failures", async () => {
  let reads = 0;
  const readGuildCommands = async () => { reads++; return [command]; };
  for (const invalidPin of [{ id: "0", version: pin.version }, { ...pin, version: "18446744073709551616" }]) {
    const result = await verifyStagingAutomodCommandParity({ enabled: true, pin: invalidPin, readGuildCommands });
    assert.deepEqual(result, { verdict: "fail" });
  }
  assert.deepEqual(await verifyStagingAutomodCommandParity({ enabled: true, readGuildCommands }), { verdict: "fail" });
  assert.deepEqual(await verifyStagingAutomodCommandParity({ enabled: true, pin }), { verdict: "fail" });
  assert.equal(reads, 0);
  const result = await verifyStagingAutomodCommandParity({ enabled: true, pin,
    readGuildCommands: async () => { throw new Error("synthetic private readback details"); } });
  assert.deepEqual(result, { verdict: "fail" });
  assert.ok(Object.isFrozen(result));
});

test("keeps the independently supplied revision across the asynchronous read", async () => {
  const mutablePin = { ...pin };
  const result = await verifyStagingAutomodCommandParity({ enabled: true, pin: mutablePin,
    readGuildCommands: async () => {
      mutablePin.version = fixture.replies.help.version;
      return [{ ...command, version: mutablePin.version }];
    } });
  assert.deepEqual(result, { verdict: "fail" });
});
