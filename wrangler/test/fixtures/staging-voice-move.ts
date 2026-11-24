import { newRequestId } from "../../src/request-id.ts";
import contract from "../../../crates/core/tests/fixtures/bot-site-contract-v2.json" with { type: "json" };

const wire = contract.verbs["voice.move_member"];

export function mockVoiceMoveWindow() {
  const fixture = {
    environment: "staging",
    guildId: "100",
    discordId: wire.request.discord_id,
    channelIds: [wire.results.moved.from_channel_id, wire.request.channel_id],
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
          signed.push({ ...wire.request, discord_id: session.discordId, channel_id: channelId });
          const request_id = newRequestId(now(), new Uint8Array(16).fill(signed.length));
          let outcome;
          if (connectedChannelId === null) {
            outcome = "not_connected";
          } else if (!canConnect(channelId)) {
            outcome = "not_permitted";
          } else if (connectedChannelId === channelId) {
            outcome = "already_in_room";
          } else {
            outcome = "moved";
            connectedChannelId = channelId;
            moves.push({ discordId: session.discordId, channelId });
          }
          return { status: 200, body: { ok: true, request_id, result: { ...wire.results[outcome] } } };
        },
      },
    },
  };
}
