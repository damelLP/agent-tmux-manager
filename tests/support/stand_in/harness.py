#!/usr/bin/env python3
"""Test-only pane program. Pi mode replays envelopes, never loads pi-atm."""
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import termios
import time
import tty

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from model import InputParser, Model

VERSIONS = {"claude": "2.1.325", "codex": "0.167.0", "pi": "1.5.0"}


def private_path(key, root):
    value = os.environ.get(key, "")
    path = Path(value)
    if not value or not path.is_absolute() or not path.resolve().is_relative_to(root):
        raise ValueError(f"{key} must be an absolute path inside ATM_STANDIN_ROOT")
    if path.resolve() == Path("/tmp/atm.sock"):
        raise ValueError("refusing live socket")
    return path


def patch_paths(value, root):
    if isinstance(value, dict):
        return {key: patch_paths(item, root) for key, item in value.items()}
    if isinstance(value, list):
        return [patch_paths(item, root) for item in value]
    if isinstance(value, str) and (value == "/fixture" or value.startswith("/fixture/")):
        return str(root) + value[len("/fixture"):]
    return value


class Runtime:
    def __init__(self):
        self.mode = Path(sys.argv[0]).name
        if self.mode not in VERSIONS:
            raise ValueError("launch through a staged claude, codex or pi name")
        root = os.environ.get("ATM_STANDIN_ROOT", "")
        if not root or not Path(root).is_absolute() or Path(root).resolve() == Path("/"):
            raise ValueError("ATM_STANDIN_ROOT must name a private directory")
        self.root = Path(root).resolve()
        self.socket_path = private_path("ATM_SOCKET", self.root)
        self.log_path = private_path("ATM_STANDIN_LOG", self.root)
        if not os.environ.get("TMUX_PANE") or not os.environ.get("TMUX", "").startswith(str(self.root) + "/"):
            raise ValueError("stand-in requires its private tmux pane")
        self.prefix = os.environ["ATM_STANDIN_ID_PREFIX"]
        self.generation = 1
        frames = [json.loads(line) for line in
                  (HERE / "fixtures" / f"{self.mode}_{VERSIONS[self.mode]}.jsonl").read_text().splitlines()]
        self.model = Model(self.mode, frames, self.next_id(),
                           int(os.environ.get("ATM_STANDIN_WORK_MS", "500")),
                           int(os.environ.get("ATM_STANDIN_CLEAR_GAP_MS", "100")))
        self.stream = None
        self.running = True
        self.log = self.log_path.open("a", buffering=1)
        Path("/proc/self/comm").write_text(self.mode)
        if Path("/proc/self/comm").read_text().strip() != self.mode:
            raise RuntimeError("failed to set process comm")
        self.record({"kind": "identity", "pid": os.getpid(), "pane": os.environ["TMUX_PANE"],
                     "mode": self.mode, "argv": sys.argv})

    def next_id(self):
        return f"{self.prefix}-{self.generation}"

    def record(self, effect):
        self.log.write(json.dumps(dict(effect, at_ms=int(time.monotonic()*1000))) + "\n")

    def forward(self, data):
        data = patch_paths(data, self.root)
        if self.mode != "pi":
            hook = "atm-hook" if self.mode == "claude" else "atm-codex-hook"
            script = HERE.parents[2] / "crates" / "atm" / "scripts" / hook
            # No injected pid/pane: the real forwarder must enrich these.
            subprocess.run(["/bin/bash", str(script)], input=json.dumps(data),
                           text=True, timeout=3, check=True, env=dict(os.environ, ATM_DEBUG="0"))
            return
        if self.stream is None:
            self.stream = socket.socket(socket.AF_UNIX)
            self.stream.settimeout(3)
            self.stream.connect(str(self.socket_path))
            self.send({"protocol_version": {"major": 1, "minor": 0},
                       "type": "connect", "client_id": f"stand-in-pi-{os.getpid()}"})
            reply = bytearray()
            while not reply.endswith(b"\n"):
                byte = self.stream.recv(1)
                if not byte or len(reply) >= 65536:
                    raise RuntimeError("missing pi replay handshake")
                reply.extend(byte)
            if json.loads(reply).get("type") != "connected":
                raise RuntimeError("pi replay handshake refused")
        data.update(pid=os.getpid(), tmux_pane=os.environ["TMUX_PANE"])
        self.send({"protocol_version": {"major": 1, "minor": 0}, "type": "pi_event", "data": data})
        if data["event"] == "session_shutdown":
            self.stream.close()
            self.stream = None

    def send(self, frame):
        self.stream.sendall((json.dumps(frame) + "\n").encode())

    def effects(self, effects):
        for effect in effects:
            self.record(effect)
            if effect["kind"] == "event":
                self.forward(effect["data"])
            elif effect["kind"] == "render":
                state = effect["state"]
                text = f"stand-in {self.mode} {self.model.session_id}: {state}\n"
                if state == "dialog":
                    text += (HERE / "fixtures/claude_permission_2.1.325.txt").read_text()
                else:
                    text += effect["detail"] + "\n❯ "
                print("\x1b[2J\x1b[H" + text.replace("\n", "\r\n"), end="", flush=True)
                if state == "idle":
                    self.record({"kind": "ready", "session_id": self.model.session_id})
            elif effect["kind"] == "exit":
                self.running = False

    def action(self, action):
        if action == ("submit", "/clear" if self.mode == "claude" else "/new"):
            self.generation += 1
        self.effects(self.model.handle(action, int(time.monotonic()*1000), self.next_id()))

    def run(self):
        saved = termios.tcgetattr(sys.stdin.fileno())
        parser = InputParser()
        try:
            tty.setraw(sys.stdin.fileno())
            # tmux paste-buffer maps LF to CR; restore the tty's usual input
            # translation while retaining noncanonical/no-echo/no-signal input.
            settings = termios.tcgetattr(sys.stdin.fileno())
            settings[0] |= termios.ICRNL
            termios.tcsetattr(sys.stdin.fileno(), termios.TCSANOW, settings)
            print("\x1b[?2004h", end="", flush=True)
            self.effects(self.model.start())
            if len(sys.argv) > 2:
                raise ValueError("stand-in accepts at most one positional initial prompt")
            if len(sys.argv) == 2:
                self.action(("submit", sys.argv[1]))
            while self.running:
                readable, _, _ = select.select([sys.stdin], [], [], .02)
                if readable:
                    data = os.read(sys.stdin.fileno(), 65536)
                    if not data:
                        self.action(("exit", ""))
                        break
                    for action in parser.feed(data):
                        self.action(action)
                else:
                    for action in parser.flush_escape():
                        self.action(action)
                self.effects(self.model.tick(int(time.monotonic()*1000)))
        finally:
            print("\x1b[?2004l\r\n", end="", flush=True)
            termios.tcsetattr(sys.stdin.fileno(), termios.TCSANOW, saved)
            if self.stream is not None:
                self.stream.close()
            self.log.close()


def terminate(_signum, _frame):
    raise SystemExit(0)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, terminate)
    try:
        Runtime().run()
    except Exception as error:
        print(f"stand-in error: {error}", file=sys.stderr, flush=True)
        sys.exit(1)
