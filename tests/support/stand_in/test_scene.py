"""Lifecycle scene mapping and explicitly synthetic editor/selection mutations."""
import unittest

import scene
from test_screens import catalog, assets


SPECS = catalog()
ASSETS = {path: text for spec in SPECS for path, text in assets(spec).items()}


def render(mode, frame, draft="", variant="u5", columns=120, rows=40, selection=1):
    return scene.render_state(SPECS, ASSETS, mode, frame, draft, variant, columns, rows, selection)


class SceneTests(unittest.TestCase):
    def test_all_runtime_frames_have_real_sources(self):
        common = ["fresh", "working", "idle", "clearing", "clear-complete", "interrupted"]
        extras = {"claude": ["permission", "denied", "folder-trust"],
                  "codex": ["permission", "folder-trust", "update", "hooks-review", "hooks-list", "worktree"],
                  "pi": []}
        for mode in extras:
            for frame in common + extras[mode]:
                with self.subTest(mode=mode, frame=frame):
                    result = render(mode, frame)
                    self.assertIn(result.source, ASSETS)
                    self.assertNotIn("stand-in", result.terminal)
        # Unknown states must not silently become fresh editors.
        with self.assertRaises(ValueError):
            render("pi", "invented")

    def test_native_source_is_exact_even_when_model_has_diagnostic_detail(self):
        for mode, variant in [("claude", "rich-enabled"), ("codex", "rich")]:
            result = render(mode, "fresh", variant=variant, columns=100, rows=30)
            expected = ASSETS[result.source].partition("\n")[2]
            self.assertEqual("\n".join(result.lines) + "\n", expected)
            self.assertIsNotNone(result.styled_lines)

    def test_draft_replaces_only_active_slot_and_controls_are_visible(self):
        for mode, prefix in [("claude", "❯\u00a0"), ("codex", "› "), ("pi", "")]:
            result = render(mode, "idle", "new\x1b[2J\tλ\nnext")
            text = "\n".join(result.lines)
            self.assertIn(prefix + "new\\x1b[2J\\tλ", text)
            self.assertIn("next", text)
            self.assertNotIn("\x1b", text)
            self.assertNotIn("\t", text)
            self.assertEqual(len(result.lines), 40)

    def test_cleared_interrupt_draft_does_not_restore_fixture_prompt(self):
        result = render("claude", "interrupted", "")
        active = [line for line in result.lines if line.startswith("❯")][-1]
        self.assertEqual(active, "❯\u00a0")

    def test_real_draft_value_replays_captured_styling(self):
        for mode, variant in [("claude", "rich-enabled"), ("codex", "rich")]:
            result = render(mode, "fresh", "baseline draft 123", variant, 100, 30)
            self.assertIn("different-draft", result.source)
            self.assertIsNotNone(result.styled_lines)
            self.assertIn("baseline draft 123", "\n".join(result.lines))

    def test_claude_selected_trust_uses_real_selected_frame(self):
        result = render("claude", "folder-trust", selection=2)
        self.assertIn("selected-120x40", result.source)
        self.assertIn("❯ Yes, I trust this folder", "\n".join(result.lines))

    def test_configured_folder_mutates_only_trust_path_and_escapes_controls(self):
        result = scene.render_state(SPECS, ASSETS, "claude", "folder-trust", "", "u5",
                                    120, 40, 1, folder="/fixture/custom\x1b[2J")
        text = "\n".join(result.lines)
        self.assertIn(" /fixture/custom\\x1b[2J", text)
        self.assertIn("❯ No, exit", text)
        self.assertNotIn("\x1b", text)
        self.assertNotIn("/fixture/xxxx", text)

    def test_numbered_modal_selection_moves_marker_not_editor(self):
        for frame in ["permission", "hooks-review", "folder-trust", "worktree"]:
            result = render("codex", frame, "ignored", selection=2)
            text = "\n".join(result.lines)
            self.assertIn("› 2.", text)
            self.assertNotIn("› 1.", text)
            self.assertNotIn("ignored", text)

    def test_codex_new_draft_is_real_completion_menu(self):
        result = render("codex", "idle", "/new")
        self.assertIn("clear-poll-00", result.source)
        self.assertIn("› /new", "\n".join(result.lines))

    def test_small_viewport_removes_blank_padding_before_dialog_content(self):
        result = render("codex", "update", columns=100, rows=32)
        text = "\n".join(result.lines)
        self.assertIn("Update available", text)
        self.assertIn("› 1. Update now", text)
        self.assertIn("esc skip", text)

    def test_synthetic_small_viewport_never_wraps_or_overflows(self):
        from screens import cell_width
        result = render("pi", "idle", "界" * 100 + "\nsecond", columns=80, rows=24)
        self.assertEqual(len(result.lines), 24)
        self.assertTrue(all(cell_width(line) <= 80 for line in result.lines))


if __name__ == "__main__":
    unittest.main()
