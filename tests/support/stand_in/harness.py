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
import uuid

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from model import InputParser, Model, parse_args

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
        config_path, self.prompt = parse_args(sys.argv[1:])
        if config_path:
            path = Path(config_path).resolve()
            if not path.is_relative_to(self.root):
                raise ValueError("config must be inside ATM_STANDIN_ROOT")
            config = json.loads(path.read_text())
            for key in ("LOG", "ID_PREFIX", "WORK_MS", "CLEAR_GAP_MS"):
                if key in config:
                    os.environ["ATM_STANDIN_" + key] = str(config[key])
        self.log_path = private_path("ATM_STANDIN_LOG", self.root)
        if not os.environ.get("TMUX_PANE") or not os.environ.get("TMUX", "").startswith(str(self.root) + "/"):
            raise ValueError("stand-in requires its private tmux pane")
        self.prefix = os.environ["ATM_STANDIN_ID_PREFIX"] + "-" + uuid.uuid4().hex
        self.generation = 1
        frames = [json.loads(line) for line in
                  (HERE / "fixtures" / f"{self.mode}_{VERSIONS[self.mode]}.jsonl").read_text().splitlines()]
        self.model = Model(self.mode, frames, self.next_id(),
                           int(os.environ.get("ATM_STANDIN_WORK_MS", "500")),
                           int(os.environ.get("ATM_STANDIN_CLEAR_GAP_MS", "100")))
        self.stream = None
        self.outbox = []
        self.retry_at = 0
        self.parser = InputParser()
        self.running = True
        self.pending_renders = []
        self.render_sequence = 0
        self.draft_dirty = False
        self.log = self.log_path.open("a", buffering=1)
        Path("/proc/self/comm").write_text(self.mode)
        if Path("/proc/self/comm").read_text().strip() != self.mode:
            raise RuntimeError("failed to set process comm")
        start_time = Path("/proc/self/stat").read_text().rsplit(")", 1)[1].split()[19]
        self.record({"kind": "identity", "pid": os.getpid(), "pane": os.environ["TMUX_PANE"],
                     "mode": self.mode, "argv": sys.argv, "start_time": start_time,
                     "session_id": self.model.session_id})

    def next_id(self):
        return f"{self.prefix}-{self.generation}"

    def record(self, effect):
        self.log.write(json.dumps(dict(effect, pid=os.getpid(), at_ms=int(time.monotonic()*1000))) + "\n")

    def forward(self, data):
        data = patch_paths(data, self.root)
        if self.mode != "pi":
            hook = "atm-hook" if self.mode == "claude" else "atm-codex-hook"
            script = HERE.parents[2] / "crates" / "atm" / "scripts" / hook
            # No injected pid/pane: the real forwarder must enrich these.
            subprocess.run(["/bin/sh", "-c", '/bin/bash "$1"; result=$?; exit "$result"', "stand-in-hook", str(script)], input=json.dumps(data),
                           text=True, timeout=3, check=True, env=dict(os.environ, ATM_DEBUG="0"))
            return
        data.update(pid=os.getpid(), tmux_pane=os.environ["TMUX_PANE"])
        self.outbox.append({"protocol_version": {"major": 1, "minor": 0}, "type": "pi_event", "data": data})
        self.flush_pi()

    def flush_pi(self):
        if self.mode != "pi" or time.monotonic() < self.retry_at:
            return
        try:
            if self.stream is not None and select.select([self.stream], [], [], 0)[0]:
                if not self.stream.recv(65536):
                    self.stream.close()
                    self.stream = None
            if self.stream is None:
                self.stream = socket.socket(socket.AF_UNIX)
                self.stream.settimeout(.2)
                self.stream.connect(str(self.socket_path))
                self.send({"protocol_version": {"major": 1, "minor": 0},
                           "type": "connect", "client_id": f"pi-atm-{os.getpid()}"})
                reply = bytearray()
                deadline = time.monotonic() + 3
                while not reply.endswith(b"\n"):
                    byte = self.stream.recv(1)
                    if not byte or len(reply) >= 65536 or time.monotonic() > deadline:
                        raise OSError("missing pi replay handshake")
                    reply.extend(byte)
                if json.loads(reply).get("type") != "connected":
                    raise OSError("pi replay handshake refused")
                self.record({"kind": "connected"})
            while self.outbox:
                self.send(self.outbox[0])
                self.outbox.pop(0)
        except (OSError, ValueError) as error:
            if self.stream is not None:
                self.stream.close()
                self.stream = None
            self.retry_at = time.monotonic() + .1
            self.record({"kind": "reconnecting", "error": str(error)})

    def send(self, frame):
        self.stream.sendall((json.dumps(frame) + "\n").encode())

    def effects(self, effects):
        for effect in effects:
            if not self.running:
                break
            if effect["kind"] == "render":
                if self.parser.pasting or self.parser.pending:
                    self.last_render = effect
                    self.draft_dirty = True
                    continue
                self.render_sequence += 1
                effect = dict(effect, sequence=self.render_sequence)
            self.record(effect)
            if effect["kind"] in ("event", "status"):
                self.forward(effect["data"])
            elif effect["kind"] == "draft":
                self.parser.draft = bytearray(effect["text"].encode())
            elif effect["kind"] == "render":
                self.last_render = effect
                self.draft_dirty = False
                state = effect["state"]
                text = f"stand-in {self.mode} {self.model.session_id}: {state}\n"
                if state == "dialog":
                    dialog = (HERE / "fixtures/claude_permission_2.1.325.txt").read_text()
                    if effect["selection"] == 2:
                        dialog = dialog.replace("❯ 1.", "  1.").replace("  2.", "❯ 2.")
                    text += dialog
                else:
                    draft = self.parser.draft.decode("utf-8", errors="replace")
                    # Render control bytes visibly; submitted text never controls the terminal.
                    draft = "".join(c if c.isprintable() or c == "\n" else repr(c)[1:-1] for c in draft)
                    text += effect["detail"] + "\n❯ " + draft
                # DSR is answered after tmux parses the preceding frame. A flush
                # alone only proves bytes reached the tty, not the captured pane.
                self.pending_renders.append(effect)
                print("\x1b[2J\x1b[H" + text.replace("\n", "\r\n") + "\x1b[6n", end="", flush=True)
            elif effect["kind"] == "exit":
                self.running = False
                self.pending_renders.clear()

    def input(self, data):
        # Process each action before subsequent input changes the draft.
        for byte in data:
            if not self.running:
                break
            before = bytes(self.parser.draft)
            actions = self.parser.feed(bytes([byte]))
            self.draft_dirty |= self.parser.draft != before
            for action in actions:
                if action[0] != "cursor_report":
                    self.action(action)
                elif self.pending_renders:
                    effect = self.pending_renders.pop(0)
                    # A newer frame may already be on screen by the time this
                    # reply is read; never acknowledge the superseded frame.
                    if effect["sequence"] == self.render_sequence:
                        self.record(dict(effect, kind="rendered"))
                        if effect["state"] == "idle":
                            self.record(dict(effect, kind="ready"))
        if self.draft_dirty and self.running and not self.parser.pasting and not self.parser.pending:
            self.effects([self.last_render])

    def action(self, action):
        if action == ("submit", "/clear" if self.mode == "claude" else "/new"):
            self.generation += 1
        self.effects(self.model.handle(action, int(time.monotonic()*1000), self.next_id()))

    def run(self):
        saved = termios.tcgetattr(sys.stdin.fileno())
        try:
            tty.setraw(sys.stdin.fileno())
            # tmux paste-buffer maps LF to CR; restore the tty's usual input
            # translation while retaining noncanonical/no-echo/no-signal input.
            settings = termios.tcgetattr(sys.stdin.fileno())
            settings[0] |= termios.ICRNL
            termios.tcsetattr(sys.stdin.fileno(), termios.TCSANOW, settings)
            print("\x1b[?2004h", end="", flush=True)
            self.effects(self.model.start())
            if self.prompt is not None:
                self.action(("submit", self.prompt))
            while self.running:
                readable, _, _ = select.select([sys.stdin], [], [], .02)
                if readable:
                    data = os.read(sys.stdin.fileno(), 65536)
                    if not data:
                        self.action(("exit", ""))
                        break
                    self.input(data)
                else:
                    for action in self.parser.flush_escape():
                        self.action(action)
                if self.running:
                    self.effects(self.model.tick(int(time.monotonic()*1000)))
                    self.flush_pi()
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
