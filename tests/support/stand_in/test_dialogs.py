"""Synthetic actions against the captured P1 dialog key contracts."""
import unittest

from test_model import make, events


class DialogTests(unittest.TestCase):
    def test_wrong_mode_options_refuse_before_start(self):
        for mode, option in [("pi", "folder_trust"), ("pi", "update_prompt"),
                             ("claude", "worktree_choice"), ("claude", "hooks_review"),
                             ("pi", "permission_dialog")]:
            with self.subTest(mode=mode, option=option), self.assertRaises(ValueError):
                make(mode, **{option: True})

    def test_trust_default_declines_without_lifecycle_even_for_pasted_exit(self):
        for action in [("submit", ""), ("escape", ""), ("submit", "/exit")]:
            m = make(folder_trust=True)
            self.assertEqual(events(m.start()), [])
            self.assertEqual(m.render()[0]["frame"], "folder-trust")
            self.assertEqual(events(m.handle(action, 0)), [])
            self.assertEqual(m.state, "exited")
            self.assertEqual(m.tick(10000), [])
            self.assertEqual(m.handle(("submit", "later"), 1), [])

    def test_trust_accept_announces_once_and_discards_modal_text(self):
        m = make(folder_trust=True)
        m.start()
        m.handle(("down", ""), 0)
        self.assertEqual(m.selection, 2)
        effects = m.handle(("submit", "not a prompt"), 1)
        self.assertEqual(events(effects), ["SessionStart"])
        self.assertEqual(m.state, "idle")
        self.assertEqual(m.render()[0]["frame"], "fresh")
        self.assertEqual(events(m.handle(("submit", "actual prompt"), 2)), ["UserPromptSubmit"])

    def test_codex_start_chain_update_folder_hooks_and_list(self):
        m = make("codex", update_prompt=True, folder_trust=True, hooks_review=True)
        self.assertEqual(events(m.start()), [])
        self.assertEqual(m.render()[0]["frame"], "update")
        self.assertEqual(events(m.handle(("submit", "npm update"), 0)), [])
        self.assertEqual(m.render()[0]["frame"], "update")
        m.handle(("escape", ""), 1)
        self.assertEqual(m.render()[0]["frame"], "folder-trust")
        m.handle(("submit", "ignored paste"), 2)
        self.assertEqual(m.render()[0]["frame"], "hooks-review")
        m.handle(("submit", ""), 3)
        self.assertEqual(m.render()[0]["frame"], "hooks-list")
        m.handle(("escape", ""), 4)
        self.assertEqual(m.render()[0]["frame"], "hooks-review")
        m.handle(("submit", ""), 5)
        self.assertEqual(events(m.handle(("key", "t"), 6)), [])
        self.assertEqual(m.state, "idle")
        self.assertEqual(events(m.handle(("submit", "first"), 7)), ["SessionStart", "UserPromptSubmit"])

    def test_codex_trust_all_and_continue_without_trusting(self):
        for option in (2, 3):
            m = make("codex", hooks_review=True)
            m.start()
            for _ in range(option - 1):
                m.handle(("down", ""), 0)
            m.handle(("submit", ""), 1)
            self.assertEqual(m.state, "idle")
            self.assertEqual(events(m.handle(("submit", "first"), 2)),
                             ["SessionStart", "UserPromptSubmit"] if option == 2 else [])

    def test_codex_folder_back_never_announces_or_enters_editor(self):
        m = make("codex", folder_trust=True)
        m.start()
        m.handle(("down", ""), 0)
        effects = m.handle(("submit", ""), 1)
        self.assertEqual(m.state, "dialog")
        self.assertEqual(m.render()[0]["frame"], "folder-trust")
        self.assertTrue(any(e["kind"] == "rejected" for e in effects))
        self.assertEqual(events(effects), [])

    def test_worktree_cancel_and_unsupported_option_preserve_generation(self):
        m = make("codex", worktree_choice=True)
        m.start()
        m.handle(("submit", "turn"), 0)
        m.tick(500)
        m.handle(("submit", "/new"), 501, "test-2")
        self.assertEqual(m.render()[0]["frame"], "worktree")
        self.assertEqual(m.session_id, "test-1")
        m.handle(("down", ""), 502)
        effects = m.handle(("submit", "ignored"), 503)
        self.assertEqual(m.state, "dialog")
        self.assertTrue(any(e["kind"] == "rejected" for e in effects))
        m.handle(("escape", ""), 504)
        self.assertEqual(m.render()[0]["frame"], "idle")
        self.assertEqual(m.ids, ["test-1"])
        m.handle(("submit", "/new"), 505, "test-3")
        self.assertEqual(events(m.handle(("submit", "modal paste"), 506)), [])
        self.assertEqual(m.state, "clearing")
        self.assertEqual(m.session_id, "test-3")
        m.tick(606)
        self.assertEqual(m.render()[0]["frame"], "clear-complete")
        self.assertEqual(events(m.handle(("submit", "new prompt"), 607)),
                         ["SessionStart", "UserPromptSubmit"])

    def test_permission_approval_runs_tool_before_stop(self):
        for mode, selections in [("claude", [1]), ("codex", [1, 2])]:
            for selection in selections:
                with self.subTest(mode=mode, selection=selection):
                    m = make(mode, permission_dialog=True)
                    m.start()
                    m.handle(("submit", "work"), 0)
                    opened = m.handle(("submit", "/atm/dialog"), 1)
                    self.assertEqual(events(opened), ["PreToolUse", "PermissionRequest"])
                    self.assertEqual(opened[-1]["frame"], "permission")
                    for _ in range(selection - 1):
                        m.handle(("down", ""), 2)
                    approved = m.handle(("submit", ""), 3)
                    self.assertEqual(events(approved), ["PostToolUse"])
                    self.assertEqual(m.state, "working")
                    self.assertEqual(approved[-1]["frame"], "working")
                    self.assertEqual(m.tick(502), [])
                    self.assertEqual(events(m.tick(503)), ["Stop"])
                    self.assertEqual(m.state, "idle")
                    self.assertEqual(events(m.tick(1000)), [])

    def test_approval_from_idle_cannot_restore_a_completed_prompt(self):
        for mode in ["claude", "codex"]:
            with self.subTest(mode=mode):
                m = make(mode, permission_dialog=True)
                m.start()
                m.handle(("submit", "earlier prompt A"), 0)
                m.tick(500)
                m.handle(("submit", "/atm/dialog"), 501)
                m.handle(("submit", ""), 502)
                effects = m.handle(("escape", ""), 503)
                self.assertEqual(m.state, "idle")
                self.assertEqual(m.render()[0]["frame"], "interrupted")
                self.assertFalse(any(e["kind"] == "draft" and e["text"] for e in effects))

    def test_abandoned_approval_cannot_complete_in_a_later_turn(self):
        for mode, clear in [("claude", "/clear"), ("codex", "/new")]:
            for cancel in ["clear", "escape", "deny-next-modal"]:
                with self.subTest(mode=mode, cancel=cancel):
                    m = make(mode, permission_dialog=True)
                    m.start()
                    m.handle(("submit", "/atm/dialog"), 0)
                    m.handle(("submit", ""), 1)
                    if cancel == "clear":
                        m.handle(("submit", clear), 2, "test-2")
                        m.tick(102)
                    elif cancel == "escape":
                        m.handle(("escape", ""), 2)
                    else:
                        m.handle(("submit", "/atm/dialog"), 2)
                        m.handle(("escape", ""), 3)
                    m.handle(("submit", "unrelated turn"), 103)
                    self.assertEqual(events(m.tick(603)), ["Stop"])

    def test_optional_codex_permission_consumes_keys_and_never_submits(self):
        for action in [("escape", ""), ("submit", "ignored")]:
            m = make("codex", permission_dialog=True)
            m.start()
            m.handle(("submit", "/atm/dialog"), 0)
            self.assertEqual(m.render()[0]["frame"], "permission")
            m.handle(("down", ""), 1)
            self.assertEqual(m.selection, 2)
            m.handle(("down", ""), 1)  # Option 3 is No; both Yes options approve.
            self.assertEqual(events(m.handle(action, 2)), [])
            self.assertEqual(m.state, "idle")

    def test_frame_tracks_lifecycle_not_diagnostic_details(self):
        for mode, command in [("claude", "/clear"), ("codex", "/new"), ("pi", "/new")]:
            m = make(mode)
            m.start()
            self.assertEqual(m.render()[0]["frame"], "fresh")
            m.handle(("submit", "/atm/drop-next Stop"), 0)
            self.assertEqual(m.render()[0]["frame"], "fresh")
            m.handle(("submit", "work"), 1)
            self.assertEqual(m.render()[0]["frame"], "working")
            m.tick(501)
            self.assertEqual(m.render()[0]["frame"], "idle")
            m.handle(("submit", command), 502, "test-2")
            self.assertEqual(m.render()[0]["frame"], "clearing")
            m.tick(602)
            self.assertEqual(m.render()[0]["frame"], "clear-complete")
            m.handle(("submit", "again"), 603)
            m.handle(("escape", ""), 604)
            self.assertEqual(m.render()[0]["frame"], "interrupted")


if __name__ == "__main__":
    unittest.main()
