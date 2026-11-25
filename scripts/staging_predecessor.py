"""Build and qualify the reviewed attendance rollback pair on isolated staging."""

from datetime import datetime, timezone
from hashlib import file_digest, sha256
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
from urllib.parse import unquote, urlsplit
from urllib.request import Request

from bot_promotion import configure_bundle
from rollback_proof import ProofError
from script_intake import read_with_overflow
from staging_migrate_approval import digest_json
from staging_qualification import check_evidence
from staging_recovery_status import github
from staging_rollout import Client, GateError, decode, mapping, read_records, require, runtime_ready, save
from worker_container_rollback import VERSION_ID, app_rollouts, application_by_name, rollout_image, serving_version, version_sha

SOURCE = "b689bc3545777c801b8758bcb7160e72a0321724"
INPUT_TREE = "217e63471c39fc6177e13cefa87693c2a3c1572a"
WORKER = "two-bot-next-predecessor"
APPLICATION = f"{WORKER}-twobotcontainer"
WORKFLOW = "TogetherWeOwn/two-bot-next/.github/workflows/staging-predecessor.yml@refs/heads/main"
WRANGLER = "4.152.0"
LEDGERS = {
    "original-pre": "0b5332bfd0baa945b045dfa302c814af39df0804988d1e8c6d413519e9ef5290",
    "retained-pre": "b27f21de2a1670d08e7ae2c6e8e47196977f3324f9cf09a102ea0c2c734170c5",
}


def command(args, *, cwd=None, env=None, timeout=30):
    try:
        return subprocess.run(
            args, cwd=cwd, env=env, check=True, timeout=timeout, capture_output=True,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        raise GateError("predecessor_command_failed") from None


def prepare(root, workflow_sha):
    require(command(["git", "rev-parse", "HEAD"]).decode().strip() == workflow_sha, "workflow_checkout_mismatch")
    command(["git", "merge-base", "--is-ancestor", SOURCE, "origin/main"])
    check_evidence(SOURCE, github)
    root.mkdir()
    package, output = root / "package", root / "export"
    command(["git", "worktree", "add", "--detach", str(package), SOURCE])
    try:
        command(["python3", "scripts/predecessor_build_source.py", "--source", SOURCE, str(output)], cwd=package)
    finally:
        command(["git", "worktree", "remove", str(package)])
    tree = command(["git", "rev-parse", f"{SOURCE}^{{tree}}"]).decode().strip()
    receipt = mapping(decode((output / "provenance.json").read_bytes()))
    with (output / "source.tar").open("rb") as archive:
        require(receipt.get("archive_sha256") == file_digest(archive, "sha256").hexdigest()
                and receipt.get("input_tree") == INPUT_TREE and receipt.get("source_tree") == tree
                and receipt.get("source_revision") == receipt.get("worker_message") == SOURCE
                and receipt.get("container_build_args") == {"BOT_BUILD_REVISION": SOURCE}, "export_identity_mismatch")
        archive.seek(0)
        with tarfile.open(fileobj=archive, mode="r:") as tar:
            tar.extractall(root / "source", filter="data")


def target(env):
    endpoint = env["PREDECESSOR_ENDPOINT"]
    host, database = env["PREDECESSOR_DB_HOST"], env["PREDECESSOR_DB_NAME"]
    origin = env["PREDECESSOR_WORKER_URL"]
    require(endpoint in LEDGERS and re.fullmatch(r"[a-z0-9-]+[.]pg[.]psdb[.]cloud", host)
            and "prod" not in host and "-pooler" not in host
            and re.fullmatch(r"two_bot_predecessor(?:_[a-z0-9_]{1,38})?", database)
            and re.fullmatch(rf"https://{WORKER}[.][a-z0-9-]+[.]workers[.]dev", origin)
            and re.fullmatch(r"[0-9]{17,20}", env["PREDECESSOR_GUILD_ID"])
            and re.fullmatch(r"[A-Za-z0-9_-]{32,128}", env["PREDECESSOR_CONTROL_TOKEN"])
            and bool(env["PREDECESSOR_DISCORD_TOKEN"]), "isolated_staging_target_unproved")
    users = []
    for key in ("PREDECESSOR_PLAN_URL", "PREDECESSOR_RUNTIME_URL"):
        uri = urlsplit(env[key])
        # Accept the canonical authority only. libpq must not see a second
        # host, service, options or an overridden database in the query.
        authority = uri.netloc.rsplit("@", 1)[-1]
        require(uri.scheme == "postgresql" and authority in (host, f"{host}:5432")
                and uri.path == f"/{database}" and uri.query == "sslmode=verify-full"
                and not uri.fragment and re.fullmatch(r"[A-Za-z0-9_-]{1,63}", uri.username or "")
                and uri.password and uri.netloc.count("@") == 1, "isolated_database_binding_mismatch")
        users.append(uri.username)
    require(len(set(users)) == 2, "isolated_database_roles_match")
    return endpoint, origin


def database_sample(env, endpoint):
    ledger_query = (
        "SELECT COALESCE(json_agg(row_to_json(ledger) ORDER BY version), '[]') "
        "FROM (SELECT version, description, encode(checksum,'hex') AS sha384, success "
        "FROM public.{} ORDER BY version LIMIT {}) ledger"
    )
    cutover = ledger_query.format("_sqlx_migrations", 117)
    store = ledger_query.format("_two_bot_migrations", 21)
    query = f"""
        SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY;
        SET LOCAL ROLE two_bot_migrator_ro;
        SELECT json_build_object(
            'database', current_database(),
            'readonly_role', current_user = 'two_bot_migrator_ro'
                AND NOT pg_has_role(session_user, 'two_bot_migrator', 'MEMBER')
                AND NOT (SELECT rolsuper FROM pg_roles WHERE rolname = session_user),
            'ledgers', json_build_array(({cutover}), ({store})),
            'retained', EXISTS (SELECT FROM pg_attribute
                WHERE attrelid = 'public.event_instance_windows'::regclass
                    AND attname = 'attendance_recovered_at' AND NOT attisdropped),
            'fence', (SELECT encode(timestamptz_send(installed_on), 'hex')
                FROM public._sqlx_migrations WHERE version = 475 AND success));
    """
    uri = urlsplit(env["PREDECESSOR_PLAN_URL"])
    binding = {"PATH": os.defpath, "PGHOST": uri.hostname, "PGPORT": "5432", "PGUSER": uri.username,
               "PGPASSWORD": unquote(uri.password), "PGDATABASE": env["PREDECESSOR_DB_NAME"],
               "PGCONNECT_TIMEOUT": "8", "PGSSLMODE": "verify-full", "PGSSLROOTCERT": "/etc/ssl/certs/ca-certificates.crt"}
    raw = command(["psql", "-XqAt", "-v", "ON_ERROR_STOP=1", "-1", "-c", query], env=binding)
    observation = mapping(decode(raw))
    retained = endpoint == "retained-pre"
    checksum = digest_json(observation.get("ledgers"))
    require(observation.get("database") == env["PREDECESSOR_DB_NAME"] and observation.get("readonly_role") is True
            and checksum == LEDGERS[endpoint] and observation.get("retained") is retained, "pre_ledger_mismatch")
    fence = observation.get("fence")
    if retained:
        require(isinstance(fence, str) and re.fullmatch(r"[0-9a-f]{16}", fence)
                and sha256(bytes.fromhex(fence)).hexdigest() == env.get("PREDECESSOR_RETAINED_FENCE"),
                "retained_0475_fence_mismatch")
    else:
        require(fence is None, "unexpected_0475_fence")
    return {"ledgers_sha256": checksum, "attendance_fence_sha256": env["PREDECESSOR_RETAINED_FENCE"] if retained else None}


def control(client, origin, token, version, change=None):
    payload = json.dumps(change).encode() if change is not None else None
    request = Request(origin + "/internal/ownership", data=payload, headers={
        "Authorization": f"Bearer {token}", "Content-Type": "application/json",
        "Cache-Control": "no-cache", "User-Agent": "two-bot-predecessor",
    })
    with client.opener.open(request, timeout=15) as response:
        raw = read_with_overflow(response, 4096)
        require(response.status == 200 and len(raw) <= 4096
                and (change is not None or response.headers.get("x-two-bot-exact-takeover") == "1"),
                "ownership_capability_unproved")
    result = mapping(decode(raw))
    require(result.get("deploymentId") == version and type(result.get("running")) is bool
            and "owner" in result, "ownership_version_mismatch")
    return result


def expect_state(state, phase, deployment, epoch=None, actor=None):
    owner = mapping(state.get("owner"))
    value = owner.get("epoch")
    require(type(value) is int and 0 < value < 2**53 and owner.get("phase") == phase
            and "deploymentId" in owner and owner["deploymentId"] == deployment
            and (epoch is None or value == epoch) and (actor is None or owner.get("actor") == actor)
            and state.get("running") is False, "ownership_transition_unproved")
    return value


def exercise(read, change, ready, version, actor):
    current = read()
    if current["owner"] is None:
        require(current["running"] is False, "unowned_container_running")
        expect_state(change("fence", 0), "fenced", None, 1, actor)
        current = read()
    stopped = expect_state(current, "fenced", None)
    try:
        expect_state(change("takeover-exact", stopped), "active", version, stopped + 1, actor)
        expect_state(read(), "active", version, stopped + 1, actor)
        ready()
    finally:
        # A lost takeover response still requires a fresh ownership read.
        observed = read()
        if observed["owner"] == current["owner"] and observed["running"] is False:
            expect_state(observed, "fenced", None, stopped)
        else:
            owner = mapping(observed["owner"])
            require(owner.get("phase") == "active" and owner.get("deploymentId") == version
                    and owner.get("epoch") == stopped + 1 and owner.get("actor") == actor,
                    "cleanup_ownership_unproved")
            try:
                expect_state(change("fence-exact", stopped + 1), "fenced", None, stopped + 2, actor)
            finally:
                expect_state(read(), "fenced", None, stopped + 2, actor)
    return {"takeover_epoch": stopped + 1, "stopped_epoch": stopped + 2}


def deploy(root, env, build, deploy_id, token):
    source = root / "source"
    config = tomllib.loads((source / "wrangler/wrangler.toml").read_text())
    config = {key: config[key] for key in ("compatibility_date", "compatibility_flags", "version_metadata",
                                          "containers", "durable_objects", "exports", "vars")}
    config.update(name=WORKER, main=str(source / "wrangler/src/index.ts"), workers_dev=True, preview_urls=False)
    config["containers"][0].update(
        name=APPLICATION, image=str(source / "Dockerfile"), image_build_context=str(source), rollout_kind="full_auto",
        image_vars={"BOT_BUILD_REVISION": SOURCE, "BOT_BUILD_ID": build, "BOT_DEPLOY_ID": deploy_id},
    )
    config["vars"]["GUILD_ID"] = env["PREDECESSOR_GUILD_ID"]
    config_path, modules = root / "wrangler.json", root / "modules"
    save(config_path, config)
    argv = [str(source / "wrangler/node_modules/.bin/wrangler"), "deploy", "--config", str(config_path)]
    command([*argv, "--dry-run", "--containers-rollout=none", "--outdir", str(modules)], cwd=source / "wrangler",
            env={"PATH": env["PATH"], "WRANGLER_SEND_METRICS": "false"}, timeout=120)
    configure_bundle(config, modules, "index.js")
    save(config_path, config)
    records = root / "deploy.ndjson"
    with tempfile.TemporaryDirectory(dir=root) as scratch:
        secrets = Path(scratch) / "bindings.json"
        save(secrets, {"DATABASE_URL": env["PREDECESSOR_RUNTIME_URL"], "DISCORD_TOKEN": env["PREDECESSOR_DISCORD_TOKEN"],
                       "OWNERSHIP_CONTROL_TOKEN": token})
        command([*argv, "--no-bundle", "--message", SOURCE, "--secrets-file", str(secrets)], cwd=source / "wrangler",
                env={key: env[key] for key in ("PATH", "HOME", "CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN")}
                | {"WRANGLER_OUTPUT_FILE_PATH": str(records), "WRANGLER_SEND_METRICS": "false"}, timeout=2400)
    sessions = [row for row in read_records(records) if row.get("type") in ("deploy", "wrangler-session")]
    require(len(sessions) == 2, "deploy_receipt_unproved")
    session, deployed = sessions
    version = deployed.get("version_id", "")
    require(session.get("type") == "wrangler-session" and session.get("wrangler_version") == WRANGLER
            and deployed.get("type") == "deploy" and deployed.get("worker_name") == WORKER
            and deployed.get("worker_name_overridden") is False
            and isinstance(version, str) and VERSION_ID.fullmatch(version), "deploy_receipt_unproved")
    return version, {str(path.relative_to(modules)): sha256(path.read_bytes()).hexdigest()
                     for path in sorted(modules.rglob("*")) if path.is_file()}


def qualify(root, env):
    endpoint, origin = target(env)
    database = database_sample(env, endpoint)
    account, token = env["CLOUDFLARE_ACCOUNT_ID"], env["PREDECESSOR_CONTROL_TOKEN"]
    client = Client(account, env["CLOUDFLARE_API_TOKEN"])
    subdomain = mapping(client.api("/workers/subdomain")).get("subdomain")
    require(origin == f"https://{WORKER}.{subdomain}.workers.dev", "worker_origin_mismatch")
    scripts = client.api("/workers/scripts")
    if any(mapping(row).get("id") == WORKER for row in scripts):
        before = serving_version(client, WORKER)
        expect_state(control(client, origin, token, before), "fenced", None)
    build = env["GITHUB_RUN_ID"] + "-1"
    deploy_id = datetime.now(timezone.utc).strftime("%Y.%m.%d") + f".1+{SOURCE[:7]}"
    version, modules = deploy(root, env, build, deploy_id, token)
    require(serving_version(client, WORKER) == version and version_sha(client, WORKER, version) == SOURCE,
            "serving_worker_mismatch")
    app = application_by_name(client, APPLICATION)
    app_id = app["id"]
    names = mapping(client.api(f"/workers/scripts/{WORKER}/versions/{version}"))["resources"]["bindings"]
    require({row["name"] for row in names if row.get("type") == "secret_text"}
            == {"DATABASE_URL", "DISCORD_TOKEN", "OWNERSHIP_CONTROL_TOKEN"}, "isolated_worker_bindings_mismatch")
    namespaces = [row.get("namespace_id") for row in names if row.get("type") == "durable_object_namespace"
                  and row.get("class_name") == "TwoBotContainer" and row.get("name") == "TWO_BOT"]
    require(len(namespaces) == 1 and namespaces[0] and app["durable_objects"]["namespace_id"] == namespaces[0],
            "container_namespace_mismatch")
    image_tag = f"registry.cloudflare.com/{account}/{APPLICATION}:{version.split('-')[0]}"
    image, = decode(command(["docker", "image", "inspect", image_tag, "--format", "{{json .RepoDigests}}"]))
    require(re.fullmatch(rf"registry[.]cloudflare[.]com/{account}/{APPLICATION}@sha256:[0-9a-f]{{64}}", image)
            and mapping(app.get("configuration")).get("image") == image,
            "built_image_digest_unproved")

    def read():
        return control(client, origin, token, version)

    def change(action, epoch):
        body = {"action": action, "expectedEpoch": epoch, "actor": f"predecessor:{build}:{SOURCE}"}
        if action != "fence":
            body["expectedDeploymentId"] = version
        return control(client, origin, token, version, body)

    def ready():
        until, passes = time.monotonic() + 300, 0
        while time.monotonic() < until:
            status, headers, body = client.request(origin + "/readyz")
            passes = passes + 1 if runtime_ready(status, headers, body, version, SOURCE, build, deploy_id) else 0
            if passes == 2:
                return
            time.sleep(5)
        raise GateError("predecessor_readiness_timeout")

    ownership = exercise(read, change, ready, version, f"predecessor:{build}:{SOURCE}")
    rows = sorted(app_rollouts(client, app_id), key=lambda row: row["created_at"])
    require(bool(rows) and rows[-1].get("status") == "completed" and rollout_image(rows[-1], APPLICATION) == image
            and serving_version(client, WORKER) == version and database_sample(env, endpoint) == database
            and application_by_name(client, APPLICATION).get("configuration", {}).get("image") == image,
            "final_pair_or_database_mismatch")
    save(root / "qualification.json", {"endpoint": endpoint, "workflow_sha": env["GITHUB_SHA"], "source_sha": SOURCE,
                                      "input_tree": INPUT_TREE, "worker_version": version, "module_sha256": modules,
                                      "image_digest": image.split("@")[1], "build_id": build, "deploy_id": deploy_id,
                                      **database, **ownership})


def main(env):
    try:
        require(env.get("GITHUB_WORKFLOW_REF") == WORKFLOW and env.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
                and env.get("GITHUB_REF_PROTECTED") == "true" and env.get("GITHUB_RUN_ATTEMPT") == "1"
                and re.fullmatch(r"[1-9][0-9]{0,19}", env.get("GITHUB_RUN_ID", "")), "untrusted_predecessor_dispatch")
        root = Path(env["RUNNER_TEMP"]) / "predecessor"
        if sys.argv[1:] == ["prepare"]:
            prepare(root, env["GITHUB_SHA"])
        else:
            require(sys.argv[1:] == ["qualify"], "invalid_predecessor_operation")
            qualify(root, env)
    except (GateError, ProofError) as error:
        print(f"Predecessor qualification refused: {error}", file=sys.stderr)
        return 1
    except (KeyError, OSError, TypeError, ValueError):
        print("Predecessor qualification refused: invalid or unavailable evidence", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(os.environ))
