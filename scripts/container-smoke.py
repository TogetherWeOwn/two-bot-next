"""Smoke-test a locally loaded runtime image, without Discord or a database."""

import argparse
import gzip
import hashlib
import http.client
import json
import os
from pathlib import Path
import subprocess
import tarfile
import threading
import time
from urllib.parse import urlsplit
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
    # A hand-rolled connection (not urlopen) so any 3xx is returned as-is;
    # following a redirect would let one healthy endpoint masquerade as
    # another and falsely satisfy the contract.
    parts = urlsplit(url)
    connection = http.client.HTTPConnection(parts.hostname, parts.port, timeout=2)
    try:
        connection.request("GET", parts.path or "/")
        response = connection.getresponse()
        try:
            return response.status, json.loads(response.read())
        except ValueError:
            # A status with no JSON body (e.g. a bare 3xx) is still an
            # answer; the caller's exact status/body assertions reject it.
            return response.status, None
    except (OSError, TimeoutError, http.client.HTTPException):
        # The server may still be starting (connection refused/reset by peer)
        # or slow to answer; the caller retries until its deadline. A 4xx/5xx
        # is a real answer, not a transport failure, so it is returned above.
        return None, None
    finally:
        connection.close()


def saved_layer_bytes(stream, layer_ids, limit):
    # Match archive contents to inspect's DiffIDs, not archive paths. Classic
    # Docker saves layer.tar; containerd can save gzip blobs in an OCI layout.
    # Count *all* layers (including overwritten files) plus tar padding. This
    # is conservative versus the original uncompressed filesystem-byte gate.
    require(layer_ids, "image must have rootfs layers")
    sizes = {}
    with tarfile.open(fileobj=stream, mode="r|*") as archive:
        for member in archive:
            if not member.isfile():
                continue
            with archive.extractfile(member) as payload:
                compressed = payload.peek(2)[:2] == b"\x1f\x8b"
                reader = gzip.GzipFile(fileobj=payload) if compressed else payload
                try:
                    digest = hashlib.sha256()
                    size = 0
                    while chunk := reader.read(256 * 1024):
                        size += len(chunk)
                        require(size <= limit, "image exceeds size budget")
                        digest.update(chunk)
                    layer_id = "sha256:" + digest.hexdigest()
                    if layer_id in layer_ids:
                        sizes[layer_id] = size
                finally:
                    if compressed:
                        reader.close()
    require(all(layer in sizes for layer in layer_ids),
            "saved image is missing a verified rootfs layer")
    return sum(sizes[layer] for layer in layer_ids)


def image_layer_bytes(image, metadata, limit):
    # inspect Size is storage-backend-dependent: containerd includes compressed
    # content AND unpacked snapshots. Never substitute that disk-usage number
    # for the calibrated uncompressed-layer budget, or use a merged export
    # that hides bytes from overwritten layers. Stream save without scratch.
    with subprocess.Popen(["docker", "image", "save", image], stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL) as process:
        timer = threading.Timer(120, process.kill)
        timer.start()
        try:
            size = saved_layer_bytes(process.stdout, metadata["RootFS"]["Layers"], limit)
            # tarfile stops at the tar terminator; drain any trailing padding
            # before waiting, so the producer cannot block on its stdout pipe.
            while process.stdout.read(256 * 1024):
                pass
            require(process.wait(timeout=5) == 0, "docker image save failed or timed out")
            return size
        finally:
            timer.cancel()
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)


def smoke(image, image_max_bytes=IMAGE_MAX_BYTES, binary_max_bytes=BINARY_MAX_BYTES):
    metadata = json.loads(docker("image", "inspect", image).stdout)[0]
    report(f"Docker stored Size (backend-dependent, not the layer budget): {metadata['Size']} bytes")
    image_bytes = image_layer_bytes(image, metadata, image_max_bytes)
    # Named (not --rm/unnamed) so a timed-out Docker client cannot leave an
    # orphan behind; same memory cap as the main run.
    measure = "two-bot-measure-" + uuid.uuid4().hex
    try:
        binary_bytes = int(docker(
            "run", "--name", measure, "--memory", "256m",
            "--network", "none", "--entrypoint", "stat", image,
            "-c", "%s", BINARY,
        ).stdout)
    finally:
        docker("rm", "--force", measure, check=False)
    for label, size, limit in (
        ("image (verified uncompressed layer archives)", image_bytes, image_max_bytes),
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
        require(isinstance(body, dict), "/readyz body must be a JSON object")
        require(body.get("components") == [
            ["process", "ready"], ["gateway", "down"], ["token_invalid", "ready"],
        ], "/readyz body must report a ready process, parked gateway and valid token state")
        # The runtime always reports informational job status alongside
        # readiness; with no credentials all six jobs must be parked,
        # non-running and never started. Jobs never flip the 503 above.
        parked = {"parked": True, "running": False, "last_start": None,
                  "last_success": None, "last_error_class": None,
                  "consecutive_failures": 0}
        require(body.get("jobs") == {
            name: dict(parked) for name in (
                "counter", "rank", "scheduled_events", "presence_probe",
                "community_scorecard", "inactivity",
            )
        }, "/readyz body must report all six jobs parked, non-running, never started")
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
        # Named (not --rm/unnamed) so a timed-out Docker client cannot leave
        # an orphan behind; cleaned with the same memory cap as the main run.
        probe = name + "-probe"
        try:
            result = docker(
                "run", "--name", probe, "--memory", "256m",
                "--network", "none", image, "--healthcheck",
                timeout=30, check=False,
            )
            require(result.returncode == 1, "--healthcheck without a server must exit 1")
        finally:
            docker("rm", "--force", probe, check=False)
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
    except (RuntimeError, subprocess.SubprocessError, ValueError, KeyError,
            tarfile.TarError, OSError, EOFError) as error:
        raise SystemExit(f"FAIL container smoke: {error}") from error


if __name__ == "__main__":
    main()
