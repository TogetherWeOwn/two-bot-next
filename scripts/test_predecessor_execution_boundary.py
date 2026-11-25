"""Exercise a finite, networkless writer on the disposable Docker smoke runner."""

import select
import subprocess
import unittest
import uuid


# Reuse the Bookworm image pinned by check.yml. Only its shell and GNU timeout
# run here. The container has no database, Discord binding or mounted host files.
FIXTURE_IMAGE = "postgres:18.6-bookworm@sha256:3725f4e2499eef5134592b3b4ab79a543ed7f8e533b05b5b637af926630f6650"
WINDOW_SECONDS = 8
WRITER = """
while :; do
    printf 'synthetic write\\n' >/tmp/writer
    if [ -z "$started" ]; then
        printf 'writer started\\n'
        started=1
    fi
    sleep 0.1
done
"""


class PredecessorExecutionBoundaryTests(unittest.TestCase):
    def docker(self, *args, timeout=15):
        result = subprocess.run(["docker", *args], capture_output=True, text=True,
                                timeout=timeout)
        self.assertEqual(result.returncode, 0, f"Docker {args[0]} failed")
        return result.stdout.strip()

    def create(self, fixture):
        name = f"predecessor-boundary-{uuid.uuid4().hex}"
        self.docker("create", "--name", name, "--network=none", "--restart=no", "--init",
                    "--no-healthcheck", "--read-only", "--tmpfs=/tmp:rw,noexec,nosuid,nodev,size=1m",
                    "--cap-drop=ALL", "--security-opt=no-new-privileges", "--pids-limit=32",
                    "--memory=64m", "--user=65534:65534", "--log-driver=local",
                    "--entrypoint=/usr/bin/timeout", FIXTURE_IMAGE,
                    "--signal=KILL", str(WINDOW_SECONDS), "/bin/sh", "-c", fixture, timeout=120)
        self.addCleanup(self.remove, name)
        return name

    def remove(self, name):
        self.docker("stop", "--time=1", name)
        self.docker("rm", "--volumes", name)

    def observe_start_without_reply(self, name):
        # Observe startup in the attach stream, then kill the client while its
        # request is outstanding. No Docker stop or cleanup runs before wait.
        with subprocess.Popen(["docker", "start", "--attach", name], stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, text=True) as client:
            try:
                readable, _, _ = select.select([client.stdout], [], [], 5)
                self.assertTrue(readable, "synthetic writer did not start")
                self.assertEqual(client.stdout.readline(), "writer started\n")
                self.assertIsNone(client.poll())
            finally:
                client.kill()
                client.wait(timeout=5)
        self.assertEqual(self.docker("inspect", "--format", "{{.State.Running}}", name), "true")

    def assert_stopped(self, name, exit_code):
        self.assertEqual(self.docker("wait", name), str(exit_code))
        state_format = "{{.State.Running}} {{.State.Restarting}} {{.State.Pid}} {{.State.OOMKilled}}"
        self.assertEqual(self.docker("inspect", "--format", state_format, name), "false false 0 false")

    def test_lost_start_reply_still_expires_running_writer(self):
        name = self.create("trap '' TERM\n" + WRITER)
        self.observe_start_without_reply(name)
        self.assert_stopped(name, 137)

    def test_stalled_start_expires_without_readiness(self):
        name = self.create("exec sleep 60")
        self.docker("start", name)
        self.assert_stopped(name, 137)
        self.assertEqual(self.docker("logs", name), "")

    def test_normal_stop_confirms_writer_exited(self):
        name = self.create("trap 'exit 0' TERM\n" + WRITER)
        self.observe_start_without_reply(name)
        self.docker("stop", "--time=1", name)
        self.assert_stopped(name, 0)


if __name__ == "__main__":
    unittest.main()
