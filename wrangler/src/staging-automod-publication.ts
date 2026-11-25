import { isSnowflake } from "./container-env.ts";
import { boundRead, readReceiverBytes, releaseUpstream } from "./internal-actions.ts";
import type { CommandRevision } from "./staging-automod-recovery.ts";
import type { admitStagingAutomod } from "./staging-automod.ts";
import { MAX_STAGING_USE_MS, parseStagingUseChange, type StagingUse } from "./staging-lifecycle.ts";
import { parseRegistryJson, type PreviewEnv } from "./staging-preview.ts";

const target = Object.freeze({
  applicationId: "1469137636663758888",
  guildId: "1545644954272137297",
  name: "automod-preview",
});
const api = "https://discord.com/api/v10";
const commands = `${api}/applications/${target.applicationId}/guilds/${target.guildId}/commands`;
// Match automod_preview_definition and command_to_twilight for the guild endpoint.
const definition = {
  name: target.name,
  type: 1,
  description: "Preview which automod filter fires on text. Smallest synthetic phrase; do not paste personal data.",
  default_member_permissions: "32",
  options: [
    { name: "text", description: "Sample text to evaluate (1-2000 characters)", type: 3,
      required: true, min_length: 1, max_length: 2000 },
    { name: "mentions", description: "Synthetic explicit user-mention count (0-100, default 0)", type: 4,
      required: false, min_value: 0, max_value: 100 },
    { name: "attachments", description: "1-10 bare filenames separated only by comma + ASCII space (e.g. a.txt, b.png); no trimming",
      type: 3, required: false, max_length: 1000 },
  ],
};
const refused = () => new Error("staging_automod_publication_refused");
const uncertain = () => new Error("staging_automod_publication_uncertain");

export type StagingAutomodAdmission = Readonly<Awaited<ReturnType<typeof admitStagingAutomod>>>;
export type StagingAutomodPublicationIntent = Readonly<{
  lease: Readonly<StagingUse>;
  publishBefore: number;
  target: typeof target;
  baseline: string;
}>;
export type StagingAutomodOwnedCommand = Readonly<{ intent: StagingAutomodPublicationIntent; id: string; version: string }>;
export type StagingAutomodCleanupGrant = Readonly<{ owned: StagingAutomodOwnedCommand; removeBefore: number }>;

type DiscordTransport = {
  env: Readonly<PreviewEnv> & { readonly TWO_DEPLOY_ENV?: string };
  // One Fetch API attempt, with abort and without redirects or retries.
  request: (url: string, init: RequestInit) => Promise<Response>;
};
type Publication = DiscordTransport & {
  // Return admission only while the caller holds the exclusive AutoMod window.
  currentAdmission: () => Promise<StagingAutomodAdmission | undefined>;
  // Return true only for a new, exclusive durable intent. Keep it for recovery.
  persistIntent: (intent: StagingAutomodPublicationIntent) => Promise<boolean>;
  // Bind the validated 201 receipt to its intent. Return true after durable commit.
  recordCreated: (owned: StagingAutomodOwnedCommand) => Promise<boolean>;
};
type Cleanup = DiscordTransport & {
  // Issue only after publication is fenced and drained and the durable owned
  // record is reread. Recovery retains the original lease and the hold.
  currentCleanupGrant: () => Promise<StagingAutomodCleanupGrant | undefined>;
};

type JsonRecord = Record<string, unknown>;
type RegistryRow = JsonRecord & { id: string; version: string };

function record(value: unknown): JsonRecord | undefined {
  return value && typeof value === "object" && !Array.isArray(value) ? value as JsonRecord : undefined;
}

function snowflake(value: unknown): value is string {
  return typeof value === "string" && /^[1-9][0-9]{16,19}$/.test(value) && isSnowflake(value);
}

function identity(row: JsonRecord | undefined): row is RegistryRow {
  return !!row && row.application_id === target.applicationId
    && (!Object.hasOwn(row, "guild_id") || row.guild_id === target.guildId)
    && snowflake(row.id) && snowflake(row.version);
}

function registry(value: unknown): RegistryRow[] {
  if (!Array.isArray(value) || value.length > 110) throw refused();
  const ids = new Set<string>(), commands = new Set<string>();
  return value.map(raw => {
    const row = record(raw);
    const type = row && !Object.hasOwn(row, "type") ? 1 : row?.type;
    if (!identity(row) || (type !== 1 && type !== 2 && type !== 3)
      || typeof row.name !== "string" || !row.name.trim()) throw refused();
    const key = `${type}:${row.name}`;
    if (ids.has(row.id) || commands.has(key)) throw refused();
    ids.add(row.id);
    commands.add(key);
    return row;
  });
}

async function fingerprint(rows: RegistryRow[]): Promise<string> {
  const sorted = [...rows].sort((a, b) => a.id < b.id ? -1 : a.id === b.id ? 0 : 1);
  const bytes = new TextEncoder().encode(JSON.stringify(sorted, (_key, value: unknown) => {
    if (typeof value === "number" && !Number.isFinite(value)) throw refused();
    const row = record(value);
    return row ? Object.fromEntries(Object.keys(row).sort().map(key => [key, row[key]])) : value;
  }));
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return Array.from(hash, byte => byte.toString(16).padStart(2, "0")).join("");
}

function emptyLocalizations(value: unknown): boolean {
  const map = record(value);
  return value == null || (map !== undefined && Object.keys(map).length === 0);
}

function preview(row: JsonRecord | undefined): row is RegistryRow {
  // Global-only contexts, integration types and DM permissions do not apply here.
  return identity(row) && row.name === definition.name && row.type === definition.type
    && row.description === definition.description && row.default_member_permissions === definition.default_member_permissions
    && (row.default_permission === undefined || row.default_permission === true)
    && (row.nsfw === undefined || row.nsfw === false) && row.handler == null
    && emptyLocalizations(row.name_localizations) && emptyLocalizations(row.description_localizations)
    && Array.isArray(row.options) && row.options.length === definition.options.length
    && definition.options.every((expected, index) => {
      const option = record((row.options as unknown[])[index]);
      return !!option && option.name === expected.name && option.description === expected.description
        && option.type === expected.type && (option.required === undefined ? false : option.required) === expected.required
        && (option.autocomplete === undefined || option.autocomplete === false)
        && ["min_length", "max_length", "min_value", "max_value"].every(key =>
          option[key] === expected[key as keyof typeof expected])
        && ["choices", "options", "channel_types"].every(key =>
          option[key] === undefined || (Array.isArray(option[key]) && option[key].length === 0))
        && emptyLocalizations(option.name_localizations) && emptyLocalizations(option.description_localizations);
    });
}

function matching(rows: RegistryRow[], owned: StagingAutomodOwnedCommand): RegistryRow[] {
  const candidates = rows.filter(row => row.name === target.name || row.id === owned.id);
  if (candidates.length !== 1 || !sameCommand(candidates[0], owned)) throw refused();
  return rows.filter(row => row.id !== owned.id);
}

function sameCommand(row: JsonRecord | undefined, owned: StagingAutomodOwnedCommand): boolean {
  return preview(row) && row.id === owned.id && row.version === owned.version;
}

function validLease(lease: Readonly<StagingUse> | undefined): lease is Readonly<StagingUse> {
  return !!lease && lease.phase === "active" && !("voiceMove" in lease)
    && !!parseStagingUseChange({ ...lease, action: "stop", expectedEpoch: lease.epoch })
    && Number.isSafeInteger(lease.acquiredAt) && lease.acquiredAt >= 0
    && lease.expiresAt > lease.acquiredAt && lease.expiresAt - lease.acquiredAt <= MAX_STAGING_USE_MS;
}

function sameLease(a: Readonly<StagingUse>, b: Readonly<StagingUse> | undefined): boolean {
  return validLease(b) && a.deploymentId === b.deploymentId && a.sha === b.sha && a.epoch === b.epoch
    && a.runId === b.runId && a.runAttempt === b.runAttempt
    && a.acquiredAt === b.acquiredAt && a.expiresAt === b.expiresAt;
}

function validCutoff(lease: Readonly<StagingUse>, cutoff: number): boolean {
  return Number.isSafeInteger(cutoff) && lease.acquiredAt < cutoff && cutoff < lease.expiresAt;
}

function validOwned(owned: StagingAutomodOwnedCommand | undefined): owned is StagingAutomodOwnedCommand {
  const intent = owned?.intent;
  return !!owned && !!intent && snowflake(owned.id) && snowflake(owned.version)
    && validLease(intent.lease) && validCutoff(intent.lease, intent.publishBefore)
    && intent.target?.applicationId === target.applicationId && intent.target?.guildId === target.guildId
    && intent.target?.name === target.name && /^[0-9a-f]{64}$/.test(intent.baseline);
}

function transport(deps: DiscordTransport, cutoff: number, now: () => number) {
  const token = deps.env.DISCORD_TOKEN;
  const left = cutoff - now();
  if (deps.env.TWO_DEPLOY_ENV !== "staging" || deps.env.DISCORD_APPLICATION_ID !== target.applicationId
    || deps.env.GUILD_ID !== target.guildId || !token?.trim() || !Number.isSafeInteger(left) || left <= 0) throw refused();
  const signal = AbortSignal.timeout(Math.min(left, 8_000));
  const check = () => {
    if (signal.aborted || !(now() < cutoff)) throw refused();
  };
  const settle = async <T>(work: Promise<T>): Promise<T> => {
    const result = await boundRead(work, signal);
    check();
    return result;
  };
  const json = async (method: "GET" | "POST" | "DELETE", url: string, status: number): Promise<unknown> => {
    check();
    const response = await settle(deps.request(url, {
      method, redirect: "error", cache: "no-store", signal,
      headers: { authorization: `Bot ${token}`, ...(method === "POST" ? { "content-type": "application/json" } : {}) },
      ...(method === "POST" ? { body: JSON.stringify(definition) } : {}),
    }).then(response => {
      if (signal.aborted || !(now() < cutoff)) releaseUpstream(response);
      return response;
    }));
    if (response.status !== status || response.redirected || response.headers.has("location")) {
      releaseUpstream(response);
      throw refused();
    }
    if (status === 204) return;
    const bytes = await settle(readReceiverBytes(response, 256 * 1024, signal));
    if (!bytes) throw refused();
    return parseRegistryJson(new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes));
  };
  return {
    check, settle, json,
    list: async () => registry(await json("GET", `${commands}?with_localizations=true`, 200)),
    bot: async () => {
      const bot = record(await json("GET", `${api}/users/@me`, 200));
      if (bot?.id !== target.applicationId || bot.bot !== true) throw refused();
    },
  };
}

/** Discord POST is an upsert. The ownership gate must exclude other writers
 * throughout this operation. Uncertain results retain intent for recovery. */
export async function publishStagingAutomod(admission: StagingAutomodAdmission | undefined, deps: Publication,
  now = Date.now): Promise<StagingAutomodOwnedCommand> {
  let intentAttempted = false;
  try {
    if (admission?.baselineAbsent !== true || !validLease(admission.lease)) throw refused();
    const { publishBefore } = admission, lease = Object.freeze({ ...admission.lease });
    if (!validCutoff(lease, publishBefore) || !(now() >= lease.acquiredAt)) throw refused();
    const discord = transport(deps, publishBefore, now);
    const requireCurrent = async () => {
      const current = await discord.settle(deps.currentAdmission());
      if (current?.baselineAbsent !== true || current.publishBefore !== publishBefore
        || !sameLease(lease, current.lease)) throw refused();
    };
    await requireCurrent();
    await discord.bot();
    const absent = async () => {
      const rows = await discord.list();
      if (rows.some(row => row.name === target.name)
        || rows.filter(row => row.type === 1 || !Object.hasOwn(row, "type")).length >= 100) throw refused();
      return rows;
    };
    const before = await absent();
    const baseline = await discord.settle(fingerprint(before));
    const intent = Object.freeze({ lease, publishBefore, target, baseline });
    await requireCurrent();
    intentAttempted = true;
    // Hold the gate through each durable commit, even past the Discord cutoff.
    if (await deps.persistIntent(intent) !== true) throw refused();
    discord.check();
    if (await discord.settle(fingerprint(await absent())) !== baseline) throw refused();
    await requireCurrent();
    const created = record(await discord.json("POST", commands, 201));
    if (!preview(created) || before.some(row => row.id === created.id)) throw refused();
    const owned = Object.freeze({ intent, id: created.id, version: created.version });
    if (await deps.recordCreated(owned) !== true) throw refused();
    discord.check();
    if (await discord.settle(fingerprint(matching(await discord.list(), owned))) !== baseline
      || !sameCommand(record(await discord.json("GET", `${commands}/${owned.id}`, 200)), owned)) throw refused();
    await requireCurrent();
    return owned;
  } catch {
    throw intentAttempted ? uncertain() : refused();
  }
}

/** Independent scoped read after the protected caller has fenced, drained and
 * stopped the window. A lost receipt can prove absence against the baseline. */
export async function inspectStagingAutomod(intent: StagingAutomodPublicationIntent,
  command: CommandRevision | null, deps: DiscordTransport, readBefore: number,
  now = Date.now): Promise<"absent" | "owned"> {
  const owned = command && { intent, ...command };
  if (owned && !validOwned(owned)) throw refused();
  const discord = transport(deps, readBefore, now);
  await discord.bot();
  const rows = await discord.list();
  if (rows.some(row => row.name === target.name || row.id === owned?.id)) {
    if (!owned) throw refused();
    matching(rows, owned);
    if (!sameCommand(record(await discord.json("GET", `${commands}/${owned.id}`, 200)), owned)) throw refused();
    return "owned";
  }
  if (owned) {
    if (record(await discord.json("GET", `${commands}/${owned.id}`, 404))?.code !== 10063) throw refused();
  } else if (await discord.settle(fingerprint(rows)) !== intent.baseline) throw refused();
  return "absent";
}

/** Delete the exact durable owned record. Recovery owns state and affirmative stop. */
export async function removeOwnedStagingAutomod(grant: StagingAutomodCleanupGrant | undefined, deps: Cleanup,
  now = Date.now): Promise<string> {
  let deleteAttempted = false;
  try {
    if (!grant || !validOwned(grant.owned)) throw refused();
    const { owned: candidate, removeBefore } = grant;
    const owned = Object.freeze({ id: candidate.id, version: candidate.version, intent: Object.freeze({
      ...candidate.intent, target, lease: Object.freeze({ ...candidate.intent.lease }),
    }) });
    const discord = transport(deps, removeBefore, now);
    const requireCurrent = async () => {
      const current = await discord.settle(deps.currentCleanupGrant());
      if (current?.removeBefore !== removeBefore || !validOwned(current?.owned)
        || current.owned.id !== owned.id || current.owned.version !== owned.version
        || current.owned.intent.publishBefore !== owned.intent.publishBefore
        || current.owned.intent.baseline !== owned.intent.baseline
        || !sameLease(owned.intent.lease, current.owned.intent.lease)) throw refused();
    };
    await requireCurrent();
    await discord.bot();
    const before = await discord.settle(fingerprint(matching(await discord.list(), owned)));
    const url = `${commands}/${owned.id}`;
    if (!sameCommand(record(await discord.json("GET", url, 200)), owned)) throw refused();
    await requireCurrent();
    deleteAttempted = true;
    await discord.json("DELETE", url, 204);
    const after = await discord.settle(fingerprint(await discord.list()));
    if (after !== before || record(await discord.json("GET", url, 404))?.code !== 10063) throw refused();
    await requireCurrent();
    return after;
  } catch {
    throw deleteAttempted ? uncertain() : refused();
  }
}
