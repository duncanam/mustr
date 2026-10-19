"""Run only the test-owned hub in an isolated PTY; never attach to user terminals.

Invoked inside a temporary uv project by hub.test.mjs. No third-party packages.
"""
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
    os.execv(sys.argv[1], sys.argv[1:])

fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
output = b""
ready = None
expected_space = os.environ.get("MUSTR_TEST_SPACE_LABEL", "First · w1").encode()
sent_quit = False
deadline = time.monotonic() + 10
try:
    while time.monotonic() < deadline:
        if select.select([terminal], [], [], 0.05)[0]:
            try:
                chunk = os.read(terminal, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            output += chunk
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()
        if (ready is None and b"mustr" in output and b"\x1b[?25l" in output
                and expected_space in output):
            ready = time.monotonic()
            os.write(terminal, b"\t\x1b[Z\x1b[C\x1b[Dhlf\x1b[B\x1b[Af?")
        if ready is not None and not sent_quit and time.monotonic() - ready > 0.2:
            os.write(terminal, b"q")
            sent_quit = True
    else:
        raise TimeoutError(f"Test hub did not render bound space {expected_space!r} or quit")
    _, status = os.waitpid(pid, 0)
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
