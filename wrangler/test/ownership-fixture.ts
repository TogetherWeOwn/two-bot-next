// Local workerd fixture: real SQLite DO storage and concurrency gates, synthetic
// container starts only. No Discord, Docker or database network connections.
import { DurableObject } from "cloudflare:workers";
import { OwnershipFence, OWNER_KEY, AUDIT_PREFIX, OwnershipRefused } from "../src/ownership.ts";

export class OwnershipFixture extends DurableObject {
  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const input = await request.json() as Record<string, any>;
    return this.ctx.blockConcurrencyWhile(async () => {
      try {
        const storage = input.readError ? {
          get: () => { throw new Error("synthetic read failure"); },
        } as unknown as DurableObjectStorage : this.ctx.storage;
        const fence = new OwnershipFence(storage);
        switch (url.pathname) {
          case "/change": {
            const record = await fence.change(input.id, {
              action: input.action ?? "takeover", actor: "fixture-operator", expectedEpoch: input.epoch,
            }, async () => {
              // A failure here must leave a persisted revocation, not an owner.
              if (input.stopError) throw new Error("synthetic stop failure");
              await this.ctx.storage.put("running", null);
            });
            return Response.json(record);
          }
          case "/probe": {
            await fence.require(input.id);
            const running = await this.ctx.storage.get<string>("running");
            if (running && running !== input.id) throw new Error("two concurrent owners");
            const starts = await this.ctx.storage.get<string[]>("starts") ?? [];
            if (!running) starts.push(input.id);
            await this.ctx.storage.put({ running: input.id, starts });
            return Response.json({ running: input.id, starts });
          }
          case "/corrupt":
            await this.ctx.storage.put(OWNER_KEY, { epoch: -1 });
            return Response.json({ corrupted: true });
          case "/state":
            return Response.json({
              owner: await fence.read() ?? null,
              running: await this.ctx.storage.get("running") ?? null,
              starts: await this.ctx.storage.get("starts") ?? [],
              audit: Object.fromEntries(await this.ctx.storage.list({ prefix: AUDIT_PREFIX })),
            });
          default: return new Response(null, { status: 404 });
        }
      } catch (error) {
        return Response.json({ reason: error instanceof OwnershipRefused ? error.reason : "stop_failed" }, { status: 503 });
      }
    });
  }
}

export default {
  fetch(request: Request, env: { FIXTURE: DurableObjectNamespace }) {
    return env.FIXTURE.getByName("singleton").fetch(request);
  },
};
