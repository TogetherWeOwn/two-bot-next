"""Compile live private lifecycle code in an offline integration-test adapter.

--run prepares the transient integration target, runs it through the bounded
Cargo cache, then removes only that target. --prepare leaves it for hosted CI's
ordinary workspace integration run. Never commit the generated target: doing
so would freeze the implementation under test instead of reading live source.
"""

import argparse
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


def prepare():
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
    poll = re.search(r"const FLUSH_POLL_INTERVAL:.*?;", activity, re.S).group()
    signature = schedule[schedule.index("pub async fn flush_loop("):]
    signature = signature[:signature.index("{")]
    # Baseline has no shutdown input: run that actual loop, not a test-written
    # approximation. The behavioral tests still compile and show the lost tail.
    arguments = "pool, collector" if "shutdown" not in signature else "pool, collector, shutdown"
    wrapper = f"""
    async fn launch(pool: sqlx::PgPool, collector: Arc<GameActivityCollector>,
                    shutdown: tokio::sync::watch::Receiver<bool>) {{
        let _ = &shutdown;
        flush_loop({arguments}).await;
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
    TARGET.write_text(template)
    try:
        subprocess.run(["rustfmt", "--edition", "2021", str(TARGET)], check=True)
    except BaseException:
        # A half-prepared target would block every retry and leak into the
        # ordinary workspace integration run.
        TARGET.unlink()
        raise
    print(f"Prepared {TARGET.relative_to(ROOT)} from current production source", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--prepare", action="store_true")
    group.add_argument("--run", action="store_true")
    args, extra = parser.parse_known_args()
    prepare()
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
