"""Test-owned interactive PTY bridge; invoked only inside a temporary uv project."""
import errno
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios

def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


pid, terminal = pty.fork()
if pid == 0:
    os.execv(sys.argv[1], sys.argv[1:])

fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
signal.signal(signal.SIGTERM, interrupted)
signal.signal(signal.SIGINT, interrupted)
try:
    while True:
        ready, _, _ = select.select([terminal, sys.stdin.fileno()], [], [], 0.05)
        if terminal in ready:
            try:
                chunk = os.read(terminal, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()
        if sys.stdin.fileno() in ready:
            chunk = os.read(sys.stdin.fileno(), 4096)
            if not chunk:
                break
            os.write(terminal, chunk)
        finished, status = os.waitpid(pid, os.WNOHANG)
        if finished:
            pid = 0
            sys.exit(os.waitstatus_to_exitcode(status))
finally:
    os.close(terminal)
    if pid:
        try:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except ProcessLookupError:
            pass
