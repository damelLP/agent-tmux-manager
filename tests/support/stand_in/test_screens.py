"""Exact fixture replay, independent of model transitions or adapter results."""
import json
from pathlib import Path
import re
import unittest

import screens

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]


def catalog():
    return json.loads((HERE / "screens.json").read_text())


def assets(spec):
    return {spec[k]: (ROOT / spec[k]).read_text() for k in ("source", "style") if k in spec}


class ScreenTests(unittest.TestCase):
    def test_every_catalog_frame_preserves_native_text_and_layout(self):
        specs = catalog()
        self.assertEqual(set(s["mode"] for s in specs), {"claude", "codex", "pi"})
        self.assertEqual(len({s["key"] for s in specs}), len(specs))
        for spec in specs:
            with self.subTest(key=spec["key"]):
                raw = assets(spec)
                plain = raw[spec["source"]]
                if plain.startswith("Captured:"):
                    plain = plain.partition("\n")[2]
                rendered = screens.render_screen(spec, raw)
                self.assertEqual("\n".join(rendered.lines) + "\n", plain)
                self.assertEqual(rendered.columns, spec["columns"])
                self.assertEqual(rendered.rows, spec["rows"])
                self.assertEqual(rendered.source, spec["source"])
                self.assertLessEqual(len(rendered.lines), rendered.rows)
                self.assertNotIn("Captured:", rendered.terminal)
                self.assertNotIn("stand-in ", rendered.terminal)

    def test_styles_are_carried_not_reconstructed_from_plain_text(self):
        styled = [s for s in catalog() if "style" in s]
        self.assertEqual({s["mode"] for s in styled}, {"claude", "codex"})
        for spec in styled:
            with self.subTest(key=spec["key"]):
                raw = assets(spec)
                original = raw[spec["style"]].partition("\n")[2]
                rendered = screens.render_screen(spec, raw)
                self.assertEqual("\n".join(rendered.styled_lines) + "\n", original)
                stripped = re.sub(r"\x1b\[[0-9;:]*m", "", original)
                # Plain -p drops trailing ASCII cells; styled cursor spaces survive.
                self.assertEqual([line.rstrip(" ") for line in stripped.splitlines()],
                                 [line.rstrip(" ") for line in rendered.lines])
                self.assertIn("\x1b[", rendered.terminal)

    def test_terminal_output_positions_rows_without_scrolling_last_row(self):
        spec = {"source": "frame", "columns": 5, "rows": 2}
        rendered = screens.render_screen(spec, {"frame": "12345\nlast!\n"})
        self.assertEqual(rendered.terminal,
                         "\x1b[0m\x1b[2J\x1b[1;1H12345\x1b[2;1Hlast!\x1b[0m")
        self.assertNotIn("\n", rendered.terminal)

    def test_provenance_removal_does_not_trim_nbsp_padding_or_blank_rows(self):
        spec = {"source": "frame", "columns": 8, "rows": 4}
        raw = "Captured: test-only source label\n\n❯\u00a0   \n\n\n"
        rendered = screens.render_screen(spec, {"frame": raw})
        self.assertEqual(rendered.lines, ("", "❯\u00a0   ", "", ""))
        self.assertIsNone(rendered.styled_lines)

    def test_styled_cursor_space_is_preserved_even_when_plain_capture_drops_it(self):
        spec = {"source": "plain", "style": "style", "columns": 4, "rows": 1}
        raw = {"plain": "❯\u00a0\n", "style": "❯\u00a0\x1b[7m \x1b[0m\n"}
        rendered = screens.render_screen(spec, raw)
        self.assertEqual(rendered.lines, ("❯\u00a0",))
        self.assertEqual(rendered.styled_lines, ("❯\u00a0\x1b[7m \x1b[0m",))
        self.assertIn("\x1b[7m \x1b[0m", rendered.terminal)
        with self.assertRaises(ValueError):
            screens.render_screen(spec, dict(raw, style="❯ \x1b[7m \x1b[0m\n"))

    def test_mismatched_style_and_unsafe_terminal_controls_refuse(self):
        spec = {"source": "plain", "style": "style", "columns": 20, "rows": 1}
        for text in ["different\n", "\x1b[2Jplain\n", "\x1b]52;c;payload\x07plain\n"]:
            with self.subTest(style=repr(text)), self.assertRaises(ValueError):
                screens.render_screen(spec, {"plain": "plain\n", "style": text})
        for text in ["\x1b[31mplain\n", "bad\rreturn\n", "bad\tcolumn\n"]:
            with self.subTest(plain=repr(text)), self.assertRaises(ValueError):
                screens.render_screen({"source": "plain", "columns": 20, "rows": 1}, {"plain": text})

    def test_oversize_or_invalid_geometry_refuses_instead_of_silent_crop(self):
        for columns, rows, text in [(0, 2, "x\n"), (5, 0, "x\n"), (5, 1, "a\nb\n"),
                                    (2, 1, "wide\n"), (1, 1, "界\n")]:
            with self.subTest(columns=columns, rows=rows), self.assertRaises(ValueError):
                screens.render_screen({"source": "x", "columns": columns, "rows": rows}, {"x": text})
        # A combining mark consumes no cell; East Asian wide glyphs consume two.
        rendered = screens.render_screen({"source": "x", "columns": 3, "rows": 1}, {"x": "e\u0301界\n"})
        self.assertEqual(rendered.lines, ("e\u0301界",))


if __name__ == "__main__":
    unittest.main()
