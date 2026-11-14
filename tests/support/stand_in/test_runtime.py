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
    r.model = make()
    r.parser = InputParser()
    r.running = True
    r.log = io.StringIO()
    r.generation = 1
    r.prefix = "test"
    r.pending_renders = []
    r.render_sequence = 0
    r.draft_dirty = False
    r.manual_clock = True
    r.clock_ms = 0
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


class RenderTests(unittest.TestCase):
    def test_render_uses_capture_not_diagnostic_or_identity_chrome(self):
        r = runtime()
        with redirect_stdout(io.StringIO()) as output:
            r.effects(r.model.render("status sent"))
        self.assertIn("Claude Code v2.1.325", output.getvalue())
        self.assertNotIn("stand-in", output.getvalue())
        self.assertNotIn("test-1", output.getvalue())
        self.assertNotIn("status sent", output.getvalue())
        self.assertIn("screen_claude-ready", journal(r)[-1]["source"])

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


if __name__ == "__main__":
    unittest.main()
