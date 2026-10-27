"""Compile live private lifecycle code in an offline integration-test adapter.

--run prepares the transient integration target, runs it through the bounded
Cargo cache, then removes only that target. --prepare leaves it for hosted CI's
ordinary workspace integration run. Never commit the generated target: doing
so would freeze the implementation under test instead of reading live source.
"""

import argparse
import importlib.util
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
TARGET = ROOT / "crates/bot/tests/gateway_lifecycle_generated.rs"


def block(source, start):
    """Return a balanced Rust block, ignoring comments and string literals."""
    opening = source.index("{", start)
    depth = 0
    index = opening
    state = "code"
    comment_depth = 0
    while index < len(source):
        char = source[index]
        pair = source[index:index + 2]
        if state == "line":
            if char == "\n":
                state = "code"
        elif state == "comment":
            if pair == "/*":
                comment_depth += 1
                index += 1
            elif pair == "*/":
                comment_depth -= 1
                index += 1
                if not comment_depth:
                    state = "code"
        elif state == "string":
            if char == "\\":
                index += 1
            elif char == '"':
                state = "code"
        elif pair == "//":
            state = "line"
            index += 1
        elif pair == "/*":
            state = "comment"
            comment_depth = 1
            index += 1
        elif char == '"':
            state = "string"
        elif char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return source[start:index + 1]
        index += 1
    raise ValueError("unterminated source item")


def used_constants(source, region, excluded=()):
    """Keep live constants used by an extracted region, including dependencies."""
    candidates = re.findall(
        r"(?m)^((?:pub(?:\(crate\))?\s+)?const (\w+)\s*:[^;]+;)", source
    )
    selected = {}
    while True:
        context = region + "\n" + "\n".join(selected.values())
        needed = {
            name: item for item, name in candidates
            if name not in excluded and re.search(rf"\b{re.escape(name)}\b", context)
        }
        if needed == selected:
            return "\n".join(selected.values())
        selected = needed


def loop_arguments(schedule):
    signature = schedule[schedule.index("pub async fn flush_loop("):]
    signature = signature[:signature.index("{")]
    arguments = ["pool", "collector"]
    if "shutdown" in signature:
        arguments.append("shutdown")
    return ", ".join(arguments)


def without_final_flush(source):
    """Mutation only in the generated adapter, never in a production file."""
    call = re.compile(
        r"flush_remaining_guilds\(&pool,\s*&collector,\s*now_ms\(\),\s*"
        r"GAME_ACTIVITY_GAP_TIMEOUT_MS\)\.await;"
    )
    result, count = call.subn("", source)
    if count != 1:
        raise ValueError("final-flush mutant requires exactly one live shutdown call")
    return result


def prepare(mutant=None):
    if TARGET.exists():
        raise FileExistsError(f"refusing to overwrite {TARGET}")
    activity = (ROOT / "crates/bot/src/voice_rooms/game_activity.rs").read_text()
    gateway = (ROOT / "crates/bot/src/gateway.rs").read_text()
    onboarding = (ROOT / "crates/bot/src/onboarding.rs").read_text()

    # Include the scheduling region and any new drain helpers in that region.
    # Clock and persistence are the only substituted boundaries.
    schedule = activity[activity.index("pub async fn flush_due_guilds("):]
    schedule = schedule[:schedule.index("#[cfg(test)]")]
    clock = block(schedule, schedule.index("fn now_ms("))
    schedule = schedule.replace(clock, "", 1)
    if mutant == "drop-final-flush":
        schedule = without_final_flush(schedule)
    poll = used_constants(activity, schedule, excluded=("GAME_ACTIVITY_GAP_TIMEOUT_MS",))
    # Keep the original cases runnable against the pre-shutdown baseline too;
    # the production service, not this adapter, owns the absolute join budget.
    arguments = loop_arguments(schedule)
    wrapper = f"""
    async fn launch(pool: sqlx::PgPool, collector: Arc<GameActivityCollector>,
                    shutdown: tokio::sync::watch::Receiver<bool>) {{
        let _ = &shutdown;
        let _ = flush_loop({arguments}).await;
    }}
    """

    cache = gateway[gateway.index("struct LiveInteraction {"):]
    cache = cache[:cache.index("async fn generation_changed(")]
    # Capacity constants may live beside the other gateway limits.
    constants = "\n".join(re.findall(
        r"(?:pub(?:\(crate\))?\s+)?const LIVE_INTERACTION\w*\s*:[^;]+;", gateway
    ))
    for constant in constants.splitlines():
        if constant in cache:
            constants = constants.replace(constant, "")
    insertion_start = gateway.index("if let Some(OnboardingJob::Interaction(interaction)) = onboarding_job")
    insertion_start = gateway.index("Some(ticket) =>", insertion_start)
    insertion = block(gateway, insertion_start)
    insertion = insertion[insertion.index("{") + 1:-1]
    claim_start = gateway.index("let live = live_interactions", gateway.index("async fn onboarding_queue("))
    claim_end = gateway.index("if let Some(job) = job {", claim_start)
    claim = gateway[claim_start:claim_end]
    jobs = block(onboarding, onboarding.index("pub enum OnboardingJob {"))
    jobs = "#[derive(serde::Serialize, serde::Deserialize)]\n" + jobs
    jobs += "\n" + block(onboarding, onboarding.index("impl OnboardingJob {"))

    template = Path(__file__).with_name("acceptance.rs").read_text()
    replacements = {
        "// SCHEDULING_SOURCE": poll + "\n" + schedule + "\n" + wrapper,
        "// CACHE_SOURCE": constants + "\n" + cache,
        "// INSERT_SOURCE": insertion,
        "// CLAIM_SOURCE": claim,
        "// RECOVERY_SOURCE": jobs,
    }
    for marker, source in replacements.items():
        if template.count(marker) != 1:
            raise ValueError(f"missing or duplicate adapter marker {marker}")
        template = template.replace(marker, source)
    # The added failure cases keep collector carry/drain/requeue and the
    # complete persist path live. Only transaction I/O and time are doubled.
    persistence_source = activity[activity.index("struct OpenSpan {"):]
    persistence_source = persistence_source[:persistence_source.index("\n#[cfg(test)]\nmod tests")]
    persistence_source = persistence_source.replace(clock, "", 1)
    if mutant == "drop-final-flush":
        persistence_source = without_final_flush(persistence_source)
    due = block(activity, activity.index("pub fn flush_due("))
    persistence_source = (
        used_constants(activity, due + persistence_source) + "\n" + due + "\n" + persistence_source
    )
    persistence_source += f"""
    async fn launch(pool: sqlx::PgPool, collector: std::sync::Arc<GameActivityCollector>,
                    shutdown: tokio::sync::watch::Receiver<bool>,
                    deadline: tokio::time::Instant) -> bool {{
        trait Completion {{ fn clean(self) -> bool; }}
        impl Completion for () {{ fn clean(self) -> bool {{ true }} }}
        impl<T, E> Completion for Result<T, E> {{
            fn clean(self) -> bool {{ self.is_ok() }}
        }}
        let _ = deadline;
        flush_loop({arguments}).await.clean()
    }}
    """
    persistence = Path(__file__).with_name("persistence.rs").read_text()
    if persistence.count("// PERSISTENCE_SOURCE") != 1:
        raise ValueError("missing or duplicate persistence adapter marker")
    template += "\n" + persistence.replace("// PERSISTENCE_SOURCE", persistence_source)
    spec = importlib.util.spec_from_file_location(
        "gateway_lifecycle_composition", Path(__file__).with_name("composition.py")
    )
    composition = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(composition)
    template += "\n" + composition.render(
        ROOT, block, mutant=mutant if mutant == "drop-flush-retention" else None
    )

    # Exclusive creation keeps both the refusal and cleanup ownership safe if
    # another invocation creates the target after the initial existence check.
    output = TARGET.open("x")
    try:
        with output:
            output.write(template)
        subprocess.run(["rustfmt", "--edition", "2021", str(TARGET)], check=True)
    except BaseException:
        try:
            TARGET.unlink(missing_ok=True)
        except OSError:
            # Cleanup must never replace the original formatter/write error.
            pass
        raise
    print(f"Prepared {TARGET.relative_to(ROOT)} from current production source", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--prepare", action="store_true")
    group.add_argument("--run", action="store_true")
    parser.add_argument("--mutant", choices=("drop-final-flush", "drop-flush-retention"))
    args, extra = parser.parse_known_args()
    prepare(args.mutant)
    if args.prepare:
        return 0
    try:
        result = subprocess.run([
            sys.executable, "scripts/cargo_cache.py", "run", "--", "test",
            "-p", "two-bot", "--test", "gateway_lifecycle_generated", "--", *extra,
        ], cwd=ROOT)
        return result.returncode
    finally:
        TARGET.unlink()


if __name__ == "__main__":
    sys.exit(main())
