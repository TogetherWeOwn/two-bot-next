"""Offline workflow integration for immutable staging-byte promotion.

Executes the ACTUAL inline shell of the promotion path with controlled
fake `curl` / `python3` / `npx` / `sleep` boundaries and asserts on real
outputs and logged arguments:

  staging:   prepare (promotion_clean) -> bundle -> snapshot -> deploy ->
             receipt -> takeover -> verify -> error-rate gate ->
             auto-rollback -> record -> evidence upload (always) ->
             voice-synthetic -> pinned-history-guard ->
             certify-promotion (only when clean)
  production: guard run/attempt -> admit (before the production
             render/deploy) -> render (--artifact, no-bundle) ->
             deploy --no-bundle -> serving proof -> stable readyz gate
             (artifact/version/image/headers) -> serving recheck ->
             error-rate gate -> auto-rollback -> record (gated) ->
             evidence upload (only on success)

Structural YAML covers only what is not executed here: the job graph
(`needs`), `if:` conditions, step ordering, and `permissions:`.

Hermetic: temp dirs only, synthetic SHAs/images/versions, no network, no
credentials. The sandbox environment is explicit (no inherited process
env); `gh`/`docker`/`wrangler`/`node`/`npm`/`wget`/`ssh`/`timeout` are
fail-closed guards. The fake `python3` stubs only the known promotion
tools, passes through the workflow's own inline `-c` evidence printers and
`-m zipfile` extraction, and refuses any other script path. This binds the
inline shells, not a live service: the Python tools themselves are covered
by their own unit tests.
"""

import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
STAGING_PATH = ROOT / ".github/workflows/deploy-staging.yml"
PRODUCTION_PATH = ROOT / ".github/workflows/deploy-production.yml"

SHA = "0123456789abcdef0123456789abcdef01234567"
DIGEST = "sha256:" + "ab" * 32
PROD_IMAGE = f"registry.cloudflare.com/acct/two-bot-next-twobotcontainer-production@{DIGEST}"
NEW_VERSION = "0b6e5e3c-1f2a-4b3c-8d4e-5f6a7b8c9d0e"
DEPLOY_ID = "2026.10.25.1+0123456"


def load_workflow(path):
    with open(path, encoding="utf-8") as handle:
        return yaml.safe_load(handle)


def steps_of(job):
    return job.get("steps") or []


def find_step(job, key, value=None):
    for index, step in enumerate(steps_of(job)):
        if value is None:
            if step.get(key):
                return index, step
        elif step.get(key) == value:
            return index, step
    raise AssertionError(f"no step with {key}={value!r}")


def find_step_by_name(job, name):
    return find_step(job, "name", name)


def run_of(step):
    body = step.get("run", "")
    assert isinstance(body, str) and body.strip(), "expected an inline shell body"
    return body


# ---------------------------------------------------------------------------
# Execution sandbox with fake boundaries.
# ---------------------------------------------------------------------------

FAKE_PYTHON = r"""#!/usr/bin/env bash
# Controlled python3 boundary: log every call, stub only the known promotion
# tools, pass through the workflow's own inline `-c` evidence printers and
# `-m zipfile` extraction. Any other script path fails closed so a future
# workflow addition cannot silently execute an unstubbed tool here.
log="$FAKE_LOG_DIR/calls.jsonl"
"$REAL_PYTHON" -c 'import json,os,sys; open(os.environ["FAKE_LOG_DIR"]+"/calls.jsonl","a").write(json.dumps({"cmd":"python3","argv":sys.argv[1:]})+"\n")' "$@"
script="$1"
shift || true
case "$script" in
  -c) exec "$REAL_PYTHON" "$script" "$@" ;; # inline evidence printer: workflow's own code
  -m)
    if [ "$1" = "zipfile" ]; then exec "$REAL_PYTHON" "$script" "$@"; fi
    echo "forbidden python module in hermetic test: $1" >&2
    exit 1 ;;
  -) exit 0 ;; # configure_bundle heredoc: reviewed stub, no-op
esac
base="$(basename "$script")"
sub="$1"
shift || true
case "$base" in
  staging_rollout.py)
    case "$sub" in
      prepare)
        while [ $# -gt 0 ]; do
          case "$1" in --receipt|--output|--evidence|--deploy-config)
            mkdir -p "$(dirname "$2")"; printf '{}' > "$2" ;; esac
          shift
        done
        exit 0 ;;
      receipt|verify)
        while [ $# -gt 0 ]; do
          case "$1" in --evidence) mkdir -p "$(dirname "$2")"; printf '{"ok":true}' > "$2" ;; esac
          shift
        done
        exit "${STAGING_EXIT:-0}" ;;
    esac
    ;;
  bot_promotion.py)
    case "$sub" in
      certify)
        out=""
        prev=""
        for arg in "$@"; do
          if [ "$prev" = "--out" ]; then out="$arg"; fi
          prev="$arg"
        done
        src="$FAKE_LOG_DIR/certify-src"
        rm -rf "$src"; mkdir -p "$src/modules"
        printf '{"schema_version":1}' > "$src/promotion-receipt.json"
        printf 'export default {}' > "$src/modules/index.js"
        # Match the real closed shape: receipt at top level, modules under modules/.
        # (`python -m zipfile -c` would flatten modules/index.js to index.js.)
        CERTIFY_OUT="$out" CERTIFY_SRC="$src" "$REAL_PYTHON" -c 'import os,zipfile; out=os.environ["CERTIFY_OUT"]; src=os.environ["CERTIFY_SRC"]; os.makedirs(os.path.dirname(out),exist_ok=True); zf=zipfile.ZipFile(out,"w",zipfile.ZIP_DEFLATED); zf.write(src+"/promotion-receipt.json","promotion-receipt.json"); zf.write(src+"/modules/index.js","modules/index.js"); zf.close()'
        exit 0 ;;
      admit)
        out=""
        prev=""
        for arg in "$@"; do
          if [ "$prev" = "--out" ]; then out="$arg"; fi
          prev="$arg"
        done
        mkdir -p "$out"
        printf '{"schema_version":1,"receipt":{},"production_image":"%s"}' "$PROD_IMAGE" > "$out/admitted.json"
        exit "${ADMIT_EXIT:-0}" ;;
    esac
    ;;
  production_deploy.py)
    case "$sub" in
      readyz)
        line=""
        if [ -f "$READYZ_QUEUE" ]; then
          line="$(head -n 1 "$READYZ_QUEUE")"
          tail -n +2 "$READYZ_QUEUE" > "$READYZ_QUEUE.tmp" && mv "$READYZ_QUEUE.tmp" "$READYZ_QUEUE"
        fi
        if [ "$line" = "0" ]; then echo "gateway ready (stub)"; exit 0; fi
        echo "gateway not ready (stub)"; exit 1 ;;
      excerpt) echo "excerpt (stub)"; exit 0 ;;
      serving)
        out=""
        preflight=false
        runtime_url=""
        prev=""
        for arg in "$@"; do
          if [ "$prev" = "--out" ]; then out="$arg"; fi
          if [ "$prev" = "--url" ]; then runtime_url="$arg"; fi
          if [ "$arg" = "--preflight" ]; then preflight=true; fi
          prev="$arg"
        done
        # Takeover transfers ownership without starting the singleton. Only
        # /health in the real readiness shell can warm this external double.
        if [ "${COLD_START_REQUIRED:-0}" = "1" ] && [ "$preflight" != "true" ]; then
          if [ ! -f "$FAKE_LOG_DIR/container-started" ]; then
            echo 'cold container: convergence requested before first /health' >&2
            exit 1
          fi
          if [ "$runtime_url" != "$PRODUCTION_URL" ]; then
            echo 'final serving proof needs the runtime URL' >&2
            exit 1
          fi
        fi
        phase=convergence
        if [ "$preflight" = "true" ]; then phase=identity; fi
        mkdir -p "$(dirname "$out")"
        printf '{"production_image":"%s","worker_version":"%s","phase":"%s"}' "$PROD_IMAGE" "$NEW_VERSION" "$phase" > "$out"
        exit "${SERVING_EXIT:-0}" ;;
      render)
        out=""
        prev=""
        for arg in "$@"; do
          if [ "$prev" = "--out" ]; then out="$arg"; fi
          prev="$arg"
        done
        mkdir -p "$(dirname "$out")"
        printf '{"rendered":true}' > "$out"
        exit 0 ;;
    esac
    ;;
  worker_container_rollback.py)
    while [ $# -gt 0 ]; do
      case "$1" in --evidence) mkdir -p "$(dirname "$2")"; printf '{"rolled_back":true}' > "$2" ;; esac
      shift
    done
    exit 0 ;;
  deploy_id.py)
    exit 0 ;;
  *)
    echo "forbidden python tool in hermetic test: $script $sub" >&2
    exit 1 ;;
esac
echo "forbidden python subcommand in hermetic test: $script $sub" >&2
exit 1
"""

FAKE_CURL = r"""#!/usr/bin/env bash
# Controlled curl boundary: pop one queued response per call.
# Queue lines look like: CODE|BODY  (BODY written to -o file, CODE printed).
out=""
hdr=""
url=""
prev=""
for arg in "$@"; do
  case "$prev" in
    -o) out="$arg" ;;
    -D) hdr="$arg" ;;
  esac
  case "$arg" in -o|-D|-w|-H|--max-time|--max-filesize) prev="$arg" ;;
    -*) prev="$arg" ;;
    *) url="$arg"; prev="" ;;
  esac
done
line=""
if [ -f "$CURL_QUEUE" ]; then
  line="$(head -n 1 "$CURL_QUEUE")"
  tail -n +2 "$CURL_QUEUE" > "$CURL_QUEUE.tmp" && mv "$CURL_QUEUE.tmp" "$CURL_QUEUE"
fi
code="${line%%|*}"
body="${line#*|}"
[ -n "$code" ] || code="000"
if [ -n "$out" ]; then printf '%s' "$body" > "$out"; fi
if [ -n "$hdr" ]; then printf 'HTTP/1.1 200 OK\r\nX-Stub: fake\r\n\r\n' > "$hdr"; fi
printf 'url=%s\n' "$url" >> "$FAKE_LOG_DIR/curl-urls.log"
"$REAL_PYTHON" -c 'import json,os,sys; open(os.environ["FAKE_LOG_DIR"]+"/calls.jsonl","a").write(json.dumps({"cmd":"curl","url":sys.argv[1]})+"\n")' "$url"
case "$url" in */health) touch "$FAKE_LOG_DIR/container-started" ;; esac
printf '%s\n' "$code"
"""

FAKE_NPX = r"""#!/usr/bin/env bash
echo "npx $*" >> "$FAKE_LOG_DIR/npx.log"
exit "${NPX_EXIT:-0}"
"""

FAKE_SLEEP = r"""#!/usr/bin/env bash
echo "sleep $*" >> "$FAKE_LOG_DIR/sleep.log"
exit 0
"""

# Fail-closed guard for network/credential-capable tools: any current or
# future inline shell that reaches for these fails loudly instead of
# touching the network. (curl/npx/sleep keep their explicit stubs above.)
FAKE_FORBIDDEN = r"""#!/usr/bin/env bash
"$REAL_PYTHON" -c 'import json,os,sys; open(os.environ["FAKE_LOG_DIR"]+"/calls.jsonl","a").write(json.dumps({"cmd":"FORBIDDEN","argv":sys.argv[1:]})+"\n")' "$0" "$@"
echo "forbidden in hermetic test: $0 $*" >&2
exit 1
"""

FORBIDDEN_TOOLS = ("gh", "docker", "wrangler", "node", "npm", "wget", "ssh",
                   "timeout", "cargo")


class Sandbox:
    """Temp execution root with fake boundaries first on PATH."""

    def __init__(self, test):
        self.test = test
        self.tmp = tempfile.TemporaryDirectory()
        test.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "log"
        self.log.mkdir()
        self.runner_temp = self.root / "runner-temp"
        self.runner_temp.mkdir()
        self.work = self.root / "work"
        self.work.mkdir()
        self._write("python3", FAKE_PYTHON)
        self._write("curl", FAKE_CURL)
        self._write("npx", FAKE_NPX)
        self._write("sleep", FAKE_SLEEP)
        for tool in FORBIDDEN_TOOLS:
            self._write(tool, FAKE_FORBIDDEN)
        # Minimal repo layout the gate script expects beside its cwd.
        (self.root / "Dockerfile").write_text(
            "FROM scratch\nARG BOT_DEPLOY_ID\n", encoding="utf-8")
        self.env = {
            "PATH": f"{self.bin}{os.pathsep}{os.environ.get('PATH', '')}",
            "REAL_PYTHON": sys.executable,
            "FAKE_LOG_DIR": str(self.log),
            "PROD_IMAGE": PROD_IMAGE,
            "NEW_VERSION": NEW_VERSION,
            "RUNNER_TEMP": str(self.runner_temp),
            "GITHUB_OUTPUT": str(self.root / "github-output.txt"),
            "GITHUB_STEP_SUMMARY": str(self.root / "github-summary.md"),
            "GITHUB_ENV": str(self.root / "github-env.txt"),
            "GITHUB_RUN_ID": "900",
            "GITHUB_RUN_ATTEMPT": "3",
            "CURL_QUEUE": str(self.root / "curl-queue.txt"),
            "READYZ_QUEUE": str(self.root / "readyz-queue.txt"),
            "HOME": str(self.root),
        }
        Path(self.env["GITHUB_OUTPUT"]).write_text("", encoding="utf-8")
        Path(self.env["GITHUB_STEP_SUMMARY"]).write_text("", encoding="utf-8")
        Path(self.env["GITHUB_ENV"]).write_text("", encoding="utf-8")

    def _write(self, name, text):
        path = self.bin / name
        path.write_text(text, encoding="utf-8")
        path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    def set_dockerfile(self, text):
        (self.root / "Dockerfile").write_text(text, encoding="utf-8")

    def queue_curl(self, entries):
        """entries: list of (code, body) strings consumed in order."""
        with open(self.env["CURL_QUEUE"], "w", encoding="utf-8") as handle:
            for code, body in entries:
                handle.write(f"{code}|{body}\n")

    def queue_readyz(self, verdicts):
        """verdicts: list of 0 (pass) / 1 (fail) consumed in order."""
        with open(self.env["READYZ_QUEUE"], "w", encoding="utf-8") as handle:
            for verdict in verdicts:
                handle.write(f"{verdict}\n")

    def run(self, script, extra_env=None):
        env = dict(self.env)
        if extra_env:
            env.update(extra_env)
        script_path = self.root / "script.sh"
        script_path.write_text(script, encoding="utf-8")
        return subprocess.run(["bash", str(script_path)], cwd=str(self.work),
                              env=env, capture_output=True, text=True, timeout=300)

    def output_vars(self):
        values = {}
        path = Path(self.env["GITHUB_OUTPUT"])
        if path.exists():
            for line in path.read_text(encoding="utf-8").splitlines():
                if "=" in line:
                    key, value = line.split("=", 1)
                    values[key] = value
        return values

    def env_vars(self):
        values = {}
        path = Path(self.env["GITHUB_ENV"])
        if path.exists():
            for line in path.read_text(encoding="utf-8").splitlines():
                if "=" in line:
                    key, value = line.split("=", 1)
                    values[key] = value
        return values

    def summary(self):
        path = Path(self.env["GITHUB_STEP_SUMMARY"])
        return path.read_text(encoding="utf-8") if path.exists() else ""

    def calls(self, cmd="python3"):
        path = self.log / "calls.jsonl"
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()
                if line.strip() and json.loads(line)["cmd"] == cmd]

    def curl_urls(self):
        path = self.log / "curl-urls.log"
        if not path.exists():
            return []
        return [line.split("=", 1)[1] for line in
                path.read_text(encoding="utf-8").splitlines() if line.startswith("url=")]


PASS_BODY = '{"components":[["gateway","ready"]]}'
FAIL_BODY = '{"components":[["gateway","down"]]}'


class SandboxHygieneTests(unittest.TestCase):
    def test_sandbox_carries_no_inherited_credentials(self):
        canaries = {"HERMETIC_CANARY_GH": "canary-gh-token",
                    "HERMETIC_CANARY_CF": "canary-cf-token"}
        for key, value in canaries.items():
            old = os.environ.get(key)
            os.environ[key] = value
            self.addCleanup(lambda key=key, old=old: (
                os.environ.pop(key, None)
                if old is None else os.environ.__setitem__(key, old)))
        box = Sandbox(self)
        result = box.run("env")
        self.assertEqual(result.returncode, 0)
        for value in canaries.values():
            self.assertNotIn(value, result.stdout)
        for forbidden in ("GH_TOKEN", "GITHUB_TOKEN", "CLOUDFLARE_API_TOKEN",
                          "CLOUDFLARE_ACCOUNT_ID", "TWO_BOT_STAGING_PLAN_DATABASE_URL",
                          "STAGING_OWNERSHIP_CONTROL_TOKEN",
                          "PRODUCTION_OWNERSHIP_CONTROL_TOKEN"):
            self.assertNotIn(forbidden + "=", result.stdout)

    def test_forbidden_tools_fail_closed(self):
        box = Sandbox(self)
        for tool in ("gh api repos/x", "docker pull example.invalid/x",
                     "wrangler deploy", "node scripts/x.mjs"):
            with self.subTest(tool=tool):
                result = box.run(tool)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("forbidden in hermetic test", result.stderr)
        forbidden = [c for c in box.calls(cmd="FORBIDDEN")]
        self.assertEqual(len(forbidden), 4)


class WorkflowFilesTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.staging = load_workflow(STAGING_PATH)
        cls.production = load_workflow(PRODUCTION_PATH)


# ---------------------------------------------------------------------------
# Staging: certify downstream of every gate; promotion_clean excludes drills.
# ---------------------------------------------------------------------------

class CertifyGraphTests(WorkflowFilesTests):
    def test_certify_runs_downstream_of_deploy_voice_and_pinned_history(self):
        job = self.staging["jobs"]["certify-promotion"]
        self.assertEqual(job["needs"], ["deploy", "voice-synthetic", "user-flow-synthetic",
                                        "pinned-history-guard"])
        voice = self.staging["jobs"]["voice-synthetic"]
        self.assertEqual(voice["needs"], "deploy")
        synthetic = self.staging["jobs"]["user-flow-synthetic"]
        self.assertEqual(synthetic["needs"], "deploy")
        guard = self.staging["jobs"]["pinned-history-guard"]
        self.assertEqual(guard["needs"], "voice-synthetic")

    def test_certify_requires_clean_promotion_and_voice_gate(self):
        job = self.staging["jobs"]["certify-promotion"]
        condition = job.get("if", "")
        self.assertIn("success()", condition)
        self.assertIn("needs.deploy.outputs.promotion_clean == 'true'", condition)
        self.assertIn("vars.VOICE_SYNTHETIC_REQUIRED != 'false'", condition)

    def test_prepare_declares_promotion_clean_output(self):
        job = self.staging["jobs"]["deploy"]
        self.assertIn("promotion_clean", job.get("outputs", {}))
        _, prepare = find_step_by_name(job, "Snapshot staging identity and generate per-deploy image provenance")
        self.assertEqual(prepare.get("id"), "prepare")
        body = run_of(prepare)
        # Every fault drill must feed the dirty branch: no drill may certify.
        for flag in ("INJECT_GATEWAY_FAILURE", "INJECT_VOICE_STALL", "INJECT_UNPAIRED_DISCONNECT"):
            self.assertIn(flag, body)
        self.assertIn('promotion_clean=true', body)
        self.assertIn('promotion_clean=false', body)

    def test_fault_inputs_are_dispatch_only_and_never_forwarded(self):
        _, prepare = find_step_by_name(
            self.staging["jobs"]["deploy"],
            "Snapshot staging identity and generate per-deploy image provenance")
        for name, expr in prepare.get("env", {}).items():
            if name.startswith("INJECT_"):
                self.assertIn("workflow_dispatch", expr, name)
        _, gate = find_step_by_name(self.staging["jobs"]["deploy"], "Gate promotion on REST 5xx error rate")
        self.assertIn("workflow_dispatch", gate["env"]["INJECT_ERROR_RATE_SPIKE"])


class PromotionCleanExecutionTests(unittest.TestCase):
    def prepare_body(self):
        workflow = load_workflow(STAGING_PATH)
        _, prepare = find_step_by_name(
            workflow["jobs"]["deploy"],
            "Snapshot staging identity and generate per-deploy image provenance")
        return run_of(prepare)

    def run_prepare(self, gateway="false", voice="false", unpaired="false"):
        box = Sandbox(self)
        result = box.run(self.prepare_body(), extra_env={
            "GITHUB_SHA": SHA,
            "INJECT_GATEWAY_FAILURE": gateway,
            "INJECT_VOICE_STALL": voice,
            "INJECT_UNPAIRED_DISCONNECT": unpaired,
            "ROLLOUT_DIR": str(box.runner_temp),
            "WRANGLER_OUTPUT_FILE_PATH": str(box.runner_temp / "wrangler-output.ndjson"),
        })
        return box, result

    def test_clean_run_marks_promotion_clean(self):
        box, result = self.run_prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(box.output_vars().get("promotion_clean"), "true")

    def test_each_fault_drill_marks_promotion_dirty(self):
        cases = [("gateway", {"gateway": "true"}),
                 ("voice", {"voice": "true"}),
                 ("unpaired", {"unpaired": "true"}),
                 ("all", {"gateway": "true", "voice": "true", "unpaired": "true"})]
        for name, flags in cases:
            with self.subTest(drill=name):
                box, result = self.run_prepare(**flags)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(box.output_vars().get("promotion_clean"), "false",
                                 f"{name} drill must never certify")


class CertifyExecutionTests(unittest.TestCase):
    def certify_body(self):
        workflow = load_workflow(STAGING_PATH)
        job = workflow["jobs"]["certify-promotion"]
        for step in steps_of(job):
            if "bot_promotion.py certify" in (step.get("run") or ""):
                return run_of(step)
        raise AssertionError("no certify step")

    def test_certify_packages_evidence_stamped_archive(self):
        box = Sandbox(self)
        source = box.runner_temp / "promotion-source"
        (source / "worker-modules").mkdir(parents=True)
        (source / "worker-modules" / "index.js").write_text("export default {}", encoding="utf-8")
        (source / "evidence.json").write_text(
            json.dumps({"deploy_id": DEPLOY_ID}), encoding="utf-8")
        result = box.run(self.certify_body(), extra_env={
            "SOURCE_SHA": SHA, "GITHUB_RUN_ID": "501", "GITHUB_RUN_ATTEMPT": "2",
            "GH_TOKEN": "synthetic-token",
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((box.runner_temp / "promotion.zip").is_file())
        upload = box.runner_temp / "promotion-upload"
        self.assertTrue((upload / "promotion-receipt.json").is_file())
        self.assertTrue((upload / "modules" / "index.js").is_file())
        certify_calls = [c for c in box.calls()
                         if any("bot_promotion.py" in a for a in c["argv"])]
        self.assertEqual(len(certify_calls), 1)
        argv = certify_calls[0]["argv"]
        for flag in ("--modules", "--entrypoint", "--evidence", "--config",
                     "--sha", "--run-id", "--attempt", "--deploy-id",
                     "--readiness-result", "--synthetic-result", "--fault-mode", "--out"):
            self.assertIn(flag, argv)
        self.assertIn("--fault-mode", argv)
        self.assertEqual(argv[argv.index("--fault-mode") + 1], "none")
        self.assertEqual(argv[argv.index("--readiness-result") + 1], "success")
        self.assertEqual(argv[argv.index("--synthetic-result") + 1], "success")

    def test_promotion_artifact_is_separate_from_rollout_diagnostics(self):
        workflow = load_workflow(STAGING_PATH)
        deploy_steps = steps_of(workflow["jobs"]["deploy"])
        diagnostics = [s for s in deploy_steps if (s.get("name") or "") == "Upload the rollout evidence"]
        self.assertEqual(len(diagnostics), 1)
        self.assertIn("always()", diagnostics[0].get("if", ""))
        self.assertEqual(diagnostics[0]["with"]["name"], "staging-rollout-evidence")
        certify_steps = steps_of(workflow["jobs"]["certify-promotion"])
        promotions = [s for s in certify_steps if (s.get("uses") or "").startswith("actions/upload-artifact@")]
        self.assertEqual(len(promotions), 1)
        self.assertIn("bot-promotion-", promotions[0]["with"]["name"])
        self.assertNotEqual(promotions[0]["with"]["name"], "staging-rollout-evidence")
        _, certify = find_step(workflow["jobs"]["certify-promotion"], "run")
        # Diagnostics upload in deploy must not reference the promotion ZIP.
        self.assertNotIn("promotion.zip", diagnostics[0].get("with", {}).get("path", ""))
        self.assertIn("promotion.zip", run_of(certify))


# ---------------------------------------------------------------------------
# Production: admission before mutation on the guard run/attempt; recovery
# never touches admission.
# ---------------------------------------------------------------------------

class AdmissionOrderingTests(WorkflowFilesTests):
    def production_job(self):
        return self.production["jobs"]["production"]

    def test_admission_precedes_every_production_mutation(self):
        job = self.production_job()
        admit_at, admit = find_step_by_name(
            job, "Admit the immutable staging artifact before production mutation")
        render_at, _ = find_step_by_name(job, "Render the production deploy config with build identity")
        deploy_at, _ = find_step_by_name(job, "Deploy to production")
        self.assertLess(admit_at, render_at)
        self.assertLess(admit_at, deploy_at)
        condition = admit.get("if", "")
        self.assertIn("env.MODE == 'deploy'", condition)
        self.assertIn("env.ALLOW_UNSTAGED != 'true'", condition)

    def test_admission_consumes_the_guard_run_and_attempt(self):
        _, admit = find_step_by_name(
            self.production_job(),
            "Admit the immutable staging artifact before production mutation")
        env = admit.get("env", {})
        self.assertEqual(env.get("STAGING_RUN_ID"), "${{ needs.guard.outputs.staging_run_id }}")
        self.assertEqual(env.get("STAGING_ATTEMPT"), "${{ needs.guard.outputs.staging_attempt }}")
        body = run_of(admit)
        for flag in ("--sha", "--run-id", "--attempt", "--account-id", "--out"):
            self.assertIn(flag, body)
        self.assertIn("$STAGING_RUN_ID", body)
        self.assertIn("$STAGING_ATTEMPT", body)

    def test_recovery_paths_never_touch_admission(self):
        job = self.production_job()
        for name in ("Roll back production Worker version and container image",
                     "Auto-roll back Worker version and container image"):
            _, step = find_step_by_name(job, name)
            body = run_of(step)
            self.assertNotIn("bot_promotion", body)
            self.assertNotIn("admit", body)
            self.assertNotIn("--artifact", body)
        _, render = find_step_by_name(job, "Render the production deploy config with build identity")
        self.assertIn("ALLOW_UNSTAGED", run_of(render))
        # The unstaged recovery branch packages its own source, never an artifact.
        self.assertIn("recovery-modules", run_of(render))

    def test_admit_executes_with_guard_run_and_attempt(self):
        _, admit = find_step_by_name(
            self.production_job(),
            "Admit the immutable staging artifact before production mutation")
        box = Sandbox(self)
        result = box.run(run_of(admit), extra_env={
            "SHA": SHA, "STAGING_RUN_ID": "501", "STAGING_ATTEMPT": "2",
            "CLOUDFLARE_ACCOUNT_ID": "acct", "GH_TOKEN": "synthetic",
            "CLOUDFLARE_API_TOKEN": "synthetic",
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((box.runner_temp / "bot-promotion" / "admitted.json").is_file())
        admit_calls = [c for c in box.calls()
                       if any("bot_promotion.py" in a for a in c["argv"])]
        self.assertEqual(len(admit_calls), 1)
        argv = admit_calls[0]["argv"]
        self.assertEqual(argv[argv.index("--run-id") + 1], "501")
        self.assertEqual(argv[argv.index("--attempt") + 1], "2")
        self.assertEqual(argv[argv.index("--sha") + 1], SHA)

    def test_admit_failure_fails_closed(self):
        _, admit = find_step_by_name(
            self.production_job(),
            "Admit the immutable staging artifact before production mutation")
        box = Sandbox(self)
        result = box.run(run_of(admit), extra_env={
            "SHA": SHA, "STAGING_RUN_ID": "501", "STAGING_ATTEMPT": "2",
            "CLOUDFLARE_ACCOUNT_ID": "acct", "ADMIT_EXIT": "1",
        })
        self.assertNotEqual(result.returncode, 0)


class NoBundleRenderTests(WorkflowFilesTests):
    def render_body(self):
        _, render = find_step_by_name(
            self.production["jobs"]["production"],
            "Render the production deploy config with build identity")
        return run_of(render)

    def test_ordinary_render_routes_the_admitted_artifact(self):
        box = Sandbox(self)
        admitted = box.runner_temp / "bot-promotion" / "admitted.json"
        admitted.parent.mkdir(parents=True)
        admitted.write_text("{}", encoding="utf-8")
        result = box.run(self.render_body(), extra_env={
            "SHA": SHA, "GITHUB_RUN_ID": "900", "GITHUB_RUN_ATTEMPT": "1",
            "DEPLOY_ID": DEPLOY_ID, "ALLOW_UNSTAGED": "false",
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        renders = [c for c in box.calls()
                   if any("production_deploy.py" in a for a in c["argv"])]
        self.assertEqual(len(renders), 1)
        argv = renders[0]["argv"]
        self.assertIn("render", argv)
        self.assertIn("--artifact", argv)
        self.assertIn(str(admitted), argv)
        self.assertEqual(box.env_vars().get("PRODUCTION_DEPLOY_CONFIG"),
                         str(box.runner_temp / "production-deploy.json"))
        self.assertFalse(self.npx_called(box),
                         "ordinary render must not rebundle")

    def npx_called(self, box):
        return (box.log / "npx.log").exists()

    def test_recovery_builds_own_modules_without_artifact(self):
        box = Sandbox(self)
        result = box.run(self.render_body(), extra_env={
            "SHA": SHA, "GITHUB_RUN_ID": "900", "GITHUB_RUN_ATTEMPT": "1",
            "DEPLOY_ID": DEPLOY_ID, "ALLOW_UNSTAGED": "true",
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        renders = [c for c in box.calls()
                   if any("production_deploy.py" in a for a in c["argv"])]
        self.assertEqual(len(renders), 1)
        self.assertNotIn("--artifact", renders[0]["argv"])
        self.assertTrue(self.npx_called(box), "recovery must package its own source")
        npx_log = (box.log / "npx.log").read_text(encoding="utf-8")
        self.assertIn("--dry-run", npx_log)
        self.assertIn("--outdir", npx_log)

    def test_deploy_command_is_no_bundle_on_the_rendered_config(self):
        _, deploy = find_step_by_name(self.production["jobs"]["production"], "Deploy to production")
        command = deploy["with"]["command"]
        self.assertIn("--no-bundle", command)
        self.assertIn("PRODUCTION_DEPLOY_CONFIG", command)


# ---------------------------------------------------------------------------
# Stable readiness: headers + artifact/version/image; convergence proven
# before AND after the window; failures stay under auto-rollback.
# ---------------------------------------------------------------------------

class StableGateExecutionTests(unittest.TestCase):
    def gate_body(self):
        workflow = load_workflow(PRODUCTION_PATH)
        _, gate = find_step_by_name(
            workflow["jobs"]["production"], "Gate on stable /health + /readyz gateway-ready")
        return run_of(gate)

    def base_env(self, box, mode="deploy", unstaged="false", deploy_id=DEPLOY_ID):
        (box.runner_temp / "production-promotion-evidence.json").write_text(
            json.dumps({"production_image": PROD_IMAGE}), encoding="utf-8")
        env = {"SHA": SHA, "MODE": mode, "ALLOW_UNSTAGED": unstaged,
               "NEW_VERSION": NEW_VERSION, "PRODUCTION_URL": "https://prod.example.invalid"}
        if deploy_id is not None:
            env["DEPLOY_ID"] = deploy_id
        return env

    def readyz_calls(self, box):
        return [c for c in box.calls()
                if any("production_deploy.py" in a for a in c["argv"])
                and "readyz" in c["argv"]]

    def test_stable_window_passes_with_artifact_image_and_headers(self):
        box = Sandbox(self)
        box.queue_curl([("200", PASS_BODY), ("200", PASS_BODY)] * 19)
        box.queue_readyz([0] * 19)
        result = box.run(self.gate_body(), extra_env=self.base_env(box))
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.readyz_calls(box)
        self.assertEqual(len(calls), 19, "1 first pass + 18 stable polls")
        for call in calls:
            argv = call["argv"]
            for flag in ("--artifact", "--expected-worker-version", "--image",
                         "--headers", "--health-headers"):
                self.assertIn(flag, argv)
            self.assertEqual(argv[argv.index("--expected-worker-version") + 1], NEW_VERSION)
            self.assertEqual(argv[argv.index("--image") + 1], PROD_IMAGE)
            self.assertEqual(argv[argv.index("--deploy-id") + 1], DEPLOY_ID)
        urls = box.curl_urls()
        self.assertEqual(len(urls), 38)
        for index, url in enumerate(urls):
            self.assertTrue(url.endswith("/health" if index % 2 == 0 else "/readyz"), url)
        self.assertIn("stable 18/18 polls", box.summary())

    def test_legacy_sha_without_stamp_judges_revision_shape(self):
        box = Sandbox(self)
        box.set_dockerfile("FROM scratch\n")  # predates BOT_DEPLOY_ID stamping
        box.queue_curl([("200", PASS_BODY), ("200", PASS_BODY)] * 19)
        box.queue_readyz([0] * 19)
        result = box.run(self.gate_body(), extra_env=self.base_env(box))
        self.assertEqual(result.returncode, 0, result.stderr)
        for call in self.readyz_calls(box):
            argv = call["argv"]
            self.assertNotIn("--deploy-id", argv)
            # Artifact binding still applies: the admitted image is the proof.
            self.assertIn("--artifact", argv)
            self.assertIn("--image", argv)

    def test_unstaged_recovery_reads_without_artifact_or_headers(self):
        box = Sandbox(self)
        box.queue_curl([("200", PASS_BODY), ("200", PASS_BODY)] * 19)
        box.queue_readyz([0] * 19)
        result = box.run(self.gate_body(),
                         extra_env=self.base_env(box, unstaged="true"))
        self.assertEqual(result.returncode, 0, result.stderr)
        for call in self.readyz_calls(box):
            argv = call["argv"]
            for flag in ("--artifact", "--expected-worker-version", "--image",
                         "--headers", "--health-headers"):
                self.assertNotIn(flag, argv)

    def test_no_passing_answer_fails_before_the_stable_window(self):
        box = Sandbox(self)
        box.queue_curl([("200", FAIL_BODY), ("200", FAIL_BODY)] * 30)
        box.queue_readyz([1] * 30)
        result = box.run(self.gate_body(), extra_env=self.base_env(box))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.readyz_calls(box)), 30)
        self.assertIn("no fully-passing answer", box.summary())

    def test_unstable_gateway_fails_after_the_window(self):
        box = Sandbox(self)
        box.queue_curl([("200", PASS_BODY), ("200", PASS_BODY)] +
                       [("200", FAIL_BODY), ("200", FAIL_BODY)] * 60)
        box.queue_readyz([0] + [1] * 60)
        result = box.run(self.gate_body(), extra_env=self.base_env(box))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.readyz_calls(box)), 61)
        self.assertIn("no 3 min stable gateway-ready window", box.summary())


class ImageConvergenceTests(WorkflowFilesTests):
    def test_serving_proof_brackets_the_stable_window(self):
        job = self.production["jobs"]["production"]
        serving_at, serving = find_step_by_name(job, "Verify the promoted production Worker and image")
        gate_at, _ = find_step_by_name(job, "Gate on stable /health + /readyz gateway-ready")
        recheck_at, recheck = find_step_by_name(
            job, "Recheck the promoted image after the stable readiness window")
        self.assertLess(serving_at, gate_at)
        self.assertLess(gate_at, recheck_at)
        for step in (serving, recheck):
            body = run_of(step)
            self.assertIn("production_deploy.py serving", body)
            self.assertIn("--artifact", body)
            self.assertIn("--expected-worker-version", body)
            condition = step.get("if", "")
            self.assertIn("env.MODE == 'deploy'", condition)
            self.assertIn("env.ALLOW_UNSTAGED != 'true'", condition)

    def test_cold_takeover_reaches_health_before_final_convergence(self):
        # Removing --preflight deadlocks the actual inline shell before its
        # first /health; omitting --url from the final call loses lag proof.
        job = self.production["jobs"]["production"]
        _, identity = find_step_by_name(job, "Verify the promoted production Worker and image")
        _, gate = find_step_by_name(job, "Gate on stable /health + /readyz gateway-ready")
        _, final = find_step_by_name(job, "Recheck the promoted image after the stable readiness window")
        box = Sandbox(self)
        snapshot = box.runner_temp / "production-rollback-snapshot.json"
        snapshot.write_text("{}", encoding="utf-8")
        box.queue_curl([("200", PASS_BODY), ("200", PASS_BODY)] * 19)
        box.queue_readyz([0] * 19)
        result = box.run("\n".join(map(run_of, (identity, gate, final))), extra_env={
            "SHA": SHA, "DEPLOY_ID": DEPLOY_ID, "NEW_VERSION": NEW_VERSION,
            "MODE": "deploy", "ALLOW_UNSTAGED": "false",
            "COLD_START_REQUIRED": "1",
            "PRODUCTION_URL": "https://prod.example.invalid",
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(box.curl_urls()), 38)
        calls = [json.loads(line) for line in
                 (box.log / "calls.jsonl").read_text(encoding="utf-8").splitlines()]
        serving = [(index, call["argv"]) for index, call in enumerate(calls)
                   if call["cmd"] == "python3" and "serving" in call["argv"]]
        health = [index for index, call in enumerate(calls)
                  if call["cmd"] == "curl" and call["url"].endswith("/health")]
        self.assertEqual(len(serving), 2)
        self.assertLess(serving[0][0], min(health))
        self.assertLess(max(health), serving[1][0])
        self.assertIn("--preflight", serving[0][1])
        self.assertNotIn("--preflight", serving[1][1])
        self.assertNotIn("--url", serving[0][1])
        self.assertEqual(serving[1][1][serving[1][1].index("--url") + 1],
                         "https://prod.example.invalid")
        for _, argv in serving:
            self.assertIn("--snapshot", argv)
            self.assertEqual(argv[argv.index("--snapshot") + 1], str(snapshot))
        proof = json.loads((box.runner_temp / "production-promotion-evidence.json").read_text())
        self.assertEqual(proof["phase"], "convergence")

    def test_initial_serving_handoff_is_identity_not_convergence(self):
        _, identity = find_step_by_name(
            self.production["jobs"]["production"], "Verify the promoted production Worker and image")
        box = Sandbox(self)
        result = box.run(run_of(identity), extra_env={
            "SHA": SHA, "DEPLOY_ID": DEPLOY_ID, "NEW_VERSION": NEW_VERSION,
            "PRODUCTION_URL": "https://prod.example.invalid",
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        proof = json.loads((box.runner_temp / "production-promotion-evidence.json").read_text())
        self.assertEqual(proof["phase"], "identity",
                         "the first call may verify identity, not claim a cold instance is ready")
        self.assertEqual(proof["worker_version"], NEW_VERSION)
        self.assertEqual(proof["production_image"], PROD_IMAGE)
        self.assertEqual(box.curl_urls(), [])

    def test_serving_failure_fails_closed(self):
        _, serving = find_step_by_name(
            self.production["jobs"]["production"],
            "Verify the promoted production Worker and image")
        box = Sandbox(self)
        result = box.run(run_of(serving), extra_env={
            "SHA": SHA, "DEPLOY_ID": DEPLOY_ID, "NEW_VERSION": NEW_VERSION,
            "SERVING_EXIT": "1",
        })
        self.assertNotEqual(result.returncode, 0)

    def test_record_and_evidence_require_post_window_success(self):
        job = self.production["jobs"]["production"]
        _, record = find_step_by_name(job, "Record the production deployment")
        body = run_of(record)
        self.assertIn("PROMOTION_OUTCOME", body)
        self.assertIn("GATE_OUTCOME", body)
        self.assertIn("ERROR_GATE_OUTCOME", body)
        _, upload = find_step_by_name(job, "Upload immutable production promotion evidence")
        condition = upload.get("if", "")
        self.assertIn("success()", condition)
        self.assertIn("env.MODE == 'deploy'", condition)
        self.assertIn("env.ALLOW_UNSTAGED != 'true'", condition)
        self.assertIn("steps.promotion_recheck.outcome == 'success'", condition)


class RecordRoutingExecutionTests(WorkflowFilesTests):
    def record_body(self):
        _, record = find_step_by_name(
            self.production["jobs"]["production"], "Record the production deployment")
        return run_of(record)

    def run_record(self, gate="success", error_gate="success", promotion="success",
                   mode="deploy", unstaged="false"):
        box = Sandbox(self)
        result = box.run(self.record_body(), extra_env={
            "SHA": SHA, "DEPLOY_ID": DEPLOY_ID, "MODE": mode,
            "ALLOW_UNSTAGED": unstaged, "GATE_OUTCOME": gate,
            "ERROR_GATE_OUTCOME": error_gate, "PROMOTION_OUTCOME": promotion,
            "RUN_URL": "https://example.invalid/run", "WORKER_URL": "https://prod.example.invalid",
            "GH_TOKEN": "synthetic",
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        records = [c for c in box.calls()
                   if any("deploy_id.py" in a for a in c["argv"])]
        self.assertEqual(len(records), 1)
        return records[0]["argv"]

    def test_gated_success_records_success(self):
        argv = self.run_record()
        self.assertIn("--state", argv)
        self.assertEqual(argv[argv.index("--state") + 1], "success")
        self.assertNotIn("--rollback", argv)

    def test_gate_failures_record_failure(self):
        for kwargs in ({"gate": "failure"}, {"error_gate": "failure"},
                       {"error_gate": "cancelled"}, {"promotion": "failure"}):
            with self.subTest(**kwargs):
                argv = self.run_record(**kwargs)
                self.assertEqual(argv[argv.index("--state") + 1], "failure")

    def test_skipped_error_gate_still_records_success(self):
        argv = self.run_record(error_gate="skipped")
        self.assertEqual(argv[argv.index("--state") + 1], "success")

    def test_unstaged_recovery_does_not_need_post_window_proof(self):
        argv = self.run_record(unstaged="true", promotion="failure")
        self.assertEqual(argv[argv.index("--state") + 1], "success")

    def test_rollback_record_names_itself(self):
        argv = self.run_record(mode="rollback")
        self.assertIn("--rollback", argv)


class AutoRollbackTests(WorkflowFilesTests):
    def test_staging_failures_stay_under_auto_rollback(self):
        job = self.staging["jobs"]["deploy"]
        _, rollback = find_step_by_name(job, "Auto-roll back Worker version and container image")
        condition = rollback.get("if", "")
        self.assertIn("failure()", condition)
        self.assertIn("steps.deploy.outcome == 'success'", condition)
        self.assertIn("steps.snapshot.outcome == 'success'", condition)
        box = Sandbox(self)
        snapshot = box.runner_temp / "staging-rollback-snapshot.json"
        snapshot.write_text("{}", encoding="utf-8")
        result = box.run(run_of(rollback), extra_env={
            "ROLLOUT_DIR": str(box.runner_temp),
            "STAGING_WORKER_URL": "https://staging.example.invalid",
            "ROLLBACK_ACTOR": "github-actions:1:" + SHA,
            "DEPLOY_SHA": SHA,
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((box.runner_temp / "auto-rollback-evidence.json").is_file())

    def test_production_failures_stay_under_auto_rollback(self):
        job = self.production["jobs"]["production"]
        _, rollback = find_step_by_name(job, "Auto-roll back Worker version and container image")
        condition = rollback.get("if", "")
        self.assertIn("failure()", condition)
        self.assertIn("env.MODE == 'deploy'", condition)
        self.assertIn("env.TAKEOVER == 'true'", condition)
        self.assertIn("steps.deploy.outcome == 'success'", condition)
        self.assertIn("steps.snapshot.outcome == 'success'", condition)
        box = Sandbox(self)
        snapshot = box.runner_temp / "production-rollback-snapshot.json"
        snapshot.write_text("{}", encoding="utf-8")
        result = box.run(run_of(rollback), extra_env={
            "SHA": SHA, "MODE": "deploy", "TAKEOVER": "true",
            "PRODUCTION_WORKER_URL": "https://prod.example.invalid",
            "STAGING_WORKER_URL": "https://staging.example.invalid",
            "ROLLBACK_ACTOR": "github-actions:1:" + SHA,
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((box.runner_temp / "production-auto-rollback-evidence.json").is_file())


class LeastPrivilegeTests(WorkflowFilesTests):
    def test_certify_job_is_read_only(self):
        permissions = self.staging["jobs"]["certify-promotion"].get("permissions", {})
        self.assertEqual(permissions.get("contents"), "read")
        self.assertEqual(permissions.get("actions"), "read")
        self.assertNotIn("write", json.dumps(permissions))

    def test_production_job_reads_artifacts_and_records_deployments(self):
        permissions = self.production["jobs"]["production"].get("permissions", {})
        self.assertEqual(permissions.get("actions"), "read")
        self.assertEqual(permissions.get("deployments"), "write")


if __name__ == "__main__":
    unittest.main()
