import { authenticated } from "./ownership.ts";
import { withSecurityHeaders } from "./security-headers.ts";

export const PREVIEW_HEADER = "x-two-staging-preview";
// Match the pinned staging application and guild in the bot command registry.
const APP = "1469137636663758888";
const GUILD = "1545644954272137297";
const API = "https://discord.com/api/v10";
const COMMANDS = `${API}/applications/${APP}/guilds/${GUILD}/commands`;
const NAME = "automod-preview";
const OPTIONS = [
  { name: "text", type: 3, required: true, min_length: 1, max_length: 2000 },
  { name: "mentions", type: 4, required: false, min_value: 0, max_value: 100 },
  { name: "attachments", type: 3, required: false, max_length: 1000 },
] satisfies Record<string, unknown>[];
const BOUNDS = ["min_length", "max_length", "min_value", "max_value"];
const EMPTY = ["choices", "options", "channel_types"];

export async function previewRefusal(request: Request, url: URL, token?: string): Promise<Response | null> {
  const reply = (status: number, extra = {}) =>
    new Response(null, { status, headers: withSecurityHeaders({ "cache-control": "no-store", ...extra }) });
  if (request.method !== "GET") return reply(405, { allow: "GET" });
  if (!await authenticated(request, token)) return reply(401);
  return url.search || request.body ? reply(400) : null;
}

function record(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
}

function snowflake(value: unknown): value is string {
  return typeof value === "string" && /^[1-9][0-9]{16,19}$/.test(value);
}

function identity(row: Record<string, unknown> | undefined): boolean {
  return !!row && row.name === NAME && row.type === 1 && row.application_id === APP
    && (!Object.hasOwn(row, "guild_id") || row.guild_id === GUILD) && snowflake(row.version);
}

export async function readPreview(env: { GUILD_ID?: string; DISCORD_APPLICATION_ID?: string; DISCORD_TOKEN?: string },
  get: (url: string, token: string) => Promise<unknown>): Promise<void> {
  const token = env.DISCORD_TOKEN;
  const refuse = () => new Error("staging_preview_registry_mismatch");
  if (env.GUILD_ID !== GUILD || env.DISCORD_APPLICATION_ID !== APP || !token?.trim()) throw refuse();
  const bot = record(await get(`${API}/users/@me`, token));
  if (bot?.id !== APP || bot.bot !== true) throw refuse();
  const list = await get(COMMANDS, token);
  if (!Array.isArray(list)) throw refuse();
  const matches = list.map(record).filter((entry) => entry?.name === NAME);
  const entry = matches[0];
  if (matches.length !== 1 || !entry || !identity(entry) || !snowflake(entry.id)
    || list.filter((row) => record(row)?.id === entry.id).length !== 1) throw refuse();
  const detail = record(await get(`${COMMANDS}/${entry.id}`, token));
  if (!detail || !identity(detail) || detail.id !== entry.id || detail.version !== entry.version
    || detail.default_member_permissions !== "32" || !Array.isArray(detail.options)
    || detail.options.length !== OPTIONS.length) throw refuse();
  for (const [i, raw] of detail.options.entries()) {
    const actual = record(raw);
    const expected: Record<string, unknown> = OPTIONS[i]!;
    if (!actual || actual.name !== expected.name || actual.type !== expected.type
      || (Object.hasOwn(actual, "required") ? actual.required : false) !== expected.required
      || (Object.hasOwn(actual, "autocomplete") && actual.autocomplete !== false)
      || BOUNDS.some((key) => (actual[key] ?? undefined) !== expected[key])
      || EMPTY.some((key) => actual[key] != null && !(Array.isArray(actual[key]) && actual[key].length === 0))) {
      throw refuse();
    }
  }
}
