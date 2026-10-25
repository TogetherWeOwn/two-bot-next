"""Smoke-test a locally loaded runtime image, without Discord or a database.

Cleanup failure exits nonzero even if all smoke checks passed. Removal is
attempted only for this invocation's generated container names. If a smoke
check already failed, preserve that primary failure and report cleanup
failure separately with fixed stage/reason vocabulary, never Docker bodies.
"""

import argparse
from contextlib import contextmanager
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
# Recalibrated for the linked S4 self-role runtime: PR head measured
# 10,805,344 bytes (10.30 MiB) on the ephemeral runner vs main
# baseline 10,377,112 bytes (9.90 MiB) at ec49663. Growth is linked
# runtime/handlers/REST + previously-dead domain/store code, no new
# dependencies; release profile already minimal (opt-level=z, lto, strip).
# Per b1-baseline calibration (measured * 1.4 rounded up to the next MiB):
# 10.30 * 1.4 = 14.42 -> 15 MiB. Image still within budget (101.61/112).
# 16 MiB holds for the landed slices: the internal-actions families (event
# executors plus channel-moderation union) measured 15,741,344 bytes
# (15.01 MiB), and the RSVP bound-admissions slice (PR 709) measured
# 15,734,960 bytes (15.01 MiB) vs main 15,712,080 bytes — each a few tens of
# KiB over the old 15 MiB budget with no new dependencies. The settings
# hot-reload head measured 15,730,600 bytes (15.00 MiB), also below this
# limit. This branch's automation-only head measured 15,769,872 bytes
# (15.04 MiB) before the event-executor merge; the combined-head
# measurement comes from hosted CI on the merged head. 16 MiB trips on main
# itself (main-red) with no new dependencies and the same minimal release
# profile (opt-level=z, lto, strip). Main at d7774644 measured 16,768,848 bytes
# (15.99 MiB), main tip 89503f71 measured 16,776,096 bytes (1,120 bytes of
# headroom), and another calibration (main 89c859b1, run 39061354880,
# byte-identical on main HEAD 89c859b) measured 16,777,376 bytes (160 over);
# this head measured 16,778,152 bytes (936 over), including this branch's
# feed-fetch outcome gauges (~2,240 bytes of bounded-vocabulary counters and
# pure mapping functions; PR merge head 16,778,336 bytes, 1,120 over 16 MiB).
# The stream retention-expiry slice (ac8fd66, fence-preserving expiry of
# delivered go-live dedup rows) tipped the full 16 MiB budget with no new
# dependencies and no release-profile change. The backfill rate-limit client
# head measured 16,780,912 bytes (16.00 MiB) — the guard's bucket/pacing code.
# The voice rename-decision logs add ~3,160 bytes (merged head 6a9c394e:
# 16,779,256 bytes, 2,040 over 16 MiB). The game-activity aggregate-expiry
# slice measured 16,782,608 bytes (16.01 MiB, 5,392 over) — store expiry plus
# its per-guild stale listing and the retention-cadence driver. The go-live
# receiver-ledger slice independently measured 16,801,560 bytes (16.02 MiB,
# 24,344 over) on CI run 39056648109 job 117470241279 after ~450 lines of
# ledger/receiver code. The landed slices since the 16 MiB bump consumed its
# headroom, so 17 MiB restores a working tripwire with ~1 MiB of headroom;
# revisit if the binary approaches it.
# The voice actor-init retry measured 16,782,904 bytes (+6,808, no new dependencies).
# 2026-10-23 recalibration: main measured 17,761,096 bytes (16.94 MiB) in
# hosted CI (check.yml run 39957230025), leaving ~64 KiB of headroom. The
# giveaway create/entry slice (runtime, admission, REST paths, countdown
# supervisor job; PR #1662 merge build, run 39986772829) adds ~71 KiB with
# no new dependencies and no release-profile change (still opt-level=z,
# lto, strip): 17,833,864 bytes (17.01 MiB), 8,072 over 17 MiB. 18 MiB
# restores ~1 MiB of headroom while keeping a working tripwire; revisit if
# the binary approaches it.
# 2026-10-24 merged-tree re-measurement: PR #1662 head f3e325d3 merged with
# main 87d53098 (including the REST-acquisition 1d8129b1 and dispatcher
# e07140dd repairs) measures 17,854,776 bytes (17.03 MiB) in hosted CI
# (run 40094727805), 28,984 over 17 MiB. 18 MiB stands on merged-tree
# evidence with no new dependencies and no release-profile change.
# 2026-10-24: the feed pause/resume slice (~24,800 bytes of command, store
# and poller code, no new dependencies, same minimal release profile)
# measured 17,831,440 bytes (5,648 over 17 MiB) on its pre-giveaway base
# (CI run 40099776675 job 120889388251). The giveaway+feed union below the
# 18 MiB ceiling is confirmed by this merge's own container-smoke run.
BINARY_MAX_BYTES = 18 * MIB
# CLI refusal bounds (not budget policy): --image-max-bytes/--binary-max-bytes
# outside 1..cap are unusable — a non-positive budget can never pass and a
# gigantic one silently disables the budget — so main() refuses them with a
# fixed message before any Docker call. The caps sit far above any sane
# override (defaults are 112 MiB/18 MiB) and far below budget-disabling
# magnitudes; sane values pass through byte-identically.
IMAGE_BUDGET_CAP_BYTES = 1024 * MIB
BINARY_BUDGET_CAP_BYTES = 256 * MIB
# The distroless runtime has no shell, coreutils or grep: file checks read
# docker cp archives from a never-started container instead of exec helpers.
BINARY = "/home/nonroot/two-bot"
CA_BUNDLE = "/etc/ssl/certs/ca-certificates.crt"
# Body-cap refusal contract (same shape as staging_probe.py MAX_BODY_BYTES and
# the contract probe BODY_CAP): every response read takes at most
# MAX_BODY_BYTES+1 bytes so a hidden suffix is detected. Any body longer than
# MAX_BODY_BYTES (64 KiB) is refused before parsing with a fixed message;
# exact-cap valid input passes, and the extra byte never reaches the JSON
# path. The refusal never echoes the body, headers, URL or parse exceptions.
MAX_BODY_BYTES = 64 * 1024
USER_AGENT = "two-bot-next-container-smoke/1.0 (loopback)"


class BodyTooLarge(RuntimeError):
    """A loopback response body exceeds MAX_BODY_BYTES (fixed vocabulary only).

    A RuntimeError so main() reports it as a smoke failure, and not a
    transport miss the caller would retry until its deadline.
    """


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def check_budget(value, flag, cap, cap_label):
    """Refuse an unusable CLI size budget before any Docker call.

    Raises RuntimeError with a fixed message naming the flag; main()
    reports it through the FAIL container smoke contract. The supplied
    value is never echoed.
    """
    if not 1 <= value <= cap:
        raise RuntimeError(f"refusing: {flag} must be a positive size within {cap_label}")


def docker(*args, timeout=30, check=True, text=True):
    return subprocess.run(
        ["docker", *args], capture_output=True, text=text, timeout=timeout, check=check
    )


def inspect_state(name):
    """The State object of `docker inspect NAME`, or a clean FAIL.

    A non-JSON reply, an empty inspect array or a missing/non-object
    entry/State is a malformed-probe FAIL naming the inspect check --
    never an IndexError/KeyError/TypeError traceback, and Docker output
    is never echoed into the failure.
    """
    try:
        entries = json.loads(docker("inspect", name).stdout)
    except ValueError:
        raise RuntimeError("docker inspect output is not valid JSON") from None
    require(isinstance(entries, list) and entries, "docker inspect returned no entries")
    require(isinstance(entries[0], dict), "docker inspect entry must be a JSON object")
    state = entries[0].get("State")
    require(isinstance(state, dict), "docker inspect entry has no State")
    return state


def image_file(container, path):
    """Return (tar header, bytes) for one regular file, following symlinks."""
    archive = docker("cp", "--follow-link", f"{container}:{path}", "-", timeout=60, text=False).stdout
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as tar:
        members = tar.getmembers()
        require(len(members) == 1 and members[0].isreg(), f"{path} must be one regular file")
        return members[0], tar.extractfile(members[0]).read()


def pid1_uids(name):
    """Real, effective, saved and filesystem uid of the container's PID 1."""
    pid = inspect_state(name).get("Pid")
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


def report_diagnostic(message):
    """Diagnostic I/O must not replace the smoke or cleanup failure."""
    try:
        report(message)
    except OSError:
        pass


class CleanupFailure(RuntimeError):
    """An owned container could not be removed (fixed vocabulary only)."""


@contextmanager
def owned_container(name, stage):
    """Attempt removal even when creation times out or a smoke check fails."""
    primary_failed = False
    try:
        yield
    except BaseException:
        primary_failed = True
        raise
    finally:
        reason = None
        try:
            result = docker("rm", "--force", name, timeout=30, check=False)
            if result.returncode != 0:
                reason = "nonzero removal exit"
        except subprocess.TimeoutExpired:
            reason = "removal timed out"
        except (OSError, subprocess.SubprocessError):
            reason = "removal process failed"
        if reason is not None:
            message = f"FAIL cleanup {stage}: {reason}"
            report_diagnostic(message)
            if not primary_failed:
                raise CleanupFailure(message) from None


def http_response(url):
    # A hand-rolled connection (not urlopen) so any 3xx is returned as-is;
    # following a redirect would let one healthy endpoint masquerade as
    # another and falsely satisfy the contract.
    parts = urlsplit(url)
    connection = http.client.HTTPConnection(parts.hostname, parts.port, timeout=2)
    try:
        connection.request("GET", parts.path or "/", headers={"User-Agent": USER_AGENT})
        response = connection.getresponse()
        # Bounded read: a status line alone is an answer, a body past the cap
        # is a refusal raised before any decoding (not caught below).
        raw = response.read(MAX_BODY_BYTES + 1)
        if len(raw) > MAX_BODY_BYTES:
            raise BodyTooLarge("loopback response body exceeds the 64 KiB cap")
        if response.length:
            # read(amt) returns a short Content-Length body without raising
            # (unlike an unbounded read); a connection dropped mid-body is
            # still a transport miss for the caller to retry.
            raise http.client.IncompleteRead(raw, response.length)
        try:
            return response.status, json.loads(raw)
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
    image_entries = json.loads(docker("image", "inspect", image).stdout)
    require(isinstance(image_entries, list) and image_entries,
            "docker image inspect returned no entries")
    require(isinstance(image_entries[0], dict),
            "docker image inspect entry must be a JSON object")
    metadata = image_entries[0]
    # The Id is passed to docker() as a subprocess argument, so a
    # present-but-non-string Id must FAIL here, not as a TypeError.
    require(isinstance(metadata.get("Id"), str), "docker image inspect entry has no Id")
    require("Size" in metadata, "docker image inspect entry has no Size")
    # An empty Id would reach docker as a vacuous argument; reject it with
    # the field named, like every other malformed image field here.
    require(metadata["Id"], "docker image inspect Id must be a nonempty string")
    # Config and Healthcheck shapes are validated here, with the rest of
    # the image probe, so a malformed entry fails before the history call
    # and before any container exists.
    require(isinstance(metadata.get("Config"), dict),
            "docker image inspect entry has no Config")
    require(isinstance(metadata["Config"].get("Healthcheck", {}), dict),
            "docker image inspect Config Healthcheck must be an object")
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
    with owned_container(measure, "measurement"):
        docker("create", "--name", measure, "--memory", "256m", "--network", "none", image)
        binary, _ = image_file(measure, BINARY)
        binary_bytes = binary.size
        bundle_header, bundle = image_file(measure, CA_BUNDLE)
    for label, size, limit in (
        ("image (summed uncompressed Docker history layer bytes)", measured_image_bytes, image_max_bytes),
        ("release binary", binary_bytes, binary_max_bytes),
    ):
        report(f"{label}: {size} bytes ({size / MIB:.2f} MiB); budget {limit} bytes ({limit / MIB:.2f} MiB)")
    require(measured_image_bytes <= image_max_bytes, "image exceeds size budget")
    require(binary_bytes <= binary_max_bytes, "release binary exceeds size budget")
    # Config and Healthcheck shapes were validated with the image probe
    # above, before the history call, so both are objects here. An absent
    # Healthcheck keeps the {} default and fails on the Test assertion below.
    config = metadata["Config"]
    require(config.get("User") not in (None, "", "root", "0", "0:0"), "image must specify a non-root user")
    healthcheck = config.get("Healthcheck", {})
    require(healthcheck.get("Test") == ["CMD", BINARY, "--healthcheck"],
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
    with owned_container(name, "main"):
        try:
            docker(
                "run", "--detach", "--name", name, "--memory", "256m",
                "--publish", "127.0.0.1::8080", "--health-interval", "1s",
                "--health-start-period", "0s", image,
            )
            port = docker("port", name, "8080/tcp").stdout.strip()
            require(port.startswith("127.0.0.1:"), f"unexpected published port: {port}")
            url = "http://" + port
            # Each wait loop below owns a full 30s budget measured from its
            # own start; the timeout text states the budget its loop enforces.
            deadline = time.monotonic() + 30
            while True:
                code, body = http_response(url + "/health")
                if code == 200:
                    require(body == {"status": "ok"}, "/health body must report status ok")
                    break
                require(time.monotonic() < deadline, "/health did not return 200 within 30s")
                state = inspect_state(name)
                require("Running" in state, "docker inspect State has no Running")
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
            # readiness; with no credentials all sixteen jobs must be parked,
            # non-running and never started. Jobs never flip the 503 above. The
            # audit-retry, scheduled-messages, self-role recovery, feeds,
            # settings-poll, member-unban-sweep, LFG sweep, retention and
            # giveaway-countdown/draw entries are always listed (parked when
            # their services are unregistered).
            parked = {"parked": True, "running": False, "last_start": None,
                      "last_success": None, "last_error_class": None,
                      "consecutive_failures": 0}
            require(body.get("jobs") == {
                name: dict(parked) for name in (
                    "counter", "rank", "scheduled_events", "settings", "presence_probe",
                    "community_scorecard", "inactivity", "audit_retry",
                    "self_role_recovery", "scheduled_messages", "feeds",
                    "member_unban_sweep", "retention", "lfg_sweep", "giveaway_countdown",
                    "giveaway_draw",
                )
            }, "/readyz body must report all sixteen jobs parked, non-running, never started")
            # Check PID 1, not merely Docker's configured user or an exec helper.
            uid = pid1_uids(name)
            require(len(uid) == 4 and all(value != 0 for value in uid), "runtime PID 1 is root")
            require(docker("exec", name, BINARY, "--healthcheck", timeout=5, check=False).returncode == 0,
                    "--healthcheck must exit 0 against the live process")
            # The HEALTHCHECK wait restarts the budget: time the /health loop
            # consumed must not shrink this loop's documented 30s.
            deadline = time.monotonic() + 30
            while True:
                state = inspect_state(name)
                require("Running" in state, "docker inspect State has no Running")
                require(state["Running"], "container exited during healthcheck")
                health = state.get("Health")
                require(isinstance(health, dict), "docker inspect entry has no Health")
                if health.get("Status") == "healthy":
                    break
                require(time.monotonic() < deadline, "Docker HEALTHCHECK did not become healthy within 30s")
                time.sleep(0.25)
            report("PASS /health 200; /readyz 503 parked; non-root PID 1; --healthcheck 0; Docker HEALTHCHECK healthy")

            # A fresh no-network process has no listening server: liveness must
            # fail honestly rather than being an unconditional success command.
            # Named (not --rm/unnamed) so a timed-out Docker client cannot leave
            # an orphan behind; cleaned with the same memory cap as the main run.
            probe = name + "-probe"
            with owned_container(probe, "health-probe"):
                result = docker(
                    "run", "--name", probe, "--memory", "256m",
                    "--network", "none", image, "--healthcheck",
                    timeout=30, check=False,
                )
                require(result.returncode == 1, "--healthcheck without a server must exit 1")
            report("PASS --healthcheck exits 1 without a listening server")

            # Send SIGTERM directly; no docker stop fallback may hide SIGKILL.
            started = time.monotonic()
            docker("kill", "--signal", "TERM", name, timeout=2)
            result = docker("wait", name, timeout=max(0.1, 10 - (time.monotonic() - started)))
            elapsed = time.monotonic() - started
            require(elapsed <= 10, f"SIGTERM took {elapsed:.3f}s (maximum 10s)")
            require(result.stdout.strip() == "0", f"SIGTERM exit code was {result.stdout.strip()}, expected 0")
            state = inspect_state(name)
            require("Running" in state, "docker inspect State has no Running")
            require("OOMKilled" in state, "docker inspect State has no OOMKilled")
            require(not state["Running"] and not state["OOMKilled"], "container failed to shut down cleanly")
            report(f"PASS SIGTERM exits 0 in {elapsed:.3f}s (maximum 10s); no OOM")
        except CleanupFailure:
            raise
        except Exception:
            try:
                logs = docker("logs", "--tail", "80", name, check=False)
                print(logs.stdout + logs.stderr)
            except (OSError, subprocess.SubprocessError):
                report_diagnostic("FAIL container smoke: diagnostic logs unavailable")
            raise


def dry_run_plan(image):
    """Print the container-mutation plan without touching Docker or the network.

    Static text plus the image under test: no container names, ports or
    timestamps are generated, so the plan is byte-identical across runs.
    Size budgets are never enforced here because nothing is measured.
    """
    print("\n".join((
        "dry-run plan for container smoke (no mutations performed)",
        f"image: {image}",
        "1. measure: docker create a measurement container (--memory 256m, "
        "--network none), read the binary and CA bundle, then remove it",
        "2. main run: docker run --detach --memory 256m "
        "--publish 127.0.0.1::8080, then probe /health until 200 and assert "
        "/readyz 503 parked over loopback HTTP",
        "3. health-probe: docker run --network none --memory 256m <image> "
        "--healthcheck (expects exit 1), then remove the probe container",
        "4. shutdown: docker kill --signal TERM, docker wait (10s budget), "
        "then remove every owned container",
    )))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image")
    parser.add_argument("--image-max-bytes", type=int, default=IMAGE_MAX_BYTES)
    parser.add_argument("--binary-max-bytes", type=int, default=BINARY_MAX_BYTES)
    parser.add_argument("--dry-run", action="store_true",
                        help="print the container-mutation plan without running anything")
    args = parser.parse_args()
    if args.dry_run:
        dry_run_plan(args.image)
        return
    try:
        check_budget(args.image_max_bytes, "--image-max-bytes", IMAGE_BUDGET_CAP_BYTES, "1 GiB")
        check_budget(args.binary_max_bytes, "--binary-max-bytes", BINARY_BUDGET_CAP_BYTES, "256 MiB")
        smoke(args.image, args.image_max_bytes, args.binary_max_bytes)
    except (RuntimeError, subprocess.SubprocessError, ValueError, KeyError) as error:
        raise SystemExit(f"FAIL container smoke: {error}") from error


if __name__ == "__main__":
    main()
