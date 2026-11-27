import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, type TestContext } from "node:test";
import { fileURLToPath } from "node:url";
import { pipelineUse } from "../scripts/staging-use-pipeline.mjs";
import {
  bindStagingVoiceMoveHandoffProvider,
  bindStagingVoiceMoveProvider,
  runStagingVoiceMoveEntry,
  runStagingVoiceMoveRehearsal,
} from "../scripts/staging-voice-move-entry.mjs";
import { runStagingVoiceMoveJob } from "../scripts/staging-voice-move-job.mjs";
import { readStagingVoiceWebReady } from "../scripts/staging-voice-web-handoff.mjs";
import { mockVoiceMoveWindow } from "./fixtures/staging-voice-move.ts";

function beginFixture(ready: ReturnType<typeof mockVoiceMoveWindow>["args"]["ready"], dir: string) {
  const { use, deploymentId, sha } = ready;
  const path = join(dir, "start.jsonl");
  const output = join(dir, "output");
  const run = { GITHUB_RUN_ID: use.runId, GITHUB_RUN_ATTEMPT: String(use.runAttempt) };
  // Replace fetch before loading the real CLI. Only synthetic responses exist
  // in this child. Begin writes the actual receipt and GitHub output files.
  const transport = `
    import assert from "node:assert/strict";
    const use = ${JSON.stringify(use)};
    const { acquiredAt, deploymentId, sha, epoch, runId, runAttempt, expiresAt } = use;
    Date.now = () => acquiredAt;
    let active = false;
    globalThis.fetch = async (url, init) => {
      if (url.pathname === "/readyz") {
        assert.equal(init.method, "GET");
        assert.equal(active, true);
        return Response.json({ build_revision: sha, components: [["gateway", "ready"]] },
          { headers: { "x-two-worker-version": deploymentId } });
      }
      assert.equal(url.pathname, "/internal/staging-use");
      if (init.method === "POST") {
        assert.equal(active, false);
        assert.deepEqual(JSON.parse(init.body), { action: "acquire", environment: "staging",
          deploymentId, sha, expectedEpoch: epoch, runId, runAttempt, expiresAt, voiceMove: true });
        active = true;
      } else {
        assert.equal(init.method, "GET");
      }
      return Response.json({ deploymentId, sha, owner: { deploymentId, epoch, phase: "active" },
        running: active, use: active ? use : null, voiceMoveEnabled: active });
    };
  `;
  const cli = fileURLToPath(new URL("../scripts/staging-use-pipeline.mjs", import.meta.url));
  const result = spawnSync(process.execPath, ["--import", "data:text/javascript," + encodeURIComponent(transport), cli, "begin"],
    { env: { ...run, GITHUB_OUTPUT: output, STAGING_USE_RECEIPT: path, STAGING_USE_VOICE_MOVE: "1",
      STAGING_USE_MINUTES: "15", STAGING_USE_EXPECTED_SHA: sha,
      OWNERSHIP_CONTROL_TOKEN: "synthetic".repeat(4),
      STAGING_WORKER_URL: "https://two-bot-next-staging.example.workers.dev/" }, encoding: "utf8", timeout: 5000 });
  assert.equal(result.status, 0, result.stderr);
  const outputs = Object.fromEntries(readFileSync(output, "utf8").trimEnd().split("\n").map((line) => line.split("=")));
  assert.deepEqual(outputs, { expires_at: String(use.expiresAt), deployment_id: deploymentId, sha });
  assert.deepEqual(JSON.parse(readFileSync(path, "utf8").trimEnd().split("\n").at(-1)!), {
    ...ready, runId: use.runId, runAttempt: use.runAttempt, observedAt: new Date(use.acquiredAt).toISOString(),
  });
  return { ...run, STAGING_USE_RECEIPT: path, STAGING_USE_EXPIRES_AT: outputs.expires_at,
    STAGING_USE_SHA: outputs.sha, STAGING_USE_DEPLOYMENT: outputs.deployment_id };
}

function jobInputs(
  t: TestContext,
  windowOrPrepareReady: ReturnType<typeof mockVoiceMoveWindow> | typeof beginFixture = mockVoiceMoveWindow(),
  prepareReady = beginFixture,
) {
  const dir = mkdtempSync(join(tmpdir(), "staging-voice-move-job-"));
  t.after(() => rmSync(dir, { recursive: true }));
  const window = typeof windowOrPrepareReady === "function" ? mockVoiceMoveWindow() : windowOrPrepareReady;
  const readyFactory = typeof windowOrPrepareReady === "function" ? windowOrPrepareReady : prepareReady;
  const { args, calls, signed, moves, restoreSource } = window;
  const { ready, fixture, caller, evidence, now } = args;
  const env = { ...readyFactory(ready, dir), STAGING_VOICE_MOVE_PROOF: "true" };
  const expectedRevision = { version_id: "11111111-2222-3333-4444-555555555555", commit: "b".repeat(40) };
  const reads: string[] = [];
  const inputs = {
    fixture, expectedRevision, now, control: args.control,
    previousReady: evidence.previousReady, previousOff: evidence.previousOff,
    webSession: {
      ...caller,
      origin: "https://next.togetherweown.com",
      async request(url: string, init: RequestInit) {
        init.signal?.throwIfAborted();
        reads.push("session");
        return Object.defineProperty(new Response(JSON.stringify({
          matched: true, revision: expectedRevision, observed_at: evidence.session.observedAt,
        }), { headers: { "content-type": "application/json", "cache-control": "private, no-store" } }),
        "url", { value: url });
      },
    },
    observer: {
      async readChannels({ signal }: { signal: AbortSignal }) {
        signal.throwIfAborted();
        reads.push("channels");
        return evidence.readback;
      },
      async observe(request: Parameters<typeof args.observe>[0]) {
        const reading = await args.observe(request);
        return { ...reading, observedAt: new Date(now()).toISOString(), channelType: 2 };
      },
    },
  };
  function assertOff(off: Awaited<ReturnType<typeof args.control>>) {
    assert.equal(off.action, "status");
    assert.deepEqual(off.use, { ...ready.use, phase: "released" });
    assert.equal(off.sha, ready.sha);
    assert.equal(off.deploymentId, ready.deploymentId);
    assert.equal(off.epoch, ready.epoch);
    assert.equal(off.voiceMoveEnabled, false);
    assert.equal(off.running, false);
  }
  async function finish(control = args.control) {
    // The workflow contract pins this as an always() step with the original
    // expiry from begin, independent of the proof's exit status.
    const off = await pipelineUse({
      action: "finish", voiceMove: true, runId: env.GITHUB_RUN_ID,
      runAttempt: Number(env.GITHUB_RUN_ATTEMPT), expiresAt: Number(env.STAGING_USE_EXPIRES_AT),
    }, control, now);
    assertOff(off);
    return off;
  }
  return { env, inputs, signed, moves, calls, reads, restoreSource, assertOff, finish };
}

test("job defaults off before reading proof inputs", async (t) => {
  const { env, inputs, signed, calls, reads, finish } = jobInputs(t);
  for (const selected of [undefined, "false", "TRUE", true]) {
    assert.equal(await runStagingVoiceMoveJob({ ...env, STAGING_VOICE_MOVE_PROOF: selected,
      STAGING_USE_RECEIPT: undefined }, inputs), "disabled");
  }
  assert.deepEqual({ signed, calls, reads }, { signed: [], calls: [], reads: [] });
  await finish();
  assert.deepEqual(calls, ["status", "release", "status"]);
});

test("selected job refuses incomplete or mismatched inputs before any self-move", async (t) => {
  const { env, inputs, signed, calls, reads, finish } = jobInputs(t);
  for (const change of [
    { GITHUB_RUN_ID: "54321" },
    { GITHUB_RUN_ATTEMPT: "01" },
    { STAGING_USE_EXPIRES_AT: "" },
    { STAGING_USE_EXPIRES_AT: String(Number(env.STAGING_USE_EXPIRES_AT) + 1) },
    { STAGING_USE_SHA: "c".repeat(40) },
    { STAGING_USE_DEPLOYMENT: "other-deployment" },
    { STAGING_USE_RECEIPT: undefined },
  ]) {
    assert.equal(await runStagingVoiceMoveJob({ ...env, ...change }, inputs), "failed");
  }
  for (const change of [
    { previousOff: undefined },
    { previousOff: { ...inputs.previousOff, voiceMoveEnabled: true } },
    { webSession: { ...inputs.webSession, origin: "https://example.invalid" } },
  ]) {
    assert.equal(await runStagingVoiceMoveJob(env, { ...inputs, ...change }), "failed");
  }
  // A ready line followed by a failure or malformed row must not authorize the proof.
  const beginReceipt = readFileSync(env.STAGING_USE_RECEIPT, "utf8");
  for (const invalidRow of ['{"action":"failed"}', "{"]) {
    writeFileSync(env.STAGING_USE_RECEIPT, beginReceipt + invalidRow + "\n");
    assert.equal(await runStagingVoiceMoveJob(env, inputs), "failed", invalidRow);
  }
  assert.deepEqual({ signed, calls, reads }, { signed: [], calls: [], reads: [] });
  await finish();
  assert.deepEqual(calls, ["status", "release", "status"]);
});

test("workflow CLI has no adapter provider even with explicit selection", (t) => {
  const { env } = jobInputs(t);
  const cli = fileURLToPath(new URL("../scripts/staging-voice-move-job.mjs", import.meta.url));
  const result = spawnSync(process.execPath, [cli], { env, encoding: "utf8" });
  assert.equal(result.status, 1);
  assert.equal(result.stdout, "");
  assert.equal(result.stderr, "staging_voice_move_proof_failed\n");
});

test("protected entry CLI stays off by default and refuses selection without a provider", (t) => {
  const { env } = jobInputs(t);
  const cli = fileURLToPath(new URL("../scripts/staging-voice-move-entry.mjs", import.meta.url));
  const disabled = spawnSync(process.execPath, [cli], { env: {}, encoding: "utf8" });
  assert.equal(disabled.status, 0);
  assert.equal(disabled.stdout, "");
  assert.equal(disabled.stderr, "");

  const selected = spawnSync(process.execPath, [cli], { env, encoding: "utf8" });
  assert.equal(selected.status, 1);
  assert.equal(selected.stdout, "");
  assert.equal(selected.stderr, "staging_voice_move_proof_failed\n");
});

test("protected entry skips provider assembly while disabled", async () => {
  let calls = 0;
  for (const selected of [undefined, "false", "TRUE", true]) {
    assert.equal(await runStagingVoiceMoveEntry({ STAGING_VOICE_MOVE_PROOF: selected }, () => {
      calls += 1;
      throw new Error("provider must not run");
    }), "disabled");
  }
  assert.equal(calls, 0);
});

test("protected entry refuses unconfirmed inputs and leaves the original lease to finish", async (t) => {
  const { env, inputs, signed, calls, reads, finish } = jobInputs(t);
  const ready = JSON.parse(readFileSync(env.STAGING_USE_RECEIPT, "utf8").trimEnd().split("\n").at(-1)!);
  assert.equal(await runStagingVoiceMoveEntry(env), "failed");
  assert.equal(await runStagingVoiceMoveEntry(env, async () => {
    throw new Error("private provider error");
  }), "failed");
  assert.equal(await runStagingVoiceMoveEntry(env, () => ({ ...inputs, previousOff: undefined })), "failed");
  for (const mismatch of [
    { STAGING_USE_DEPLOYMENT: "other-deployment" },
    { STAGING_USE_EXPIRES_AT: String(Number(env.STAGING_USE_EXPIRES_AT) + 1) },
  ]) {
    // Even a provider carrying the correct lease cannot replace mismatched job outputs.
    assert.equal(await runStagingVoiceMoveEntry({ ...env, ...mismatch }, () => ({
      ...inputs, ready, window: ready.use,
    })), "failed");
  }
  assert.deepEqual({ signed, calls, reads }, { signed: [], calls: [], reads: [] });
  await finish();
  assert.deepEqual(calls, ["status", "release", "status"]);
});

test("selected synthetic entry finishes the exact lease after either proof outcome", async (t) => {
  for (const fail of [false, true]) {
    const { env, inputs, signed, moves, calls, reads, finish } = jobInputs(t);
    if (fail) {
      const observe = inputs.observer.observe;
      inputs.observer.observe = async (request) => {
        if (request.checkpoint === "after:moved") throw new Error("private observer error");
        return observe(request);
      };
    }
    let providerCalls = 0;
    try {
      assert.equal(await runStagingVoiceMoveEntry(env, () => {
        providerCalls += 1;
        return inputs;
      }), fail ? "failed" : "passed");
      assert.equal(providerCalls, 1);
      assert.deepEqual(reads, ["session", "channels"]);
      const [source, destination] = inputs.fixture.channelIds;
      assert.deepEqual(signed.map(({ channel_id }) => channel_id), fail ? [destination] : [destination, source, destination]);
      assert.deepEqual(moves, [{ discordId: inputs.fixture.discordId, channelId: destination }]);
      assert.deepEqual(calls, [...Array(fail ? 1 : 3).fill("assert"), "status", "release", "status"]);
    } finally {
      await finish();
    }
    assert.deepEqual(calls.slice(-3), ["status", "release", "status"]);
  }
});

test("protected entry finishes a cancelled pending move and refuses the same attempt", { timeout: 5000 }, async (t) => {
  const { env, inputs, signed, moves, calls, reads, restoreSource, assertOff, finish } = jobInputs(t);
  const { now, ...dependencies } = inputs;
  t.mock.method(Date, "now", now);
  const { webSession, observer, control } = dependencies;
  const { request, move } = webSession;
  const checkpoints: string[] = [];
  const controlReadbacks: Awaited<ReturnType<typeof control>>[] = [];
  const admitted = Promise.withResolvers<Awaited<ReturnType<typeof move>>>();
  const lateReply = Promise.withResolvers<Awaited<ReturnType<typeof move>>>();
  const web = new AbortController();

  webSession.request = function (...args) {
    assert.equal(this, webSession);
    return request.apply(this, args);
  };
  const moveCall = t.mock.method(webSession, "move", async function (this: typeof webSession, args: Parameters<typeof move>[0]) {
    assert.equal(this, webSession);
    const response = await move.call(this, args);
    // Model Web cancelling its own pending reply after the synthetic move took effect.
    const cancelled = new Promise<never>((_, reject) => {
      web.signal.addEventListener("abort", () => reject(web.signal.reason), { once: true });
    });
    admitted.resolve(response);
    return Promise.race([lateReply.promise, cancelled]);
  });
  const provider = bindStagingVoiceMoveProvider({
    ...dependencies,
    observer: { ...observer, observe(check) {
      checkpoints.push(check.checkpoint);
      return observer.observe(check);
    } },
    async control(args: Parameters<typeof control>[0]) {
      if (args.action === "release") {
        assert.partialDeepStrictEqual(args, {
          runId: env.GITHUB_RUN_ID, runAttempt: Number(env.GITHUB_RUN_ATTEMPT),
          expiresAt: Number(env.STAGING_USE_EXPIRES_AT), sha: env.STAGING_USE_SHA,
          deploymentId: env.STAGING_USE_DEPLOYMENT,
        });
      }
      const state = await control(args);
      if (args.action === "assert" || args.action === "status") controlReadbacks.push(state);
      return state;
    },
  });

  const proof = runStagingVoiceMoveEntry(env, provider);
  try {
    const response = await Promise.race([admitted.promise, proof.then((verdict) => {
      assert.fail(`Proof returned ${verdict} before move admission`);
    })]);
    const destination = inputs.fixture.channelIds[1];
    const expectedMoves = [{ discordId: inputs.fixture.discordId, channelId: destination }];
    const finishCalls = ["assert", "status", "release", "status"];
    assert.deepEqual(reads, ["session", "channels"]);
    assert.deepEqual(moves, expectedMoves);
    assert.deepEqual(calls, ["assert"]);

    web.abort();
    assert.equal(await proof, "failed");
    assert.deepEqual(checkpoints, ["before:moved"]);
    assert.deepEqual(calls, finishCalls);
    assertOff(controlReadbacks.at(-1)!);

    lateReply.resolve(response);
    // Isolate the released-window guard from the first move's occupancy change.
    restoreSource();
    assert.equal(await runStagingVoiceMoveEntry(env, provider), "failed");
    assert.deepEqual(reads, ["session", "channels", "session", "channels"]);
    assert.deepEqual(checkpoints, ["before:moved", "before:moved"]);
    assert.deepEqual(calls, [...finishCalls, ...finishCalls]);
    assert.equal(controlReadbacks.findLast(({ action }) => action === "assert")?.use.phase, "released");
    assertOff(controlReadbacks.at(-1)!);
    assert.equal(moveCall.mock.callCount(), 1);
    assert.deepEqual(signed.map(({ channel_id }) => channel_id), [destination]);
    assert.deepEqual(moves, expectedMoves);
    await assert.rejects(pipelineUse({ action: "begin", voiceMove: true, minutes: 15,
      runId: env.GITHUB_RUN_ID, runAttempt: Number(env.GITHUB_RUN_ATTEMPT),
    }, control, now), { code: "staging_use_ended" });
  } finally {
    web.abort();
    await proof;
    await finish();
  }
});

test("bound provider supplies only the protected dependencies to the existing entry", async (t) => {
  const { env, inputs, signed, calls, reads, finish } = jobInputs(t);
  const { now, ...dependencies } = inputs;
  t.mock.method(Date, "now", now);
  const provider = bindStagingVoiceMoveProvider({
    ...dependencies,
    get now() { throw new Error("the job owns the clock"); },
    get ready() { throw new Error("the job owns the ready receipt"); },
    get window() { throw new Error("the job owns the lease"); },
  });

  try {
    assert.deepEqual(provider(), dependencies);
    assert.equal(await runStagingVoiceMoveEntry(env, provider), "passed");
    assert.deepEqual(reads, ["session", "channels"]);
    assert.equal(signed.length, 3);
    assert.deepEqual(calls, ["assert", "assert", "assert", "status", "release", "status"]);
  } finally {
    await finish();
  }
});

test("bound provider skips disabled selection and refuses incomplete dependencies before proof", async (t) => {
  const { env, inputs, signed, moves, calls, reads, finish } = jobInputs(t);
  const { now, ...dependencies } = inputs;
  const clock = t.mock.method(Date, "now", now);
  let fixtureReads = 0;
  const disabledProvider = bindStagingVoiceMoveProvider({
    get fixture() { fixtureReads += 1; return dependencies.fixture; },
  });

  try {
    assert.equal(await runStagingVoiceMoveEntry({}, disabledProvider), "disabled");
    assert.equal(fixtureReads, 0);
    assert.equal(await runStagingVoiceMoveEntry(env, bindStagingVoiceMoveProvider()), "failed");
    for (const key of Object.keys(dependencies)) {
      const provider = bindStagingVoiceMoveProvider({ ...dependencies, [key]: undefined });
      assert.equal(await runStagingVoiceMoveEntry(env, provider), "failed", key);
    }
    assert.equal(clock.mock.callCount(), 0);
    assert.deepEqual({ signed, moves, calls, reads }, { signed: [], moves: [], calls: [], reads: [] });
  } finally {
    await finish();
  }
});

test("rehearsal entry requires the immutable fixture and Web session from this run", async (t) => {
  // Supply the final ready row directly. The local rehearsal has no begin transport.
  const { env, inputs, signed, moves, calls, reads, finish } = jobInputs(t, (ready, dir) => {
    const { runId, runAttempt, expiresAt, sha, deploymentId } = ready.use;
    const path = join(dir, "ready.jsonl");
    writeFileSync(path, JSON.stringify(ready) + "\n");
    return { GITHUB_RUN_ID: runId, GITHUB_RUN_ATTEMPT: String(runAttempt), STAGING_USE_RECEIPT: path,
      STAGING_USE_EXPIRES_AT: String(expiresAt), STAGING_USE_SHA: sha, STAGING_USE_DEPLOYMENT: deploymentId };
  });
  const { now, ...dependencies } = inputs;
  t.mock.method(Date, "now", now);
  const approval = Object.freeze({
    runId: env.GITHUB_RUN_ID,
    runAttempt: env.GITHUB_RUN_ATTEMPT,
    fixture: Object.freeze({ ...inputs.fixture, channelIds: Object.freeze(inputs.fixture.channelIds) }),
    expectedRevision: Object.freeze(inputs.expectedRevision),
    webSession: Object.freeze(inputs.webSession),
  });
  const scoped = { ...dependencies, fixture: approval.fixture };
  const fetch = t.mock.method(globalThis, "fetch", () => { throw new Error("unexpected network request"); });
  let receiptReads = 0;
  const selection = { ...env, get STAGING_USE_RECEIPT() { receiptReads += 1; return env.STAGING_USE_RECEIPT; } };

  try {
    assert.equal(await runStagingVoiceMoveRehearsal(), "disabled");
    const mutableFixture = Object.freeze({ ...approval.fixture, channelIds: [...approval.fixture.channelIds] });
    for (const [label, pin, supplied] of [
      ["empty approval", Object.freeze({}), scoped],
      ["partial approval without the session pin", Object.freeze({ ...approval, webSession: undefined }), scoped],
      ["mutable channel pair", Object.freeze({ ...approval, fixture: mutableFixture }), { ...scoped, fixture: mutableFixture }],
      ["prior run", Object.freeze({ ...approval, runId: "12344" }), scoped],
      ["other attempt", Object.freeze({ ...approval, runAttempt: "2" }), scoped],
      ["different fixture object", approval, { ...scoped, fixture: Object.freeze({ ...scoped.fixture }) }],
      ["different revision object", approval, { ...scoped, expectedRevision: Object.freeze({ ...scoped.expectedRevision }) }],
      ["different session with the same metadata", approval, { ...scoped, webSession: Object.freeze({ ...scoped.webSession }) }],
    ] as const) {
      assert.equal(await runStagingVoiceMoveRehearsal(selection, pin, supplied), "failed", label);
      assert.equal(receiptReads, 0, label);
      assert.deepEqual({ signed, moves, calls, reads }, { signed: [], moves: [], calls: [], reads: [] }, label);
    }
    assert.equal(await runStagingVoiceMoveRehearsal(selection, approval, scoped), "passed");
    assert.equal(receiptReads, 1);
    assert.deepEqual(reads, ["session", "channels"]);
    const [source, destination] = approval.fixture.channelIds;
    assert.deepEqual(signed.map(({ channel_id }) => channel_id), [destination, source, destination]);
    assert.deepEqual(moves, [{ discordId: approval.fixture.discordId, channelId: destination }]);
    assert.deepEqual(calls, ["assert", "assert", "assert", "status", "release", "status"]);
    assert.equal(fetch.mock.callCount(), 0);
  } finally {
    await finish();
  }
});

function mockWebHandoff(inputs: ReturnType<typeof jobInputs>["inputs"]) {
  const { now, ...dependencies } = inputs;
  const { commit, version_id: workerVersion } = dependencies.expectedRevision;
  const sampledAt = now() - 60_000;
  // The Web producer retains this shape. Web Operations issues its separate decision after validation.
  const soakReceipt = {
    schema: "staging-soak/1", status: "sampled", sha: commit, workerVersion,
    deployedAt: new Date(sampledAt - 31 * 60_000).toISOString(), deployRunId: 100, deployAttempt: 1,
    e2eRunId: 200, e2eAttempt: 1, e2eCompletedAt: new Date(sampledAt - 30 * 60_000).toISOString(),
    floorAt: new Date(sampledAt).toISOString(),
    sample: { workerVersion, observedAt: new Date(sampledAt).toISOString(), requests: 50, errors: 0 },
  };
  const qualification = { ...structuredClone(soakReceipt), verdict: "qualified" };
  const match = {
    matched: true, observed_at: new Date(Date.now()).toISOString(), revision: dependencies.expectedRevision,
  };
  const reply = { status: 200, body: match as object };
  let reads = 0;
  const webSession = {
    ...dependencies.webSession,
    async request(url: string, init: RequestInit) {
      init.signal?.throwIfAborted();
      assert.equal(this === webSession, true, "calls use the same scoped adapter");
      assert.equal(url === webSession.origin + "/__probe/voice-move/session", true, "companion endpoint only");
      assert.equal(init.method, "POST");
      assert.equal(init.body === JSON.stringify({
        discord_id: dependencies.fixture.discordId, channel_ids: dependencies.fixture.channelIds,
      }), true, "ordered fixture assertions");
      reads += 1;
      return Object.defineProperty(Response.json(reply.body, {
        status: reply.status, headers: { "cache-control": "private, no-store" },
      }), "url", { value: url });
    },
  };
  const scopedInputs = { ...dependencies, webSession };
  const readReady = (signal: AbortSignal) => readStagingVoiceWebReady(scopedInputs, { signal });
  async function complete(signal: AbortSignal) {
    const ready = await readReady(signal);
    reply.status = 404;
    reply.body = { error: "not_found", request_id: "0".repeat(26) };
    return ready.confirmDisabled({ signal });
  }
  function enableCompanion() {
    reply.status = 200;
    reply.body = { ...match, observed_at: new Date(Date.now()).toISOString() };
  }
  return { scopedInputs, qualification, soakReceipt, reply, readReady, complete, enableCompanion, readCount: () => reads };
}

test("qualified Web handoff reaches the existing mock proof with a fresh preflight", async (t) => {
  const { env, inputs, signed, calls, finish } = jobInputs(t);
  // Earlier readbacks can be followed by a fresh qualification of the restored Worker.
  let now = inputs.now() - 35 * 60_000;
  t.mock.method(Date, "now", () => now);
  const web = mockWebHandoff(inputs);
  try {
    const webHandoff = await web.complete(t.signal);
    assert.equal(web.readCount(), 2);
    assert.equal(signed.length, 0);
    const provider = bindStagingVoiceMoveHandoffProvider(web.scopedInputs, {
      qualification: web.qualification, soakReceipt: web.soakReceipt, webHandoff,
    });
    assert.equal(await runStagingVoiceMoveEntry({}, provider), "disabled");
    assert.equal(web.readCount(), 2);
    now = inputs.now();
    web.enableCompanion();
    assert.equal(await runStagingVoiceMoveEntry(env, provider), "passed");
    assert.equal(web.readCount(), 3, "the proof reads its own current session");
    assert.equal(signed.length, 3);
    assert.deepEqual(calls, ["assert", "assert", "assert", "status", "release", "status"]);
  } finally {
    await finish();
  }
});

test("completed Web handoff cannot replace the enabled companion at proof preflight", async (t) => {
  const { env, inputs, signed, calls, finish } = jobInputs(t);
  t.mock.method(Date, "now", inputs.now);
  const web = mockWebHandoff(inputs);
  try {
    const webHandoff = await web.complete(t.signal);
    const provider = bindStagingVoiceMoveHandoffProvider(web.scopedInputs, {
      qualification: web.qualification, soakReceipt: web.soakReceipt, webHandoff,
    });
    assert.equal(await runStagingVoiceMoveEntry(env, provider), "failed");
    assert.equal(web.readCount(), 3, "the runner attempted its own current-session read");
    assert.equal(signed.length, 0);
    assert.deepEqual(calls, ["status", "release", "status"]);
  } finally {
    await finish();
  }
});

test("qualified same-session Web handoff refuses a replaced voice fixture assignment", async (t) => {
  for (const replaced of [false, true]) {
    await t.test(replaced ? "replaced destination" : "original assignment", async (t) => {
      const { env, inputs, signed, moves, calls, reads, finish } = jobInputs(t);
      t.mock.method(Date, "now", inputs.now);
      const web = mockWebHandoff(inputs);
      const { fixture, webSession, control } = web.scopedInputs;
      const [source, destination] = fixture.channelIds;
      const move = t.mock.method(webSession, "move");
      const cleanupRequests: Parameters<typeof control>[0][] = [];

      try {
        const webHandoff = await web.complete(t.signal);
        assert.equal(web.readCount(), 2);
        web.enableCompanion();
        // The session, qualified build, and Bot lease are identical. Only the
        // destination in the assignment supplied to Bot Operations changes.
        const provider = bindStagingVoiceMoveHandoffProvider({
          ...web.scopedInputs,
          fixture: replaced ? { ...fixture, channelIds: [source, "999000000000000001"] } : fixture,
        }, { qualification: web.qualification, soakReceipt: web.soakReceipt, webHandoff });

        assert.equal(await runStagingVoiceMoveEntry(env, provider), replaced ? "failed" : "passed");
        assert.equal(web.readCount(), replaced ? 2 : 3);
        assert.deepEqual(reads, replaced ? [] : ["channels"]);
        assert.equal(move.mock.callCount(), replaced ? 0 : 3);
        assert.deepEqual(signed.map(({ channel_id }) => channel_id), replaced ? [] : [destination, source, destination]);
        assert.deepEqual(moves, replaced ? [] : [{ discordId: fixture.discordId, channelId: destination }]);
        assert.deepEqual(calls, replaced ? [] : ["assert", "assert", "assert", "status", "release", "status"]);
      } finally {
        await finish((request: Parameters<typeof control>[0]) => {
          cleanupRequests.push(request);
          return control(request);
        });
      }

      assert.deepEqual(cleanupRequests.map(({ action }) => action), ["status", "release", "status"]);
      assert.partialDeepStrictEqual(cleanupRequests[1], {
        runId: env.GITHUB_RUN_ID, runAttempt: Number(env.GITHUB_RUN_ATTEMPT),
        expiresAt: Number(env.STAGING_USE_EXPIRES_AT), sha: env.STAGING_USE_SHA,
        deploymentId: env.STAGING_USE_DEPLOYMENT,
      });
    });
  }
});

test("an earlier Web handoff requires a fresh companion match in the next reservation", async (t) => {
  for (const refresh of [false, true]) {
    await t.test(refresh ? "new reservation with current match" : "new reservation with earlier match", async (t) => {
      const first = jobInputs(t);
      let now = first.inputs.now();
      t.mock.method(Date, "now", () => now);
      let move = first.inputs.webSession.move;
      const web = mockWebHandoff({
        ...first.inputs,
        webSession: { ...first.inputs.webSession, move: (request) => move(request) },
      });
      const moveCall = t.mock.method(web.scopedInputs.webSession, "move");
      const firstReady = JSON.parse(readFileSync(first.env.STAGING_USE_RECEIPT, "utf8").trimEnd().split("\n").at(-1)!);
      const { qualification, soakReceipt } = web;
      const [source, destination] = first.inputs.fixture.channelIds;
      const moved = [{ discordId: first.inputs.fixture.discordId, channelId: destination }];
      let webHandoff;
      let firstOff;
      try {
        webHandoff = await web.complete(t.signal);
        web.enableCompanion();
        const provider = bindStagingVoiceMoveHandoffProvider(web.scopedInputs, { qualification, soakReceipt, webHandoff });
        assert.equal(await runStagingVoiceMoveEntry(first.env, provider), "passed");
        assert.equal(moveCall.mock.callCount(), 3);
        assert.deepEqual(first.moves, moved);
        assert.equal(web.readCount(), 3);
      } finally {
        firstOff = await first.finish();
      }

      // The old match is only seconds old, but it precedes the next acquisition.
      // Keep the build, ordered fixture, handoff, and request/move functions identical.
      const second = jobInputs(t, mockVoiceMoveWindow(1000, { acquiredAt: now + 1000, runId: "12346" }));
      try {
        now = second.inputs.now();
        move = second.inputs.webSession.move;
        moveCall.mock.resetCalls();
        if (refresh) web.enableCompanion();
        const provider = bindStagingVoiceMoveHandoffProvider({
          ...second.inputs, webSession: web.scopedInputs.webSession, previousReady: firstReady, previousOff: firstOff,
        }, { qualification, soakReceipt, webHandoff });

        assert.equal(await runStagingVoiceMoveEntry(second.env, provider), refresh ? "passed" : "failed");
        assert.equal(web.readCount(), 4, "the second job reads the companion again");
        assert.deepEqual(second.reads, ["channels"]);
        assert.equal(moveCall.mock.callCount(), refresh ? 3 : 0);
        assert.deepEqual(second.signed.map(({ channel_id }) => channel_id), refresh ? [destination, source, destination] : []);
        assert.deepEqual(second.moves, refresh ? moved : []);
        const finishCalls = ["status", "release", "status"];
        assert.deepEqual(second.calls, refresh ? ["assert", "assert", "assert", ...finishCalls] : finishCalls);
      } finally {
        await second.finish();
      }
    });
  }
});

test("Web handoff refuses stale qualification and cross-session or unconfirmed evidence", async (t) => {
  const { env, inputs, signed, calls, finish } = jobInputs(t);
  t.mock.method(Date, "now", inputs.now);
  const web = mockWebHandoff(inputs);
  try {
    const webHandoff = await web.complete(t.signal);
    const { qualification, soakReceipt, scopedInputs } = web;
    const evidence = { qualification, soakReceipt, webHandoff };
    const futureSample = { ...soakReceipt, sample: { ...soakReceipt.sample, observedAt: new Date(inputs.now() + 1).toISOString() } };
    const staleSha = { ...soakReceipt, sha: "d".repeat(40) };
    const differentVersion = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const wrongSampleWorker = { ...soakReceipt, sample: { ...soakReceipt.sample, workerVersion: differentVersion } };
    const wrongWorker = { ...soakReceipt, workerVersion: differentVersion };
    for (const [label, change] of Object.entries({
      "missing soak receipt": { soakReceipt: undefined },
      "soak receipt without a terminal decision": { qualification: soakReceipt },
      "stale Web SHA": { soakReceipt: staleSha, qualification: { ...staleSha, verdict: "qualified" } },
      "different Worker": { soakReceipt: wrongWorker, qualification: { ...wrongWorker, verdict: "qualified" } },
      "different sample Worker": { soakReceipt: wrongSampleWorker, qualification: { ...wrongSampleWorker, verdict: "qualified" } },
      "superseded soak": { soakReceipt: { ...soakReceipt, status: "superseded" } },
      "different E2E run": { soakReceipt: { ...soakReceipt, e2eRunId: soakReceipt.e2eRunId + 1 } },
      "different sample": { soakReceipt: { ...soakReceipt, sample: { ...soakReceipt.sample, requests: 51 } } },
      "unqualified soak": { qualification: { ...qualification, verdict: "pending" } },
      "future soak sample": { soakReceipt: futureSample, qualification: { ...futureSample, verdict: "qualified" } },
    })) {
      const provider = bindStagingVoiceMoveHandoffProvider(scopedInputs, { ...evidence, ...change });
      assert.equal(await runStagingVoiceMoveEntry(env, provider), "failed", label);
    }
    const crossSession = bindStagingVoiceMoveHandoffProvider({ ...scopedInputs, webSession: { ...scopedInputs.webSession } }, evidence);
    assert.equal(await runStagingVoiceMoveEntry(env, crossSession), "failed", "another session with identical metadata");

    web.enableCompanion();
    const readyOnly = await web.readReady(t.signal);
    for (const incomplete of [undefined, readyOnly, { ...webHandoff }]) {
      const provider = bindStagingVoiceMoveHandoffProvider(scopedInputs, { ...evidence, webHandoff: incomplete });
      assert.equal(await runStagingVoiceMoveEntry(env, provider), "failed", "refusal was not confirmed by this handoff");
    }
    assert.equal(web.readCount(), 3);
    assert.equal(signed.length, 0);
    assert.equal(calls.length, 0);
  } finally {
    await finish();
  }
});

test("Web handoff refuses invalid readiness and disabled responses with a fixed error", async (t) => {
  const { inputs, signed, calls, finish } = jobInputs(t);
  t.mock.method(Date, "now", inputs.now);
  const web = mockWebHandoff(inputs);
  const refused = (error: unknown) => error instanceof Error
    && error.message === "staging_voice_web_handoff_unconfirmed" && !("cause" in error);
  try {
    const valid = web.reply.body;
    for (const invalid of [
      { ...valid, observed_at: "2030-02-30T12:00:00.000Z" },
      { ...valid, observed_at: new Date(inputs.now() - 60_001).toISOString() },
      { ...valid, revision: { ...inputs.expectedRevision, commit: "e".repeat(40) } },
      { ...valid, matched: false },
    ]) {
      web.reply.body = invalid;
      await assert.rejects(web.readReady(t.signal), refused);
    }
    web.reply.body = valid;
    const ready = await web.readReady(t.signal);
    // An enabled match, a different refusal, or an identity-bearing extra field cannot complete the handoff.
    await assert.rejects(ready.confirmDisabled({ signal: t.signal }), refused);
    web.reply.status = 404;
    for (const body of [{ error: "forbidden" }, { error: "not_found", member: "synthetic-only" }]) {
      web.reply.body = body;
      await assert.rejects(ready.confirmDisabled({ signal: t.signal }), refused);
    }
    web.reply.body = { error: "not_found" };
    web.scopedInputs.webSession.request = async () => { throw new Error("synthetic replacement transport"); };
    await assert.rejects(ready.confirmDisabled({ signal: t.signal }), refused);
    assert.equal(web.readCount(), 8);
    assert.equal(signed.length, 0);
    assert.equal(calls.length, 0);
  } finally {
    await finish();
  }
});
