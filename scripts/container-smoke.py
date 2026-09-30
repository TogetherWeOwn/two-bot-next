"""Smoke-test a locally loaded runtime image, without Discord or a database."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import time
import urllib.error
import urllib.request
import uuid

MIB = 1024 * 1024
IMAGE_MAX_BYTES = 112 * MIB
BINARY_MAX_BYTES = 10 * MIB
BINARY = "/home/two-bot/two-bot"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def docker(*args, timeout=30, check=True):
    return subprocess.run(
        ["docker", *args], capture_output=True, text=True, timeout=timeout, check=check
    )


def report(message):
    print(message, flush=True)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as summary:
            summary.write(message + "\n\n")


def http_response(url):
    try:
        with urllib.request.urlopen(url, timeout=2) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as response:
        with response:
            return response.code, json.loads(response.read())
    except (urllib.error.URLError, TimeoutError):
        return None, None


def smoke(image, image_max_bytes=IMAGE_MAX_BYTES, binary_max_bytes=BINARY_MAX_BYTES):
    metadata = json.loads(docker("image", "inspect", image).stdout)[0]
    image_bytes = metadata["Size"]
    binary_bytes = int(docker(
        "run", "--rm", "--network", "none", "--entrypoint", "stat", image,
        "-c", "%s", BINARY,
    ).stdout)
    for label, size, limit in (
        ("image (uncompressed Docker Size)", image_bytes, image_max_bytes),
        ("release binary", binary_bytes, binary_max_bytes),
    ):
        report(f"{label}: {size} bytes ({size / MIB:.2f} MiB); budget {limit} bytes ({limit / MIB:.2f} MiB)")
    require(image_bytes <= image_max_bytes, "image exceeds size budget")
    require(binary_bytes <= binary_max_bytes, "release binary exceeds size budget")
    config = metadata["Config"]
    require(config.get("User") not in (None, "", "root", "0", "0:0"), "image must specify a non-root user")
    require(config.get("Healthcheck", {}).get("Test") == ["CMD", BINARY, "--healthcheck"],
            "image HEALTHCHECK must invoke the runtime's --healthcheck")

    # Docker does not inherit host environment without -e. No secrets or DB
    # are supplied, and no deployment/registry access is needed by this test.
    name = "two-bot-smoke-" + uuid.uuid4().hex
    try:
        docker(
            "run", "--detach", "--name", name, "--memory", "256m",
            "--publish", "127.0.0.1::8080", "--health-interval", "1s",
            "--health-start-period", "0s", image,
        )
        port = docker("port", name, "8080/tcp").stdout.strip()
        require(port.startswith("127.0.0.1:"), f"unexpected published port: {port}")
        url = "http://" + port
        deadline = time.monotonic() + 30
        while True:
            code, body = http_response(url + "/health")
            if code == 200:
                require(body == {"status": "ok"}, "/health body must report status ok")
                break
            require(time.monotonic() < deadline, "/health did not return 200 within 30s")
            state = json.loads(docker("inspect", name).stdout)[0]["State"]
            require(state["Running"], "container exited before becoming healthy")
            time.sleep(0.25)
        code, body = http_response(url + "/readyz")
        require(code == 503, "/readyz must be 503 while the gateway is parked")
        require(body == {"components": [["process", "ready"], ["gateway", "down"]]},
                "/readyz body must report a ready process and parked gateway")
        # Check PID 1, not merely Docker's configured user or an exec helper.
        status = docker("exec", name, "cat", "/proc/1/status").stdout
        uid = next(line.split()[1:] for line in status.splitlines() if line.startswith("Uid:"))
        require(len(uid) == 4 and all(int(value) != 0 for value in uid), "runtime PID 1 is root")
        require(docker("exec", name, BINARY, "--healthcheck", timeout=5, check=False).returncode == 0,
                "--healthcheck must exit 0 against the live process")
        while True:
            state = json.loads(docker("inspect", name).stdout)[0]["State"]
            require(state["Running"], "container exited during healthcheck")
            if state["Health"]["Status"] == "healthy":
                break
            require(time.monotonic() < deadline, "Docker HEALTHCHECK did not become healthy within 30s")
            time.sleep(0.25)
        report("PASS /health 200; /readyz 503 parked; non-root PID 1; --healthcheck 0; Docker HEALTHCHECK healthy")

        # A fresh no-network process has no listening server: liveness must
        # fail honestly rather than being an unconditional success command.
        result = docker("run", "--rm", "--network", "none", image, "--healthcheck", check=False)
        require(result.returncode == 1, "--healthcheck without a server must exit 1")
        report("PASS --healthcheck exits 1 without a listening server")

        # Send SIGTERM directly; no docker stop fallback may hide SIGKILL.
        started = time.monotonic()
        docker("kill", "--signal", "TERM", name, timeout=2)
        result = docker("wait", name, timeout=max(0.1, 10 - (time.monotonic() - started)))
        elapsed = time.monotonic() - started
        require(elapsed <= 10, f"SIGTERM took {elapsed:.3f}s (maximum 10s)")
        require(result.stdout.strip() == "0", f"SIGTERM exit code was {result.stdout.strip()}, expected 0")
        state = json.loads(docker("inspect", name).stdout)[0]["State"]
        require(not state["Running"] and not state["OOMKilled"], "container failed to shut down cleanly")
        report(f"PASS SIGTERM exits 0 in {elapsed:.3f}s (maximum 10s); no OOM")
    except Exception:
        logs = docker("logs", "--tail", "80", name, check=False)
        print(logs.stdout + logs.stderr)
        raise
    finally:
        docker("rm", "--force", name, check=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image")
    parser.add_argument("--image-max-bytes", type=int, default=IMAGE_MAX_BYTES)
    parser.add_argument("--binary-max-bytes", type=int, default=BINARY_MAX_BYTES)
    args = parser.parse_args()
    try:
        smoke(args.image, args.image_max_bytes, args.binary_max_bytes)
    except (RuntimeError, subprocess.SubprocessError, ValueError, KeyError) as error:
        raise SystemExit(f"FAIL container smoke: {error}") from error


if __name__ == "__main__":
    main()
