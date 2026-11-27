import { isSnowflake } from "../src/container-env.ts";
import { STAGING_AUTOMOD_TARGET, type CommandRevision } from "../src/staging-automod-recovery.ts";

const target = Object.freeze({ ...STAGING_AUTOMOD_TARGET, type: 1 } as const);
const MAX_GUILD_COMMANDS = 110;
const DISABLED = Object.freeze({ verdict: "disabled" } as const);
const PASS = Object.freeze({ verdict: "pass" } as const);
const FAIL = Object.freeze({ verdict: "fail" } as const);

// Pin the same wire schema as ../src/staging-automod-publication.ts and the preview spec.
const description = "Preview which automod filter fires on text. Smallest synthetic phrase; do not paste personal data.";
type Option = Readonly<{
  name: string;
  description: string;
  type: number;
  required: boolean;
  min_length?: number;
  max_length?: number;
  min_value?: number;
  max_value?: number;
}>;
const options: readonly Option[] = [
  { name: "text", description: "Sample text to evaluate (1-2000 characters)", type: 3,
    required: true, min_length: 1, max_length: 2000 },
  { name: "mentions", description: "Synthetic explicit user-mention count (0-100, default 0)", type: 4,
    required: false, min_value: 0, max_value: 100 },
  { name: "attachments", description: "1-10 bare filenames separated only by comma + ASCII space (e.g. a.txt, b.png); no trimming",
    type: 3, required: false, max_length: 1000 },
];
const bounds = ["min_length", "max_length", "min_value", "max_value"] as const;

export type StagingAutomodCommandParityInput = Readonly<{
  enabled?: boolean;
  // Obtain the revision independently of this registry read, from the pinned publication receipt.
  pin?: CommandRevision;
  // Supply the complete decoded guild list from an authenticated read of this target, with localizations.
  readGuildCommands?: (scope: typeof target) => Promise<unknown>;
}>;
export type StagingAutomodCommandParityResult = typeof DISABLED | typeof PASS | typeof FAIL;

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    && (Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null);
}

function snowflake(value: unknown): value is string {
  return typeof value === "string" && /^[1-9][0-9]{16,19}$/.test(value) && isSnowflake(value);
}

function emptyLocalizations(value: unknown): boolean {
  return value == null || (record(value) && Object.keys(value).length === 0);
}

function matchesOption(value: unknown, expected: Option): boolean {
  return record(value) && value.name === expected.name && value.description === expected.description
    && value.type === expected.type && (value.required === undefined ? false : value.required) === expected.required
    && (value.autocomplete === undefined || value.autocomplete === false)
    && bounds.every(key => value[key] === expected[key])
    && ["choices", "options", "channel_types"].every(key => {
      const list = value[key];
      return list === undefined || (Array.isArray(list) && list.length === 0);
    })
    && emptyLocalizations(value.name_localizations) && emptyLocalizations(value.description_localizations);
}

function matchesCommand(row: Record<string, unknown>, pin: CommandRevision): boolean {
  const readOptions = row.options;
  const installations = row.integration_types;
  // The authenticated guild endpoint and explicit guild ID establish the
  // effective context. Discord applies dm_permission and contexts to global commands.
  return row.guild_id === target.guildId && row.id === pin.id && row.version === pin.version
    && row.name === target.name && row.type === target.type
    && row.description === description && row.default_member_permissions === "32"
    && (row.default_permission === undefined || row.default_permission === true)
    && (installations == null || (Array.isArray(installations) && installations.length === 1 && installations[0] === 0))
    && (row.nsfw === undefined || row.nsfw === false) && row.handler == null
    && emptyLocalizations(row.name_localizations) && emptyLocalizations(row.description_localizations)
    && Array.isArray(readOptions) && readOptions.length === options.length
    && options.every((expected, index) => matchesOption(readOptions[index], expected));
}

function matchesRegistry(value: unknown, pin: CommandRevision): boolean {
  if (!Array.isArray(value) || value.length > MAX_GUILD_COMMANDS) return false;
  const ids = new Set<string>();
  const names = new Set<string>();
  let matched = false;
  for (const row of value) {
    if (!record(row) || row.application_id !== target.applicationId
      || (row.guild_id !== undefined && row.guild_id !== target.guildId)
      || !snowflake(row.id) || !snowflake(row.version) || typeof row.name !== "string"
      || !row.name.trim()) return false;
    // The registry defaults an omitted type to slash for duplicate detection.
    const type = row.type === undefined ? target.type : row.type;
    if (type !== 1 && type !== 2 && type !== 3) return false;
    const key = `${type}:${row.name}`;
    if (ids.has(row.id) || names.has(key)) return false;
    ids.add(row.id);
    names.add(key);
    if (row.name === target.name || row.id === pin.id) {
      if (matched || !matchesCommand(row, pin)) return false;
      matched = true;
    }
  }
  return matched;
}

/** Compare authenticated guild schema and default permission evidence offline. */
export async function verifyStagingAutomodCommandParity(
  input: StagingAutomodCommandParityInput = {},
): Promise<StagingAutomodCommandParityResult> {
  if (input.enabled !== true) return DISABLED;
  try {
    const { pin, readGuildCommands } = input;
    const { id, version } = pin ?? {};
    if (!snowflake(id) || !snowflake(version) || typeof readGuildCommands !== "function") return FAIL;
    return matchesRegistry(await readGuildCommands(target), { id, version }) ? PASS : FAIL;
  } catch {
    return FAIL;
  }
}
