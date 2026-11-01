"""Recreate the Docker dependency layer; --cargo runs its real locked fetch."""

import argparse
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


class Refusal(Exception):
    """Fixed-vocabulary refusal shared by both dependency-layer verifiers.

    Every validation path raises this (never ``assert``/``AssertionError``)
    so the verifier fails closed under ``python -O``, where asserts are
    stripped into a silent pass.
    """


def require(condition, message):
    """Raise :class:`Refusal` when condition is false, else return None."""
    if not condition:
        raise Refusal(message)
    return None


def _command_words(command):
    """Keep COPY and RUN quoting failures on the same typed refusal path."""
    try:
        return shlex.split(command)
    except ValueError as error:
        raise Refusal(
            "unsupported dependency-layer command: malformed quoting") from error


def _relative_path(path):
    """Only replay literal relative operands, never host paths or traversal."""
    parsed = Path(path)
    require(path and not parsed.is_absolute() and ".." not in parsed.parts,
            "unsupported dependency-layer path: expected a nonempty relative path")
    return parsed


def _copy_parts(instruction):
    parts = _command_words(instruction)[1:]
    require(len(parts) >= 2,
            "unsupported dependency-layer command: "
            "COPY needs sources and destination")
    for path in parts:
        _relative_path(path)
    *sources, destination = parts
    return sources, destination


def _run_commands(command):
    """Parse the supported shell subset, keeping operators distinct from words.

    Non-POSIX lexing retains quotes/escapes on each token; only the raw `>`
    and `&&` tokens are operators. Decode operands separately, after checking
    their lexical role. In particular, `'>'` and `\\>` are not redirections.
    Unsupported shell syntax refuses instead of being executed on the host.
    """
    lexer = shlex.shlex(command, posix=False, punctuation_chars="&>|;()<")
    lexer.whitespace_split = True
    lexer.commenters = ""
    try:
        tokens = list(lexer)
    except ValueError as error:
        raise Refusal(
            "unsupported dependency-layer command: malformed quoting") from error
    segments = [[]]
    for token in tokens:
        if token == "&&":
            segments.append([])
        else:
            segments[-1].append(token)
    operations = []
    for segment in segments:
        require(segment, "unsupported dependency-layer command: empty segment")
        words = []
        for index, token in enumerate(segment):
            require(not token.startswith("#"),
                    "unsupported dependency-layer command: shell comment")
            operator = all(char in "&>|;()<" for char in token)
            require(not operator or (segment[0] == "echo"
                                     and index == 2 and token == ">"),
                    f"unsupported dependency-layer command: {segment[0]} shell operator")
            decoded = _command_words(token)
            require(len(decoded) == 1,
                    "unsupported dependency-layer command: malformed quoting")
            # Shell expansion would not be a literal replay. Single-quoted
            # operands are literal, including characters meaningful to sh.
            require(token.startswith("'") or not any(
                char in token for char in "$`*?~"),
                "unsupported dependency-layer command: shell expansion")
            words.append(decoded[0])
        if len(words) >= 3 and words[:2] == ["mkdir", "-p"]:
            for path in words[2:]:
                require(not path.startswith("-"),
                        "unsupported dependency-layer command: mkdir operand")
                _relative_path(path)
            operations.append(("mkdir", words[2:]))
        elif (len(words) == 4 and words[0] == "echo"
              and segment[2] == ">" and not words[1].startswith("-")):
            _relative_path(words[3])
            operations.append(("echo", [words[1], words[3]]))
        elif words == ["cargo", "fetch", "--locked"]:
            operations.append(("fetch", []))
        else:
            raise Refusal(
                f"unsupported dependency-layer command: {words[0]}")
    return operations


def replay_layer(instructions, root, stage):
    """Validate the entire replay before writing any dependency-layer files."""
    operations = []
    for kind, instruction in instructions:
        if kind == "copy":
            sources, destination = _copy_parts(instruction)
            operations.append(("copy", [sources, destination]))
        elif kind == "run":
            operations.extend(_run_commands(instruction[4:]))
    # A caller may supply an existing layer. Refuse its symlink escapes too;
    # replay itself creates only directories and ordinary file contents.
    for kind, operands in operations:
        paths = (operands[1],) if kind in ("copy", "echo") else operands
        if kind == "copy":
            sources, destination = operands
            for source in sources:
                require((root / source).resolve().is_relative_to(root.resolve()),
                        "unsupported dependency-layer path: source escapes root")
            paths = [destination, *(str(Path(destination) / Path(source).name)
                                    for source in sources)]
        for path in paths:
            require((stage / path).resolve().is_relative_to(stage.resolve()),
                    "unsupported dependency-layer path: target escapes replay")
    for kind, operands in operations:
        if kind == "copy":
            sources, destination = operands
            target = stage / destination
            target.mkdir(parents=True, exist_ok=True)
            for source in sources:
                shutil.copyfile(root / source, target / Path(source).name)
        elif kind == "mkdir":
            for path in operands:
                (stage / path).mkdir(parents=True, exist_ok=True)
        elif kind == "echo":
            (stage / operands[1]).write_text(operands[0] + "\n")
    return any(kind == "fetch" for kind, _ in operations)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cargo", action="store_true")
    args = parser.parse_args()

    # Both images must retain the complete dependency layer. Print no PASS
    # until every layer and wiring pin holds, including the voice-probe copy.
    reports = []
    for dockerfile in ("Dockerfile", "crates/voice-probe/Dockerfile"):
        layer = (ROOT / dockerfile).read_text().split("COPY . .", 1)[0]
        instructions = []
        for instruction in layer.replace("\\\n", " ").splitlines():
            instruction = instruction.strip()
            if not instruction or instruction.startswith("#"):
                continue
            kind = instruction.split()[0]
            if kind == "COPY":
                _copy_parts(instruction)
                instructions.append(("copy", instruction))
            elif kind == "RUN":
                require(instruction.endswith("&& cargo fetch --locked"),
                        "unsupported dependency-layer command: missing locked fetch suffix")
                instructions.append(("run", instruction))
        with tempfile.TemporaryDirectory(
                dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
                or os.environ.get("RUNNER_TEMP")) as tmp:
            stage = Path(tmp)
            require(replay_layer(instructions, ROOT, stage),
                    "dependency fetch layer was not checked")
            manifest = tomllib.loads((stage / "Cargo.toml").read_text())
            for member in [".", *manifest["workspace"]["members"]]:
                package = stage / member
                require((package / "Cargo.toml").is_file(), member)
                require((package / "src/lib.rs").is_file()
                        or (package / "src/main.rs").is_file(),
                        f"Missing dependency-layer target: {member}")
            if args.cargo:
                subprocess.run(["cargo", "fetch", "--locked"], cwd=stage,
                               check=True)
            reports.append(
                f"PASS Docker dependency layer ({dockerfile}): "
                f"{len(manifest['workspace']['members']) + 1} valid package targets"
                + (", cargo fetch --locked passed" if args.cargo
                   else " (Cargo not run)"))

    # The root image's default build must not pull the experimental probe in.
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]
    members = workspace["members"]
    require("crates/voice-probe" in members,
            "voice-probe must stay a workspace member")
    default_members = workspace.get("default-members", members)
    require("crates/voice-probe" not in default_members,
            "voice-probe must stay out of default-members")
    for report in reports:
        print(report)


if __name__ == "__main__":
    main()
