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
# Worker secrets live outside wrangler.toml and are optional per environment.
# Never require this binding (absence is log-only), or accept a plaintext URL.
OPTIONAL_SECRET_BINDINGS = {"OPS_ALERT_WEBHOOK_URL", "METRICS_SCRAPE_TOKEN"}
# Absent tuning uses the code default; a named env may opt in independently.
OPTIONAL_VARS = {"UNREADY_ALERT_FAILURES"}
# TOG-12980 (CISO TOG-12979 C1/C10): the default-dark POST /internal/actions
# ingress exists in staging only. A top-level var would be required in every
# environment, so it is denied there too; production must never carry it.
INGRESS_VAR = "INTERNAL_ACTIONS_INGRESS"
INGRESS_ENVS = {"staging"}
# Private-receiver config is Operator-set Worker secrets, never wrangler.toml.
RECEIVER_CONFIG_PREFIX = "TWO_INTERNAL_"
# Boot publication of the guild command registry is a full replacement of the
# guild's commands. wrangler.toml may opt in for staging only; production
# publication stays an Operator-approved Worker binding, never a committed var.
BOOT_PUBLISH_VAR = "TWO_COMMANDS_PUBLISH_ON_BOOT"
BOOT_PUBLISH_ENVS = {"staging"}


def check(path: Path) -> list[str]:
    with path.open("rb") as f:
        cfg = tomllib.load(f)

    errors: list[str] = []
    # Every top-level var must be repeated in each named env (values may
    # differ, e.g. TWO_GUILD_NAME). This auto-covers future additions — the
    # REDIRECT_* vars from TOG-9696 were top-level-only until this check.
    required_vars = (set(cfg.get("vars", {})) - OPTIONAL_SECRET_BINDINGS - OPTIONAL_VARS) | EXTRA_REQUIRED_VARS
    envs = cfg.get("env", {})
    if not envs:
        return ["no [env.*] sections found"]

    for prefix, section in [("[vars]", cfg), *[(f"[env.{name}.vars]", env) for name, env in envs.items()]]:
        variables = section.get("vars", {})
        for key in sorted(OPTIONAL_SECRET_BINDINGS & set(variables)):
            errors.append(f"{prefix}: {key} must be an optional Worker secret, never a plain var")
        env_name = prefix[len("[env."):-len(".vars]")] if prefix.startswith("[env.") else None
        if INGRESS_VAR in variables and env_name not in INGRESS_ENVS:
            errors.append(
                f"{prefix}: {INGRESS_VAR} is staging-only "
                "(top-level vars are required in every env; production must never set it)"
            )
        if variables.get(INGRESS_VAR, "1") != "1":
            errors.append(f'{prefix}: {INGRESS_VAR} must be exactly "1" or absent')
        if BOOT_PUBLISH_VAR in variables and env_name not in BOOT_PUBLISH_ENVS:
            errors.append(
                f"{prefix}: {BOOT_PUBLISH_VAR} is staging-only "
                "(top-level vars are required in every env; production publication is Operator-approved)"
            )
        if variables.get(BOOT_PUBLISH_VAR, "1") != "1":
            errors.append(f'{prefix}: {BOOT_PUBLISH_VAR} must be exactly "1" or absent')
        for key in sorted(k for k in variables if k.startswith(RECEIVER_CONFIG_PREFIX)):
            errors.append(f"{prefix}: {key} is an Operator-set Worker secret, never a wrangler.toml var")
        threshold = variables.get("UNREADY_ALERT_FAILURES")
        if threshold is not None and (
            not isinstance(threshold, str)
            or not threshold.isascii()
            or not threshold.isdecimal()
            or not 1 <= int(threshold) <= 9007199254740991
        ):
            errors.append(f"{prefix}: UNREADY_ALERT_FAILURES must be a positive safe-integer string")

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
        if env.get("version_metadata", {}).get("binding") != "CF_VERSION_METADATA":
            errors.append(f"{prefix}: missing version_metadata binding 'CF_VERSION_METADATA'")
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
