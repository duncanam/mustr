"""Test-owned stop confirmation PTY; run only in a temporary uv project."""
import errno
import json
import os
import pty
import select
import signal
import sys
import time


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


pid, terminal = pty.fork()
if pid == 0:
    os.execv(sys.argv[1], sys.argv[1:])
signal.signal(signal.SIGTERM, interrupted)
signal.signal(signal.SIGINT, interrupted)
output = b""


def read_output():
    global output
    if select.select([terminal], [], [], 0.05)[0]:
        try:
            output += os.read(terminal, 65536)
        except OSError as error:
            if error.errno != errno.EIO:
                raise


try:
    deadline = time.monotonic() + 10
    while b"Type 'yes' to close: " not in output:
        if time.monotonic() >= deadline:
            raise TimeoutError("No stop confirmation: " + output.decode(errors="replace"))
        read_output()
    print(json.dumps({"prompt": output.decode(errors="replace")}), flush=True)
    if not select.select([sys.stdin], [], [], 10)[0]:
        raise TimeoutError("Test did not answer confirmation")
    answer = json.loads(sys.stdin.readline())["answer"]
    os.write(terminal, (answer + "\n").encode())
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        read_output()
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited:
            pid = 0
            read_output()
            print(json.dumps({"code": os.waitstatus_to_exitcode(status), "output": output.decode(errors="replace")}), flush=True)
            break
    else:
        raise TimeoutError("Stop command did not exit")
finally:
    os.close(terminal)
    if pid:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)
