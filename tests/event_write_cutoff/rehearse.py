"""Exercise the event-denied fallback against two locally present immutable images."""

import argparse
import contextlib
import hashlib
import json
import re
import secrets
import subprocess
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from recorder import CHANNEL, EVENT, GUILD, MEMBER, ROLE

HERE = Path(__file__).resolve().parent
BOT_PORTS = {"current": 8091, "retained": 8092}
REVISION = "org.opencontainers.image.revision"
DIGEST = re.compile(r"[^\s@]+@sha256:[0-9a-f]{64}")
DB_IMAGE = "postgres@sha256:372621a978f4513b082d946d6320e2b11a3b359648b81fba8a2ba0df33958f62"
RECORDER_IMAGE = "node@sha256:e7add2a490bb17d97b485b8641bd88455fff7839ade0f5eba493c3508124d413"


def run(*argv, data=None, timeout=30):
    result = subprocess.run(argv, input=data, capture_output=True, timeout=timeout, check=False)
    if result.returncode:
        raise RuntimeError(f"{argv[0]} {argv[1]} exited {result.returncode}: {result.stderr.decode()[-1200:]}")
    return result.stdout


def require(condition, detail):
    if not condition:
        raise RuntimeError(detail)


def wait_for(check, what, timeout=10):
    end = time.monotonic() + timeout
    while (remaining := end - time.monotonic()) > 0:
        try:
            if check(remaining):
                return
        except subprocess.TimeoutExpired:
            break
        time.sleep(0.05)
    raise RuntimeError(f"Timed out waiting for {what}")


class Rehearsal:
    def __init__(self, work, cleanup, images):
        self.work = work
        self.cleanup = cleanup
        self.images = images
        self.prefix = "event-cutoff-" + secrets.token_hex(6)
        self.pg = self.prefix + "-pg"
        self.recorder = self.prefix + "-recorder"
        self.token = "synthetic-" + secrets.token_hex(24)
        self.key = secrets.token_hex(32)
        self.password = secrets.token_hex(24)
        self.database = "event_cutoff"
        self.generation = 0
        self.write("barrier", "none")
        self.set_policy("open")

    def write(self, name, value):
        temporary = self.work / (name + ".new")
        temporary.write_text(value + "\n")
        temporary.replace(self.work / name)

    def set_policy(self, value):
        self.write("policy", value)
        print(f"policy={value}", flush=True)

    def docker_run(self, name, *args, namespace=None, entrypoint=None, env=None):
        argv = ["docker", "create", "--pull=never", "--name", name,
                "--network", f"container:{namespace}" if namespace else "none"]
        if entrypoint:
            argv.extend(["--entrypoint", entrypoint])
        for key, value in (env or {}).items():
            argv.extend(["-e", f"{key}={value}"])
        container_id = run(*argv, *args).decode().strip()
        self.cleanup.callback(run, "docker", "container", "rm", "--force", "--volumes", container_id)
        run("docker", "start", container_id)

    def sql(self, sql, db=None):
        return run("docker", "exec", "-i", self.pg, "psql", "-XAtq", "-v", "ON_ERROR_STOP=1", "-U",
                   "postgres", "-d", db or self.database, data=sql.encode(), timeout=60).decode().strip()

    def start(self, schema_revision):
        run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-subj", "/CN=Discord Recorder CA", "-keyout", str(self.work / "ca.key"),
            "-out", str(self.work / "ca.crt"))
        run("openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=discord.com",
            "-keyout", str(self.work / "server.key"), "-out", str(self.work / "server.csr"))
        (self.work / "server.ext").write_text("subjectAltName=DNS:discord.com\nextendedKeyUsage=serverAuth\n")
        run("openssl", "x509", "-req", "-days", "1", "-in", str(self.work / "server.csr"), "-CA",
            str(self.work / "ca.crt"), "-CAkey", str(self.work / "ca.key"), "-CAcreateserial",
            "-extfile", str(self.work / "server.ext"), "-out", str(self.work / "server.crt"))
        self.docker_run(self.pg, "--add-host", "discord.com:127.0.0.1", DB_IMAGE, "postgres",
                        "-c", "listen_addresses=127.0.0.1", env={"POSTGRES_PASSWORD": self.password,
                                                                  "POSTGRES_DB": self.database})
        wait_for(lambda remaining: subprocess.run(
            ["docker", "exec", self.pg, "pg_isready", "-h", "127.0.0.1", "-q"], check=False, timeout=remaining
        ).returncode == 0, "Postgres")
        for name in ("store", "cutover"):
            folder = f"crates/{name}/migrations"
            paths = run("git", "ls-tree", "-r", "--name-only", schema_revision, folder).decode().splitlines()
            require(bool(paths), f"Empty {name} migrations")
            for path in paths:
                if path.endswith(".sql"):
                    sql = run("git", "show", f"{schema_revision}:{path}").decode()
                    self.sql(f"BEGIN;\n{sql}\nCOMMIT;")
        self.sql(run("git", "show", f"{schema_revision}:sql/web_v1.sql").decode())
        self.sql(f"INSERT INTO internal_event_keys (guild_id, event_key, event_id) VALUES ('{GUILD}', 'mapped', '{EVENT}');")
        self.start_recorder()

    def start_recorder(self):
        (self.work / "recorder-ready").unlink(missing_ok=True)
        args = ("--mount", f"type=bind,src={self.work},dst=/fixture",
                "--mount", f"type=bind,src={HERE},dst=/source,readonly",
                RECORDER_IMAGE, "/source/recorder.py")
        if self.was_started(self.recorder):
            run("docker", "start", self.recorder)
        else:
            self.docker_run(self.recorder, *args, namespace=self.pg, entrypoint="python3",
                            env={"SYNTHETIC_BOT_TOKEN": self.token, "SYNTHETIC_HMAC_KEY": self.key})
        wait_for(lambda _: (self.work / "recorder-ready").exists(), "TLS recorder")
        interfaces = run("docker", "exec", self.recorder, "cat", "/proc/net/dev").decode()
        require([line.split(":")[0].strip() for line in interfaces.splitlines()[2:]] == ["lo"],
                "External network interface in rehearsal")
        hosts = run("docker", "exec", self.recorder, "getent", "hosts", "discord.com").decode()
        require(hosts.split()[0] == "127.0.0.1", "Discord fixture DNS is not loopback")

    def was_started(self, name):
        return subprocess.run(["docker", "inspect", name], check=False,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10).returncode == 0

    def start_bot(self, which):
        name = self.bot_name(which)
        port = BOT_PORTS[which]
        if self.was_started(name):
            run("docker", "start", name)
        else:
            self.docker_run(
                name, "--mount", f"type=bind,src={self.work / 'ca.crt'},dst=/etc/ssl/certs/ca-certificates.crt,readonly",
                self.images[which], namespace=self.pg, env={
                    "LISTEN_ADDR": f"127.0.0.1:{port + 100}", "DISCORD_TOKEN": self.token,
                    "GUILD_ID": GUILD, "DATABASE_URL": f"postgres://postgres:{self.password}@127.0.0.1/{self.database}",
                    "TWO_DATABASE_TLS": "local-only", "DISCORD_GATEWAY_URL": "ws://127.0.0.1:9999",
                    # Park background REST clients. The shipping internal receiver
                    # ignores this override and uses its actual Discord TLS path.
                    "DISCORD_API_BASE": "invalid fixture background origin",
                    "TWO_INTERNAL_ACTIONS": "1", "TWO_INTERNAL_BIND": f"127.0.0.1:{port}",
                    "TWO_INTERNAL_KEYS": f"rehearsal:{self.key}", "TWO_INTERNAL_CALLERS": "rehearsal:fixture",
                    "TWO_INTERNAL_CHANNEL_KEYS": f"ann:{CHANNEL}", "TWO_INTERNAL_ROLE_KEYS": f"member:{ROLE}",
                    "TWO_INTERNAL_ALLOW_EVENT_CANCEL": "1",
                },
            )
        try:
            wait_for(lambda remaining: subprocess.run(
                ["docker", "exec", self.recorder, "python3", "-c",
                 f"import socket; socket.create_connection(('127.0.0.1',{port}),0.2).close()"],
                check=False, capture_output=True, timeout=remaining,
            ).returncode == 0, f"{which} receiver")
        except RuntimeError:
            print(run("docker", "logs", name).decode()[-4000:], flush=True)
            raise
        info = json.loads(run("docker", "inspect", name))[0]
        network_mode = info["HostConfig"]["NetworkMode"]
        anchor_id = run("docker", "inspect", "--format", "{{.Id}}", self.pg).decode().strip()
        require(network_mode in ("container:" + self.pg, "container:" + anchor_id)
                and not info["HostConfig"]["PortBindings"], "Bot namespace changed")
        print(f"started={which}", flush=True)

    def bot_name(self, which):
        return f"{self.prefix}-{which}-{self.generation}"

    def stop_bot(self, which):
        run("docker", "stop", "-t", "8", self.bot_name(which))

    def request(self, which, key, body):
        result = json.loads(run("docker", "exec", self.recorder, "python3", "/source/request.py",
                                str(BOT_PORTS[which]), key, json.dumps(body)))
        print(json.dumps({"runtime": which, "attempt": key, **result}), flush=True)
        return result

    def journal(self):
        lines = self.work / "journal.jsonl"
        return [tuple(json.loads(line)) for line in lines.read_text().splitlines()] if lines.exists() else []

    def claim(self, key):
        hashed = hashlib.sha256(key.encode()).hexdigest()
        return self.sql(f"SELECT state FROM internal_idempotency WHERE key_hash = '{hashed}';")

    def snapshot_database(self):
        return run("docker", "exec", self.pg, "pg_dump", "-U", "postgres", "--format=p", self.database)

    def restore_database(self, archive):
        original = self.database
        self.database = f"event_cutoff_restored_{self.generation + 1}"
        self.sql(f"CREATE DATABASE {self.database};", "postgres")
        self.sql(archive.decode())
        self.generation += 1
        print(f"restored={original} -> {self.database}", flush=True)

    def paused(self, which, key, body, phase):
        outcome = "paused-before-permission" if phase == "before" else "paused-after-effect"
        since = len(self.journal())
        (self.work / "resume").unlink(missing_ok=True)
        self.write("barrier", phase)
        with ThreadPoolExecutor(max_workers=1) as worker:
            response = worker.submit(self.request, which, key, body)
            try:
                wait_for(lambda _: response.done() or any(row[3] == outcome for row in self.journal()[since:]),
                         outcome, timeout=4)
                require(any(row[3] == outcome for row in self.journal()[since:]),
                        f"Receiver answered before the {outcome} barrier")
                require(self.claim(key) == "in_flight", "Missing committed in-flight claim at barrier")
                require(self.sql("SELECT count(*) FROM discord_send_admission WHERE in_flight;") == "1",
                        "Missing Bot token permit at barrier")
                print(f"observed={outcome} claim=in_flight token_permit=held", flush=True)
                self.set_policy("closed")
            finally:
                (self.work / "resume").touch()
            answer = response.result()
        self.write("barrier", "none")
        return answer

    def exercise(self):
        update = {
            "action": "event.upsert", "event_key": "mapped", "name": "Synthetic event",
            "starts_at": "2099-01-01T00:00:00Z", "ends_at": "2099-01-01T01:00:00Z",
            "location": "Synthetic location",
        }
        create = {**update, "event_key": "unmapped"}
        cancel = {"action": "event.cancel", "event_key": "mapped"}
        role = {"action": "role.assign", "discord_id": MEMBER, "role_key": "member"}
        self.start_bot("current")
        self.start_bot("retained")
        cases = (("create", create), ("edit", update), ("cancel", cancel))
        for which in BOT_PORTS:
            for label, body in cases:
                self.set_policy("open")
                answer = self.paused(which, f"{which}-{label}", body, "before")
                require(answer["code"] == "discord_rejected", "Event denial was not a Discord rejection")
            require(self.request(which, f"{which}-role", role)["outcome"] == "assigned", "Role was not assigned")

        self.stop_bot("current")
        self.stop_bot("retained")
        self.restore_database(self.snapshot_database())
        self.restart_recorder_with_cutoff()
        self.start_bot("retained")
        answer = self.request("retained", "rollback-create", create)
        require(answer["code"] == "discord_rejected", "A restored rollback bypassed the cutoff")
        self.start_bot("current")
        before_lost_claim = self.snapshot_database()
        self.set_policy("open")
        key = "lost-create"
        answer = self.paused("current", key, create, "after")
        require(answer["status"] != 200 and self.claim(key) == "unknown", "Lost response was not held unknown")
        self.stop_bot("current")
        answer = self.request("retained", key, create)
        require(answer["code"] == "needs_reconciliation", "Rollback replay did not preserve ambiguity")
        self.stop_bot("retained")
        self.restore_database(self.snapshot_database())
        self.restart_recorder_with_cutoff()
        self.start_bot("retained")
        require(self.claim(key) == "unknown", "Restored claim lost ambiguity")
        answer = self.request("retained", key, create)
        require(answer["code"] == "needs_reconciliation", "Restored replay did not preserve ambiguity")

        self.stop_bot("retained")
        self.restore_database(before_lost_claim)
        self.restart_recorder_with_cutoff()
        self.start_bot("retained")
        require(self.claim(key) == "", "Preclaim backup unexpectedly contains the lost attempt")
        # Force the orphaned original key through the retained Bot only in
        # this fixture. Web must keep ambiguous work held across this restore.
        answer = self.request("retained", key, create)
        require(answer["code"] == "discord_rejected", "Restore without the claim bypassed the cutoff")

        entries = self.journal()
        denied = [row[:3] for row in entries if row[3] == "denied"]
        events_path = f"/api/v10/guilds/{GUILD}/scheduled-events"
        create_denial = ("POST", events_path, "create")
        denied_per_runtime = [create_denial, ("PATCH", f"{events_path}/{EVENT}", "edit"),
                              ("PATCH", f"{events_path}/{EVENT}", "cancel")]
        require(denied == denied_per_runtime * 2 + [create_denial] * 2,
                "Expected ordered event denials at the fixture routes on both runtimes and after both restores")
        applied = [operation for _, _, operation, outcome in entries if outcome == "applied"]
        require(applied == ["role", "role", "create"], "Expected two role writes and one pre-cutoff event effect")
        require(all(row[3] != "unexpected" for row in entries), "Unexpected fixture mutation route")
        print(json.dumps({"recorder": entries, "event_and_role_verdict": "passed"}), flush=True)

    def restart_recorder_with_cutoff(self):
        run("docker", "stop", "-t", "1", self.recorder)
        self.start_recorder()
        require((self.work / "policy").read_text() == "closed\n", "Restore or restart reopened event authority")

    def announcement_control(self, which):
        self.set_policy("closed")
        self.start_bot(which)
        body = {"action": "announcement.post", "channel_key": "ann", "body": "Synthetic announcement"}
        answer = self.request(which, f"{which}-announcement", body)
        writes = sum(operation == "announcement" and outcome == "applied"
                     for _, _, operation, outcome in self.journal())
        print(json.dumps({"runtime": which, "announcement_recorder_writes": writes}), flush=True)
        return answer["status"] == 200 and writes == 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for which in BOT_PORTS:
        parser.add_argument(f"--{which}", required=True, help=" locally present registry image@sha256:…")
    args = parser.parse_args()
    images = vars(args)
    revisions = {}
    for which, image in images.items():
        require(DIGEST.fullmatch(image), "Bot image must be digest pinned")
        info = json.loads(run("docker", "image", "inspect", image))[0]
        require(image in info["RepoDigests"], "Exact registry digest is absent")
        source = info["Config"]["Labels"].get(REVISION, "")
        require(re.fullmatch(r"[0-9a-f]{40}", source), "Missing exact source revision")
        revisions[which] = source
    print(json.dumps({
        "images": images, "source_revisions": revisions, "postgres": DB_IMAGE, "recorder_image": RECORDER_IMAGE,
        "recorder_sha256": hashlib.sha256((HERE / "recorder.py").read_bytes()).hexdigest(),
        "runner_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
    }), flush=True)
    with tempfile.TemporaryDirectory(prefix="two-bot-event-cutoff-") as directory, contextlib.ExitStack() as cleanup:
        rehearsal = Rehearsal(Path(directory), cleanup, images)
        rehearsal.start(revisions["current"])
        rehearsal.exercise()

    # An ambiguous announcement can hold the token-wide lane. Give each
    # distinct image its own isolated probe so a refusal cannot mask the other.
    seen = set()
    announcements_passed = True
    for which, image in images.items():
        if image in seen:
            continue
        seen.add(image)
        with tempfile.TemporaryDirectory(prefix="two-bot-event-cutoff-") as directory, contextlib.ExitStack() as cleanup:
            rehearsal = Rehearsal(Path(directory), cleanup, images)
            rehearsal.start(revisions["current"])
            passed = rehearsal.announcement_control(which)
            announcements_passed = announcements_passed and passed
    require(announcements_passed, "Announcement positive control did not reach and settle at the recorder")
    print(json.dumps({"verdict": "passed"}), flush=True)


if __name__ == "__main__":
    main()
