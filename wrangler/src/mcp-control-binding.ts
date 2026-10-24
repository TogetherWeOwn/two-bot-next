// Request-local control context, not registry authority. Only a server-side
// authorization result may supply grants/step-up when control tools land.
export type McpControlScope = "read" | "moderate" | "admin";

export interface McpControlBinding {
  readonly bearer: string;
  readonly scopes: readonly McpControlScope[];
  readonly steppedUp: boolean;
}

export class ControlBindingError extends Error {
  readonly code: string;

  constructor(code: string) {
    super("control request refused");
    this.name = "ControlBindingError";
    this.code = code;
  }
}

export function bindControlBearer(
  bearer: string | null | undefined,
  scopes: readonly McpControlScope[],
  steppedUp = false,
): McpControlBinding {
  if (!bearer) throw new ControlBindingError("control_binding_required");
  // Copy grants so caller mutation cannot elevate a live binding. Keep the
  // credential non-enumerable: JSON/spread/ordinary inspection never emit it.
  const binding = Object.defineProperty(
    { scopes: Object.freeze([...scopes]), steppedUp },
    "bearer",
    { get: () => bearer, enumerable: false },
  ) as McpControlBinding;
  return Object.freeze(binding);
}

export function requireControlScope(
  binding: McpControlBinding | null | undefined,
  scope: McpControlScope,
): void {
  if (!binding) throw new ControlBindingError("control_binding_required");
  if (!binding.scopes.includes(scope)) {
    throw new ControlBindingError("control_scope_insufficient");
  }
  if (!binding.steppedUp) throw new ControlBindingError("control_step_up_required");
}
