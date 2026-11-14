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
from scene import render_state
from screens import render_screen
from pi_replay import BACKOFF_MS, enqueue

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
            for key in ("LOG", "ID_PREFIX", "WORK_MS", "CLEAR_GAP_MS", "MANUAL_CLOCK",
                        "FOLDER_TRUST", "UPDATE_PROMPT", "WORKTREE_CHOICE", "HOOKS_REVIEW",
                        "PERMISSION_DIALOG", "SCREEN_VARIANT", "FOLDER"):
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
                           int(os.environ.get("ATM_STANDIN_CLEAR_GAP_MS", "100")),
                           **{key.lower(): os.environ.get("ATM_STANDIN_" + key, "0") == "1"
                              for key in ("FOLDER_TRUST", "UPDATE_PROMPT", "WORKTREE_CHOICE",
                                          "HOOKS_REVIEW", "PERMISSION_DIALOG")})
        self.screen_catalog = json.loads((HERE / "screens.json").read_text())
        self.screen_assets = {spec[key]: (HERE.parents[2] / spec[key]).read_text()
                              for spec in self.screen_catalog for key in ("source", "style") if key in spec}
        self.screen_variant = os.environ.get("ATM_STANDIN_SCREEN_VARIANT", "u5")
        self.screen_folder = os.environ.get("ATM_STANDIN_FOLDER") or None
        self.columns, self.rows = os.get_terminal_size(sys.stdin.fileno())
        self.stream = None
        self.outbox = []
        self.retry_at = 0
        self.parser = InputParser()
        self.running = True
        self.pending_renders = []
        self.render_sequence = 0
        self.draft_dirty = False
        self.manual_clock = os.environ.get("ATM_STANDIN_MANUAL_CLOCK", "0") == "1"
        self.clock_ms = 0
        self.event_sequence = 0
        self.control_buffer = bytearray()
        self.event_stream = None
        if os.environ.get("ATM_STANDIN_EVENTS"):
            event_path = private_path("ATM_STANDIN_EVENTS", self.root)
            self.event_stream = socket.socket(socket.AF_UNIX)
            self.event_stream.settimeout(30)
            self.event_stream.connect(str(event_path))
        self.log = self.log_path.open("a", buffering=1)
        Path("/proc/self/comm").write_text(self.mode)
        if Path("/proc/self/comm").read_text().strip() != self.mode:
            raise RuntimeError("failed to set process comm")
        self.start_time = Path("/proc/self/stat").read_text().rsplit(")", 1)[1].split()[19]
        self.pane = os.environ["TMUX_PANE"]
        self.record({"kind": "identity", "mode": self.mode, "argv": sys.argv,
                     "session_id": self.model.session_id})

    def next_id(self):
        return f"{self.prefix}-{self.generation}"

    def record(self, effect):
        self.event_sequence += 1
        line = json.dumps(dict(effect, pid=os.getpid(), start_time=self.start_time,
                               pane=self.pane, event_sequence=self.event_sequence,
                               at_ms=self.now_ms())) + "\n"
        self.log.write(line)
        if self.event_stream is not None:
            self.event_stream.sendall(line.encode())

    def now_ms(self):
        return self.clock_ms if self.manual_clock else int(time.monotonic() * 1000)

    def control(self, message):
        """Private controls never pass through the simulated editor."""
        reply = {"kind": "control", "id": message.get("id")}
        if message.get("command") == "advance_to":
            value = message.get("now_ms")
            if not self.manual_clock or type(value) is not int or value < self.clock_ms:
                reply["error"] = "advance_to requires manual mode and nondecreasing integer time"
            else:
                self.clock_ms = value
                self.effects(self.model.tick(value))
        elif message.get("command") == "render_source":
            spec = next((s for s in self.screen_catalog if s["key"] == message.get("key")
                         and s["mode"] == self.mode), None)
            if spec is None:
                reply["error"] = "unknown source for this mode"
            else:
                # Supplementary exact source conformance, not a lifecycle or
                # readiness claim. The ordinary editor state remains unchanged.
                self.effects([dict(self.model.render()[0], state="source-replay",
                                   frame="source-replay", replay_spec=spec)])
        elif message.get("command") != "barrier":
            reply["error"] = "unknown control"
        self.record(dict(reply, state=self.model.state, now_ms=self.now_ms(),
                         render_sequence=self.render_sequence))

    def receive_controls(self):
        data = self.event_stream.recv(65536)
        if not data:
            raise RuntimeError("private event observer disconnected")
        self.control_buffer.extend(data)
        while b"\n" in self.control_buffer:
            line, _, rest = self.control_buffer.partition(b"\n")
            self.control_buffer = bytearray(rest)
            message = json.loads(line)
            self.control(message)
        if len(self.control_buffer) > 65536:
            raise ValueError("oversize private control")

    def next_timeout(self, now_ms):
        deadlines = []
        if not self.manual_clock and self.model.deadline is not None:
            deadlines.append(max(0, (self.model.deadline - now_ms) / 1000))
        if self.parser.pending == b"\x1b" and not self.parser.pasting:
            # Only the ambiguous standalone Escape key needs a terminal timeout.
            deadlines.append(.02)
        return min(deadlines) if deadlines else None

    def forward(self, data):
        data = patch_paths(data, self.root)
        if self.mode != "pi":
            hook = "atm-hook" if self.mode == "claude" else "atm-codex-hook"
            script = HERE.parents[2] / "crates" / "atm" / "scripts" / hook
            # No injected pid/pane: the real forwarder must enrich these.
            subprocess.run(["/bin/bash", str(script)], input=json.dumps(data),
                           text=True, timeout=3, check=True, env=dict(os.environ, ATM_DEBUG="0"))
            return
        self.observe_pi()
        data.update(pid=os.getpid(), tmux_pane=self.pane)
        frame = {"protocol_version": {"major": 1, "minor": 0}, "type": "pi_event", "data": data}
        self.outbox, accepted = enqueue(self.outbox, frame, self.now_ms(), self.retry_at)
        if not accepted:
            self.record({"kind": "transport_dropped", "event": data.get("event"), "reason": "close backoff"})
            return
        self.flush_pi()

    def disconnect_pi(self, error):
        if self.stream is not None:
            self.stream.close()
            self.stream = None
        self.retry_at = self.now_ms() + BACKOFF_MS
        self.record({"kind": "disconnected", "error": str(error), "retry_at": self.retry_at})

    def observe_pi(self):
        if self.mode != "pi" or self.stream is None:
            return
        try:
            if select.select([self.stream], [], [], 0)[0] and not self.stream.recv(65536):
                self.disconnect_pi("EOF")
        except OSError as error:
            self.disconnect_pi(error)

    def flush_pi(self):
        # Only a new accepted send invokes this. Missing sockets leave a bounded
        # FIFO for a future event; no timer or clock advancement opens a socket.
        if self.mode != "pi" or (self.stream is None and not self.socket_path.exists()):
            return
        try:
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
                # Once a write is attempted its delivery is unknown; don't turn
                # a transport exception into a promise to replay the same input.
                self.send(self.outbox.pop(0))
        except (OSError, ValueError) as error:
            self.disconnect_pi(error)

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
                draft = self.parser.draft.decode("utf-8", errors="replace")
                if "replay_spec" in effect:
                    screen = render_screen(effect["replay_spec"], self.screen_assets)
                else:
                    screen = render_state(self.screen_catalog, self.screen_assets, self.mode,
                                          effect["frame"], draft, self.screen_variant,
                                          self.columns, self.rows, effect["selection"], self.screen_folder)
                effect = dict(effect, sequence=self.render_sequence, draft=draft,
                              source=screen.source)
            self.record(effect)
            if effect["kind"] in ("event", "status"):
                self.forward(effect["data"])
            elif effect["kind"] == "draft":
                self.parser.draft = bytearray(effect["text"].encode())
            elif effect["kind"] == "render":
                if "replay_spec" not in effect:
                    self.last_render = effect
                self.draft_dirty = False
                # DSR is answered after tmux parses the preceding frame. A flush
                # alone only proves bytes reached the tty, not the captured pane.
                self.pending_renders.append(effect)
                print(screen.terminal + "\x1b[6n", end="", flush=True)
            elif effect["kind"] == "exit":
                self.running = False
                self.pending_renders.clear()

    def input(self, data):
        # Process each action before subsequent input changes the draft.
        for byte in data:
            if not self.running:
                break
            before = bytes(self.parser.draft)
            modal = self.model.dialog is not None
            hotkey = (self.model.dialog == "hooks-list" and byte == ord("t")
                      and not self.parser.pasting and not self.parser.pending)
            actions = self.parser.feed(bytes([byte]))
            if modal:
                self.parser.draft.clear()
                if hotkey:
                    actions.append(("key", "t"))
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
        if self.model.dialog is None and action == ("submit", "/clear" if self.mode == "claude" else "/new"):
            self.generation += 1
        self.effects(self.model.handle(action, self.now_ms(), self.next_id()))

    def run(self):
        saved = termios.tcgetattr(sys.stdin.fileno())
        try:
            tty.setraw(sys.stdin.fileno())
            # tmux paste-buffer maps LF to CR; restore the tty's usual input
            # translation while retaining noncanonical/no-echo/no-signal input.
            settings = termios.tcgetattr(sys.stdin.fileno())
            settings[0] |= termios.ICRNL
            termios.tcsetattr(sys.stdin.fileno(), termios.TCSANOW, settings)
            print(("\x1b[?1049h" if self.mode != "pi" else "") + "\x1b[?2004h", end="", flush=True)
            self.effects(self.model.start())
            if self.prompt is not None:
                self.action(("submit", self.prompt))
            while self.running:
                sources = [sys.stdin]
                if self.event_stream is not None:
                    sources.append(self.event_stream)
                if self.stream is not None:
                    sources.append(self.stream)
                readable, _, _ = select.select(sources, [], [], self.next_timeout(self.now_ms()))
                if sys.stdin in readable:
                    data = os.read(sys.stdin.fileno(), 65536)
                    if not data:
                        self.action(("exit", ""))
                        break
                    self.input(data)
                elif not readable:
                    for action in self.parser.flush_escape():
                        self.action(action)
                if self.running:
                    if self.event_stream in readable:
                        self.receive_controls()
                    if not self.manual_clock:
                        self.effects(self.model.tick(self.now_ms()))
                    if self.stream in readable:
                        self.observe_pi()
        finally:
            print("\x1b[?2004l" + ("\x1b[?1049l" if self.mode != "pi" else
                                     f"\x1b[{self.rows};1H") + "\r\n", end="", flush=True)
            termios.tcsetattr(sys.stdin.fileno(), termios.TCSANOW, saved)
            if self.stream is not None:
                self.stream.close()
            if self.event_stream is not None:
                self.event_stream.close()
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
