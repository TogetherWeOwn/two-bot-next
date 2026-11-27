"""Opt-in direct-container rehearsal. Run with --help for the private handoff contract."""

import argparse
import contextlib
from dataclasses import dataclass, field
from functools import partial
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time
import uuid


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from pre_target import Inventory, Target, TargetError, check_inventory
from script_intake import decode_legacy_json, unique_object

from fixtures.predecessor_discord import GATEWAY, REST, TOKEN
from fixtures.predecessor_snapshots import GUILD, schema_check, snapshot_sql


MOCK = Path(__file__).with_name("fixtures") / "predecessor_discord.py"
# These local inputs are bounded by the pin reader, mock probe or Docker daemon.
decode_json = partial(decode_legacy_json, object_pairs_hook=unique_object, parse_constant=int)
SOURCE = "b689bc3545777c801b8758bcb7160e72a0321724"
DIGEST = r"sha256:[a-f0-9]{64}"
STAMPS = dict(zip(
    ("build_revision", "build_id", "deploy_id"),
    ("org.opencontainers.image.revision", "com.togetherweown.build-id",
     "com.togetherweown.deploy-id"),
))


class Refused(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Refused(message)


@dataclass(frozen=True)
class AppDatabaseReadback:
    """Fresh authenticated app inventory and its effective synthetic binding."""

    inventory: Inventory
    database_url: str = field(repr=False)


def run_bound_endpoint(docker, image, postgres, mock_image, stamps, *, target, expected_app_id, readback):
    """Rehearse the PRE shape of an independently admitted app.

    The caller pins the reservation and app UUID independently of readback.
    It authenticates both observations together for the exact app and scope.
    The observed URL is evidence only. The rehearsal connects to its own
    loopback snapshot using the existing local fixture configuration.
    """
    require(type(target) is Target and type(readback) is AppDatabaseReadback,
            "missing isolated PRE admission or readback")
    try:
        check_inventory(target, readback.inventory)
    except TargetError:
        raise Refused("isolated PRE app inventory mismatch") from None
    app = readback.inventory.container
    require(app is not None and type(expected_app_id) is str and app.id == expected_app_id,
            "isolated PRE app identity mismatch")
    url = readback.database_url
    require(type(url) is str and url == target.synthetic_database_url,
            "isolated PRE database identity mismatch")
    return run_endpoint(docker, image, postgres, mock_image, stamps, int(target.shape.split("/")[0]))


def verify_image(docker, pin, synthetic):
    require(re.fullmatch(DIGEST, pin["image_id"]) and re.fullmatch(DIGEST, pin["manifest_digest"]),
            "invalid image digest pin")
    stamps = {key: pin[key] for key in STAMPS}
    require(re.fullmatch(r"[a-f0-9]{40}", stamps["build_revision"])
            and (synthetic or stamps["build_revision"] == SOURCE)
            and all(re.fullmatch(r"[A-Za-z0-9.+_-]{1,128}", value) and value != "unknown"
                    for value in stamps.values()), "invalid predecessor stamp pin")
    image, = decode_json(docker("image", "inspect", pin["image_id"]))
    config = image["Config"]
    labels = config.get("Labels") or {}
    require(image["Id"] == pin["image_id"] and image["Os"] == "linux"
            and image["Architecture"] == "amd64" and not config.get("Volumes")
            and (synthetic or config["Entrypoint"] == ["/home/nonroot/two-bot"])
            and all(labels.get(label) == stamps[key] for key, label in STAMPS.items()),
            "local image identity or stamps differ from the admitted pin")
    return stamps


def verify_support(docker, postgres, mock):
    db, server = decode_json(docker("image", "inspect", postgres, mock))
    require(db["Id"] == postgres and server["Id"] == mock
            and "PG_MAJOR=17" in db["Config"]["Env"]
            and set(db["Config"]["Volumes"]) == {"/var/lib/postgresql/data"}
            and not server["Config"].get("Volumes"), "unexpected fixture support image config")


def wait_for(probe, message, seconds=35):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        result = probe()
        if result:
            return result
        time.sleep(0.2)
    raise Refused(message)


@contextlib.contextmanager
def disposable(docker, prefix):
    try:
        yield
    finally:
        # Removing the namespace owner kills its PID namespace even if
        # Docker's start reply for a joiner was lost. --rm removes its tmpfs.
        for suffix in ("-db", "-bot", "-mock"):
            try:
                docker("rm", "-f", prefix + suffix, missing_ok=True)
            except (Refused, OSError, subprocess.SubprocessError):
                # A lost reply or an auto-removal race is resolved by readback.
                pass
        wait_for(lambda: not docker("ps", "-aq", "--filter", f"label=offline-pre={prefix}").strip(),
                 "fixture container teardown unverified", seconds=5)


def start_fixture_bot(docker, mock, reservation, *command):
    readback = docker("exec", mock, "python3", "-B", "/fixture.py", "--check-peers", reservation,
                      missing_ok=True)
    peers = decode_json(readback or "{}")
    require(type(peers) is dict and peers.get("gateway") is True, "isolated Discord gateway fixture unconfirmed")
    require(peers.get("rest") is True, "isolated Discord REST fixture unconfirmed")
    docker(*command)


def run_endpoint(docker, image, postgres, mock_image, stamps, tip):
    setup = snapshot_sql(tip)
    prefix = "offline-pre-" + uuid.uuid4().hex
    db, mock, bot = (prefix + suffix for suffix in ("-db", "-mock", "-bot"))
    common = ("run", "-d", "--rm", "--pull=never", "--no-healthcheck", "--label", f"offline-pre={prefix}")
    joined = ("--network", f"container:{db}", "--pid", f"container:{db}",
              "--read-only", "--cap-drop=ALL", "--security-opt=no-new-privileges")
    with disposable(docker, prefix):
        # PID 1 bounds the lifetime even if the host harness is killed. All
        # listeners use the shared loopback of an interface-less namespace.
        docker(*common, "--name", db, "--network=none", "--tmpfs",
               "/var/lib/postgresql/data:rw,size=256m,mode=1777",
               "-e", "POSTGRES_PASSWORD=offline-admin", "--entrypoint", "/usr/bin/timeout",
               postgres, "-k", "5s", "150s", "docker-entrypoint.sh", "postgres",
               "-c", "listen_addresses=127.0.0.1")

        def sql(query):
            return docker("exec", "-i", db, "psql", "-XqAt", "-v", "ON_ERROR_STOP=1",
                          "-U", "postgres", input_data=query, timeout=30).strip()

        wait_for(lambda: docker("exec", db, "pg_isready", "-h", "127.0.0.1",
                                missing_ok=True), "PostgreSQL did not start")
        sql(setup)
        check = schema_check(tip)
        require(sql(check) == "t", "synthetic PRE snapshot readback failed")
        require(sql("SELECT NOT EXISTS (SELECT FROM gateway_sessions);") == "t",
                "new PRE snapshot already has a gateway checkpoint")
        docker(*common, *joined, "--name", mock, "--user=1000:1000",
               "-e", f"OFFLINE_PRE_RESERVATION={prefix}",
               "--mount", f"type=bind,src={MOCK},dst=/fixture.py,readonly",
               "--mount", f"type=bind,src={ROOT / 'scripts/script_intake.py'},dst=/script_intake.py,readonly",
               "--entrypoint", "python3", mock_image, "-B", "/fixture.py")

        def probe(target, path):
            return decode_json(docker("exec", mock, "python3", "-B", "/fixture.py", target, path) or "null")

        wait_for(lambda: probe("mock", "/fixture"), "mock did not start")
        env = {
            "DISCORD_GATEWAY_URL": GATEWAY,
            "DISCORD_API_BASE": REST,
            "LISTEN_ADDR": "127.0.0.1:8080",
            "DISCORD_TOKEN": TOKEN,
            "GUILD_ID": GUILD,
            "DATABASE_URL": "postgres://offline_bot:offline-fixture@127.0.0.1/postgres",
            "TWO_DATABASE_TLS": "local-only",
        }
        start_fixture_bot(docker, mock, prefix, *common, *joined, "--restart=no", "--name", bot, "--user=65532:65532",
                          *(arg for key, value in env.items() for arg in ("-e", f"{key}={value}")), image)
        health = wait_for(lambda: probe("bot", "/health"), "process did not answer")
        require(health == [200, {"status": "ok"}], "process liveness response invalid")
        version = probe("bot", "/version")
        require(version == [200, {"sha": stamps["build_revision"], "build_id": stamps["build_id"],
                                 "deploy_id": stamps["deploy_id"]}], "version stamp mismatch")

        def readiness():
            response = probe("bot", "/readyz")
            if response is None:
                return None
            status, body = response
            require(isinstance(body, dict) and all(body.get(key) == value for key, value in stamps.items()),
                    "readiness stamp mismatch")
            pairs = body["components"]
            components = dict(pairs)
            require(len(pairs) == len(components) and components.get("process") == "ready",
                    "readiness component report invalid")
            return status, components

        status, components = wait_for(readiness, "missing readiness report")
        require(status == 503 and components.get("database") == "ready"
                and components.get("gateway") in ("starting", "down"), "pre-READY state invalid")
        require(probe("mock", "/release")[0] == 200, "mock READY release failed")

        def ready():
            result = readiness()
            return result and result[0] == 200 and all(result[1].get(name) == "ready"
                                                       for name in ("database", "gateway"))

        wait_for(ready, "database and mock gateway did not become ready")
        require(probe("mock", "/fixture") == [200, {"ready_sent": True}], "mock READY was not sent")
        require(sql("SELECT count(*) FROM gateway_sessions WHERE "
                    f"guild_id = '{GUILD}' AND shard_id = 0 "
                    "AND session_id = 'offline-fixture-session' AND seq = 1;") == "1",
                "mock gateway checkpoint missing from the running database")
        require(sql(check) == "t", "PRE snapshot changed during startup")
    return {"cutover": tip, "store": 465, "process": "live", "database": "ready",
            "mock_gateway": "checkpointed", "pre_ready_status": 503, "ready_status": 200}


def main():
    parser = argparse.ArgumentParser(description=__doc__, epilog=(
        "Provide a private JSON pin from the protected admission handoff with image_id "
        "(the admitted OCI config digest), manifest_digest, build_revision, build_id and deploy_id. "
        "Load the admitted image unchanged on the local Docker daemon first. Supply trusted local "
        "image IDs for PostgreSQL 17 and a support image with Python 3. Use --synthetic only "
        "with a test double. The historical Worker cannot forward the loopback overrides."))
    parser.add_argument("--pin", type=Path, required=True)
    parser.add_argument("--postgres", required=True)
    parser.add_argument("--mock-image", required=True)
    parser.add_argument("--synthetic", action="store_true")
    args = parser.parse_args()

    def cancel(*_):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, cancel)
    try:
        require(all(re.fullmatch(DIGEST, image) for image in (args.postgres, args.mock_image)),
                "support images must use immutable local image IDs")
        with args.pin.open("rb") as stream:
            raw = stream.read(4097)
        require(len(raw) <= 4096, "private image pin exceeds the fixture limit")
        pin = decode_json(raw)
        with tempfile.TemporaryDirectory(prefix="offline-pre-docker-") as config:
            def docker(*command, input_data=None, timeout=10, missing_ok=False):
                result = subprocess.run(
                    ["docker", "--config", config, "--host", "unix:///var/run/docker.sock", *command],
                    env={"PATH": os.defpath}, input=input_data, capture_output=True, text=True,
                    timeout=timeout, check=False,
                )
                if result.returncode == 0:
                    return result.stdout
                if missing_ok and (command[0] == "exec"
                                   or "No such container" in result.stderr
                                   or "removal of container" in result.stderr):
                    return ""
                raise Refused(f"Docker {command[0]} failed")

            stamps = verify_image(docker, pin, args.synthetic)
            verify_support(docker, args.postgres, args.mock_image)
            endpoints = [run_endpoint(docker, pin["image_id"], args.postgres, args.mock_image,
                                      stamps, tip) for tip in (473, 475)]
        print(json.dumps({"scope": "direct-container-only", "synthetic_image": args.synthetic,
                          "image_id": pin["image_id"], "admitted_manifest_digest": pin["manifest_digest"],
                          **stamps, "endpoints": endpoints}))
    except Refused as error:
        parser.exit(1, f"Fixture refused: {error}\n")
    except KeyboardInterrupt:
        parser.exit(130, "Fixture cancelled\n")
    except (KeyError, ValueError, TypeError, OSError, RecursionError, subprocess.SubprocessError):
        parser.exit(1, "Fixture refused: malformed input or local command failed\n")


if __name__ == "__main__":
    main()
