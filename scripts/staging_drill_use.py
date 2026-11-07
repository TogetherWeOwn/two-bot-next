"""Finite staging authority for manual drills; never inherit a host loader or token."""
import os
from pathlib import Path
import re
import subprocess
import time

from worker_container_rollback import (
    CONTROL_TIMEOUT_SECONDS, GateError, require, validate_staging_restore_policy,
    version_sha,
)


class StagingDrillUse:
    def __init__(self, env, url, token, *, now=time.time, root=None):
        values = {key: env.get(key, "") for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT")}
        require(all(isinstance(value, str) and re.fullmatch(r"[1-9][0-9]{0,19}", value)
                    for value in values.values()), "staging_drill_run_invalid")
        require(int(values["GITHUB_RUN_ATTEMPT"]) <= 2 ** 53 - 1, "staging_drill_run_invalid")
        receipt = env.get("STAGING_USE_RECEIPT", "")
        require(isinstance(receipt, str) and bool(receipt), "staging_drill_receipt_missing")
        self.env = dict(values, PATH=env.get("PATH", os.defpath),
                        STAGING_WORKER_URL=url, OWNERSHIP_CONTROL_TOKEN=token,
                        STAGING_USE_RECEIPT=receipt)
        self.root = Path(root or Path(__file__).resolve().parents[1])
        self.now = now
        self.expires_at = None
        self.failed = False
        self.shas = {}
        self.version = None

    def validate(self, client, versions):
        """Only immutable finite-policy targets; no network mutation or public probe."""
        for version in versions:
            sha = version_sha(client, "two-bot-next-staging", version)
            validate_staging_restore_policy(client, "two-bot-next-staging", version, sha)
            self.shas[version] = sha

    def call(self, action, version=None, *, pipeline=False):
        try:
            require(self.expires_at is not None, "staging_drill_window_missing")
            env = dict(self.env, STAGING_USE_EXPIRES_AT=str(self.expires_at))
            if version is not None:
                require(version in self.shas, "staging_drill_identity_unproved")
                env.update(STAGING_USE_DEPLOYMENT=version, STAGING_USE_SHA=self.shas[version])
            helper = "staging-use-pipeline.mjs" if pipeline else "staging-use.mjs"
            result = subprocess.run(
                ["node", str(self.root / "wrangler/scripts" / helper), action],
                env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                timeout=CONTROL_TIMEOUT_SECONDS, check=False)
            require(result.returncode == 0, "staging_drill_control_failed")
            if action == "acquire":
                self.version = version
        except (OSError, subprocess.SubprocessError):
            self.failed = True
            raise GateError("staging_drill_control_unavailable") from None
        except GateError:
            # The redacting child deliberately hides HTTP bodies/status details.
            # Treat any refusal as a possible auth failure: no later control call,
            # restore or credential substitution; native expiry owns teardown.
            self.failed = True
            raise

    def begin(self, version):
        require(self.expires_at is None, "staging_drill_window_already_started")
        self.expires_at = int(self.now() * 1000) + 25 * 60 * 1000
        self.call("acquire", version)
        self.call("ready", version)

    def check(self, *, recovery=False):
        require(not self.failed and self.expires_at is not None
                and int(self.now() * 1000) < self.expires_at, "staging_drill_window_ended")
        # A partial leg can leave ownership fenced, so the runtime's active-owner
        # assertion is unavailable. Recovery retains the already-asserted grant
        # and deadline under the workflow's full-window lock; it cannot begin or
        # extend use. Takeover itself cannot start bounded staging: reacquisition
        # must validate the persisted same-run grant at the new epoch first.
        if not recovery:
            self.call("restore-check", pipeline=True)

    def rebind(self, version):
        # Keep the original readiness/outage measurement rather than hiding a
        # failure behind the pipeline helper's second readiness loop.
        self.call("acquire", version)

    def finish(self):
        if self.expires_at is not None and not self.failed:
            self.call("release", self.version)
