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

    # Synthetic: adversarial paste non-acceptance, not vendor editor behavior.
    # Break caught: ignoring the refusal or consuming only one input fragment.
    def test_refused_fragmented_paste_preserves_draft_and_consumes_its_enter(self):
        parser = model.InputParser()
        parser.feed(b"keep draft")
        parser.arm_paste_refusal()
        text = "/clear\nλ hello\x1b[1;1R;"
        actions = []
        for byte in ("\x1b[200~" + text + "\x1b[201~\r").encode():
            actions += parser.feed(bytes([byte]))
        self.assertEqual(actions, [("paste_refused", text)])
        self.assertEqual(parser.draft, b"keep draft")
        self.assertEqual(parser.feed(b"\x1b[200~ + next\x1b[201~\r"),
                         [("submit", "keep draft + next")])

    def test_refusal_ignores_typing_cursor_reports_and_empty_paste_is_consumed(self):
        parser = model.InputParser()
        parser.arm_paste_refusal()
        self.assertEqual(parser.feed(b"typed\r\x1b[3;4R"),
                         [("submit", "typed"), ("cursor_report", "3;4")])
        self.assertEqual(parser.feed(b"\x1b[200~\x1b[201~\x1b[3;4R\r"),
                         [("paste_refused", ""), ("cursor_report", "3;4")])
        self.assertEqual(parser.feed(b"normal\r"), [("submit", "normal")])

    def test_typing_after_refusal_is_not_swallowed_with_its_enter(self):
        parser = model.InputParser()
        parser.arm_paste_refusal()
        self.assertEqual(parser.feed(b"\x1b[200~discard\x1b[201~"),
                         [("paste_refused", "discard")])
        self.assertEqual(parser.feed(b"typed instead\r"), [("submit", "typed instead")])

    def test_duplicate_or_incomplete_paste_refusal_cannot_overwrite_input(self):
        for prefix in [b"\x1b", b"\x1b[200~partial"]:
            with self.subTest(prefix=prefix):
                parser = model.InputParser()
                parser.feed(prefix)
                with self.assertRaises(ValueError):
                    parser.arm_paste_refusal()
        parser = model.InputParser()
        parser.arm_paste_refusal()
        with self.assertRaises(ValueError):
            parser.arm_paste_refusal()
        self.assertEqual(parser.feed(b"\x1b[200~kept armed\x1b[201~"),
                         [("paste_refused", "kept armed")])
        with self.assertRaises(ValueError):
            parser.arm_paste_refusal()
        parser.feed(b"\r")
        parser.arm_paste_refusal()

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

    def test_no_daemon_does_not_consume_model_or_initial_prompt(self):
        for flag in ["-m", "--model"]:
            with self.subTest(flag=flag):
                self.assertEqual(model.parse_args([
                    "--no-daemon", "--profile", "a b", flag, "role model",
                    "initial 'quoted' prompt",
                ]), (None, "initial 'quoted' prompt"))

    def test_escape_inside_paste_is_data(self):
        parser = model.InputParser()
        self.assertEqual(parser.feed(b"\x1b[200~a\x1bb\x1b[201~\r"),
                         [("submit", "a\x1bb")])


class HookHoldTests(unittest.TestCase):
    # Synthetic: ordered lifecycle delay; forwarding agent_end early is a bug.
    def test_literal_next_hold_keeps_pi_completion_order_and_exact_payloads(self):
        from copy import deepcopy
        m = make("pi")
        m.start()
        m.handle(("submit", "turn"), 0)
        m.arm_hook_hold()
        held = m.tick(500)
        self.assertEqual(events(held), [])
        payloads = deepcopy([e["data"] for e in held if e["kind"] == "held"])
        self.assertEqual([e["event"] for e in held if e["kind"] == "held"],
                         ["turn_end", "agent_end"])
        self.assertEqual(m.state, "idle")
        released = m.release_hooks()
        self.assertEqual(events(released), ["turn_end", "agent_end"])
        self.assertEqual(data(released), payloads)
        self.assertEqual([d["session_id"] for d in data(released)], ["test-1", "test-1"])
        self.assertEqual(events(m.handle(("submit", "normal now"), 501)), ["input", "agent_start"])
        self.assertEqual(events(m.tick(1001)), ["turn_end", "agent_end"])
        with self.assertRaises(ValueError):
            m.release_hooks()

    def test_named_hold_preserves_status_and_ids_across_clear(self):
        m = make()
        m.start()
        m.arm_hook_hold("Stop")
        self.assertEqual(events(m.handle(("submit", "turn"), 0)), ["UserPromptSubmit"])
        held = m.tick(500)
        self.assertEqual(events(held), [])
        self.assertEqual([e["kind"] for e in held], ["held", "status", "render"])
        m.handle(("submit", "/clear"), 501, "test-2")
        self.assertEqual(events(m.tick(601)), [])
        released = m.release_hooks()
        self.assertEqual(events(released), ["Stop", "SessionEnd", "SessionStart"])
        self.assertEqual([d["session_id"] for d in data(released)],
                         ["test-1", "test-1", "test-2"])
        self.assertEqual(data(released)[1]["reason"], "clear")
        self.assertEqual(data(released)[2]["source"], "clear")

    def test_drop_takes_precedence_without_consuming_or_resurrecting_hold(self):
        m = make()
        m.start()
        m.arm_hook_hold()
        m.handle(("submit", "/atm/drop-next UserPromptSubmit"), 0)
        dropped = m.handle(("submit", "lost input"), 1)
        self.assertEqual([e["event"] for e in dropped if e["kind"] == "dropped"],
                         ["UserPromptSubmit"])
        with self.assertRaises(ValueError):
            m.release_hooks()
        held = m.tick(501)
        self.assertEqual([e["event"] for e in held if e["kind"] == "held"], ["Stop"])
        m.handle(("submit", "/atm/drop-next UserPromptSubmit"), 502)
        dropped = m.handle(("submit", "also lost while holding"), 503)
        self.assertEqual([e["event"] for e in dropped if e["kind"] == "dropped"],
                         ["UserPromptSubmit"])
        self.assertEqual(events(m.release_hooks()), ["Stop"])

    def test_invalid_and_duplicate_gate_commands_leave_original_hold_intact(self):
        for name in ["stop", "input", "*", "", 1, True, {}, []]:
            with self.subTest(name=name):
                m = make()
                with self.assertRaises(ValueError):
                    m.arm_hook_hold(name)
                self.assertEqual(events(m.start()), ["SessionStart"])
        m = make()
        m.arm_hook_hold("Stop")
        for action in [lambda: m.arm_hook_hold(), m.release_hooks]:
            with self.assertRaises(ValueError):
                action()
        self.assertEqual(events(m.start()), ["SessionStart"])
        m.handle(("submit", "turn"), 0)
        self.assertEqual(events(m.tick(500)), [])
        with self.assertRaises(ValueError):
            m.arm_hook_hold("UserPromptSubmit")
        self.assertEqual(events(m.release_hooks()), ["Stop"])

    def test_held_shutdown_keeps_exit_releasable(self):
        for mode, hook in [("claude", "SessionEnd"), ("codex", "SessionEnd"),
                           ("pi", "session_shutdown")]:
            with self.subTest(mode=mode):
                m = make(mode)
                m.start()
                m.arm_hook_hold()
                effects = m.handle(("exit", ""), 0)
                self.assertEqual([e["kind"] for e in effects], ["held"])
                released = m.release_hooks()
                self.assertEqual(events(released), [hook])
                self.assertEqual(released[-1], {"kind": "exit"})


class ContextShareTests(unittest.TestCase):
    # Synthetic: context-line injection, not measured token/cost evolution.
    # Break caught: an ignored setting leaves the captured null/tiny usage.
    def test_next_context_share_uses_real_mode_fields_without_mutating_templates(self):
        from copy import deepcopy
        for mode in ["claude", "codex", "pi"]:
            with self.subTest(mode=mode):
                m = make(mode)
                templates = deepcopy(m.frames)
                m.set_context_share(40)
                if mode == "claude":
                    window = m.status()[0]["data"]["context_window"]
                    self.assertEqual(window["used_percentage"], 40)
                    self.assertEqual(window["remaining_percentage"], 60)
                    self.assertEqual(window["current_usage"], {
                        "input_tokens": 400000, "output_tokens": 0,
                        "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0})
                    self.assertEqual(window["total_input_tokens"], 400000)
                    self.assertIsNone(m.status()[0]["data"]["context_window"]["current_usage"])
                elif mode == "pi":
                    usage = data(m.emit("turn_end"))[0]["usage"]
                    self.assertEqual(usage, {"cost": 0.0001171, "tokens": 1151,
                                             "contextTokens": 108800})
                    self.assertEqual(data(m.emit("turn_end"))[0]["usage"]["contextTokens"], 1151)
                else:
                    effects = m.emit("Stop")
                    self.assertEqual(effects[0]["kind"], "usage")
                    self.assertEqual(effects[0]["data"], {"type": "event_msg", "payload": {
                        "type": "token_count", "info": {
                            "total_token_usage": {"total_tokens": 103360},
                            "last_token_usage": {"total_tokens": 103360},
                            "model_context_window": 258400}}})
                    self.assertEqual(data(effects)[0]["transcript_path"], effects[0]["path"])
                    self.assertEqual([e["kind"] for e in m.emit("Stop")], ["event"])
                    self.assertEqual(data(m.emit("Stop"))[0]["transcript_path"], "/fixture/session.jsonl")
                self.assertEqual(m.frames, templates)

    def test_context_share_validates_boundaries_and_preserves_pending_on_error(self):
        for percent, tokens in [(0, 0), (100, 1000000), (39.5, 395000), (0.00015, 2)]:
            m = make()
            m.set_context_share(percent)
            self.assertEqual(m.status()[0]["data"]["context_window"]["current_usage"]["input_tokens"], tokens)
        m = make()
        m.set_context_share(40)
        for invalid in [None, True, False, "40", {}, [], -1, 100.01, float("nan"),
                        float("inf"), -float("inf"), 10 ** 1000]:
            with self.subTest(invalid=invalid):
                with self.assertRaises(ValueError):
                    m.set_context_share(invalid)
        self.assertEqual(m.status()[0]["data"]["context_window"]["used_percentage"], 40)
        m.set_context_share(40)
        m.set_context_share(65)  # Last valid unused setting wins.
        self.assertEqual(m.status()[0]["data"]["context_window"]["used_percentage"], 65)

    def test_context_share_waits_for_usage_not_other_pi_or_ending_codex_hooks(self):
        for mode, name in [("pi", "agent_start"), ("codex", "SessionEnd")]:
            m = make(mode)
            m.set_context_share(40)
            self.assertEqual([e["kind"] for e in m.emit(name)], ["event"])
            effects = m.emit("turn_end" if mode == "pi" else "Stop")
            if mode == "pi":
                self.assertEqual(data(effects)[0]["usage"]["contextTokens"], 108800)
            else:
                self.assertEqual(effects[0]["data"]["payload"]["info"]["last_token_usage"]["total_tokens"], 103360)

    def test_new_setting_cannot_rewrite_already_held_context_payload(self):
        for mode, name in [("pi", "turn_end"), ("codex", "Stop")]:
            m = make(mode)
            m.arm_hook_hold(name)
            m.set_context_share(40)
            first = m.emit(name)
            m.set_context_share(65)
            released = m.release_hooks()
            second = m.emit(name)
            if mode == "pi":
                self.assertEqual(data(released)[0]["usage"]["contextTokens"], 108800)
                self.assertEqual(data(second)[0]["usage"]["contextTokens"], 176800)
            else:
                self.assertEqual(first[0]["data"]["payload"]["info"]["last_token_usage"]["total_tokens"], 103360)
                self.assertEqual(second[0]["data"]["payload"]["info"]["last_token_usage"]["total_tokens"], 167960)
                self.assertEqual(data(released)[0]["transcript_path"], first[0]["path"])
                self.assertNotEqual(first[0]["path"], second[0]["path"])


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
