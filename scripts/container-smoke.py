"""Smoke-test a locally loaded runtime image, without Discord or a database."""

import argparse
import http.client
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import time
from urllib.parse import urlsplit
import uuid

MIB = 1024 * 1024
IMAGE_MAX_BYTES = 112 * MIB
# Recalibrated for the linked S4 self-role runtime (TOG-10292): PR head
# measured 10,805,344 bytes (10.30 MiB) on the ephemeral runner vs main
# baseline 10,377,112 bytes (9.90 MiB) at ec49663. Growth is linked
# runtime/handlers/REST + previously-dead domain/store code, no new
# dependencies; release profile already minimal (opt-level=z, lto, strip).
# Per b1-baseline calibration (measured * 1.4 rounded up to the next MiB):
# 10.30 * 1.4 = 14.42 -> 15 MiB. Image still within budget (101.61/112).
BINARY_MAX_BYTES = 15 * MIB
# The distroless runtime has no shell, coreutils or grep: file checks read
# docker cp archives from a never-started container instead of exec helpers.
BINARY = "/home/nonroot/two-bot"
CA_BUNDLE = "/etc/ssl/certs/ca-certificates.crt"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def docker(*args, timeout=30, check=True, text=True):
    return subprocess.run(
        ["docker", *args], capture_output=True, text=text, timeout=timeout, check=check
    )


def image_file(container, path):
    """Return (tar header, bytes) for one regular file, following symlinks."""
    archive = docker("cp", "--follow-link", f"{container}:{path}", "-", timeout=60, text=False).stdout
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as tar:
        members = tar.getmembers()
        require(len(members) == 1 and members[0].isreg(), f"{path} must be one regular file")
        return members[0], tar.extractfile(members[0]).read()


def pid1_uids(name):
    """Real, effective, saved and filesystem uid of the container's PID 1."""
    pid = json.loads(docker("inspect", name).stdout)[0]["State"]["Pid"]
    require(isinstance(pid, int) and pid > 0, "container PID 1 has no host process")
    # `docker top` runs ps beside the daemon and keeps only this container's
    # processes; select PID 1 by its host PID rather than by row order.
    table = docker("top", name, "-o", "pid,ruid,euid,suid,fsuid").stdout.split("\n")
    require(table[0].split() == ["PID", "RUID", "EUID", "SUID", "FSUID"], "unexpected docker top header")
    rows = [line.split() for line in table[1:] if line.split()[:1] == [str(pid)]]
    require(len(rows) == 1 and len(rows[0]) == 5, "docker top did not report PID 1")
    return [int(value) for value in rows[0][1:]]


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


def smoke(image, image_max_bytes=IMAGE_MAX_BYTES, binary_max_bytes=BINARY_MAX_BYTES):
    metadata = json.loads(docker("image", "inspect", image).stdout)[0]
    # Resolve the tag once; measurement and every image probe use this ID.
    image = metadata["Id"]
    report(f"Docker storage-driver Size (diagnostic only): {metadata['Size']} bytes")
    # Sum Docker's uncompressed history layer sizes. Containerd inspect Size
    # also counts compressed content blobs, so it is not the budget metric.
    history = docker("history", "--no-trunc", "--human=false", "--format", "{{.Size}}", image).stdout
    records = [line.strip() for line in history.splitlines()]
    require(records, "Docker history returned no layer sizes")
    require(all(record.isascii() and record.isdecimal() for record in records),
            "Docker history layer sizes must be nonempty nonnegative integers")
    measured_image_bytes = sum(int(record) for record in records)
    require(measured_image_bytes > 0, "Docker history returned only zero-size layers")
    # Created, never started, and named (not unnamed) so a timed-out Docker
    # client cannot leave an orphan behind; same memory cap as the main run.
    measure = "two-bot-measure-" + uuid.uuid4().hex
    try:
        docker("create", "--name", measure, "--memory", "256m", "--network", "none", image)
        binary, _ = image_file(measure, BINARY)
        binary_bytes = binary.size
        bundle_header, bundle = image_file(measure, CA_BUNDLE)
    finally:
        docker("rm", "--force", measure, check=False)
    for label, size, limit in (
        ("image (summed uncompressed Docker history layer bytes)", measured_image_bytes, image_max_bytes),
        ("release binary", binary_bytes, binary_max_bytes),
    ):
        report(f"{label}: {size} bytes ({size / MIB:.2f} MiB); budget {limit} bytes ({limit / MIB:.2f} MiB)")
    require(measured_image_bytes <= image_max_bytes, "image exceeds size budget")
    require(binary_bytes <= binary_max_bytes, "release binary exceeds size budget")
    config = metadata["Config"]
    require(config.get("User") not in (None, "", "root", "0", "0:0"), "image must specify a non-root user")
    require(config.get("Healthcheck", {}).get("Test") == ["CMD", BINARY, "--healthcheck"],
            "image HEALTHCHECK must invoke the runtime's --healthcheck")
    # Any account (including the configured non-root user) must be able to
    # read the trust bundle; no network or OpenSSL helper is needed to verify
    # that certificate data is present in the runtime image.
    pem = bundle.decode("utf-8", errors="replace")
    require("-----BEGIN CERTIFICATE-----" in pem and "-----END CERTIFICATE-----" in pem,
            "runtime CA bundle must contain PEM certificates")
    require("-----BEGIN CERTIFICATE-----" in pem.splitlines() and bundle_header.mode & 0o004,
            "runtime trust bundle must contain PEM certificates readable by the runtime user")
    report("PASS runtime trust bundle contains readable PEM certificate data")

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
            ["process", "ready"], ["gateway", "down"], ["database", "down"],
            ["token_invalid", "ready"],
        ], f"/readyz body must report a ready process, parked gateway, database down and valid token state; got status={code} body={json.dumps(body)[:2000]}")
        # The runtime always reports informational job status alongside
        # readiness; with no credentials all eleven jobs must be parked,
        # non-running and never started. Jobs never flip the 503 above. The
        # audit-retry, scheduled-messages, self-role recovery, feeds and
        # member-unban-sweep entries are always listed (parked when their
        # services are unregistered).
        parked = {"parked": True, "running": False, "last_start": None,
                  "last_success": None, "last_error_class": None,
                  "consecutive_failures": 0}
        require(body.get("jobs") == {
            name: dict(parked) for name in (
                "counter", "rank", "scheduled_events", "presence_probe",
                "community_scorecard", "inactivity", "audit_retry",
                "self_role_recovery", "scheduled_messages", "feeds",
                "member_unban_sweep",
            )
        }, "/readyz body must report all eleven jobs parked, non-running, never started")
        # Check PID 1, not merely Docker's configured user or an exec helper.
        uid = pid1_uids(name)
        require(len(uid) == 4 and all(value != 0 for value in uid), "runtime PID 1 is root")
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
    except (RuntimeError, subprocess.SubprocessError, ValueError, KeyError) as error:
        raise SystemExit(f"FAIL container smoke: {error}") from error


if __name__ == "__main__":
    main()
