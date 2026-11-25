type VoiceFixture = {
  environment: string;
  guildId: string;
  discordId: string;
  channelIds: readonly string[];
};

type Observation = {
  guildId: string;
  discordId: string;
  channelId: string;
  channelType: number;
  observedAt: string;
  connectedChannelId: string | null;
  canConnect: boolean;
};

const GUILD_VOICE = 2;
const MAX_AGE_MS = 60_000;

/** Normalize a synthetic read for readChannels or observe from the pinned fixture. */
export function mockVoiceObservation({ fixture, channelId, reading, readableChannelIds, notBefore, now }: {
  fixture: VoiceFixture;
  channelId: string;
  reading: Partial<Observation> | null;
  readableChannelIds: readonly string[];
  notBefore: number;
  now: number;
}): Observation {
  const [source, destination] = fixture.channelIds;
  const observedAt = reading?.observedAt;
  const captured = typeof observedAt === "string" ? Date.parse(observedAt) : NaN;
  if (!reading || fixture.environment !== "staging"
    || reading.guildId !== fixture.guildId || reading.discordId !== fixture.discordId
    || fixture.channelIds.length !== 2 || source === destination
    || !fixture.channelIds.includes(channelId) || reading.channelId !== channelId
    || reading.channelType !== GUILD_VOICE || typeof reading.canConnect !== "boolean"
    || typeof observedAt !== "string" || !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(observedAt)
    || !Number.isFinite(captured) || new Date(captured).toISOString() !== observedAt
    || captured < notBefore || captured > now || now - captured > MAX_AGE_MS) {
    throw new Error("mock_observer_unconfirmed");
  }

  const { connectedChannelId } = reading;
  if (connectedChannelId === undefined) throw new Error("mock_observer_unconfirmed");
  // A disconnected reading needs explicit null and observer access to both rooms.
  // When occupied, access follows the occupied room, which can differ from the target.
  const readable = connectedChannelId === null
    ? fixture.channelIds.every((id) => readableChannelIds.includes(id))
    : fixture.channelIds.includes(connectedChannelId) && readableChannelIds.includes(connectedChannelId);
  if (!readable) throw new Error("mock_observer_unconfirmed");

  return {
    guildId: reading.guildId, discordId: reading.discordId, channelId,
    channelType: GUILD_VOICE, observedAt, connectedChannelId, canConnect: reading.canConnect,
  };
}
