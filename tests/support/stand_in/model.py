"""Pure terminal parser and captured-event state machine; no I/O or clock reads."""
from copy import deepcopy


def parse_args(args):
    """Ignore harness options; -- separates a prompt beginning with a dash."""
    config, positional = None, []
    from collections import deque
    args = deque(args)
    while args:
        arg = args.popleft()
        if arg == "--":
            positional.extend(args)
            break
        if arg == "--atm-config":
            config = args.popleft() if args else None
            if not config or config.startswith("-"):
                raise ValueError("--atm-config requires a path")
        elif arg.startswith("-"):
            if "=" not in arg and not arg.startswith("--no-") and arg not in ("--dangerously-skip-permissions", "--offline"):
                if args and not args[0].startswith("-"):
                    args.popleft()
                elif arg in ("--model", "-m", "--provider", "--thinking", "--session-dir", "-p", "-c"):
                    raise ValueError(f"{arg} requires a value")
        else:
            positional.append(arg)
    if len(positional) > 1:
        raise ValueError("stand-in accepts at most one positional initial prompt")
    return config, next(iter(positional), None)


class InputParser:
    def __init__(self):
        self.pending = bytearray()
        self.draft = bytearray()
        self.pasting = False

    def feed(self, data):
        self.pending.extend(data)
        actions = []
        while self.pending:
            marker = b"\x1b[201~" if self.pasting else b"\x1b[200~"
            if self.pending.startswith(marker):
                del self.pending[:len(marker)]
                self.pasting = not self.pasting
                continue
            if marker.startswith(self.pending):
                break
            if not self.pasting and self.pending.startswith((b"\x1b[", b"\x1bO")):
                end = next((i for i, b in enumerate(self.pending) if i >= 2 and 64 <= b <= 126), None)
                if end is None:
                    break
                sequence = bytes(self.pending[:end+1])
                del self.pending[:end+1]
                arrow = {b"\x1b[A": "up", b"\x1b[B": "down", b"\x1bOA": "up", b"\x1bOB": "down"}.get(sequence)
                if arrow:
                    actions.append((arrow, ""))
                elif sequence.startswith(b"\x1b[") and sequence.endswith(b"R"):
                    position = sequence[2:-1].decode("ascii", errors="replace")
                    coordinates = position.split(";")
                    if len(coordinates) == 2 and all(c.isascii() and c.isdecimal() and int(c) > 0 for c in coordinates):
                        actions.append(("cursor_report", position))
                continue
            byte = self.pending.pop(0)
            if self.pasting:
                self.draft.append(byte)
            elif byte in (10, 13):
                actions.append(("submit", self.draft.decode("utf-8", errors="replace")))
                self.draft.clear()
            elif byte == 27:
                actions.append(("escape", ""))
            elif byte == 3:
                actions.append(("ctrl_c", ""))
            elif byte == 21:
                self.draft.clear()
            elif byte in (8, 127):
                self.draft = bytearray(self.draft.decode("utf-8", errors="replace")[:-1].encode())
            else:
                self.draft.append(byte)
        return actions

    def flush_escape(self):
        if not self.pasting and self.pending == b"\x1b":
            self.pending.clear()
            return [("escape", "")]
        return []


class Model:
    def __init__(self, mode, frames, session_id, work_ms=500, gap_ms=100):
        if mode not in ("claude", "codex", "pi") or min(work_ms, gap_ms) < 0:
            raise ValueError("invalid mode or negative duration")
        self.mode, self.frames = mode, frames
        self.session_id = session_id
        self.ids = [session_id]
        self.work_ms, self.gap_ms = work_ms, gap_ms
        self.state, self.deadline = "idle", None
        self.announced = False
        self.drop_next = None
        self.active_prompt = ""
        self.queue = []
        self.last_ctrl_c = None
        self.selection = 1

    def emit(self, name, variant=None, **updates):
        for frame in self.frames:
            raw = frame["data"]
            if raw.get("event", raw.get("hook_event_name")) != name:
                continue
            payload = raw.get("payload", raw)
            if variant is not None:
                actual = payload.get("reason", payload.get("source"))
                if name == "agent_end":
                    actual = payload["messages"][-1]["stopReason"]
                if actual != variant:
                    continue
            data = deepcopy(raw)
            data.pop("pid", None)
            data.pop("tmux_pane", None)
            data["session_id"] = self.session_id
            data.get("payload", data).update(updates)
            kind = "event"
            if self.drop_next in ("*", name):
                kind, self.drop_next = "dropped", None
            return [{"kind": kind, "event": name, "data": data}]
        raise ValueError(f"no captured template for {name}/{variant}")

    def render(self, detail=""):
        return [{"kind": "render", "state": self.state,
                 "session_id": self.session_id, "detail": detail, "selection": self.selection}]

    def announce(self, clear=False):
        self.announced = True
        if self.mode == "pi":
            return self.emit("session_start", "new" if clear else "startup")
        return self.emit("SessionStart", "clear" if clear and self.mode == "claude" else "startup")

    def status(self, session_id=None):
        if self.mode != "claude":
            return []
        # Captured shape; Synthetic: deterministic timing and replay of an old ID.
        data = deepcopy(next(f["data"] for f in self.frames if f["type"] == "status_update"))
        data["session_id"] = session_id or self.session_id
        return [{"kind": "status", "data": data}]

    def start(self):
        return ([] if self.mode == "codex" else self.announce()) + self.status() + self.render()

    def end_turn(self, aborted=False):
        self.state, self.deadline = "idle", None
        if self.mode == "pi":
            return self.emit("turn_end") + self.emit("agent_end", "aborted" if aborted else "stop")
        return [] if aborted else self.emit("Stop") + self.status()

    def handle(self, action, now_ms, next_id=None):
        kind, text = action
        if self.state == "exited":
            return []
        native_exit = "/exit" if self.mode == "claude" else "/quit"
        if kind == "exit" or text in ("/atm/exit", native_exit):
            self.state, self.deadline = "exited", None
            effects = []
            if self.mode == "pi":
                effects += self.emit("session_shutdown", "quit")
            else:
                ids = self.ids if self.mode == "codex" else [self.session_id]
                for session_id in ids:
                    self.session_id = session_id
                    effects += self.emit("SessionEnd", "other" if self.mode == "codex" else "prompt_input_exit")
            return effects + [{"kind": "exit"}]
        if kind in ("up", "down"):
            if self.state != "dialog":
                return []
            self.selection = max(1, min(2, self.selection + (1 if kind == "down" else -1)))
            return self.render()
        if kind == "ctrl_c" and self.state == "idle":
            if self.last_ctrl_c is not None and now_ms - self.last_ctrl_c <= 1000:
                return self.handle(("exit", ""), now_ms)
            self.last_ctrl_c = now_ms
            return [{"kind": "draft", "text": ""}] + self.render("draft cleared")
        if kind in ("escape", "ctrl_c"):
            working = self.state == "working"
            effects = self.end_turn(aborted=True) if working else []
            if working and self.mode == "claude":
                effects += [{"kind": "draft", "text": self.active_prompt}]
            if self.state == "dialog":
                self.state = "idle"
            return effects + self.render("interrupted")
        self.last_ctrl_c = None
        if self.state == "dialog":
            self.state = "idle"
            return [{"kind": "selection", "text": text, "option": self.selection}] + self.render("dialog selection")
        if text == "/atm/status" or (text.startswith("/atm/status ") and not any(c in text for c in "\r\n")):
            return self.status(text.partition(" ")[2] or None) + self.render("status sent")
        if self.state == "clearing":
            return [{"kind": "rejected", "text": text, "reason": "clearing"}]
        if text == "/atm/dialog":
            if self.mode != "claude":
                return self.render("unsupported control: dialog")
            self.state, self.deadline = "dialog", None
            self.selection = 1
            return self.emit("PreToolUse") + self.emit("PermissionRequest") + self.render()
        if text == "/atm/drop-next" or (text.startswith("/atm/drop-next ") and not any(c in text for c in "\r\n")):
            name = text.partition(" ")[2] or "*"
            known = {f["data"].get("event", f["data"].get("hook_event_name")) for f in self.frames}
            if name not in known and name != "*":
                return self.render("invalid event name")
            self.drop_next = name
            return self.render("drop armed")
        clear = "/clear" if self.mode == "claude" else "/new"
        if text == clear:
            if not next_id or next_id in self.ids:
                raise ValueError("clear requires a fresh next_id")
            effects = []
            if self.mode == "claude":
                effects = self.emit("SessionEnd", "clear")
            elif self.mode == "pi":
                effects = self.emit("session_shutdown", "new")
            self.session_id = next_id
            self.ids.append(next_id)
            self.announced = False
            self.queue.clear()
            self.state, self.deadline = "clearing", now_ms + self.gap_ms
            return effects + self.render()
        if self.state == "working" and self.mode == "claude" and text:
            self.queue.append(text)
            return [{"kind": "queued", "text": text}]
        if self.state != "idle" or not text:
            return [{"kind": "rejected", "text": text, "reason": self.state}]
        effects = [] if self.announced else self.announce()
        self.state, self.deadline = "working", now_ms + self.work_ms
        self.active_prompt = text
        if self.mode == "pi":
            effects += self.emit("input", text=text) + self.emit("agent_start")
        else:
            effects += self.emit("UserPromptSubmit", prompt=text)
        return [{"kind": "accepted", "text": text}] + effects + self.render()

    def tick(self, now_ms):
        if self.deadline is None or now_ms < self.deadline:
            return []
        if self.state == "clearing":
            self.state, self.deadline = "idle", None
            return ([] if self.mode == "codex" else self.announce(clear=True)) + self.status() + self.render()
        if self.state == "working":
            effects = self.end_turn()
            if self.queue:
                return effects + self.handle(("submit", self.queue.pop(0)), now_ms)
            return effects + self.render()
        return []
