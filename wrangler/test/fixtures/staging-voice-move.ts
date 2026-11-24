import { newRequestId } from "../../src/request-id.ts";

export function mockVoiceMoveWindow() {
  const fixture = {
    environment: "staging",
    guildId: "100",
    discordId: "200",
    channelIds: ["300", "400"],
  };
  const [source, destination] = fixture.channelIds;
  const session = { discordId: fixture.discordId };
  const acquiredAt = 1_800_000_000_000;
  const now = () => acquiredAt + 1000;
  const lease = {
    runId: "12345",
    runAttempt: 1,
    expiresAt: acquiredAt + 15 * 60_000,
    sha: "a".repeat(40),
    deploymentId: "synthetic-deployment",
    epoch: 2,
  };
  let connectedChannelId: string | null = source;
  let released = false;
  const signed = [];
  const moves = [];
  const calls = [];
  const canConnect = (channelId: string) => channelId === destination;
  const projection = (action: string) => ({
    schema: "two-bot-staging-use.v1",
    action,
    environment: "staging",
    sha: lease.sha,
    deploymentId: lease.deploymentId,
    epoch: lease.epoch,
    voiceMoveEnabled: !released,
    running: !released,
    use: { ...lease, acquiredAt, environment: "staging", phase: released ? "released" : "active" },
  });
  return {
    signed, moves, calls,
    args: {
      enabled: true,
      fixture,
      lease,
      ready: projection("ready"),
      now,
      async control({ action }: { action: string }) {
        calls.push(action);
        if (action === "release") released = true;
        return projection(action);
      },
      async observe({ checkpoint, channelId, signal }) {
        signal.throwIfAborted();
        // The fixture starts with the account already in the denied source.
        // Only the test driver simulates its departure after the denial case.
        if (checkpoint === "before:not_connected") connectedChannelId = null;
        return {
          guildId: fixture.guildId, discordId: fixture.discordId,
          channelId, connectedChannelId, canConnect: canConnect(channelId),
        };
      },
      caller: {
        environment: fixture.environment,
        discordId: session.discordId,
        async move({ channelId, signal }) {
          signal.throwIfAborted();
          if (released || !fixture.channelIds.includes(channelId)) throw new Error("mock caller refused");
          // Model the Web signing boundary without credentials or transport.
          signed.push({ action: "voice.move_member", discord_id: session.discordId, channel_id: channelId });
          const request_id = newRequestId(now(), new Uint8Array(16).fill(signed.length));
          let result;
          if (connectedChannelId === null) {
            result = { outcome: "not_connected" };
          } else if (!canConnect(channelId)) {
            result = { outcome: "not_permitted" };
          } else {
            result = { outcome: "moved", from_channel_id: connectedChannelId };
            connectedChannelId = channelId;
            moves.push({ discordId: session.discordId, channelId });
          }
          return { status: 200, body: { ok: true, request_id, result } };
        },
      },
    },
  };
}
