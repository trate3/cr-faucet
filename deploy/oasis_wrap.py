#!/usr/bin/env python3
"""
Drive the `oasis` CLI through a real pty so its bubbletea TUI runs to
completion, auto-replying to cursor-position queries and feeding the
account passphrase when prompted.

Background: the CLI sends `\x1b[6n` (cursor-pos query) and blocks until
the terminal responds — plain stdin piping (no pty) makes it EOF, and
naive `expect` scripts get tangled in the same handshake. We allocate a
pty, reply to the query, and feed the passphrase.

Accounts imported with `oasis wallet import --secret … -y` are encrypted
with an empty passphrase (yes, really), so the default is "". Override
with $OASIS_PASS for accounts you created interactively.

Usage:
    ./oasis_wrap.py <oasis-args...>
Env:
    OASIS_PASS    passphrase (default '')
"""
import os
import pty
import re
import select
import sys
import time

PASS = os.environ.get("OASIS_PASS", "").encode()
CPR_QUERY = re.compile(rb"\x1b\[6n")
CPR_REPLY = b"\x1b[24;80R"
PASSPHRASE = re.compile(rb"(?i)passphrase")
# Transaction-broadcasting subcommands (rofl update/deploy/…) prompt
# "Sign this transaction? (y/N)". Auto-confirm with "y" unless the caller
# opts out with OASIS_NO_CONFIRM=1 (e.g. to inspect a tx before signing).
CONFIRM = re.compile(rb"(?i)(sign this transaction|\(y/n\))")
AUTO_CONFIRM = os.environ.get("OASIS_NO_CONFIRM", "") != "1"


def main():
    if len(sys.argv) < 2:
        sys.stderr.write("usage: oasis_wrap.py <args...>\n")
        sys.exit(2)

    pid, fd = pty.fork()
    if pid == 0:
        os.execvp("oasis", ["oasis"] + sys.argv[1:])

    sent_pass = False
    sent_confirm = False
    buf = b""
    exit_code = 0
    while True:
        r, _, _ = select.select([fd], [], [], 0.5)
        if fd in r:
            try:
                d = os.read(fd, 4096)
            except OSError:
                break
            if not d:
                break
            os.write(1, d)  # passthrough so user sees what's happening
            buf += d
            buf = buf[-4096:]
            # Cursor-pos query: reply once per query
            for _ in CPR_QUERY.findall(d):
                os.write(fd, CPR_REPLY)
            # Passphrase prompt: send once
            if not sent_pass and PASSPHRASE.search(buf):
                time.sleep(0.2)
                os.write(fd, PASS + b"\r")
                sent_pass = True
                buf = b""
            # "Sign this transaction? (y/N)": confirm once (after passphrase).
            elif AUTO_CONFIRM and not sent_confirm and CONFIRM.search(buf):
                time.sleep(0.2)
                os.write(fd, b"y\r")
                sent_confirm = True
                buf = b""
        try:
            wpid, status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            break
        if wpid:
            # Drain remaining output
            try:
                while True:
                    d = os.read(fd, 4096)
                    if not d:
                        break
                    os.write(1, d)
            except OSError:
                pass
            if os.WIFEXITED(status):
                exit_code = os.WEXITSTATUS(status)
            else:
                exit_code = 1
            break
    sys.exit(exit_code)


if __name__ == "__main__":
    main()
