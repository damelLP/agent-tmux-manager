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
    return r


def journal(r):
    import json
    return [json.loads(line) for line in r.log.getvalue().splitlines()]


class RenderTests(unittest.TestCase):
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
