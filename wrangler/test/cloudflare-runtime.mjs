// Only the workerd superclass boundary is substituted. Container startup,
// option precedence, port checks and proxying use @cloudflare/containers itself.
export class DurableObject {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
  }
}

export class WorkerEntrypoint extends DurableObject {}
