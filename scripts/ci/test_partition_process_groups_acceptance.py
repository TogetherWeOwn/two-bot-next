"""RED acceptance: timeboxed partition commands must reap an isolated process GROUP.

Baseline afe2857162: run-test-partition.py uses subprocess.run(timeout=...)
which kills only direct Cargo, leaving child/grandchild alive to write delayed
markers. Fixed behavior must start Cargo isolated and terminate the whole group
on inventory or execution expiry; harness reaping adopted orphans is allowed.

Method: real runner main()/execute()/has_tests()/remaining_timeout() in an
isolated subreaper harness child with synthetic Cargo only (finite child AND
grandchild, handshake before controlled timeout). Smaller timeout via TEST
bootstrap patch of runtime_commands in that child; production 60s verified.
No real Cargo/bot/DB/network/service/global signal. Isolated temp cwd/env.
Finite synthetics (timeout about 5s, normal about 0.4s), always cleaned via
owned-pid registry with exact argv verification and bounded waits.

Run from REPO ROOT:
  PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p test_partition_process_groups_acceptance.py -v
"""

import importlib.util
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUNNER = ROOT / "scripts/ci/run-test-partition.py"
TIMEOUT_SMALL = 2.0
DESCENDANT_DELAY = 2.5
DESCENDANT_TOTAL = 5.0
MARKER_WAIT = 4.0
HARNESS_TIMEOUT = 30.0
DELIVERY_GRACE = 0.5

SYNTHETIC_CARGO = r"""import json, os, subprocess, sys, time
from pathlib import Path
state = Path(os.environ["STATE_DIR"])
role = os.environ.get("SYNTHETIC_ROLE", "cargo")
mode = os.environ.get("SYNTHETIC_MODE", "execution_expiry")
if mode == "normal":
    delay, total = 0.2, 0.4
else:
    delay = float(os.environ.get("SYNTHETIC_DELAY", "2.5"))
    total = float(os.environ.get("SYNTHETIC_TOTAL", "5.0"))
def sleep_total(s):
    end = time.monotonic() + s
    while time.monotonic() < end:
        time.sleep(0.05)
def wait_file(p, timeout=10.0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if p.exists():
            return True
        time.sleep(0.05)
    return p.exists()
if role == "grandchild":
    pid = os.getpid()
    (state / "grandchild.pid").write_text(str(pid))
    try:
        (state / "grandchild.pgid").write_text(str(os.getpgid(0)))
    except OSError:
        pass
    (state / "grandchild_ready").write_text(f"{pid} {time.monotonic()}")
    sleep_total(delay)
    (state / "grandchild_delayed").write_text(f"grandchild {pid} {time.monotonic()}")
    sleep_total(max(0.0, total - delay))
    (state / "grandchild_done").write_text("done")
    sys.exit(0)
if role == "child":
    pid = os.getpid()
    (state / "child.pid").write_text(str(pid))
    try:
        (state / "child.pgid").write_text(str(os.getpgid(0)))
    except OSError:
        pass
    env = dict(os.environ, SYNTHETIC_ROLE="grandchild")
    subprocess.Popen([sys.executable, __file__], env=env,
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                     stdin=subprocess.DEVNULL)
    if not wait_file(state / "grandchild_ready", 10.0):
        (state / "handshake_failed").write_text("child missing grandchild")
        sys.exit(101)
    (state / "child_ready").write_text(f"{pid} {time.monotonic()}")
    sleep_total(delay)
    (state / "child_delayed").write_text(f"child {pid} {time.monotonic()}")
    sleep_total(max(0.0, total - delay))
    (state / "child_done").write_text("done")
    sys.exit(0)
args = sys.argv[1:]
with open(state / "calls.log", "a") as out:
    out.write(json.dumps({"argv": args, "pid": os.getpid(),
                          "pgid": os.getpgid(0), "t": time.monotonic()}) + "\n")
(state / "cargo.pid").write_text(str(os.getpid()))
try:
    (state / "cargo.pgid").write_text(str(os.getpgid(0)))
except OSError:
    pass
def clean_tree():
    for n in ("child.pid", "child.pgid", "child_ready", "child_delayed", "child_done",
              "grandchild.pid", "grandchild.pgid", "grandchild_ready",
              "grandchild_delayed", "grandchild_done", "handshake_failed",
              "cargo_completed"):
        try:
            (state / n).unlink()
        except OSError:
            pass
def spawn_long():
    clean_tree()
    env = dict(os.environ, SYNTHETIC_ROLE="child")
    subprocess.Popen([sys.executable, __file__], env=env,
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                     stdin=subprocess.DEVNULL)
    ok = wait_file(state / "child_ready", 10.0) and wait_file(state / "grandchild_ready", 10.0)
    if not ok:
        (state / "handshake_failed").write_text("cargo handshake timeout")
        sys.exit(101)
    sleep_total(total)
    (state / "cargo_completed").write_text("no timeout")
def spawn_wait_short():
    clean_tree()
    env = dict(os.environ, SYNTHETIC_ROLE="child")
    subprocess.Popen([sys.executable, __file__], env=env,
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                     stdin=subprocess.DEVNULL)
    ok = wait_file(state / "child_ready", 10.0) and wait_file(state / "grandchild_ready", 10.0)
    ok = ok and wait_file(state / "child_done", 10.0) and wait_file(state / "grandchild_done", 10.0)
    if not ok:
        (state / "handshake_failed").write_text("normal tree incomplete")
        sys.exit(101)
is_probe = "--list" in args
is_second = "restore_drill" in args
if args and args[0] == "metadata":
    print("unexpected metadata", file=sys.stderr)
    sys.exit(101)
if mode == "inventory_expiry":
    if is_probe and not is_second:
        spawn_long()
        sys.exit(0)
    if is_probe and is_second:
        print("restore_fixture: test")
        sys.exit(0)
    (state / "unexpected_execution").write_text(" ".join(args))
    sys.exit(0)
if mode == "execution_expiry":
    if is_probe:
        print("alive_fixture: test" if not is_second else "restore_fixture: test")
        sys.exit(0)
    if not is_second:
        spawn_long()
        sys.exit(0)
    (state / "later_executed").write_text(" ".join(args))
    sys.exit(0)
if mode == "normal":
    if is_probe:
        print("alive_fixture: test" if not is_second else "restore_fixture: test")
        sys.exit(0)
    spawn_wait_short()
    (state / ("normal_second" if is_second else "normal_first")).write_text("ok")
    sys.exit(0)
if mode == "nonzero":
    if is_probe:
        print("alive_fixture: test" if not is_second else "restore_fixture: test")
        sys.exit(0)
    if not is_second:
        sys.exit(41)
    (state / "later_executed").write_text(" ".join(args))
    sys.exit(0)
print("unknown mode", file=sys.stderr)
sys.exit(101)
"""


def proc_state(pid):
    try:
        data = Path(f"/proc/{pid}/stat").read_text()
    except FileNotFoundError:
        return "missing"
    except OSError:
        return "unreadable"
    try:
        return data.rsplit(")", 1)[1].split()[0]
    except (IndexError, ValueError):
        return "unreadable"


def pid_is_live(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False
    return proc_state(pid) not in ("missing", "Z")


def read_int(path):
    try:
        text = Path(path).read_text().strip().split()[0]
    except (OSError, IndexError):
        return None
    try:
        value = int(text)
    except ValueError:
        return None
    return value if value > 1 else None


def safe_kill(pid, owned_argv):
    if not isinstance(pid, int) or pid <= 1:
        return "refused-global"
    if pid == os.getpid():
        return "refused-self"
    try:
        if pid == os.getppid():
            return "refused-parent"
    except OSError:
        pass
    if not isinstance(owned_argv, str) or "/" not in owned_argv:
        return "refused-no-owned-path"
    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\x00")
        tokens = [t.decode() for t in raw if t]
    except OSError:
        return "refused-unreadable"
    except UnicodeDecodeError:
        return "refused-decode"
    if owned_argv not in tokens:
        return "refused-not-owned"
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        return "already-dead"
    except PermissionError as exc:
        return f"denied:{exc}"
    except OSError as exc:
        return f"error:{exc}"
    return "killed"


def wait_dead(pid, timeout=2.0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if not pid_is_live(pid):
            return True
        time.sleep(0.05)
    return not pid_is_live(pid)


def harness_main():
    state = Path(os.environ["STATE_DIR"])
    mode = os.environ["SYNTHETIC_MODE"]
    result_path = Path(os.environ["RESULT_PATH"])
    owned_argv = os.environ.get("CARGO_PATH", "")
    timeout_small = float(os.environ.get("TIMEOUT_SMALL", str(TIMEOUT_SMALL)))
    result = {"mode": mode, "setup_error": None, "harness_pid": os.getpid(),
              "harness_pgid": None, "subreaper_ok": False, "subreaper_error": None,
              "runner_exception": None, "runner_is_timeout": False,
              "handshake_ok": False, "group_isolated": False, "group_unified": False}
    try:
        result["harness_pgid"] = os.getpgid(0)
    except OSError as exc:
        result["harness_pgid"] = f"error:{exc}"
    try:
        import ctypes

        libc = ctypes.CDLL("libc.so.6", use_errno=True)
        ret = libc.prctl(36, 1, 0, 0, 0)
        result["subreaper_ok"] = ret == 0
        if ret != 0:
            result["subreaper_error"] = f"prctl={ret}"
    except Exception as exc:
        result["subreaper_error"] = f"{type(exc).__name__}:{exc}"
    try:
        cwd = state / "cwd"
        cwd.mkdir(parents=True, exist_ok=True)
        os.chdir(cwd)
        sys.path.insert(0, str(ROOT / "scripts/ci"))
        spec = importlib.util.spec_from_file_location("partition_runner_harness", RUNNER)
        runner = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(runner)
        original = runner.runtime_commands
        other = original("other")
        alive = [item for item in other if "alive" in item[0]]
        if not alive or alive[0][3] != 60:
            raise RuntimeError(f"production alive timeout drifted: {[a[3] for a in alive]}")
        result["production_timeout"] = alive[0][3]

        def patched(partition):
            if partition != "other":
                return original(partition)
            first = ["test", "-p", "two-bot", "--locked", "--test", "alive",
                     "--", "--ignored", "--nocapture"]
            second = ["test", "-p", "two-bot", "--locked", "--test", "restore_drill",
                      "--", "--ignored", "--nocapture"]
            if mode in ("inventory_expiry", "execution_expiry", "normal"):
                return [runner.command(first, None, timeout=timeout_small),
                        runner.command(second, None)]
            if mode == "nonzero":
                return [runner.command(first, None), runner.command(second, None)]
            raise RuntimeError(f"unknown mode {mode}")

        runner.runtime_commands = patched
        old_argv = sys.argv
        sys.argv = ["run-test-partition.py", "--lane", "ignored-db-runtime",
                    "--partition", "other"]
        try:
            runner.main()
        except BaseException as exc:
            result["runner_exception"] = f"{type(exc).__name__}:{exc}"
            result["runner_is_timeout"] = isinstance(exc, subprocess.TimeoutExpired)
            if isinstance(exc, SystemExit):
                result["runner_system_exit"] = exc.code
        finally:
            sys.argv = old_argv
    except BaseException as exc:
        result["setup_error"] = f"{type(exc).__name__}:{exc}"
    try:
        time.sleep(DELIVERY_GRACE)
        end = time.monotonic() + MARKER_WAIT
        while time.monotonic() < end:
            if mode in ("normal", "nonzero"):
                break
            if (state / "child_delayed").exists() and (state / "grandchild_delayed").exists():
                break
            time.sleep(0.05)
        cargo_pid = read_int(state / "cargo.pid")
        child_pid = read_int(state / "child.pid")
        grand_pid = read_int(state / "grandchild.pid")
        cargo_pgid = read_int(state / "cargo.pgid")
        child_pgid = read_int(state / "child.pgid")
        grand_pgid = read_int(state / "grandchild.pgid")
        result.update({"cargo_pid": cargo_pid, "child_pid": child_pid,
                       "grandchild_pid": grand_pid, "cargo_pgid": cargo_pgid,
                       "child_pgid": child_pgid, "grandchild_pgid": grand_pgid})
        if mode in ("inventory_expiry", "execution_expiry", "normal"):
            ready = (state / "child_ready").exists() and (state / "grandchild_ready").exists()
            failed = (state / "handshake_failed").exists()
            result["handshake_ok"] = bool(ready and not failed and child_pid and grand_pid)
            result["handshake_failed"] = failed
        else:
            result["handshake_ok"] = not (state / "handshake_failed").exists()
        harness_pgid = result["harness_pgid"]
        if isinstance(harness_pgid, int) and cargo_pgid is not None:
            result["group_isolated"] = cargo_pgid not in (0, 1) and cargo_pgid != harness_pgid
        if cargo_pgid is not None:
            result["group_unified"] = child_pgid == cargo_pgid and grand_pgid == cargo_pgid
        states, alive = {}, {}
        for label, pid in (("cargo", cargo_pid), ("child", child_pid), ("grandchild", grand_pid)):
            if pid is None:
                states[label], alive[label] = None, None
            else:
                states[label] = proc_state(pid)
                alive[label] = pid_is_live(pid)
        result["states_before"] = states
        result["alive_before_cleanup"] = {k: v for k, v in alive.items() if v is not None}
        result["terminal_before"] = {k: (s in ("missing", "Z")) for k, s in states.items() if s is not None}
        result["child_delayed"] = (state / "child_delayed").exists()
        result["grandchild_delayed"] = (state / "grandchild_delayed").exists()
        try:
            result["child_delayed_text"] = (state / "child_delayed").read_text()[:200] if result["child_delayed"] else None
            result["grandchild_delayed_text"] = (state / "grandchild_delayed").read_text()[:200] if result["grandchild_delayed"] else None
        except OSError:
            pass
        calls = []
        try:
            for line in (state / "calls.log").read_text().splitlines():
                calls.append(json.loads(line)["argv"])
        except OSError:
            pass
        result["calls"] = calls
        result["later_executed"] = (state / "later_executed").exists()
        result["unexpected_execution"] = (state / "unexpected_execution").exists()
        result["normal_first"] = (state / "normal_first").exists()
        result["normal_second"] = (state / "normal_second").exists()
        receipt = Path(os.environ["RUNNER_TEMP"]) / "test-partition-receipts" / "ignored-db-runtime-other.json"
        result["receipt_exists"] = receipt.exists()
        try:
            result["receipt_text"] = receipt.read_text()[:200] if receipt.exists() else None
        except OSError:
            result["receipt_text"] = None
    except BaseException as exc:
        if result.get("setup_error") is None:
            result["setup_error"] = f"observe:{type(exc).__name__}:{exc}"
    cleanup = {"killed": [], "already": [], "refused": [], "reaped": []}
    try:
        for pid in (result.get("child_pid"), result.get("grandchild_pid"), result.get("cargo_pid")):
            if pid is None:
                continue
            if pid_is_live(pid):
                outcome = safe_kill(pid, owned_argv)
                cleanup["killed" if outcome == "killed" else "refused"].append([pid, outcome])
                wait_dead(pid, 2.0)
            else:
                cleanup["already"].append(pid)
        end = time.monotonic() + 2.0
        while time.monotonic() < end:
            try:
                pid, _ = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                break
            except InterruptedError:
                continue
            except OSError:
                break
            if pid == 0:
                time.sleep(0.05)
                continue
            cleanup["reaped"].append(pid)
    except BaseException as exc:
        cleanup["error"] = f"{type(exc).__name__}:{exc}"
    result["cleanup"] = cleanup
    try:
        states_after, remaining = {}, {}
        for label, pid in (("child", result.get("child_pid")),
                           ("grandchild", result.get("grandchild_pid")),
                           ("cargo", result.get("cargo_pid"))):
            if pid is None:
                continue
            states_after[label] = proc_state(pid)
            remaining[label] = pid_is_live(pid)
        result["states_after"] = states_after
        result["remaining_after_cleanup"] = remaining
    except OSError as exc:
        if result.get("setup_error") is None:
            result["setup_error"] = f"remaining:{exc}"
    result_path.write_text(json.dumps(result, indent=2) + "\n")
    if result.get("setup_error"):
        sys.exit(2)


def run_harness(mode):
    tmp = tempfile.TemporaryDirectory(prefix="partition-groups-")
    try:
        state = Path(tmp.name) / "state"
        bin_dir = Path(tmp.name) / "bin"
        runner_temp = Path(tmp.name) / "runner-temp"
        for path in (state, bin_dir, runner_temp):
            path.mkdir(parents=True, exist_ok=True)
        cargo_path = bin_dir / "cargo"
        cargo_path.write_text(f"#!{sys.executable}\n" + SYNTHETIC_CARGO)
        cargo_path.chmod(0o755)
        owned_argv = str(cargo_path)
        stale = runner_temp / "test-partition-receipts" / "ignored-db-runtime-other.json"
        stale.parent.mkdir(parents=True, exist_ok=True)
        stale.write_text("stale receipt from an earlier attempt\n")
        result_path = Path(tmp.name) / "result.json"
        env = {"PATH": str(bin_dir) + os.pathsep + "/usr/bin" + os.pathsep + "/bin",
               "HOME": tmp.name, "RUNNER_TEMP": str(runner_temp),
               "STATE_DIR": str(state), "SYNTHETIC_MODE": mode,
               "SYNTHETIC_DELAY": str(DESCENDANT_DELAY), "SYNTHETIC_TOTAL": str(DESCENDANT_TOTAL),
               "TIMEOUT_SMALL": str(TIMEOUT_SMALL), "RESULT_PATH": str(result_path),
               "CARGO_PATH": owned_argv, "PYTHONDONTWRITEBYTECODE": "1"}
        proc = None
        try:
            proc = subprocess.run(
                [sys.executable, str(Path(__file__).resolve()), "--harness"],
                # Contain even an erroneous signal to the harness's own group;
                # the runner must still isolate Cargo from this supervisor.
                cwd=str(ROOT), env=env, text=True, capture_output=True,
                start_new_session=True, timeout=HARNESS_TIMEOUT)
        finally:
            for name in ("cargo.pid", "child.pid", "grandchild.pid"):
                pid = read_int(state / name)
                if pid is None:
                    continue
                try:
                    if pid_is_live(pid):
                        safe_kill(pid, owned_argv)
                        wait_dead(pid, 2.0)
                except OSError:
                    pass
        if not result_path.exists():
            raise AssertionError(
                f"harness produced no result (setup failure, not defect): mode={mode} "
                f"returncode={proc.returncode if proc else None} "
                f"stdout={(proc.stdout if proc else '')[-2000:]} "
                f"stderr={(proc.stderr if proc else '')[-2000:]}")
        result = json.loads(result_path.read_text())
        result["_harness_stdout"] = (proc.stdout or "")[-2000:]
        result["_harness_stderr"] = (proc.stderr or "")[-2000:]
        result["_harness_returncode"] = proc.returncode
        return result
    finally:
        try:
            tmp.cleanup()
        except OSError:
            pass


class ProcessGroupAcceptance(unittest.TestCase):
    def check_fixture(self, result):
        with self.subTest("fixture-setup"):
            self.assertIsNone(result.get("setup_error"), f"setup failure, not defect: {result}")
            self.assertTrue(result.get("subreaper_ok"),
                            f"harness subreaper not set, not defect: {result}")
            self.assertEqual(result.get("_harness_returncode"), 0, f"harness crashed: {result}")
            self.assertEqual(result.get("production_timeout"), 60, f"production drifted: {result}")
            self.assertTrue(result.get("handshake_ok"), f"handshake failed, not defect: {result}")

    def check_cleanup(self, result):
        with self.subTest("cleanup-no-leak"):
            remaining = result.get("remaining_after_cleanup", {})
            for label, live in remaining.items():
                self.assertFalse(live, f"leaked {label} after fixture cleanup: {result}")
            cleanup = result.get("cleanup", {})
            self.assertEqual(cleanup.get("refused", []), [], f"cleanup refused owned pid: {result}")
            self.assertNotIn("error", cleanup, f"cleanup error: {result}")
            states_after = result.get("states_after", {})
            for label, pid_key in (("child", "child_pid"), ("grandchild", "grandchild_pid")):
                if result.get(pid_key) is not None:
                    self.assertEqual(states_after.get(label), "missing",
                                     f"not reaped {label}: {result}")

    def check_timeout_case(self, mode):
        result = run_harness(mode)
        self.check_fixture(result)
        with self.subTest("timeout-fatal"):
            self.assertTrue(result.get("runner_is_timeout"), f"expected TimeoutExpired: {result}")
        with self.subTest("descendants-terminal"):
            terminal = result.get("terminal_before", {})
            states = result.get("states_before", {})
            alive = result.get("alive_before_cleanup", {})
            for label in ("child", "grandchild"):
                self.assertTrue(terminal.get(label),
                                f"DEFECT {label} not terminal: state={states.get(label)} "
                                f"alive={alive.get(label)} cargo={result.get('cargo_pid')} "
                                f"child={result.get('child_pid')} grandchild={result.get('grandchild_pid')}")
            for label in ("child", "grandchild"):
                self.assertFalse(alive.get(label, True), f"DEFECT {label} live: {result}")
        with self.subTest("delayed-markers"):
            self.assertFalse(result.get("child_delayed"), f"DEFECT child delayed marker: {result}")
            self.assertFalse(result.get("grandchild_delayed"), f"DEFECT grandchild delayed marker: {result}")
        with self.subTest("group-isolated-unified"):
            self.assertTrue(result.get("group_isolated"),
                            f"DEFECT cargo group not isolated: cargo pgid={result.get('cargo_pgid')} "
                            f"harness pgid={result.get('harness_pgid')}")
            self.assertTrue(result.get("group_unified"),
                            f"DEFECT group split: cargo={result.get('cargo_pgid')} "
                            f"child={result.get('child_pgid')} grandchild={result.get('grandchild_pgid')}")
        with self.subTest("no-receipt-no-later"):
            self.assertFalse(result.get("receipt_exists"), f"timeout wrote receipt: {result}")
            self.assertFalse(result.get("later_executed"), f"lane continued after timeout: {result}")
            if mode == "inventory_expiry":
                self.assertFalse(result.get("unexpected_execution"),
                                 f"execution after inventory timeout: {result}")
        self.check_cleanup(result)

    def test_inventory_expiry_reaps_isolated_group(self):
        self.check_timeout_case("inventory_expiry")

    def test_execution_expiry_reaps_isolated_group(self):
        self.check_timeout_case("execution_expiry")

    def test_normal_completion_succeeds(self):
        result = run_harness("normal")
        self.check_fixture(result)
        with self.subTest("normal-success"):
            self.assertIsNone(result.get("runner_exception"), f"normal failed: {result}")
            self.assertTrue(result.get("receipt_exists"), f"normal wrote no receipt: {result}")
            self.assertIn("property_seconds", result.get("receipt_text") or "",
                          f"stale receipt not replaced: {result}")
        with self.subTest("normal-tree-and-second"):
            self.assertTrue(result.get("normal_first"), f"first tree did not complete: {result}")
            self.assertTrue(result.get("normal_second"), f"second command missing: {result}")
            terminal = result.get("terminal_before", {})
            for label in ("child", "grandchild"):
                self.assertTrue(terminal.get(label), f"normal {label} leaked running: {result}")
        self.check_cleanup(result)

    def test_nonzero_exit_is_fatal_without_receipt(self):
        result = run_harness("nonzero")
        self.check_fixture(result)
        with self.subTest("nonzero-fatal"):
            self.assertIn("CalledProcessError", result.get("runner_exception") or "",
                          f"nonzero not propagated: {result}")
        with self.subTest("no-receipt-no-later"):
            self.assertFalse(result.get("receipt_exists"), f"failure wrote receipt: {result}")
            self.assertFalse(result.get("later_executed"), f"lane continued after failure: {result}")
        self.check_cleanup(result)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--harness":
        harness_main()
    else:
        unittest.main()
