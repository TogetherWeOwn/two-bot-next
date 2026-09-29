#!/usr/bin/env python3
"""Fail if any named Wrangler environment omits required bindings.

Wrangler bindings (vars, durable_objects, containers, exports) are
non-inheritable: an [env.*] section deploys with ONLY what it declares.
TOG-9907: env.staging declared only TWO_GUILD_NAME, so the deployed Worker
had no TWO_BOT binding and /health + /readyz returned HTTP 1101.

Usage: python3 scripts/check-env-bindings.py [wrangler.toml]
Exit 1 lists the missing keys per environment.
"""

import sys
import tomllib
from pathlib import Path

REQUIRED_CONTAINERS_CLASS = "TwoBotContainer"
REQUIRED_DO_BINDING = "TWO_BOT"
# Extra plain vars every environment must repeat beyond the top-level [vars]
# keys (derived automatically below). Values may differ per env; presence of
# the key is what matters, since vars are not inherited.
EXTRA_REQUIRED_VARS = {"TWO_GUILD_NAME"}


def check(path: Path) -> list[str]:
    with path.open("rb") as f:
        cfg = tomllib.load(f)

    errors: list[str] = []
    # Every top-level var must be repeated in each named env (values may
    # differ, e.g. TWO_GUILD_NAME). This auto-covers future additions — the
    # REDIRECT_* vars from TOG-9696 were top-level-only until this check.
    required_vars = set(cfg.get("vars", {})) | EXTRA_REQUIRED_VARS
    envs = cfg.get("env", {})
    if not envs:
        return ["no [env.*] sections found"]

    for env_name, env in envs.items():
        prefix = f"[env.{env_name}]"
        containers = env.get("containers", [])
        if not any(c.get("class_name") == REQUIRED_CONTAINERS_CLASS for c in containers):
            errors.append(
                f"{prefix}: missing [[env.{env_name}.containers]] "
                f"with class_name = {REQUIRED_CONTAINERS_CLASS!r} "
                "(containers are not inherited from the top level)"
            )
        bindings = env.get("durable_objects", {}).get("bindings", [])
        if not any(b.get("name") == REQUIRED_DO_BINDING for b in bindings):
            errors.append(
                f"{prefix}: missing durable_objects binding {REQUIRED_DO_BINDING!r} "
                "(durable_objects are not inherited from the top level)"
            )
        exports = env.get("exports", {})
        if REQUIRED_CONTAINERS_CLASS not in exports:
            errors.append(
                f"{prefix}: missing [exports.{REQUIRED_CONTAINERS_CLASS}] "
                "DO class export (exports are not inherited from the top level)"
            )
        missing_vars = required_vars - set(env.get("vars", {}))
        if missing_vars:
            errors.append(
                f"{prefix}: missing vars {sorted(missing_vars)} "
                "(vars are not inherited from the top level)"
            )
    return errors


def main() -> int:
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("wrangler.toml")
    errors = check(path)
    for err in errors:
        print(f"::error file={path}::{err}")
    if errors:
        print(f"\n{len(errors)} missing env binding(s) in {path}")
        return 1
    print(f"env bindings OK in {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
