// Closed v1 reply from crates/core/src/realtime_wire/snapshot.rs, as consumed by
// Web's live snapshot loader. Web also checks the observation lease and cursor coverage.
import { isJsonObject } from "./internal-actions.ts";

type Check = (value: unknown) => boolean;

export function isLiveId(value: unknown): value is string {
  return typeof value === "string" && /^[1-9]\d{0,19}$/.test(value) &&
    (value.length < 20 || value <= "18446744073709551615");
}

function utcTime(value: unknown): value is string {
  if (typeof value !== "string" || !/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d{1,9})?Z$/.test(value)) return false;
  const ms = Date.parse(value);
  return Number.isFinite(ms) && new Date(ms).toISOString().slice(0, 19) === value.slice(0, 19);
}

function fields(value: unknown, schema: Record<string, Check>): value is Record<string, unknown> {
  return isJsonObject(value) && Object.keys(value).length === Object.keys(schema).length &&
    Object.entries(schema).every(([key, check]) => Object.hasOwn(value, key) && check(value[key]));
}

const nullable = (check: Check): Check => (value) => value === null || check(value);
const oneOf = (...values: string[]): Check => (value) => typeof value === "string" && values.includes(value);
const text = (min: number, max: number): Check => (value) =>
  typeof value === "string" && value.length >= min && value.length <= max;
const sequence: Check = (value) => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
const boolean: Check = (value) => typeof value === "boolean";
const permission: Check = (value) => typeof value === "string" && /^(0|[1-9]\d{0,39})$/.test(value);
const distinct = (values: unknown[]) => new Set(values).size === values.length;

function rows(value: unknown, limit: number, schema: Record<string, Check>, key: string):
  value is Record<string, unknown>[] {
  return Array.isArray(value) && value.length <= limit &&
    value.every((row) => fields(row, schema)) && distinct(value.map((row) => row[key]));
}

function overwrites(value: unknown): boolean {
  return Array.isArray(value) && value.length <= 100 &&
    value.every((row) => fields(row, overwriteFields)) &&
    distinct(value.map((row) => `${row.kind}:${row.id}`));
}

const overwriteFields = { kind: oneOf("role", "member"), id: isLiveId, allow: permission, deny: permission };
const guildFields = { ownerId: isLiveId, afkChannelId: nullable(isLiveId) };
const channelFields = {
  id: isLiveId, name: text(1, 100), kind: oneOf("voice", "stage", "category", "other"),
  parentId: nullable(isLiveId), position: nullable(sequence), overwrites,
};
const roleFields = { id: isLiveId, permissions: permission };
const memberFields = {
  userId: isLiveId, nick: nullable(text(1, 100)), bot: boolean, pending: boolean,
  roleIds: (value: unknown) => Array.isArray(value) && value.length <= 250 && value.every(isLiveId) && distinct(value),
};
const voiceFields = { userId: isLiveId, channelId: isLiveId };
const eventFields = {
  id: isLiveId, channelId: nullable(isLiveId), status: oneOf("scheduled", "active"),
  title: text(1, 100), description: nullable(text(0, 1000)), location: nullable(text(1, 100)),
  startsAt: utcTime, endsAt: nullable(utcTime),
};

export function isSnapshot(value: unknown, guildId: string): boolean {
  if (!fields(value, {
    v: (version) => version === 1, guild_id: (id) => id === guildId, seq: sequence,
    observed_at: utcTime, topics: isJsonObject,
  })) return false;
  const topics = value.topics;
  if (!isJsonObject(topics) || Object.keys(topics).length !== 6) return false;
  const { guild, channels, roles, members, voice, events } = topics;
  if (
    !fields(guild, guildFields) || !rows(channels, 500, channelFields, "id") || !rows(roles, 250, roleFields, "id") ||
    !rows(members, 10_000, memberFields, "userId") || !rows(voice, 10_000, voiceFields, "userId") ||
    !rows(events, 100, eventFields, "id")
  ) return false;

  const kinds = new Map(channels.map(({ id, kind }) => [id, kind]));
  const roleIds = new Set(roles.map(({ id }) => id));
  const users = new Set(members.map(({ userId }) => userId));
  const isVoiceRoom = (id: unknown) => kinds.get(id) === "voice" || kinds.get(id) === "stage";
  return roleIds.has(guildId) && users.has(guild.ownerId) &&
    (guild.afkChannelId === null || kinds.get(guild.afkChannelId) === "voice") &&
    channels.every(({ kind, parentId, overwrites }) =>
      (parentId === null || (kind !== "category" && kinds.get(parentId) === "category")) &&
      (overwrites as Record<string, unknown>[]).every(({ kind, id }) => kind !== "role" || roleIds.has(id))) &&
    members.every(({ roleIds: ids }) => (ids as string[]).every((id) => roleIds.has(id))) &&
    voice.every(({ userId, channelId }) => users.has(userId) && isVoiceRoom(channelId)) &&
    events.every(({ channelId, startsAt, endsAt }) =>
      (channelId === null || isVoiceRoom(channelId)) &&
      (endsAt === null || Date.parse(endsAt as string) > Date.parse(startsAt as string)));
}
