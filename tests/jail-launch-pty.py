"""Run only the synthetic native ai-jail fixture in a real PTY, via a uv project."""
import errno
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

pid, terminal = pty.fork()
if pid == 0:
    assert os.isatty(0) and os.isatty(1)
    fcntl.ioctl(1, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
    os.execv(sys.argv[1], sys.argv[1:])

def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


signal.signal(signal.SIGTERM, interrupted)
signal.signal(signal.SIGINT, interrupted)
output = bytearray()
try:
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        if select.select([terminal], [], [], 0.05)[0]:
            try:
                output.extend(os.read(terminal, 65536))
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
            if len(output) > 1024 * 1024:
                raise RuntimeError("Native PTY output exceeded fixture limit")
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited:
            pid = 0
            code = os.waitstatus_to_exitcode(status)
            if code:
                print(output.decode(errors="replace"), file=sys.stderr)
            else:
                print("native ai-jail PTY fixture exited successfully")
            sys.exit(code if code >= 0 else 128 - code)
    raise TimeoutError("Native ai-jail PTY fixture timed out: " + output.decode(errors="replace"))
finally:
    os.close(terminal)
    if pid:
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)
