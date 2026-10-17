"""Fixture-owned PTY controller. Run only via the test's temporary uv project.

stdin/stdout are JSON-line test commands/replies, never a user terminal. SQLite
reads use a read-only connection and a fresh WAL snapshot; no database writes.
"""
import codecs
import errno
import fcntl
import json
import os
import pty
import re
import select
import signal
import sqlite3
import struct
import sys
import termios
import time
import unicodedata
from urllib.parse import quote

ROWS, COLS = 40, 120


class Screen:
    """Small VT screen for Ratatui output, not a search through stale ANSI history."""
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
                    match = re.search(r"\x07|\x1b\\", self.pending)
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
        args = [int(n or "0") for n in raw.lstrip("?><=").split(";")]
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
        # SGR, cursor visibility, alternate-screen and synchronized-output modes
        # do not change cells. This fixture uses only ASCII labels/one-cell borders.

    def text(self):
        return "\n".join("".join(row).rstrip() for row in self.cells)


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


pid, terminal = pty.fork()
if pid == 0:
    os.execv(sys.argv[1], sys.argv[1:2])
fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
signal.signal(signal.SIGTERM, interrupted)
signal.signal(signal.SIGINT, interrupted)
screen = Screen()
revision = 0
entered_alternate = restored_alternate = False
ansi_tail = b""


def read_terminal(deadline):
    global revision, entered_alternate, restored_alternate, ansi_tail
    if not select.select([terminal], [], [], max(0, deadline - time.monotonic()))[0]:
        return False
    try:
        chunk = os.read(terminal, 65536)
    except OSError as error:
        if error.errno == errno.EIO:
            return False
        raise
    if not chunk:
        return False
    ansi = ansi_tail + chunk
    entered_alternate |= b"\x1b[?1049h" in ansi
    restored_alternate |= b"\x1b[?1049l" in ansi
    ansi_tail = ansi[-32:]
    screen.feed(chunk)
    revision += 1
    return True


def wait_screen(request):
    before = revision
    if request.get("keys"):
        os.write(terminal, request["keys"].encode())
    deadline = time.monotonic() + 6
    while time.monotonic() < deadline:
        text = screen.text()
        updated = not request.get("keys") or revision > before
        if updated and all(part in text for part in request.get("contains", [])) and all(part not in text for part in request.get("absent", [])):
            return text
        if not read_terminal(deadline):
            break
    raise TimeoutError("PTY screen condition failed:\n" + screen.text())


def state():
    uri = "file:" + quote(sys.argv[2], safe="/") + "?mode=ro"
    with sqlite3.connect(uri, uri=True, timeout=1) as connection:
        row = connection.execute("SELECT payload FROM hub WHERE id = 1").fetchone()
    payload = json.loads(row[0])
    # Never export reset tokens even from this fixture's private DB.
    return {
        "tasks": list(payload["tasks"].values()), "messages": payload["messages"],
        "automation": payload["automation"], "members": list(payload["members"].values()),
        "resets": {space: {"state": plan["state"], "participants": len(plan["agents"])} for space, plan in payload.get("resets", {}).items()},
    }


try:
    for line in sys.stdin:
        request = json.loads(line)
        try:
            if request["op"] == "screen":
                result = wait_screen(request)
            elif request["op"] == "state":
                result = state()
            elif request["op"] == "quit":
                os.write(terminal, b"q")
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    read_terminal(min(deadline, time.monotonic() + 0.1))
                    waited, status = os.waitpid(pid, os.WNOHANG)
                    if waited:
                        pid = 0
                        # Reap does not imply the PTY output queue has drained.
                        while select.select([terminal], [], [], 0)[0]:
                            if not read_terminal(time.monotonic()):
                                break
                        result = {"code": os.waitstatus_to_exitcode(status), "entered": entered_alternate, "restored": restored_alternate}
                        break
                else:
                    raise TimeoutError("Fixture-owned hub did not quit")
            else:
                raise ValueError("Unknown test operation")
            print(json.dumps({"id": request["id"], "result": result}), flush=True)
        except Exception as error:
            print(json.dumps({"id": request["id"], "error": str(error)}), flush=True)
        if not pid:
            break
finally:
    os.close(terminal)
    if pid:
        try:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except ProcessLookupError:
            pass
