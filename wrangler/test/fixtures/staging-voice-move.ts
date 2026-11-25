import { newRequestId } from "../../src/request-id.ts";
import contract from "../../../crates/core/tests/fixtures/bot-site-contract-v2.json" with { type: "json" };

const wire = contract.verbs["voice.move_member"];

export function mockVoiceMoveWindow(elapsedMs = 1000, scope?: { runId: string; deploymentId: string }) {
  const fixture = {
    environment: "staging",
    guildId: "100",
    discordId: wire.request.discord_id,
    channelIds: [wire.results.moved.from_channel_id, wire.request.channel_id],
  };
  const [source, destination] = fixture.channelIds;
  const session = { discordId: fixture.discordId };
  const acquiredAt = 1_800_000_000_000;
  const now = () => acquiredAt + elapsedMs;
  const lease = {
    runId: "12345",
    runAttempt: 1,
    expiresAt: acquiredAt + 15 * 60_000,
    sha: "a".repeat(40),
    deploymentId: "synthetic-deployment",
    epoch: 2,
    ...scope,
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
    observedAt: new Date(now()).toISOString(),
    voiceMoveEnabled: !released,
    running: !released,
    use: { ...lease, acquiredAt, environment: "staging", phase: released ? "released" : "active" },
  });
  const previousUse = {
    ...projection("ready").use,
    runId: "12344",
    acquiredAt: acquiredAt - 60_000,
    expiresAt: acquiredAt - 30_000,
  };
  const previousReady = {
    ...projection("ready"),
    use: previousUse,
    observedAt: new Date(previousUse.acquiredAt + 1000).toISOString(),
  };
  const previousOff = {
    ...previousReady,
    action: "status",
    use: { ...previousUse, phase: "released" },
    observedAt: new Date(previousUse.expiresAt - 1000).toISOString(),
    running: false,
    voiceMoveEnabled: false,
  };
  const discordReadback = (channelId: string) => ({
    observedAt: new Date(now()).toISOString(),
    guildId: fixture.guildId,
    discordId: fixture.discordId,
    channelId,
    channelType: 2,
    connectedChannelId: source,
    canConnect: canConnect(channelId),
  });
  return {
    signed, moves, calls,
    restoreSource() {
      connectedChannelId = source;
    },
    args: {
      enabled: true,
      fixture,
      lease,
      ready: projection("ready"),
      evidence: {
        session: {
          environment: "staging", authenticated: true, action: "voice.move_member",
          discordId: session.discordId, channelIds: [source, destination], observedAt: new Date(now()).toISOString(),
        },
        readback: { source: discordReadback(source), destination: discordReadback(destination) },
        previousReady,
        previousOff,
      },
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
          } else if (connectedChannelId === channelId) {
            outcome = "already_in_room";
          } else if (!canConnect(channelId)) {
            outcome = "not_permitted";
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
