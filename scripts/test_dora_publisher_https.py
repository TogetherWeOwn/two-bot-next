"""Replay the real publisher against authenticated, loopback Git smart HTTPS.

Only the GitHub endpoint prefix in the extracted workflow shell is substituted;
authentication, fetch, commit, push and failure handling are not reimplemented.
The server trusts only Basic x-access-token plus a deliberately synthetic token.
A generated, test-only certificate is explicitly trusted (TLS verification stays
on). No inherited credentials/config/proxies or real GitHub requests are used.
Run: python3 -m unittest discover -s scripts -p 'test_dora_publisher_https.py' -v
"""

import base64
from http.client import HTTPSConnection
from http.server import BaseHTTPRequestHandler, HTTPServer
import os
from pathlib import Path
import shutil
import signal
import ssl
import subprocess
import tempfile
import threading
import unittest
from urllib.parse import urlsplit

from test_dora_publisher import BRANCH, publisher_script


TOKEN = "synthetic-https-job-token-not-a-real-credential"
BASIC = base64.b64encode(f"x-access-token:{TOKEN}".encode()).decode()
HEADER = f"Basic {BASIC}"
GIT = shutil.which("git", path=os.defpath)
WEEK = "2026-W43"


class SmartHttpsHandler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        # Never log headers, credentials, or backend output from the fixture.
        pass

    def do_GET(self):
        self.serve_git()

    def do_POST(self):
        self.serve_git()

    def serve_git(self):
        self.connection.settimeout(5)
        fixture = self.server.fixture
        url = urlsplit(self.path)
        header = self.headers.get("Authorization", "")
        accepted = header == HEADER and not fixture.deny_auth
        masks = [line.removeprefix("::add-mask::")
                 for line in fixture.stdout.read_text().splitlines()
                 if line.startswith("::add-mask::")]
        fixture.requests.append({
            "method": self.command,
            "path": self.path,
            "scheme": header.partition(" ")[0].lower(),
            "accepted": accepted,
            "masked": any(BASIC in mask for mask in masks),
        })
        if not accepted:
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="local-git-fixture"')
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if url.path not in ("/remote.git/info/refs", "/remote.git/git-upload-pack",
                            "/remote.git/git-receive-pack"):
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 <= length <= 1024 * 1024 or self.headers.get("Transfer-Encoding"):
            self.send_error(413)
            return
        body = self.rfile.read(length)
        result = subprocess.run(
            [GIT, "http-backend"], input=body, capture_output=True, timeout=10,
            env={**fixture.env, "GIT_PROJECT_ROOT": str(fixture.root),
                 "GIT_HTTP_EXPORT_ALL": "1", "REQUEST_METHOD": self.command,
                 "PATH_INFO": url.path, "QUERY_STRING": url.query,
                 "CONTENT_TYPE": self.headers.get("Content-Type", ""),
                 "CONTENT_LENGTH": str(length), "REMOTE_USER": "synthetic-fixture",
                 "HTTP_GIT_PROTOCOL": self.headers.get("Git-Protocol", "")},
        )
        if result.returncode:
            fixture.backend_errors.append("git http-backend failed")
            self.send_error(500)
            return
        headers, separator, content = result.stdout.partition(b"\r\n\r\n")
        if not separator:
            fixture.backend_errors.append("missing CGI response headers")
            self.send_error(500)
            return
        pairs = [line.decode().split(":", 1) for line in headers.split(b"\r\n")]
        status = next((int(value.strip().split()[0]) for key, value in pairs
                       if key.lower() == "status"), 200)
        self.send_response(status)
        for key, value in pairs:
            if key.lower() not in ("status", "content-length"):
                self.send_header(key, value.strip())
        self.send_header("Content-Length", str(len(content)))
        self.end_headers()
        self.wfile.write(content)


class PublisherHttpsTests(unittest.TestCase):
    def setUp(self):
        scratch = (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
                   or os.environ.get("RUNNER_TEMP"))
        temporary = tempfile.TemporaryDirectory(prefix="dora-https-test-", dir=scratch)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        home = self.root / "home"
        runner = self.root / "runner"
        hooks = self.root / "empty-hooks"
        templates = self.root / "empty-templates"
        for directory in (home, runner, hooks, templates):
            directory.mkdir()
        # Construct rather than inherit: no job secrets, Git wrappers, askpass,
        # shell startup files, user Git configuration, or ambient proxy settings.
        self.env = {
            "PATH": os.defpath, "HOME": str(home), "XDG_CONFIG_HOME": str(home),
            "LC_ALL": "C", "TMPDIR": str(self.root), "RUNNER_TEMP": str(runner),
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_SYSTEM": os.devnull,
            "GIT_CONFIG_GLOBAL": os.devnull, "GIT_TEMPLATE_DIR": str(templates),
            "GIT_TERMINAL_PROMPT": "0", "GIT_ASKPASS": "/bin/false",
            "SSH_ASKPASS": "/bin/false", "GIT_SSH_COMMAND": "/bin/false",
            "GIT_ALLOW_PROTOCOL": "file:https", "GH_TOKEN": TOKEN,
            "GIT_CONFIG_COUNT": "2", "GIT_CONFIG_KEY_0": "http.followRedirects",
            "GIT_CONFIG_VALUE_0": "false", "GIT_CONFIG_KEY_1": "http.proxy",
            "GIT_CONFIG_VALUE_1": "",
        }
        cert = self.root / "fixture-cert.pem"
        key = self.root / "fixture-key.pem"
        subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
             "-keyout", str(key), "-out", str(cert), "-days", "1",
             "-subj", "/CN=127.0.0.1", "-addext", "subjectAltName=IP:127.0.0.1"],
            env=self.env, capture_output=True, timeout=15, check=True,
        )
        self.env["GIT_SSL_CAINFO"] = str(cert)
        self.remote = self.root / "remote.git"
        self.seed = self.root / "seed"
        self.git(self.root, "init", "--bare", "--initial-branch=main", str(self.remote))
        self.git(self.remote, "config", "core.hooksPath", str(hooks))
        self.git(self.remote, "config", "http.receivepack", "true")
        self.git(self.root, "init", "--initial-branch=main", str(self.seed))
        self.git(self.seed, "config", "user.name", "Synthetic fixture")
        self.git(self.seed, "config", "user.email", "fixture@example.invalid")
        (self.seed / "README.md").write_text("Unchanged main.\n")
        self.git(self.seed, "add", "README.md")
        self.git(self.seed, "commit", "-m", "test: seed HTTPS fixture")
        self.git(self.seed, "remote", "add", "origin", str(self.remote))
        self.git(self.seed, "push", "origin", "HEAD:refs/heads/main")
        self.main = self.git(self.remote, "rev-parse", "refs/heads/main").stdout
        self.deny_auth = False
        self.requests = []
        self.backend_errors = []
        self.stdout = self.root / "publisher.stdout"
        self.stdout.touch()
        self.server = HTTPServer(("127.0.0.1", 0), SmartHttpsHandler)
        self.server.fixture = self
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.minimum_version = ssl.TLSVersion.TLSv1_2
        context.load_cert_chain(cert, key)
        self.server.socket = context.wrap_socket(self.server.socket, server_side=True)
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(self.stop_server, thread)
        self.endpoint = f"https://127.0.0.1:{self.server.server_port}/"
        self.url = self.endpoint + "remote.git"
        # Endpoint-only substitution retains the workflow's actual auth scheme
        # and options. In particular, this must NOT repair bearer into Basic.
        self.script = publisher_script().replace("https://github.com/", self.endpoint)
        self.checkouts = []

    def stop_server(self, thread):
        self.server.shutdown()
        self.server.server_close()
        thread.join(timeout=15)
        self.assertFalse(thread.is_alive(), "fixture server did not stop")
        self.assertEqual(self.backend_errors, [])

    def command(self, directory, *args):
        return subprocess.run([GIT, *args], cwd=directory, env=self.env,
                              capture_output=True, timeout=20)

    def git(self, directory, *args):
        result = self.command(directory, *args)
        self.assertEqual(result.returncode, 0,
                         (result.stdout + result.stderr).decode(errors="replace"))
        return result

    def snapshot(self, checkout):
        return tuple(self.git(checkout, *args).stdout for args in (
            ("rev-parse", "HEAD"), ("symbolic-ref", "HEAD"), ("write-tree",),
            ("diff", "--binary"), ("diff", "--cached", "--binary"),
            ("status", "--porcelain=v1", "--untracked-files=all"),
        ))

    def prepare(self, version="first"):
        checkout = self.root / f"checkout-{len(self.checkouts)}"
        self.git(self.root, "clone", str(self.remote), str(checkout))
        self.git(checkout, "remote", "set-url", "origin", self.url)
        reports = {
            f"ops/metrics/dora-{WEEK}.json": f'{{"synthetic": "{version}"}}\n'.encode(),
            f"ops/metrics/dora-{WEEK}.md": f"# Synthetic DORA\n\n{version} — intact.\n".encode(),
        }
        for name, content in reports.items():
            path = checkout / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        self.checkouts.append((checkout, self.snapshot(checkout), reports))
        return checkout, reports

    def publish(self, checkout):
        stderr = self.root / "publisher.stderr"
        with self.stdout.open("wb") as out, stderr.open("wb") as err:
            process = subprocess.Popen(
                ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", self.script],
                cwd=checkout, env={**self.env, "WEEK": WEEK}, stdout=out, stderr=err,
                start_new_session=True,
            )
            try:
                process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
                self.fail("publisher exceeded its 30 second deadline")
        return subprocess.CompletedProcess(process.args, process.returncode,
                                           self.stdout.read_bytes(), stderr.read_bytes())

    def assert_preserved(self, expected_refs):
        self.assertEqual(self.git(self.remote, "rev-parse", "refs/heads/main").stdout,
                         self.main)
        refs = self.git(self.remote, "for-each-ref", "--format=%(refname)", "refs/heads")
        self.assertEqual(set(refs.stdout.decode().splitlines()), expected_refs)
        for checkout, before, reports in self.checkouts:
            self.assertEqual(self.snapshot(checkout), before)
            for name, content in reports.items():
                self.assertEqual((checkout / name).read_bytes(), content)
        # Linked publication worktrees share config; inspect every common/local
        # config as well as any worktree-specific config created by the shell.
        for config in (*self.root.rglob("config"), *self.root.rglob("config.worktree")):
            content = config.read_text().lower()
            for forbidden in (TOKEN.lower(), BASIC.lower(), "extraheader", "authorization",
                              "credential", "x-access-token"):
                self.assertNotIn(forbidden, content, str(config.relative_to(self.root)))

    def assert_authenticated_service(self, service):
        events = [event for event in self.requests
                  if event["method"] == "POST" and event["path"] == f"/remote.git/{service}"]
        self.assertTrue(events, f"no actual {service} POST; events={self.requests}")
        self.assertTrue(all(event["accepted"] and event["scheme"] == "basic"
                            for event in events), self.requests)

    def reject_pushes(self):
        hooks = self.root / "reject-hooks"
        hooks.mkdir()
        hook = hooks / "pre-receive"
        hook.write_text("#!/bin/sh\necho 'synthetic push refusal' >&2\nexit 1\n")
        hook.chmod(0o700)
        self.git(self.remote, "config", "core.hooksPath", str(hooks))

    def test_fixture_requires_tls_1_2_or_newer(self):
        self.assertGreaterEqual(self.server.socket.context.minimum_version,
                                ssl.TLSVersion.TLSv1_2)

    def test_fixture_accepts_basic_for_real_fetch_and_push(self):
        checkout, _ = self.prepare()
        auth = ("-c", f"http.{self.endpoint}.extraheader=AUTHORIZATION: {HEADER}")
        # Require object transfer, not just successful advertisement of refs the
        # clone already has. The only path to this new commit is smart HTTPS.
        (self.seed / "README.md").write_text("New remote-only object.\n")
        self.git(self.seed, "add", "README.md")
        self.git(self.seed, "commit", "-m", "test: require authenticated fetch")
        self.git(self.seed, "push", "origin", "HEAD:refs/heads/main")
        self.git(checkout, *auth, "fetch", "origin")
        self.assertEqual(self.git(checkout, "show", "origin/main:README.md").stdout,
                         b"New remote-only object.\n")
        self.git(checkout, "config", "user.name", "Synthetic fixture")
        self.git(checkout, "config", "user.email", "fixture@example.invalid")
        self.git(checkout, "add", "ops/metrics")
        self.git(checkout, "commit", "-m", "test: synthetic HTTPS roundtrip")
        self.git(checkout, *auth, "push", "origin", f"HEAD:{BRANCH}")
        self.assert_authenticated_service("git-upload-pack")
        self.assert_authenticated_service("git-receive-pack")
        self.assertEqual(self.git(self.remote, "rev-parse", BRANCH).stdout,
                         self.git(checkout, "rev-parse", "HEAD").stdout)

    def test_fixture_challenges_bearer_and_wrong_basic(self):
        checkout, _ = self.prepare()
        for header in (f"bearer {TOKEN}", "Basic " + base64.b64encode(b"wrong:wrong").decode()):
            with self.subTest(scheme=header.partition(" ")[0]):
                self.requests.clear()
                refused = self.command(checkout, "-c", f"http.extraheader=AUTHORIZATION: {header}",
                                       "fetch", "origin")
                self.assertNotEqual(refused.returncode, 0)
                self.assertTrue(self.requests)
                self.assertTrue(all(not event["accepted"] for event in self.requests))
                connection = HTTPSConnection(
                    "127.0.0.1", self.server.server_port, timeout=5,
                    context=ssl.create_default_context(cafile=self.env["GIT_SSL_CAINFO"]),
                )
                try:
                    connection.request("GET", "/remote.git/info/refs?service=git-upload-pack",
                                       headers={"Authorization": header})
                    response = connection.getresponse()
                    self.assertEqual(response.status, 401)
                    self.assertEqual(response.getheader("WWW-Authenticate"),
                                     'Basic realm="local-git-fixture"')
                    response.read()
                finally:
                    connection.close()
        self.assert_preserved({"refs/heads/main"})

    def test_workflow_publishes_with_authenticated_fetch_and_push(self):
        checkout, reports = self.prepare()
        result = self.publish(checkout)
        self.assert_preserved({"refs/heads/main", BRANCH} if result.returncode == 0
                              else {"refs/heads/main"})
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        self.assert_authenticated_service("git-upload-pack")
        self.assert_authenticated_service("git-receive-pack")
        for name, content in reports.items():
            self.assertEqual(self.git(self.remote, "show", f"{BRANCH}:{name}").stdout, content)

    def test_workflow_masks_derived_value_before_first_network_request(self):
        checkout, _ = self.prepare()
        result = self.publish(checkout)
        self.assertTrue(self.requests, "mask test must observe real network use")
        self.assertTrue(all(event["masked"] for event in self.requests),
                        "derived Basic value was not masked before network use")
        ordinary_output = b"\n".join(line for line in result.stdout.splitlines()
                                     if not line.startswith(b"::add-mask::")) + result.stderr
        self.assertNotIn(TOKEN.encode(), ordinary_output)
        self.assertNotIn(BASIC.encode(), ordinary_output)

    def test_workflow_auth_refusal_fails_closed_preserving_reports(self):
        self.deny_auth = True
        checkout, _ = self.prepare()
        refused = self.publish(checkout)
        self.assertNotEqual(refused.returncode, 0)
        self.assertTrue(self.requests)
        self.assertTrue(all(not event["accepted"] for event in self.requests))
        self.assertFalse(any("git-receive-pack" in event["path"] for event in self.requests))
        self.assert_preserved({"refs/heads/main"})

    def test_workflow_refused_first_push_is_authenticated_and_preserves_reports(self):
        self.reject_pushes()
        checkout, _ = self.prepare()
        refused = self.publish(checkout)
        self.assert_preserved({"refs/heads/main"})
        self.assertNotEqual(refused.returncode, 0)
        self.assert_authenticated_service("git-upload-pack")
        self.assert_authenticated_service("git-receive-pack")
        self.assertIn(b"pre-receive hook declined", refused.stderr)
        self.assertIn(b"evidence preserved", refused.stdout + refused.stderr)

    def test_workflow_refused_update_retains_previous_remote_reports(self):
        checkout, original = self.prepare()
        first = self.publish(checkout)
        self.assertEqual(first.returncode, 0, first.stderr.decode(errors="replace"))
        tip = self.git(self.remote, "rev-parse", BRANCH).stdout
        self.reject_pushes()
        replacement, _ = self.prepare("replacement")
        self.requests.clear()
        refused = self.publish(replacement)
        self.assertNotEqual(refused.returncode, 0)
        self.assert_authenticated_service("git-upload-pack")
        self.assert_authenticated_service("git-receive-pack")
        self.assertIn(b"pre-receive hook declined", refused.stderr)
        self.assertEqual(self.git(self.remote, "rev-parse", BRANCH).stdout, tip)
        for name, content in original.items():
            self.assertEqual(self.git(self.remote, "show", f"{BRANCH}:{name}").stdout, content)
        self.assert_preserved({"refs/heads/main", BRANCH})


if __name__ == "__main__":
    unittest.main()
