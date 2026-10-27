"""Live main/gateway composition generator for the offline service test.

`render` fills the LIVE_ markers in the sibling `composition.rs` template from
current production source. The main prepare imports this module from its exact
path (no root `sys.path` side effects) and concatenates the returned fragment
with the persistence fragment into one generated integration target.
"""

from pathlib import Path


def _const_line(source, name):
    """Return the full `const NAME ...;` line including any visibility prefix."""
    index = source.index(f"const {name}")
    start = source.rfind("\n", 0, index) + 1
    end = source.index(";", index) + 1
    return source[start:end]


def render(root: Path, block_fn, *, mutant=None) -> str:
    template = Path(__file__).with_name("composition.rs").read_text()
    main = (root / "crates/bot/src/main.rs").read_text()
    gateway = (root / "crates/bot/src/gateway.rs").read_text()
    server = (root / "crates/bot/src/server.rs").read_text()
    shutdown = (root / "crates/bot/src/shutdown.rs").read_text()
    dispatch = (root / "crates/bot/src/dispatch.rs").read_text()
    eventsub = (root / "crates/bot/src/eventsub_reconcile.rs").read_text()

    boot_voice_start = main.index(
        "let (shutdown, stopping) = tokio::sync::watch::channel(false);"
    )
    none = main.index("None => (None, None),", boot_voice_start)
    boot_voice = main[boot_voice_start : main.index("};", none) + len("};")]
    if boot_voice.count("build_voice_runtime") != 1:
        raise ValueError("boot voice fragment must call build_voice_runtime once")
    if "VoiceGates::from_env" not in boot_voice:
        raise ValueError("boot voice fragment lost the VoiceGates gate")

    retention_start = main.index("let latch_mark:", boot_voice_start)
    automod = main.index("let automod_slot", retention_start)
    retention = main[retention_start:automod].rstrip()
    if "latch_observer" not in retention or "worker_supervisors" not in retention:
        raise ValueError("retention fragment lost the latch observer or slot")
    needle = "flush_task.into_iter().collect()"
    if mutant == "drop-flush-retention":
        if retention.count(needle) != 1:
            raise ValueError("retention mutant needs exactly one binding")
        retention = retention.replace(needle, "Vec::new()", 1)
    elif mutant is not None:
        raise ValueError(f"unknown mutant {mutant}")

    service_start = main.index("let result = match gateway_task {", automod)
    service_end = main.index("if result.is_err() {", service_start)
    service = main[service_start:service_end].rstrip()
    for required in (
        "supervise_gateway(task, http, gateway, shutdown)",
        "latch_observer.abort()",
        "reconcile_join_budget(elapsed)",
        "eventsub_reconcile::join(",
        "worker_supervisors",
    ):
        if required not in service:
            raise ValueError(f"service fragment lost {required}")
    if "process::exit" in service:
        raise ValueError("service fragment must exclude process::exit")

    helpers = "\n\n".join(
        block_fn(main, main.index(signature))
        for signature in (
            "fn reconcile_join_budget(",
            "async fn join_worker_supervisors(",
            "async fn supervise_gateway(",
            "async fn supervise_gateway_bounded(",
        )
    )

    build = block_fn(gateway, gateway.index("pub async fn build_voice_runtime("))
    old_loop = "two_bot::voice_rooms::game_activity::flush_loop"
    new_loop = "crate::persistence_adapter::flush_loop"
    if build.count(old_loop) != 1:
        raise ValueError("voice factory must spawn the live flush loop once")
    build = build.replace(old_loop, new_loop, 1)

    guard_struct_start = gateway.index("struct VoiceConnectionGuard(")
    guard_struct = gateway[
        guard_struct_start : gateway.index(";", guard_struct_start) + 1
    ]
    guard_drop = block_fn(gateway, gateway.index("impl Drop for VoiceConnectionGuard {"))
    guard = f"{guard_struct}\n\n{guard_drop}"
    if "disconnect_from_drop" not in guard:
        raise ValueError("guard Drop must call disconnect_from_drop")

    shutdown_requested = block_fn(
        server, server.index("pub(crate) async fn shutdown_requested(")
    )
    timeout_env = _const_line(shutdown, "TIMEOUT_ENV")
    max_seconds = _const_line(shutdown, "MAX_SECONDS")
    dispatch_drain = _const_line(dispatch, "DISPATCH_DRAIN_MAX")
    default_deadline = block_fn(shutdown, shutdown.index("pub(crate) fn default_deadline("))
    if default_deadline.count("crate::dispatch::DISPATCH_DRAIN_MAX") != 1:
        raise ValueError("default deadline must use the live dispatch bound once")
    default_deadline = default_deadline.replace(
        "crate::dispatch::DISPATCH_DRAIN_MAX", "DISPATCH_DRAIN_MAX", 1
    )
    deadline_from = block_fn(shutdown, shutdown.index("pub(crate) fn deadline_from("))
    deadline = block_fn(shutdown, shutdown.index("pub(crate) fn deadline("))
    shutdown_source = "\n\n".join(
        (
            timeout_env,
            max_seconds,
            dispatch_drain,
            default_deadline,
            deadline_from,
            deadline,
        )
    )

    shutdown_drain = _const_line(eventsub, "SHUTDOWN_DRAIN")
    reconcile_join = block_fn(eventsub, eventsub.index("pub async fn join("))
    eventsub_source = f"{shutdown_drain}\n\n{reconcile_join}"

    replacements = {
        "// LIVE_BUILD_VOICE_RUNTIME": build,
        "// LIVE_VOICE_GUARD": guard,
        "// LIVE_MAIN_HELPERS": helpers,
        "// LIVE_SHUTDOWN_DEADLINE": shutdown_source,
        "// LIVE_EVENTSUB_JOIN": eventsub_source,
        "// LIVE_SHUTDOWN_REQUESTED": shutdown_requested,
        "// LIVE_BOOT_VOICE": boot_voice,
        "// LIVE_BOOT_RETENTION": retention,
        "// LIVE_SERVICE": service,
    }
    for marker, source in replacements.items():
        if template.count(marker) != 1:
            raise ValueError(f"missing or duplicate adapter marker {marker}")
        template = template.replace(marker, source)
    return template
