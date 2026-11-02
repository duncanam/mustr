"""Control a fixture-owned interactive profile startup through a PTY."""
import errno
import fcntl
import json
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

ROWS, COLS = 40, 120
pid, terminal = pty.fork()


def interrupted(_signum, _frame):
    raise SystemExit(128 + signal.SIGTERM)


signal.signal(signal.SIGTERM, interrupted)
if pid == 0:
    os.execv(sys.argv[1], sys.argv[1:])
fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
output = bytearray()

def read_until(marker, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if marker in output:
            return
        if not select.select([terminal], [], [], 0.1)[0]:
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited:
                raise RuntimeError(f"profile process exited early: {status}")
            continue
        try:
            chunk = os.read(terminal, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                break
            raise
        if not chunk:
            break
        output.extend(chunk)
    if marker not in output:
        raise TimeoutError(f"screen marker not observed: {marker!r}; output tail={bytes(output[-3000:])!r}")

try:
    read_until(b"Profile startup")
    read_until(b"starting")
    if b"architect" not in output:
        raise AssertionError("startup panel did not show the first slot")
    print(json.dumps({"event": "startup-visible"}), flush=True)
    if sys.stdin.readline().strip() != "q":
        raise RuntimeError("controller did not request cancellation")
    os.write(terminal, b"q")
    read_until(b"Cancellation requested")
    print(json.dumps({"event": "cancel-visible"}), flush=True)
    deadline = time.monotonic() + 12
    ready_reported = False
    while time.monotonic() < deadline:
        if not ready_reported and "architect · ready".encode() in output:
            print(json.dumps({"event": "status-ready"}), flush=True)
            ready_reported = True
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited:
            pid = 0
            code = os.waitstatus_to_exitcode(status)
            while select.select([terminal], [], [], 0)[0]:
                try:
                    chunk = os.read(terminal, 65536)
                    if not chunk:
                        break
                    output.extend(chunk)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
            print(json.dumps({"event": "finished", "code": code, "output": output.decode("utf-8", "replace")}), flush=True)
            sys.exit(0)
        if select.select([terminal], [], [], 0.05)[0]:
            try:
                output.extend(os.read(terminal, 65536))
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
    print(json.dumps({"event": "still-running", "output": output.decode("utf-8", "replace")}), flush=True)
finally:
    os.close(terminal)
    if pid:
        try:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
        except ProcessLookupError:
            pass
