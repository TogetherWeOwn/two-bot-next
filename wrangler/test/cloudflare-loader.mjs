// Node >= 22.15: load the pinned SDK unchanged; mock only workerd's built-ins.
// https://nodejs.org/docs/latest-v22.x/api/module.html#moduleregisterhooksoptions
import { registerHooks } from "node:module";

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier === "cloudflare:workers") {
      return {
        url: new URL("./cloudflare-runtime.mjs", import.meta.url).href,
        shortCircuit: true,
      };
    }
    // The SDK publishes extensionless relative imports for Worker bundlers.
    if (
      context.parentURL?.includes("/@cloudflare/containers/dist/") &&
      specifier.startsWith("./") &&
      !specifier.endsWith(".js")
    ) {
      return nextResolve(`${specifier}.js`, context);
    }
    return nextResolve(specifier, context);
  },
});
