#!/usr/bin/env python3
"""Drive the TUI through a pty and check it actually draws, measures and quits.

The TUI is the one part of membench that cannot be tested from inside the
process: it needs a terminal. This script gives it one with `pty`, sends real
keystrokes, and looks at what was drawn. Run it with the *release* binary.

    python3 scripts/tui-smoke.py target/release/membench
"""
import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import termios
import sys
import time

ANSI = re.compile(rb"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07|\x1b[=>]")


def main() -> int:
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/release/membench"
    # A short run so the smoke test finishes in seconds, not half a minute.
    argv = [
        binary, "tui",
        "--level", "mem,l1",
        "--only", "read,latency",
        "--trials", "1",
        "--duration-ms", "30",
    ]
    master, slave = pty.openpty()
    # ratatui asks the kernel for the size, so the window has to be set here:
    # a pty that has never been resized reports 0x0 and nothing is drawn.
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 100, 0, 0))
    env = dict(os.environ, TERM="xterm-256color")
    child = subprocess.Popen(
        argv, stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True
    )
    os.close(slave)

    screen = bytearray()

    def drain(until: float):
        while time.time() < until:
            r, _, _ = select.select([master], [], [], 0.2)
            if not r:
                continue
            try:
                chunk = os.read(master, 65536)
            except OSError:
                return False
            if not chunk:
                return False
            screen.extend(chunk)
        return True

    ok = True
    # 1. The idle screen.
    if not drain(time.time() + 1.5):
        print("FAIL: the TUI closed the pty before drawing")
        ok = False
    text = ANSI.sub(b"", bytes(screen)).decode("utf-8", "replace")
    for expected in ("membench", "Read", "Write", "Memory", "CPU"):
        if expected not in text:
            print(f"FAIL: the idle screen never drew {expected!r}")
            ok = False

    # 2. Start a run, let it finish, and expect a figure with a unit.
    os.write(master, b"s")
    drain(time.time() + 2.0)
    if "measuring" not in ANSI.sub(b"", bytes(screen)).decode("utf-8", "replace").lower():
        print("note: the 'measuring' status line was not seen (fast machine?)")
    deadline = time.time() + 40
    seen_number = False
    while time.time() < deadline:
        before = len(screen)
        drain(time.time() + 1.0)
        if len(screen) == before:
            if child.poll() is not None:
                break
            continue
        text = ANSI.sub(b"", bytes(screen)).decode("utf-8", "replace")
        # A pty stream interleaves cursor moves between characters, so the
        # digits and the unit are not always adjacent after stripping.
        if re.search(r"(GB/s|TB/s)", text) and re.search(r"\d", text):
            seen_number = True
            break
    if not seen_number:
        print("FAIL: no measured figure with a unit appeared on the grid")
        ok = False

    # 3. Wait for the run to report its total, then quit cleanly.
    deadline = time.time() + 30
    while time.time() < deadline:
        text = ANSI.sub(b"", bytes(screen)).decode("utf-8", "replace")
        if "cells in" in text or "failed" in text:
            break
        drain(time.time() + 0.5)
    os.write(master, b"q")
    try:
        code = child.wait(timeout=10)
    except subprocess.TimeoutExpired:
        child.kill()
        print("FAIL: q did not close the TUI")
        return 1
    if code != 0:
        print(f"FAIL: the TUI exited with {code}")
        ok = False

    final = ANSI.sub(b"", bytes(screen)).decode("utf-8", "replace")
    # The alternate screen must be left behind, not scribbled on.
    if "\x1b[?1049l" not in bytes(screen).decode("latin-1"):
        print("note: no leave-alternate-screen sequence seen")
    print("screen tail:\n" + "\n".join(final[-1200:].splitlines()[-18:]))
    print("PASS" if ok else "FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
