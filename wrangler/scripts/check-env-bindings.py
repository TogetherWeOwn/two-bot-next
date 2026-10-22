#!/usr/bin/env python3
"""Fail if any named Wrangler environment omits required bindings.

Wrangler bindings (vars, durable_objects, containers, exports) are
non-inheritable: an [env.*] section deploys with ONLY what it declares.
TOG-9907: env.staging declared only TWO_GUILD_NAME, so the deployed Worker
had no TWO_BOT binding and /health + /readyz returned HTTP 1101.

Required-environment contract: wrangler.toml must declare both
[env.staging] and [env.production]. A config missing either (or both)
fails with a fixed missing-environment error, before any per-binding
checks. Additional named environments are allowed and get the same
per-binding checks as the required ones. An unreadable file or invalid
TOML fails with a fixed diagnostic and no traceback or value echo.

Usage: python3 scripts/check-env-bindings.py [wrangler.toml]
Exit 1 lists the missing keys per environment.
"""

import sys
import tomllib
from pathlib import Path

REQUIRED_CONTAINERS_CLASS = "TwoBotContainer"
REQUIRED_DO_BINDING = "TWO_BOT"
# Deployment environments that must both be declared. Additional named
# environments are allowed and receive the same per-binding checks.
REQUIRED_ENVS = ("staging", "production")
# Fixed diagnostics: never echo TOML content or values, only these strings.
INVALID_TOML_ERROR = "invalid Wrangler config: TOML does not parse or has the wrong shape"
UNREADABLE_CONFIG_ERROR = "unreadable Wrangler config: file cannot be read"


def missing_env_error(missing: list[str]) -> str:
    return (
        f"missing required environment(s) {missing} "
        "(wrangler.toml must declare [env.staging] and [env.production])"
    )
# Extra plain vars every environment must repeat beyond the top-level [vars]
# keys (derived automatically below). Values may differ per env; presence of
# the key is what matters, since vars are not inherited.
EXTRA_REQUIRED_VARS = {"TWO_GUILD_NAME"}
# Worker secrets live outside wrangler.toml and are optional per environment.
# Never require this binding (absence is log-only), or accept a plaintext URL.
OPTIONAL_SECRET_BINDINGS = {
    "OPS_ALERT_WEBHOOK_URL",
    "METRICS_SCRAPE_TOKEN",
    "OPS_CONTROL_TOKEN",
    # TOG-22861: the read-only ops dashboard service-token pair. Worker
    # secrets only; a committed value would hand out dashboard access.
    "OPS_DASHBOARD_CLIENT_ID",
    "OPS_DASHBOARD_CLIENT_SECRET",
    # TOG-29201: the MCP shared bearer (docs/mcp.md). Optional Worker
    # secret only; a committed var would publish the bearer.
    "MCP_TOKEN",
}
# TOG-22861 workers.dev break-glass: staging and production serve the public
# suffix until a custom domain and Access app exist. Every named environment
# must declare workers_dev explicitly; exactly the break-glass envs enable
# it. The top level must disable it so bare deploys never serve it.
WORKERS_DEV_ENVS = {"staging", "production"}
# Absent tuning uses the code default; a named env may opt in independently.
OPTIONAL_VARS = {"UNREADY_ALERT_FAILURES"}
# TOG-12980 (CISO TOG-12979 C1/C10): the default-dark POST /internal/actions
# ingress exists in staging only. A top-level var would be required in every
# environment, so it is denied there too; production must never carry it.
INGRESS_VAR = "INTERNAL_ACTIONS_INGRESS"
INGRESS_ENVS = {"staging"}
# TOG-23364: the default-dark POST /hooks/twitch EventSub ingress exists in
# staging only, under the same rule as the internal-actions ingress above.
EVENTSUB_INGRESS_VAR = "EVENTSUB_INGRESS"
EVENTSUB_INGRESS_ENVS = {"staging"}
# Private-receiver config is Operator-set Worker secrets, never wrangler.toml.
RECEIVER_CONFIG_PREFIX = "TWO_INTERNAL_"
# The EventSub Operator switch is the last enable step for the public Twitch
# ingress (wrangler/src/eventsub.ts): the route stays dark until the Operator
# sets the Worker secret TWO_EVENTSUB=1. A committed var would turn the route
# on from a toml change alone, so it is rejected in every vars table, like
# the TWO_INTERNAL_* receiver config above. The prefix also covers the
# private receiver enable, bind and signing secret (TWO_EVENTSUB_BIND,
# TWO_TWITCH_*), which are Operator-set Worker secrets for the same reason.
EVENTSUB_RECEIVER_PREFIX = "TWO_EVENTSUB"
TWITCH_SECRET_PREFIX = "TWO_TWITCH_"
# Boot publication of the guild command registry is a full replacement of the
# guild's commands. wrangler.toml may opt in for staging only; production
# publication stays an Operator-approved Worker binding, never a committed var.
BOOT_PUBLISH_VAR = "TWO_COMMANDS_PUBLISH_ON_BOOT"
BOOT_PUBLISH_ENVS = {"staging"}
# The temporary-voice gate attaches the voice sink to the gateway and (once the
# registry wiring lands) publishes the voice commands. It reads migrated voice
# tables, so it is bound per environment only after that environment's ledger
# carries them: staging first. A top-level var would be required in every
# environment, so it is denied there too; production binding stays an
# Operator-approved Worker binding, never a committed var.
VOICE_GATE_VAR = "TWO_VOICE"
VOICE_GATE_ENVS = {"staging"}
# Privileged GUILD_PRESENCES for room-name facts (docs/voice-presence.md):
# staging-only like the voice gate it depends on; production is an
# Operator-set Worker binding after the app's Presence Intent is on.
VOICE_PRESENCE_VAR = "TWO_VOICE_PRESENCE"
VOICE_PRESENCE_ENVS = {"staging"}
# The music voice-node spike gate attaches the Lavalink voice-update
# forwarder (docs/music-voice-node-spike.md). Staging-only like the voice
# gate: a top-level var would be required in every environment, so it is
# denied there too; production music is an explicit follow-up decision,
# never a committed var.
MUSIC_GATE_VAR = "TWO_MUSIC"
MUSIC_GATE_ENVS = {"staging"}
# TOG-22862 (CISO AC4): the mutating admin controls (PUT /ops/config,
# POST /ops/kill-switch) stay dark unless the environment opts in with these
# exact-"1" switches. A top-level var would be required in every environment,
# so it is denied there too; production stays dark until the CISO review.
OPS_CONFIG_WRITES_VAR = "OPS_CONFIG_WRITES_ENABLED"
OPS_CONFIG_WRITES_ENVS = {"staging"}
OPS_KILLSWITCH_VAR = "OPS_KILLSWITCH_ENABLED"
OPS_KILLSWITCH_ENVS = {"staging"}
# Rollback-drill fence (docs/staging-rollout-gate.md): the deploy-env marker
# is exactly "staging" in the staging vars only, so neither the Worker
# forward nor the Rust readiness fence can treat another environment as
# staging. The injection switch itself is denied in every vars table,
# staging included: only the generated per-deploy staging config written
# by scripts/staging_rollout.py for a manual dispatch may carry it, so a
# toml change alone can never arm (or leave armed) a failing gateway.
DEPLOY_ENV_VAR = "TWO_DEPLOY_ENV"
DEPLOY_ENV_ENVS = {"staging"}
GATEWAY_FAIL_INJECTION_VAR = "TWO_GATEWAY_READINESS_FAIL_INJECTION"
# Remediation-drill fence (staging fault injector): the same generated-only
# rule as the rollback-drill switch above. Either switch committed anywhere —
# staging included — fails; only the generated per-deploy staging config from
# a manual drill dispatch may carry them, so a toml change alone can never
# arm (or leave armed) a synthetic stall or unpaired disconnect.
VOICE_STALL_FAULT_VAR = "TWO_VOICE_STALL_FAULT_INJECTION"
UNPAIRED_FAULT_VAR = "TWO_GATEWAY_UNPAIRED_FAULT_INJECTION"


def check(path: Path) -> list[str]:
    try:
        with path.open("rb") as f:
            cfg = tomllib.load(f)
    except OSError:
        return [UNREADABLE_CONFIG_ERROR]
    except (tomllib.TOMLDecodeError, ValueError):
        return [INVALID_TOML_ERROR]
    try:
        return _collect_errors(cfg)
    except (AttributeError, TypeError):
        # Valid TOML with the wrong shape (for example `env = "x"` or
        # `[env]` with scalar values): fail closed with the fixed
        # diagnostic. Every attribute access below runs on TOML-supplied
        # values, so these errors mean a misshapen config, never a traceback.
        return [INVALID_TOML_ERROR]


def _collect_errors(cfg: dict) -> list[str]:
    errors: list[str] = []
    # Every top-level var must be repeated in each named env (values may
    # differ, e.g. TWO_GUILD_NAME). This auto-covers future additions — the
    # REDIRECT_* vars from TOG-9696 were top-level-only until this check.
    required_vars = (set(cfg.get("vars", {})) - OPTIONAL_SECRET_BINDINGS - OPTIONAL_VARS) | EXTRA_REQUIRED_VARS
    envs = cfg.get("env", {})
    if not isinstance(envs, dict):
        return [INVALID_TOML_ERROR]
    missing = sorted(set(REQUIRED_ENVS) - set(envs))
    if missing:
        return [missing_env_error(missing)]

    # TOG-22861: workers.dev break-glass is staging + production until a
    # custom domain and Access app exist. Every named environment declares it
    # explicitly so a new env cannot inherit the public suffix by accident;
    # diagnostics name the section, never values.
    if cfg.get("workers_dev") is not False:
        errors.append(
            "top-level workers_dev must be explicitly false "
            "(only break-glass envs serve workers.dev)"
        )
    for env_name, env in envs.items():
        workers_dev = env.get("workers_dev")
        if env_name in WORKERS_DEV_ENVS:
            if workers_dev is not True:
                errors.append(
                    f"[env.{env_name}]: workers_dev must be explicitly true "
                    f"({env_name} is a dashboard break-glass origin)"
                )
        elif workers_dev is not False:
            errors.append(
                f"[env.{env_name}]: workers_dev must be explicitly false "
                "(only break-glass envs serve workers.dev)"
            )

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
        if EVENTSUB_INGRESS_VAR in variables and env_name not in EVENTSUB_INGRESS_ENVS:
            errors.append(
                f"{prefix}: {EVENTSUB_INGRESS_VAR} is staging-only "
                "(top-level vars are required in every env; production must never set it)"
            )
        if variables.get(EVENTSUB_INGRESS_VAR, "1") != "1":
            errors.append(f'{prefix}: {EVENTSUB_INGRESS_VAR} must be exactly "1" or absent')
        if BOOT_PUBLISH_VAR in variables and env_name not in BOOT_PUBLISH_ENVS:
            errors.append(
                f"{prefix}: {BOOT_PUBLISH_VAR} is staging-only "
                "(top-level vars are required in every env; production publication is Operator-approved)"
            )
        if variables.get(BOOT_PUBLISH_VAR, "1") != "1":
            errors.append(f'{prefix}: {BOOT_PUBLISH_VAR} must be exactly "1" or absent')
        if VOICE_GATE_VAR in variables and env_name not in VOICE_GATE_ENVS:
            errors.append(
                f"{prefix}: {VOICE_GATE_VAR} is staging-only "
                "(top-level vars are required in every env; production voice is Operator-approved)"
            )
        if variables.get(VOICE_GATE_VAR, "1") != "1":
            errors.append(f'{prefix}: {VOICE_GATE_VAR} must be exactly "1" or absent')
        if VOICE_PRESENCE_VAR in variables and env_name not in VOICE_PRESENCE_ENVS:
            errors.append(
                f"{prefix}: {VOICE_PRESENCE_VAR} is staging-only "
                "(top-level vars are required in every env; production presences are Operator-approved)"
            )
        if variables.get(VOICE_PRESENCE_VAR, "1") != "1":
            errors.append(f'{prefix}: {VOICE_PRESENCE_VAR} must be exactly "1" or absent')
        if MUSIC_GATE_VAR in variables and env_name not in MUSIC_GATE_ENVS:
            errors.append(
                f"{prefix}: {MUSIC_GATE_VAR} is staging-only "
                "(top-level vars are required in every env; production music is a follow-up, never a committed var)"
            )
        if variables.get(MUSIC_GATE_VAR, "1") != "1":
            errors.append(f'{prefix}: {MUSIC_GATE_VAR} must be exactly "1" or absent')
        if OPS_CONFIG_WRITES_VAR in variables and env_name not in OPS_CONFIG_WRITES_ENVS:
            errors.append(
                f"{prefix}: {OPS_CONFIG_WRITES_VAR} is staging-only "
                "(top-level vars are required in every env; production writes need the CISO review)"
            )
        if variables.get(OPS_CONFIG_WRITES_VAR, "1") != "1":
            errors.append(f'{prefix}: {OPS_CONFIG_WRITES_VAR} must be exactly "1" or absent')
        if OPS_KILLSWITCH_VAR in variables and env_name not in OPS_KILLSWITCH_ENVS:
            errors.append(
                f"{prefix}: {OPS_KILLSWITCH_VAR} is staging-only "
                "(top-level vars are required in every env; production kill-switch needs the CISO review)"
            )
        if variables.get(OPS_KILLSWITCH_VAR, "1") != "1":
            errors.append(f'{prefix}: {OPS_KILLSWITCH_VAR} must be exactly "1" or absent')
        if DEPLOY_ENV_VAR in variables and env_name not in DEPLOY_ENV_ENVS:
            errors.append(
                f"{prefix}: {DEPLOY_ENV_VAR} is staging-only "
                "(top-level vars are required in every env; production must never carry the staging fence marker)"
            )
        if variables.get(DEPLOY_ENV_VAR, "staging") != "staging":
            errors.append(f'{prefix}: {DEPLOY_ENV_VAR} must be exactly "staging" or absent')
        if GATEWAY_FAIL_INJECTION_VAR in variables:
            errors.append(
                f"{prefix}: {GATEWAY_FAIL_INJECTION_VAR} must never be a committed var "
                "(only the generated staging deploy config from a manual drill dispatch may carry it)"
            )
        for fault_var in (VOICE_STALL_FAULT_VAR, UNPAIRED_FAULT_VAR):
            if fault_var in variables:
                errors.append(
                    f"{prefix}: {fault_var} must never be a committed var "
                    "(only the generated staging deploy config from a manual drill dispatch may carry it)"
                )
        for key in sorted(k for k in variables if k.startswith(RECEIVER_CONFIG_PREFIX)):
            errors.append(f"{prefix}: {key} is an Operator-set Worker secret, never a wrangler.toml var")
        for key in sorted(
            k
            for k in variables
            if k.startswith(EVENTSUB_RECEIVER_PREFIX) or k.startswith(TWITCH_SECRET_PREFIX)
        ):
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
        print(f"\n{len(errors)} env binding problem(s) in {path}")
        return 1
    print(f"env bindings OK in {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
