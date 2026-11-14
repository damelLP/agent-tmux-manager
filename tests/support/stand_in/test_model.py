"""Contract tests: loss/duplication of input and incorrect lifecycle transitions."""
import json
from pathlib import Path
import unittest

import model

FIXTURES = Path(__file__).parent / "fixtures"


def make(mode="claude", **kwargs):
    versions = {"claude": "2.1.325", "codex": "0.167.0", "pi": "1.5.0"}
    frames = [json.loads(line) for line in
              (FIXTURES / f"{mode}_{versions[mode]}.jsonl").read_text().splitlines()]
    return model.Model(mode, frames, "test-1", **kwargs)


def events(effects):
    return [e["event"] for e in effects if e["kind"] == "event"]


def data(effects):
    return [e["data"] for e in effects if e["kind"] == "event"]


class InputTests(unittest.TestCase):
    def test_fragmented_paste_is_one_exact_unicode_prompt(self):
        parser = model.InputParser()
        actual = []
        for byte in "\x1b[200~/clear\nλ hello\rworld;\x1b[201~\r".encode():
            actual += parser.feed(bytes([byte]))
        self.assertEqual(actual, [("submit", "/clear\nλ hello\rworld;")])

    def test_keys_and_empty_input(self):
        parser = model.InputParser()
        self.assertEqual(parser.feed(b"draft\x15new\x7f!\r"), [("submit", "ne!")])
        self.assertEqual(parser.feed(b"\x1b"), [])
        self.assertEqual(parser.flush_escape(), [("escape", "")])
        self.assertEqual(parser.feed(b"\x03"), [("ctrl_c", "")])
        self.assertEqual(parser.feed(b"\r"), [("submit", "")])

    def test_arrow_sequences_are_atomic_and_preserve_draft(self):
        parser = model.InputParser()
        self.assertEqual(parser.feed(b"draft\x1b["), [])
        self.assertEqual(parser.feed(b"B\x1b[A"), [("down", ""), ("up", "")])
        self.assertEqual(parser.feed(b"\r"), [("submit", "draft")])

    def test_harness_arguments_accept_flags_and_validate_before_start(self):
        self.assertEqual(model.parse_args(["--model", "X", "--unknown=Y", "--no-tools", "prompt"]), (None, "prompt"))
        self.assertEqual(model.parse_args(["--atm-config", "/fixture/config", "--", "-prompt"]), ("/fixture/config", "-prompt"))
        self.assertEqual(model.parse_args(["--unknown"]), (None, None))
        for args in [["one", "two"], ["--atm-config"], ["--model"], ["--model", "--no-tools"]]:
            with self.assertRaises(ValueError):
                model.parse_args(args)

    def test_terminal_cursor_reply_is_atomic_and_not_editor_input(self):
        parser = model.InputParser()
        self.assertEqual(parser.feed(b"draft\x1b[12;"), [])
        self.assertEqual(parser.feed(b"34R"), [("cursor_report", "12;34")])
        self.assertEqual(parser.feed(b"\r"), [("submit", "draft")])
        self.assertEqual(parser.feed(b"\x1b[?12;34R\x1b[0;0R\x1b[12R"), [])

    def test_cursor_reply_inside_paste_remains_literal_draft(self):
        parser = model.InputParser()
        self.assertEqual(parser.feed(b"\x1b[200~\x1b[12;34R\x1b[201~\r"),
                         [("submit", "\x1b[12;34R")])

    def test_escape_inside_paste_is_data(self):
        parser = model.InputParser()
        self.assertEqual(parser.feed(b"\x1b[200~a\x1bb\x1b[201~\r"),
                         [("submit", "a\x1bb")])


class LifecycleTests(unittest.TestCase):
    def test_claude_start_prompt_end_and_exact_payload(self):
        m = make(work_ms=500)
        self.assertEqual(events(m.start()), ["SessionStart"])
        effects = m.handle(("submit", "/clear\nλ;"), 10)
        self.assertEqual(events(effects), ["UserPromptSubmit"])
        self.assertEqual(data(effects)[0]["prompt"], "/clear\nλ;")
        self.assertEqual(events(m.tick(509)), [])
        self.assertEqual(events(m.tick(510)), ["Stop"])
        self.assertEqual(events(m.tick(999)), [])

    def test_claude_status_at_start_completion_and_late_replay(self):
        m = make()
        def statuses(effects):
            return [e["data"] for e in effects if e["kind"] == "status"]
        self.assertEqual(statuses(m.start())[0]["session_id"], "test-1")
        m.handle(("submit", "turn"), 0)
        self.assertEqual(statuses(m.tick(500))[0]["model"]["id"], "claude-haiku-5-5")
        m.handle(("submit", "/clear"), 501, "test-2")
        self.assertEqual(statuses(m.handle(("submit", "/atm/status test-1"), 502))[0]["session_id"], "test-1")
        self.assertEqual(statuses(m.tick(601))[0]["session_id"], "test-2")
        for mode in ["codex", "pi"]:
            self.assertEqual(statuses(make(mode).start()), [])

    def test_clear_gap_new_id_and_no_old_timer(self):
        m = make(work_ms=500, gap_ms=100)
        m.start()
        m.handle(("submit", "old"), 0)
        self.assertEqual(events(m.handle(("submit", "/clear"), 20, "test-2")), ["SessionEnd"])
        self.assertEqual(events(m.handle(("submit", "too soon"), 50)), [])
        self.assertEqual(events(m.tick(119)), [])
        effects = m.tick(120)
        self.assertEqual(events(effects), ["SessionStart"])
        self.assertEqual(data(effects)[0]["source"], "clear")
        self.assertEqual(data(effects)[0]["session_id"], "test-2")
        self.assertEqual(events(m.tick(500)), [])

    def test_multiline_control_prefix_is_a_prompt(self):
        m = make()
        m.start()
        effects = m.handle(("submit", "/atm/drop-next Stop\nordinary text"), 0)
        self.assertEqual(events(effects), ["UserPromptSubmit"])
        self.assertIsNone(m.drop_next)

    def test_fixtures_contain_only_sanitized_paths(self):
        def strings(value):
            if isinstance(value, dict):
                for item in value.values():
                    yield from strings(item)
            elif isinstance(value, list):
                for item in value:
                    yield from strings(item)
            elif isinstance(value, str):
                yield value
        for path in FIXTURES.glob("*.jsonl"):
            for line in path.read_text().splitlines():
                for text in strings(json.loads(line)):
                    self.assertNotIn("/home/", text)
                    self.assertNotIn("/dev/shm/", text)
                    if text.startswith("/"):
                        self.assertTrue(text.startswith("/fixture"), text)

    def test_escape_cancels_completion_but_emits_nothing(self):
        for mode in ["claude", "codex"]:
            with self.subTest(mode=mode):
                m = make(mode)
                m.start()
                m.handle(("submit", "prompt"), 0)
                self.assertEqual(events(m.handle(("escape", ""), 1)), [])
                self.assertEqual(events(m.tick(10000)), [])
                self.assertEqual(m.state, "idle")

    def test_named_and_literal_hook_drops(self):
        m = make()
        m.start()
        m.handle(("submit", "/atm/drop-next Stop"), 0)
        self.assertEqual(events(m.handle(("submit", "one"), 1)), ["UserPromptSubmit"])
        dropped = m.tick(501)
        self.assertEqual(events(dropped), [])
        self.assertTrue(any(e["kind"] == "dropped" and e["event"] == "Stop" for e in dropped))
        m.handle(("submit", "/atm/drop-next"), 600)
        self.assertEqual(events(m.handle(("submit", "two"), 601)), [])
        self.assertEqual(events(m.tick(1101)), ["Stop"])

    def test_busy_input_is_confirmed_only_when_dequeued(self):
        m = make()
        m.start()
        self.assertEqual(events(m.handle(("submit", "/atm/drop-next Bogus"), 0)), [])
        self.assertIsNone(m.drop_next)
        m.handle(("submit", "first"), 1)
        self.assertEqual(events(m.handle(("submit", "second"), 2)), [])
        finished = m.tick(501)
        self.assertEqual(events(finished), ["Stop", "UserPromptSubmit"])
        self.assertEqual(data(finished)[1]["prompt"], "second")
        self.assertEqual(events(m.tick(1001)), ["Stop"])

    def test_interrupt_restores_claude_prompt_and_ctrl_c_clears_then_exits(self):
        m = make()
        m.start()
        m.handle(("submit", "restore me"), 0)
        for key in ["escape", "ctrl_c"]:
            m.handle(("submit", "restore me"), 0)
            effects = m.handle((key, ""), 1)
            self.assertIn({"kind": "draft", "text": "restore me"}, effects)
            self.assertEqual(events(effects), [])
        self.assertIn({"kind": "draft", "text": ""}, m.handle(("ctrl_c", ""), 1100))
        self.assertEqual(m.state, "idle")
        self.assertEqual(events(m.handle(("ctrl_c", ""), 2099)), ["SessionEnd"])
        self.assertEqual(m.state, "exited")

    def test_dialog_arrows_select_without_denying(self):
        m = make()
        m.start()
        m.handle(("submit", "/atm/dialog"), 0)
        self.assertEqual(events(m.handle(("down", ""), 1)), [])
        self.assertEqual(m.state, "dialog")
        selected = m.handle(("submit", ""), 2)
        self.assertIn({"kind": "selection", "text": "", "option": 2}, selected)
        self.assertEqual(events(selected), [])
        self.assertEqual(m.handle(("up", ""), 3), [])

    def test_dialog_denial_and_enter_are_not_prompt_submission(self):
        for action in [("escape", ""), ("submit", "oops")]:
            with self.subTest(action=action):
                m = make()
                m.start()
                m.handle(("submit", "work"), 0)
                self.assertEqual(events(m.handle(("submit", "/atm/dialog"), 1)),
                                 ["PreToolUse", "PermissionRequest"])
                m.handle(("down", ""), 1)  # Deny, not the approving default.
                effects = m.handle(action, 2)
                self.assertEqual(events(effects), [])
                self.assertEqual(events(m.tick(1000)), [])
                self.assertEqual(m.state, "idle")
        for mode in ["codex", "pi"]:
            m = make(mode)
            self.assertEqual(events(m.handle(("submit", "/atm/dialog"), 0)), [])
            self.assertEqual(m.state, "idle")

    def test_codex_lazy_start_clear_and_exit_all_generations(self):
        m = make("codex", gap_ms=50)
        self.assertEqual(events(m.start()), [])
        self.assertEqual(events(m.handle(("submit", "first"), 0)),
                         ["SessionStart", "UserPromptSubmit"])
        self.assertEqual(events(m.handle(("submit", "/new"), 1, "test-2")), [])
        self.assertEqual(events(m.tick(51)), [])
        effects = m.handle(("submit", "second"), 52)
        self.assertEqual(events(effects), ["SessionStart", "UserPromptSubmit"])
        self.assertEqual([d["session_id"] for d in data(effects)], ["test-2", "test-2"])
        self.assertEqual(data(effects)[0]["source"], "startup")
        ended = m.handle(("submit", "/quit"), 53)
        self.assertEqual(events(ended), ["SessionEnd", "SessionEnd"])
        self.assertEqual([d["session_id"] for d in data(ended)], ["test-1", "test-2"])

    def test_pi_replay_abort_normal_completion_and_clear(self):
        m = make("pi", gap_ms=50)
        self.assertEqual(events(m.start()), ["session_start"])
        effects = m.handle(("submit", "exact\ntext"), 0)
        self.assertEqual(events(effects), ["input", "agent_start"])
        self.assertEqual(data(effects)[0]["payload"]["text"], "exact\ntext")
        effects = m.handle(("escape", ""), 2)
        self.assertEqual(events(effects), ["turn_end", "agent_end"])
        assistant = data(effects)[1]["payload"]["messages"][-1]
        self.assertEqual(assistant["stopReason"], "aborted")
        self.assertEqual(assistant["errorMessage"], "Operation aborted")
        self.assertEqual(events(m.tick(1000)), [])
        self.assertEqual(events(m.handle(("submit", "/new"), 1001, "test-2")), ["session_shutdown"])
        self.assertEqual(events(m.tick(1051)), ["session_start"])
        m.handle(("submit", "normal"), 1052)
        effects = m.tick(1552)
        self.assertEqual(events(effects), ["turn_end", "agent_end"])
        self.assertEqual(data(effects)[1]["payload"]["messages"][-1]["stopReason"], "stop")
        self.assertEqual(events(m.handle(("exit", ""), 1600)), ["session_shutdown"])

    def test_raw_hook_payloads_leave_pid_and_pane_to_forwarder(self):
        for mode in ["claude", "codex"]:
            m = make(mode)
            effects = m.start() + m.handle(("submit", "probe"), 0)
            for payload in data(effects):
                self.assertNotIn("pid", payload)
                self.assertNotIn("tmux_pane", payload)


if __name__ == "__main__":
    unittest.main()
