"""Label the exact PR diff for review routing only; never admit a deployment."""

from contextlib import closing
from http.client import HTTPException, HTTPSConnection
import json
import os
from pathlib import Path
import re
from urllib.parse import quote

from attestation_inventory import InventoryError, MAX_FILES, changed_paths


LABELS = {"promotion-path: yes": "d93f0b", "promotion-path: no": "0e8a16"}
PREFIXES = (
    ".github/workflows/", ".github/actions/", "deploy/",
    "crates/cutover/migrations/", "crates/store/migrations/", "scripts/production_hold_",
)
# Direct callers/imports in build identity, staging qualification, production
# selection/deploy/rollback and migration admission. Do not include whole source trees.
PATHS = frozenset("""
.dockerignore Dockerfile migrations.lock
.github/scripts/promotion_path.py .github/scripts/attestation_inventory.py
wrangler/package.json wrangler/package-lock.json
crates/cutover/Cargo.toml crates/cutover/build.rs crates/cutover/src/db.rs
crates/cutover/src/bin/staging_migrate.rs crates/cutover/src/staging_migrate.rs
crates/cutover/src/bin/rollback_delta.rs crates/cutover/src/rollback_delta.rs
crates/cutover/src/rollback_import.rs crates/bot/src/database_roles_cli.rs
crates/store/build.rs crates/store/src/pool.rs crates/store/src/migrations.rs
sql/database_roles.sql sql/database_role_matrix.sql sql/verify_database_roles.sql sql/web_v1.sql
scripts/build_identity.py scripts/deploy_id.py scripts/deploy_notify.py scripts/bot_promotion.py
scripts/production_deploy.py scripts/staging_rollout.py scripts/staging_container_drill.py
scripts/staging_rollback_drill.py scripts/staging_health_contract_probe.py
scripts/promotion_regression_gate.py scripts/post_deploy_watch.py scripts/prod_passive_probe.py
scripts/incident_issue.py scripts/rollback_proof.py scripts/rollback_target_preflight.py
scripts/rollback_readiness_probe.py scripts/worker_container_rollback.py
scripts/check-migrations.py scripts/check-docker-manifests.py scripts/container-inputs.py
scripts/release-sbom-target.py scripts/ci/check-squawk-migrations.py
scripts/ci/roles_gate.py scripts/ci/schema_gate_common.py
scripts/ci/production_migrate_claim.py scripts/ci/staging_migrate_claim.py
scripts/ci/production_schema_gate.py scripts/ci/staging_schema_gate.py
wrangler/scripts/ownership-control.mjs wrangler/scripts/production-ownership-control.mjs
wrangler/scripts/ownership-control-response.mjs wrangler/scripts/is-direct-invocation.mjs
wrangler/scripts/staging-use.mjs wrangler/scripts/staging-use-pipeline.mjs
wrangler/scripts/staging-soak-window.mjs wrangler/scripts/check-readyz.mjs
wrangler/scripts/promotion-error-gate.mjs wrangler/scripts/rest-5xx-counters.mjs
wrangler/src/index.ts wrangler/src/alert-rules.ts wrangler/src/metrics-scrape.ts
wrangler/src/ownership.ts wrangler/src/ops-control.ts wrangler/src/staging-lifecycle.ts
wrangler/src/container-env.ts
""".split())


def promotion_path(path):
    return (path.startswith(PREFIXES)
            or re.fullmatch(r"wrangler(?:\.[^/]+)?\.(?:toml|jsonc?)", path.rsplit("/", 1)[-1]) is not None
            or path in PATHS)


def diff_identity(pr, repository, number):
    try:
        base, head, count = pr["base"], pr["head"], pr["changed_files"]
        if (pr["state"] == "open" and pr["number"] == number
                and base["repo"]["full_name"] == repository and base["ref"]
                and type(count) is int and 0 < count < MAX_FILES
                and all(re.fullmatch(r"[0-9a-f]{40}", side["sha"]) for side in (head, base))):
            return head["sha"], base["sha"], base["ref"], count
    except (KeyError, TypeError):
        pass
    raise InventoryError("Unusable PR snapshot or file inventory at the GitHub API cap")


def update(api, repository, number):
    repo = f"/repos/{repository}"
    pull = f"{repo}/pulls/{number}"
    labels = f"{repo}/issues/{number}/labels"

    def current():
        pr = api("GET", pull)
        identity = diff_identity(pr, repository, number)
        _, base, base_ref, _ = identity
        ref = api("GET", f"{repo}/git/ref/heads/{quote(base_ref, safe='')}")
        if ref["object"]["sha"] != base:
            raise InventoryError("PR base SHA is not the live base ref")
        return pr, identity

    start, identity = current()
    paths = changed_paths(lambda route: api("GET", route), pull, identity[3])
    desired, opposite = LABELS if any(map(promotion_path, paths)) else reversed(LABELS)
    definition = f"{repo}/labels/{quote(desired, safe='')}"
    if api("GET", definition, tolerate=(404,)) is None:
        # A concurrent run for another PR may create this same repository label.
        api("POST", f"{repo}/labels", {"name": desired, "color": LABELS[desired]}, tolerate=(422,))
        api("GET", definition)

    before, observed = current()
    if observed != identity or before["updated_at"] != start["updated_at"]:
        raise InventoryError("PR changed while collecting its file inventory")
    attached = {label["name"] for label in before["labels"]}
    if desired not in attached:
        api("POST", labels, {"labels": [desired]})
    if opposite in attached:
        api("DELETE", f"{labels}/{quote(opposite, safe='')}", tolerate=(404,))

    after, observed = current()
    if observed != identity or {label["name"] for label in after["labels"]} & LABELS.keys() != {desired}:
        raise InventoryError("PR or labels changed during publication; no current verdict confirmed")
    return *identity[:3], desired


def request(connection, token, method, route, body=None, *, tolerate=()):
    connection.request(method, route, json.dumps(body) if body is not None else None, {
        "Authorization": f"Bearer {token}", "Content-Type": "application/json",
        "Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28",
        "Cache-Control": "no-cache",
    })
    with connection.getresponse() as response:
        limit = 16 * 1024 * 1024
        raw = response.read(limit + 1)
        if len(raw) > limit or response.length not in (None, 0):
            raise InventoryError("Incomplete or oversized GitHub API response")
        if response.status in tolerate:
            return None
        if response.status not in (200, 201, 204):
            raise InventoryError(f"GitHub API returned HTTP {response.status}")
        return json.loads(raw) if raw else None


def main():
    try:
        repository, number, policy = (os.environ[key] for key in ("GITHUB_REPOSITORY", "PR_NUMBER", "POLICY_SHA"))
        if not (re.fullmatch(r"[\w.-]+/[\w.-]+", repository) and re.fullmatch(r"[1-9][0-9]{0,9}", number)
                and re.fullmatch(r"[0-9a-f]{40}", policy)):
            raise InventoryError("Invalid workflow identity")
        token = os.environ["GH_TOKEN"]
        with closing(HTTPSConnection("api.github.com", timeout=15)) as connection:
            def api(*args, **kwargs):
                return request(connection, token, *args, **kwargs)

            head, base, ref, label = update(api, repository, int(number))
        Path(os.environ["GITHUB_STEP_SUMMARY"]).write_text(
            f"Pull request: #{number}\n\nHead SHA: `{head}`\n\n"
            f"Base SHA: `{base}` (`{quote(ref, safe='/')}`)\n\nPolicy SHA: `{policy}`\n\n"
            f"Advisory verdict: `{label}`\n", encoding="utf-8",
        )
    except InventoryError as error:
        raise SystemExit(str(error)) from None
    except (HTTPException, OSError, KeyError, ValueError, TypeError):
        # Do not surface provider bodies or untrusted PR data in logs.
        raise SystemExit("Promotion-path inventory or label publication failed") from None


if __name__ == "__main__":
    main()
