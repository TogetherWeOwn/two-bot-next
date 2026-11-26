"""Linux process supervisor for the protected inventory verifier."""

import ctypes
import os
import select
import signal
import subprocess
import sys

PR_SET_CHILD_SUBREAPER = 36


def supervise(argv):
    # Adopt orphaned grandchildren so only this invocation's supervisor reaps them.
    libc = ctypes.CDLL(None)
    if libc.prctl(PR_SET_CHILD_SUBREAPER, ctypes.c_ulong(1), 0, 0, 0) != 0:
        return 1
    control = sys.stdin.fileno()
    if select.select([control], [], [], 0)[0]:
        return 1

    # The trusted verifier and its gh child inherit this session's process group.
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, start_new_session=True)
    try:
        pidfd = os.pidfd_open(child.pid)
        try:
            ready, _, _ = select.select([control, pidfd], [], [])
        finally:
            os.close(pidfd)
    finally:
        # Keep the leader unreaped until signalled. Its PID pins the group ID
        # even if it exited while a descendant was still using the pipes.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        status = child.wait()
        while True:
            try:
                os.waitpid(-1, 0)
            except ChildProcessError:
                break
    return status if control not in ready and status >= 0 else 1


if __name__ == "__main__":
    try:
        sys.exit(supervise(sys.argv[1:]))
    except (OSError, ValueError):
        sys.exit(1)
