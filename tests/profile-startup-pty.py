"""Run the fixture-owned interactive hub inside an isolated PTY via uv."""
import codecs
import errno
import fcntl
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import time
import unicodedata

ROWS, COLS = 40, 120


class Screen:
    def __init__(self):
        self.cells = [[" "] * COLS for _ in range(ROWS)]
        self.x = self.y = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, chunk):
        self.pending += self.decoder.decode(chunk)
        while self.pending:
            char = self.pending[0]
            if char == "\x1b":
                if len(self.pending) < 2:
                    return
                if self.pending[1] == "[":
                    match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending)
                    if not match:
                        return
                    self.csi(match[1], match[3])
                    self.pending = self.pending[match.end():]
                    continue
                if self.pending[1] == "]":
                    match = re.search(r"\x07|\x1b\\\\", self.pending)
                    if not match:
                        return
                    self.pending = self.pending[match.end():]
                    continue
                self.pending = self.pending[2:]
                continue
            self.pending = self.pending[1:]
            if char == "\r":
                self.x = 0
            elif char == "\n":
                self.y = min(ROWS - 1, self.y + 1)
            elif char == "\b":
                self.x = max(0, self.x - 1)
            elif char >= " " and not unicodedata.combining(char):
                if self.x < COLS:
                    self.cells[self.y][self.x] = char
                self.x = min(COLS, self.x + (2 if unicodedata.east_asian_width(char) in "WF" else 1))

    def csi(self, raw, command):
        args = [int(value or "0") for value in raw.lstrip("?><=").split(";")]
        first = args[0] or 1
        if command in "Hf":
            self.y = min(ROWS - 1, max(0, first - 1))
            self.x = min(COLS - 1, max(0, (args[1] if len(args) > 1 else 1) - 1))
        elif command == "G":
            self.x = min(COLS - 1, max(0, first - 1))
        elif command == "d":
            self.y = min(ROWS - 1, max(0, first - 1))
        elif command == "A":
            self.y = max(0, self.y - first)
        elif command == "B":
            self.y = min(ROWS - 1, self.y + first)
        elif command == "C":
            self.x = min(COLS - 1, self.x + first)
        elif command == "D":
            self.x = max(0, self.x - first)
        elif command == "J" and args[0] in (2, 3):
            self.cells = [[" "] * COLS for _ in range(ROWS)]
        elif command == "K":
            start, end = (0, COLS) if args[0] == 2 else ((0, self.x + 1) if args[0] == 1 else (self.x, COLS))
            self.cells[self.y][start:end] = [" "] * (end - start)

    def text(self):
        return "\n".join("".join(row).rstrip() for row in self.cells)


pid, terminal = pty.fork()
if pid == 0:
    os.execv(sys.argv[1], sys.argv[1:])

fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
screen = Screen()
stdin_buffer = b""
status = None

def terminate(signum, _frame):
    if pid:
        try:
            os.kill(pid, signum)
        except ProcessLookupError:
            pass


signal.signal(signal.SIGTERM, terminate)
signal.signal(signal.SIGINT, terminate)
deadline = time.monotonic() + float(os.environ.get("MUSTR_TEST_PTY_TIMEOUT", "30"))
try:
    while time.monotonic() < deadline:
        ready, _, _ = select.select([terminal, sys.stdin], [], [], 0.05)
        if sys.stdin in ready:
            line = sys.stdin.buffer.readline()
            if not line:
                continue
            key = line.rstrip(b"\r\n")
            if key == b"QUIT":
                key = b"q"
            elif key == b"ENTER":
                key = b"\r"
            else:
                key = key.replace(b"<ENTER>", b"\r").replace(b"<ESC>", b"\x1b")
            if key:
                os.write(terminal, key)
        if terminal in ready:
            try:
                chunk = os.read(terminal, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            screen.feed(chunk)
            sys.stdout.write("\n__MSTR_SCREEN__\n" + screen.text() + "\n__MSTR_END_SCREEN__\n")
            sys.stdout.flush()
        waited, child_status = os.waitpid(pid, os.WNOHANG)
        if waited:
            status = os.waitstatus_to_exitcode(child_status)
            pid = 0
            break
    else:
        raise TimeoutError("fixture-owned PTY did not exit before its test deadline")
    if pid:
        _, child_status = os.waitpid(pid, 0)
        status = os.waitstatus_to_exitcode(child_status)
        pid = 0
    sys.exit(status or 0)
finally:
    os.close(terminal)
    if pid:
        try:
            os.kill(pid, signal.SIGTERM)
            time.sleep(0.2)
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except ProcessLookupError:
            pass
