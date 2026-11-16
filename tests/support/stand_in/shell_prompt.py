"""The rig's bash prompt: acknowledge terminal parsing before publishing it.

Synthetic shell chrome, not a harness capture. PROMPT_COMMAND prints the prompt
with PS1 empty; a cursor-report round trip orders its event after tmux drew it.
"""
import json
import os
from pathlib import Path
import re
import select
import socket
import sys
import termios
import time
import tty


def prompt():
    root = Path(os.environ["ATM_STANDIN_ROOT"])
    identity = root / ("shell-" + os.environ["TMUX_PANE"] + ".json")
    if not identity.exists():
        # A new shell can already have atm spawn's command queued on its PTY.
        # No exited harness to acknowledge: leave that input to bash.
        print("stand-in-shell$ ", end="", flush=True)
        return
    event = dict(json.loads(identity.read_text()), kind="shell_prompt")
    identity.unlink()
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    try:
        tty.setraw(fd)
        os.write(sys.stdout.fileno(), b"stand-in-shell$ \x1b[6n")
        deadline = time.monotonic() + 30
        reply = b""
        while not re.search(rb"\x1b\[\d+;\d+R$", reply):
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
                raise TimeoutError("shell prompt cursor-report acknowledgment")
            data = os.read(fd, 1)
            if not data:
                raise EOFError("terminal closed before shell prompt acknowledgment")
            reply += data
    finally:
        termios.tcsetattr(fd, termios.TCSANOW, saved)
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(30)
        stream.connect(os.environ["ATM_STANDIN_EVENTS"])
        stream.sendall((json.dumps(event) + "\n").encode())


if __name__ == "__main__":
    prompt()
