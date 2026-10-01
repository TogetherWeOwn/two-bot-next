// Only the workerd superclass boundary is substituted. Container startup,
// option precedence, port checks and proxying use @cloudflare/containers itself.
export class DurableObject {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
  }
}

export class WorkerEntrypoint extends DurableObject {}

// The SDK pipes nonempty HTTP bodies through workerd's identity transform.
// Node's standard TransformStream has the same no-transform behavior.
globalThis.IdentityTransformStream = TransformStream;
