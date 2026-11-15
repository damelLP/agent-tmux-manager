"""Terminal boundary regressions; no harness CLI or real daemon required."""
from contextlib import redirect_stdout
import io
import unittest

import harness
from model import InputParser
from test_model import make


def runtime():
    """Keep the real input/effect path; only constructor I/O is omitted."""
    r = harness.Runtime.__new__(harness.Runtime)
    r.mode = "claude"
    r.root = harness.Path("/fixture")
    r.model = make()
    r.parser = InputParser()
    r.running = True
    r.log = io.StringIO()
    r.generation = 1
    r.prefix = "test"
    r.prompt = None
    r.pending_renders = []
    r.render_sequence = 0
    r.draft_dirty = False
    r.manual_clock = True
    r.clock_ms = 0
    r.transport_clock_ms = 0
    r.transport_now_ms = lambda: r.transport_clock_ms
    r.retry_at = 0
    r.event_sequence = 0
    r.event_stream = None
    r.start_time = "test-start"
    r.pane = "%test"
    from test_scene import SPECS, ASSETS
    r.screen_catalog, r.screen_assets = SPECS, ASSETS
    r.screen_variant = "u5"
    r.screen_folder = None
    r.columns, r.rows = 120, 40
    return r


def journal(r):
    import json
    return [json.loads(line) for line in r.log.getvalue().splitlines()]


class ControlTests(unittest.TestCase):
    # Synthetic: acknowledging but ignoring the control would forward the paste.
    def test_refuse_next_paste_never_reaches_model_and_next_paste_works(self):
        for mode in ["claude", "codex", "pi"]:
            with self.subTest(mode=mode):
                r = runtime()
                r.mode, r.model = mode, make(mode)
                sent = []
                r.forward = sent.append
                text = "/atm/dialog\nλ not submitted;"
                with redirect_stdout(io.StringIO()):
                    r.effects(r.model.render())
                    r.control({"id": 1, "command": "refuse_next_paste"})
                    for byte in ("\x1b[200~" + text + "\x1b[201~\r").encode():
                        r.input(bytes([byte]))
                    self.assertEqual(sent, [], "a refused paste must forward nothing")
                    self.assertFalse(any(e["kind"] in ("accepted", "queued", "selection")
                                         for e in journal(r)))
                    rejected = [e for e in journal(r) if e["kind"] == "rejected"]
                    self.assertEqual([(e["reason"], e["text"]) for e in rejected],
                                     [("refuse_next_paste", text)])
                    self.assertEqual(r.model.state, "idle")
                    self.assertEqual(r.parser.draft, b"")
                    r.input(b"\x1b[200~next paste\x1b[201~\r")
                self.assertEqual(r.model.active_prompt, "next paste")
                self.assertEqual(r.model.state, "working")
                self.assertTrue(sent)

    def test_refusal_preserves_draft_and_does_not_answer_dialog(self):
        r = runtime()
        r.forward = lambda data: self.fail("refused paste reached forwarding")
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.render())
            r.input(b"keep me")
            r.control({"id": 1, "command": "refuse_next_paste"})
            r.input(b"\x1b[200~/clear\x1b[201~\r")
        self.assertEqual(r.parser.draft, b"keep me")
        self.assertEqual(r.generation, 1)
        r = runtime()
        r.model = make(folder_trust=True)
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.start())
            r.control({"id": 2, "command": "refuse_next_paste"})
            r.input(b"\x1b[200~yes\x1b[201~\r")
        self.assertEqual(r.model.dialog, "folder-trust")
        self.assertTrue(r.running)
        self.assertFalse(any(e["kind"] == "selection" for e in journal(r)))

    def test_invalid_refusal_control_leaves_draft_and_arm_intact(self):
        r = runtime()
        r.parser.draft.extend(b"keep me")
        r.control({"id": 1, "command": "refuse_next_paste"})
        self.assertNotIn("error", journal(r)[-1])
        r.control({"id": 2, "command": "refuse_next_paste"})
        self.assertIn("error", journal(r)[-1])
        with redirect_stdout(io.StringIO()):
            r.input(b"\x1b[200~rejected once\x1b[201~\r")
        self.assertEqual(r.parser.draft, b"keep me")
        self.assertEqual([e["text"] for e in journal(r) if e["kind"] == "rejected"],
                         ["rejected once"])
        for prefix in [b"\x1b", b"\x1b[200~partial"]:
            r = runtime()
            r.parser.feed(prefix)
            r.control({"id": 3, "command": "refuse_next_paste"})
            self.assertIn("error", journal(r)[-1])

    # Break caught: an acknowledgment-only hold arm forwards completion early.
    def test_held_completion_is_not_forwarded_until_release(self):
        for mode, expected in [("claude", ["Stop"]), ("codex", ["Stop"]),
                               ("pi", ["turn_end", "agent_end"])]:
            with self.subTest(mode=mode):
                r = runtime()
                r.mode, r.model = mode, make(mode)
                sent = []
                r.forward = sent.append
                with redirect_stdout(io.StringIO()):
                    r.effects(r.model.start())
                    r.action(("submit", "turn"))
                    sent.clear()
                    r.control({"id": 1, "command": "hold_next_hook"})
                    r.control({"id": 2, "command": "advance_to", "now_ms": 500})
                    lifecycle = lambda: [d for d in sent if "event" in d or "hook_event_name" in d]
                    self.assertEqual(lifecycle(), [], "held completion must not be forwarded")
                    self.assertEqual(r.model.state, "idle")
                    held = [e for e in journal(r) if e["kind"] == "held"]
                    self.assertEqual([e["event"] for e in held], expected)
                    r.control({"id": 3, "command": "barrier"})
                    self.assertEqual(lifecycle(), [])
                    r.control({"id": 4, "command": "release_hook"})
                self.assertEqual(lifecycle(), [e["data"] for e in held])
                self.assertEqual(journal(r)[-1]["id"], 4, "release acknowledges after forwarding")
                released = [e for e in journal(r) if e["kind"] == "event"
                            and e["event"] in expected]
                self.assertEqual([e["event"] for e in released], expected)
                r.control({"id": 5, "command": "release_hook"})
                self.assertIn("error", journal(r)[-1])
                self.assertEqual(len(lifecycle()), len(expected))

    def test_invalid_hook_controls_do_not_erase_named_hold_or_draft(self):
        r = runtime()
        sent = []
        r.forward = sent.append
        r.parser.draft.extend(b"keep me")
        for name in ["stop", "input", "", [], {}, True, 1]:
            r.control({"id": 1, "command": "hold_next_hook", "event": name})
            self.assertIn("error", journal(r)[-1])
        r.control({"id": 2, "command": "hold_next_hook", "event": "Stop"})
        self.assertNotIn("error", journal(r)[-1])
        for command in ["hold_next_hook", "release_hook"]:
            r.control({"id": 3, "command": command})
            self.assertIn("error", journal(r)[-1])
        self.assertEqual(r.parser.draft, b"keep me")
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.start())
            r.action(("submit", "turn"))
            r.control({"id": 4, "command": "advance_to", "now_ms": 500})
            self.assertFalse(any(d.get("hook_event_name") == "Stop" for d in sent))
            r.control({"id": 5, "command": "release_hook"})
        self.assertEqual(sum(d.get("hook_event_name") == "Stop" for d in sent), 1)

    def test_held_shutdown_can_be_released_before_process_exit(self):
        r = runtime()
        sent = []
        r.forward = sent.append
        with redirect_stdout(io.StringIO()):
            r.control({"id": 1, "command": "hold_next_hook"})
            r.action(("exit", ""))
            self.assertTrue(r.running)
            self.assertEqual(sent, [])
            r.control({"id": 2, "command": "release_hook"})
        self.assertFalse(r.running)
        self.assertEqual([d["hook_event_name"] for d in sent], ["SessionEnd"])
        self.assertEqual(journal(r)[-2]["kind"], "exit")
        self.assertEqual(journal(r)[-1]["id"], 2)

    # Synthetic: ignoring the control must leave baseline usage, not 40%.
    def test_context_share_control_changes_next_real_status_or_usage(self):
        import json
        import tempfile
        for mode, wanted in [("claude", 400000), ("pi", 108800), ("codex", 103360)]:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                r = runtime()
                r.mode, r.model, r.root = mode, make(mode), harness.Path(directory)
                reported = []
                def forward(data):
                    if mode == "claude" and "context_window" in data:
                        usage = data["context_window"]["current_usage"]
                        reported.append(usage["input_tokens"] if usage else 0)
                    elif mode == "pi" and "usage" in data:
                        reported.append(data["usage"]["contextTokens"])
                    elif mode == "codex":
                        path = harness.Path(harness.patch_paths(data["transcript_path"], r.root))
                        if path.is_file():
                            record = json.loads(path.read_text())
                            reported.append(record["payload"]["info"]["last_token_usage"]["total_tokens"])
                r.forward = forward
                with redirect_stdout(io.StringIO()):
                    r.effects(r.model.start())
                    r.action(("submit", "turn"))
                    reported.clear()
                    sequence = r.render_sequence
                    r.control({"id": 1, "command": "set_context_share", "percent": 40})
                    self.assertEqual(reported, [], "setting usage must not send it immediately")
                    self.assertEqual(r.render_sequence, sequence, "control must not redraw")
                    r.control({"id": 2, "command": "advance_to", "now_ms": 500})
                self.assertEqual(reported, [wanted], "next payload must carry the requested context share")
                self.assertEqual(journal(r)[-1]["id"], 2)

    def test_invalid_context_control_keeps_pending_share_and_draft(self):
        r = runtime()
        r.parser.draft.extend(b"keep me")
        r.control({"id": 1, "command": "set_context_share", "percent": 40})
        self.assertNotIn("error", journal(r)[-1])
        for value in [None, True, "40", -1, 101, float("nan")]:
            r.control({"id": 2, "command": "set_context_share", "percent": value})
            self.assertIn("error", journal(r)[-1])
        r.control({"id": 3, "command": "set_context_share"})
        self.assertIn("error", journal(r)[-1])
        sent = []
        r.forward = sent.append
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.status())
        self.assertEqual(sent[0]["context_window"]["used_percentage"], 40)
        self.assertEqual(r.parser.draft, b"keep me")

    def test_codex_usage_write_refuses_a_path_outside_its_private_root(self):
        import tempfile
        with tempfile.TemporaryDirectory() as directory:
            r = runtime()
            r.root = harness.Path(directory) / "private"
            r.root.mkdir()
            outside = harness.Path(directory) / "outside.jsonl"
            with self.assertRaises(ValueError):
                r.effects([{"kind": "usage", "path": str(outside), "data": {}}])
            self.assertFalse(outside.exists())

    def test_status_record_follows_forwarding_not_the_draft_redraw(self):
        r = runtime()
        sent = []
        def forward(data):
            self.assertFalse(any(e["kind"] == "status" for e in journal(r)))
            sent.append(data["session_id"])
        r.forward = forward
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.render())
            r.input(b"/atm/status old-id")
            self.assertEqual(sent, [])
            r.input(b"\r")
        self.assertEqual(sent, ["old-id"])
        status = next(e for e in journal(r) if e["kind"] == "status")
        self.assertEqual(status["data"]["session_id"], "old-id")
        self.assertEqual(journal(r)[-1]["draft"], "")

    def test_source_replay_uses_terminal_barrier_without_model_readiness(self):
        r = runtime()
        with redirect_stdout(io.StringIO()) as output:
            r.control({"id": 1, "command": "render_source", "key": "claude.permission.u5"})
            r.input(b"\x1b[1;1R")
        self.assertIn("Do you want to proceed?", output.getvalue())
        self.assertEqual(r.model.state, "idle")
        self.assertEqual(journal(r)[-1]["kind"], "rendered")
        self.assertEqual(journal(r)[-1]["state"], "source-replay")
        self.assertFalse(any(e["kind"] == "ready" for e in journal(r)))

    def test_source_replay_during_fragmented_paste_cannot_replace_lifecycle(self):
        r = runtime()
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.render())
            r.input(b"\x1b[200~my ")
            r.control({"id": 1, "command": "render_source", "key": "claude.permission.u5"})
            self.assertIn("error", journal(r)[-1])
            r.input(b"draft\x1b[201~")
            r.input(b" more")
        self.assertEqual(journal(r)[-1]["frame"], "fresh")
        self.assertEqual(journal(r)[-1]["draft"], "my draft more")
        self.assertNotIn("permission", journal(r)[-1]["source"])

    def test_source_replay_rejects_unknown_and_foreign_mode_without_output(self):
        r = runtime()
        for key in ["missing", "pi.fresh.u5", None, {}]:
            with redirect_stdout(io.StringIO()) as output:
                r.control({"id": 1, "command": "render_source", "key": key})
            self.assertEqual(output.getvalue(), "")
            self.assertIn("error", journal(r)[-1])
            self.assertEqual(r.render_sequence, 0)

    def test_event_stream_carries_identity_and_total_order(self):
        import json
        import socket
        r = runtime()
        writer, reader = socket.socketpair()
        with writer, reader:
            reader.settimeout(30)
            r.event_stream = writer
            r.record({"kind": "first"})
            r.record({"kind": "second"})
            with reader.makefile("r") as lines:
                events = [json.loads(lines.readline()), json.loads(lines.readline())]
        self.assertEqual([e["event_sequence"] for e in events], [1, 2])
        self.assertEqual([e["kind"] for e in events], ["first", "second"])
        self.assertTrue(all(e["pane"] == "%test" and e["start_time"] == "test-start" for e in events))
        self.assertEqual(events, journal(r))

    def test_manual_clock_holds_work_and_clear_until_explicit_advance(self):
        r = runtime()
        r.forward = lambda data: None
        with redirect_stdout(io.StringIO()):
            r.action(("submit", "held"))
            self.assertEqual(r.model.deadline, 500)
            r.control({"id": 1, "command": "advance_to", "now_ms": 499})
            self.assertEqual(r.model.state, "working")
            r.control({"id": 2, "command": "advance_to", "now_ms": 500})
            self.assertEqual(r.model.state, "idle")
            r.action(("submit", "/clear"))
            r.control({"id": 3, "command": "advance_to", "now_ms": 599})
            self.assertEqual(r.model.state, "clearing")
            r.control({"id": 4, "command": "advance_to", "now_ms": 600})
        self.assertEqual(r.model.state, "idle")
        self.assertEqual(journal(r)[-1]["kind"], "control")
        self.assertEqual(journal(r)[-1]["id"], 4)

    def test_control_rejects_time_reversal_without_affecting_draft(self):
        r = runtime()
        r.parser.draft.extend(b"keep me")
        r.control({"id": 1, "command": "advance_to", "now_ms": 20})
        for value in [19, -1, True, "21", None]:
            r.control({"id": 2, "command": "advance_to", "now_ms": value})
            self.assertIn("error", journal(r)[-1])
        self.assertEqual(r.clock_ms, 20)
        self.assertEqual(r.parser.draft, b"keep me")
        r.manual_clock = False
        r.control({"id": 3, "command": "advance_to", "now_ms": 30})
        self.assertIn("error", journal(r)[-1])

    def test_fragmented_control_is_separate_from_editor_input(self):
        import socket
        r = runtime()
        r.control_buffer = bytearray()
        r.parser.draft.extend(b"untouched")
        writer, reader = socket.socketpair()
        with writer, reader:
            writer.settimeout(30)
            reader.settimeout(30)
            r.event_stream = reader
            writer.sendall(b'{"id":1,"command":"bar')
            r.receive_controls()
            self.assertEqual(journal(r), [])
            writer.sendall(b'rier"}\n{"id":2,"command":"advance_to","now_ms":40}\n')
            r.receive_controls()
        self.assertEqual([e["id"] for e in journal(r)], [1, 2])
        self.assertEqual(r.clock_ms, 40)
        self.assertEqual(r.parser.draft, b"untouched")
        self.assertEqual(r.render_sequence, 0, "barriers must not redraw")

    def test_manual_clock_has_no_automatic_model_deadline(self):
        r = runtime()
        r.mode = "codex"
        r.model.deadline = 5
        self.assertIsNone(r.next_timeout(1000))
        r.manual_clock = False
        self.assertEqual(r.next_timeout(2), .003)
        self.assertEqual(r.next_timeout(9), 0)
        r.model.deadline = None
        r.parser.pending.extend(b"\x1b")
        self.assertGreater(r.next_timeout(9), 0)


class PiTransportTests(unittest.TestCase):
    def test_connect_flushes_events_without_any_handshake_reply(self):
        import json
        import socket
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as directory, socket.socket(socket.AF_UNIX) as listener:
            r = runtime()
            r.mode, r.root = "pi", Path(directory)
            r.socket_path = r.root / "no-reply.sock"
            r.stream, r.outbox = None, []
            listener.bind(str(r.socket_path))
            listener.listen(1)
            listener.settimeout(30)
            try:
                r.forward({"event": "session_start"})
                with listener.accept()[0] as peer:
                    peer.settimeout(30)
                    self.assertIsNotNone(r.stream, "an absent reply must not disconnect pi")
                    r.forward({"event": "input", "payload": {"text": "later"}})
                    with peer.makefile("r") as lines:
                        frames = [json.loads(lines.readline()) for _ in range(3)]
                    self.assertEqual([f["type"] for f in frames], ["connect", "pi_event", "pi_event"])
                    self.assertEqual(frames[2]["data"]["payload"]["text"], "later")
                    self.assertEqual(r.outbox, [])
                    self.assertEqual(r.retry_at, 0)
            finally:
                if r.stream is not None:
                    r.stream.close()

    def test_close_drops_backoff_input_and_time_alone_never_connects(self):
        import socket
        r = runtime()
        r.mode = "pi"
        r.socket_path = r.root / "absent.sock"
        r.outbox = []
        r.retry_at = 0
        writer, reader = socket.socketpair()
        r.stream = reader
        writer.close()
        r.observe_pi()
        self.assertIsNone(r.stream)
        self.assertEqual(r.retry_at, 1000)
        r.forward({"event": "input", "payload": {"text": "outage"}})
        self.assertEqual(r.outbox, [])
        self.assertEqual(journal(r)[-1]["kind"], "transport_dropped")
        r.clock_ms = 100000
        r.forward({"event": "input", "payload": {"text": "model time is irrelevant"}})
        self.assertEqual(journal(r)[-1]["kind"], "transport_dropped")
        self.assertEqual(r.next_timeout(r.clock_ms), 1.0)
        r.transport_clock_ms = 999
        r.transport_tick()
        self.assertEqual(r.retry_at, 1000)
        r.transport_clock_ms = 1000
        r.transport_tick()
        self.assertEqual(r.retry_at, 0)
        self.assertEqual(journal(r)[-1]["kind"], "transport_retry_ready")
        self.assertIsNone(r.stream, "timer expiry must not reconnect")
        self.assertIsNone(r.next_timeout(r.clock_ms))

    def test_missing_socket_queues_bounded_fifo_until_a_later_send(self):
        import tempfile
        from pathlib import Path
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory, patch.dict(harness.os.environ, {"TMUX_PANE": "%test"}):
            r = runtime()
            r.mode = "pi"
            r.root = Path(directory)
            r.socket_path = r.root / "absent.sock"
            r.stream = None
            r.outbox = []
            r.retry_at = 0
            for number in range(260):
                r.forward({"event": "input", "number": number})
            self.assertEqual(len(r.outbox), 256)
            self.assertEqual(r.outbox[0]["data"]["number"], 4)
            self.assertEqual(r.outbox[-1]["data"]["number"], 259)
            self.assertIsNone(r.next_timeout(10000))
            r.clock_ms = 10000
            self.assertIsNone(r.stream)


class RenderTests(unittest.TestCase):
    def test_scene_error_is_reported_without_exiting_or_claiming_readiness(self):
        r = runtime()
        r.model.frame = "missing-capture"
        with redirect_stdout(io.StringIO()) as output:
            r.effects(r.model.render())
            r.input(b"still alive")
        self.assertTrue(r.running)
        self.assertIn("stand-in scene error", output.getvalue())
        errors = [e for e in journal(r) if e["kind"] == "scene_error"]
        self.assertTrue(errors)
        self.assertIn("missing-capture", errors[-1]["error"])
        self.assertFalse(any(e["kind"] in ("ready", "rendered") for e in journal(r)))

    def test_render_uses_capture_not_diagnostic_or_identity_chrome(self):
        r = runtime()
        with redirect_stdout(io.StringIO()) as output:
            r.effects(r.model.render("status sent"))
        self.assertIn("Claude Code v2.1.325", output.getvalue())
        self.assertNotIn("stand-in", output.getvalue())
        self.assertNotIn("test-1", output.getvalue())
        self.assertNotIn("status sent", output.getvalue())
        self.assertIn("screen_claude-ready", journal(r)[-1]["source"])

    def test_cli_prompt_waits_for_start_dialog_instead_of_answering_it(self):
        for mode, options, key in [("claude", {"folder_trust": True}, b"\x1b[B\r"),
                                   ("codex", {"update_prompt": True}, b"\x1b")]:
            with self.subTest(mode=mode):
                r = runtime()
                r.mode, r.model, r.prompt = mode, make(mode, **options), "initial CLI prompt"
                r.forward = lambda data: None
                with redirect_stdout(io.StringIO()):
                    r.effects(r.model.start())
                    r.submit_initial_prompt()
                    self.assertEqual(r.model.state, "dialog")
                    self.assertEqual(r.prompt, "initial CLI prompt")
                    r.input(key)
                    for action in r.parser.flush_escape():
                        r.action(action)
                self.assertEqual(r.model.state, "working")
                self.assertIsNone(r.prompt)
                submitted = [e["data"].get("payload", e["data"])["prompt"]
                             for e in journal(r) if e.get("event") == "UserPromptSubmit"]
                self.assertEqual(submitted, ["initial CLI prompt"])

    def test_startup_modal_acknowledges_frame_but_never_ready(self):
        r = runtime()
        r.model = make(folder_trust=True)
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.start())
            r.input(b"\x1b[1;1R")
        self.assertEqual([e["kind"] for e in journal(r)], ["render", "rendered"])
        self.assertEqual(journal(r)[-1]["frame"], "folder-trust")

    def test_fragmented_modal_paste_cannot_leak_into_editor(self):
        r = runtime()
        r.model = make(folder_trust=True)
        r.forward = lambda data: None
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.start())
            r.input(b"\x1b[1;1R")
            for part in [b"\x1b[20", b"0~ignored", b"\n/exit", b"\x1b[201~"]:
                r.input(part)
            r.input(b"\x1b[B\r")
        self.assertEqual(r.model.state, "idle")
        self.assertEqual(r.parser.draft, b"")
        self.assertFalse(any(e.get("event") == "UserPromptSubmit" for e in journal(r)))

    def test_hook_list_t_is_key_but_pasted_t_is_not(self):
        r = runtime()
        r.mode = "codex"
        r.model = make("codex", hooks_review=True)
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.start())
            r.input(b"\r")
            r.input(b"\x1b[200~t\x1b[201~")
            self.assertEqual(r.model.dialog, "hooks-list")
            self.assertEqual(r.parser.draft, b"")
            r.input(b"t")
        self.assertEqual(r.model.state, "idle")
        self.assertEqual(r.parser.draft, b"")

    def test_ready_requires_terminal_reply_not_stdout_flush(self):
        r = runtime()
        with redirect_stdout(io.StringIO()) as output:
            r.effects(r.model.render())
        self.assertNotIn("ready", [e["kind"] for e in journal(r)])
        self.assertTrue(output.getvalue().endswith("\x1b[6n"))
        with redirect_stdout(io.StringIO()) as output:
            r.input(b"\x1b[3;4R")
        self.assertEqual(output.getvalue(), "", "a terminal reply must not re-render")
        self.assertEqual([e["kind"] for e in journal(r)], ["render", "rendered", "ready"])
        self.assertEqual(journal(r)[-1]["sequence"], 1)
        self.assertEqual(journal(r)[-2]["session_id"], "test-1")

    def test_superseded_idle_render_cannot_signal_ready(self):
        r = runtime()
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.render())
            r.model.state = "working"
            r.effects(r.model.render())
            r.input(b"\x1b[3;4R")
        self.assertEqual([e["kind"] for e in journal(r)], ["render", "render"])
        with redirect_stdout(io.StringIO()):
            r.input(b"\x1b[3;4R")
        self.assertEqual(journal(r)[-1]["kind"], "rendered")
        self.assertEqual(journal(r)[-1]["state"], "working")
        self.assertEqual(journal(r)[-1]["sequence"], 2)
        self.assertNotIn("ready", [e["kind"] for e in journal(r)])

    def test_fragmented_reply_does_not_change_draft_or_issue_another_query(self):
        r = runtime()
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.render())
            r.input(b"draft")
        with redirect_stdout(io.StringIO()) as output:
            for byte in b"\x1b[3;4R\x1b[3;9R":
                r.input(bytes([byte]))
        self.assertEqual(output.getvalue(), "")
        self.assertEqual(r.parser.draft, b"draft")
        self.assertEqual(journal(r)[-1]["sequence"], 2)
        self.assertEqual(journal(r)[-1]["draft"], "draft")

    def test_fragmented_paste_defers_query_until_paste_end(self):
        r = runtime()
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.render())
            r.input(b"\x1b[3;4R")
        with redirect_stdout(io.StringIO()) as output:
            r.input(b"\x1b[200~first")
            r.input(b" second")
            # A lifecycle render arriving during the paste must defer too.
            r.model.state = "working"
            r.effects(r.model.render())
        self.assertEqual(output.getvalue(), "", "do not solicit a reply inside a paste")
        with redirect_stdout(io.StringIO()) as output:
            r.input(b"\x1b[201~")
        self.assertTrue(output.getvalue().endswith("\x1b[6n"))
        self.assertEqual(r.parser.draft, b"first second")
        self.assertEqual(journal(r)[-1]["state"], "working")

    def test_unsolicited_reply_cannot_signal_ready(self):
        r = runtime()
        with redirect_stdout(io.StringIO()) as output:
            r.input(b"\x1b[1;1R")
        self.assertEqual(journal(r), [])
        self.assertEqual(output.getvalue(), "")

    def test_exit_discards_pending_reply_and_remainder_of_input(self):
        r = runtime()
        # No ending hook template: exercise the real exit state with forwarding
        # replaced only at the external I/O boundary, not the effect/input path.
        r.forward = lambda data: None
        with redirect_stdout(io.StringIO()):
            r.effects(r.model.render())
        with redirect_stdout(io.StringIO()) as output:
            r.input(b"/exit\rAFTER\x1b[3;4R")
        self.assertEqual(output.getvalue(), "")
        self.assertEqual(journal(r)[-1]["kind"], "exit")
        self.assertFalse(r.running)
        self.assertEqual(r.parser.draft, b"")
        self.assertEqual(r.pending_renders, [])


class TitleTests(unittest.TestCase):
    def test_take_title_leaves_the_bare_name_and_nul_padding(self):
        # In a child: the title replaces the calling process's own cmdline.
        import subprocess, sys
        code = ("import harness, sys; harness.take_title('pi'); "
                "sys.stdout.buffer.write(open('/proc/self/cmdline', 'rb').read())")
        out = subprocess.run([sys.executable, "-B", "-c", code, "an-argument"],
                             cwd=harness.HERE, capture_output=True, timeout=30, check=True).stdout
        self.assertEqual(out.rstrip(b"\0"), b"pi")
        self.assertGreater(len(out), len(b"pi\0"))


if __name__ == "__main__":
    unittest.main()
