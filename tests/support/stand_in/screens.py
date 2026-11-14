"""Pure native-size capture replay; callers load assets and select catalog entries.

Captured attributes are trusted only after SGR-only validation and plain-text
comparison (apart from trailing ASCII cells that plain capture drops). No parser facts, runtime IDs, filesystem reads or clock reads
participate in rendering. Synthetic editor mutations belong to the caller.
"""
from dataclasses import dataclass
import re
import unicodedata

SGR = re.compile(r"\x1b\[[0-9;:]*m")


@dataclass(frozen=True)
class RenderedScreen:
    source: str
    columns: int
    rows: int
    lines: tuple[str, ...]
    styled_lines: tuple[str, ...] | None

    @property
    def terminal(self):
        # Absolute row positioning avoids LF at the bottom scrolling the frame.
        rows = self.lines if self.styled_lines is None else self.styled_lines
        return "\x1b[0m\x1b[2J" + "".join(
            f"\x1b[{row};1H{text}" for row, text in enumerate(rows, 1)
        ) + "\x1b[0m"


def body(text):
    """Remove the capture's metadata label only; never trim terminal cells."""
    return text.partition("\n")[2] if text.startswith("Captured:") else text


def cell_width(text):
    # Sufficient for these captures, not a terminal/emoji grapheme implementation.
    return sum(0 if unicodedata.category(c) in ("Mn", "Me", "Cf") else
               2 if unicodedata.east_asian_width(c) in ("W", "F") else 1 for c in text)


def validate_plain(text):
    if any(c != "\n" and (ord(c) < 32 or 127 <= ord(c) < 160) for c in text):
        raise ValueError("capture contains non-SGR terminal controls")


def render_screen(spec, assets):
    columns, rows = spec["columns"], spec["rows"]
    if type(columns) is not int or type(rows) is not int or min(columns, rows) <= 0:
        raise ValueError("invalid native capture geometry")
    text = body(assets[spec["source"]])
    validate_plain(text)
    lines = tuple(text.splitlines())
    if len(lines) > rows or any(cell_width(line) > columns for line in lines):
        raise ValueError(f"capture exceeds native geometry: {spec['source']}")
    styled = None
    if "style" in spec:
        style = body(assets[spec["style"]])
        plain_style = SGR.sub("", style)
        validate_plain(plain_style)
        # Preserve styled cursor/trailing cells; plain capture-pane -p drops them.
        if [line.rstrip(" ") for line in plain_style.splitlines()] != [line.rstrip(" ") for line in lines]:
            raise ValueError(f"styled capture text differs: {spec['style']}")
        if any(cell_width(line) > columns for line in plain_style.splitlines()):
            raise ValueError(f"styled capture exceeds native geometry: {spec['style']}")
        styled = tuple(style.splitlines())
    return RenderedScreen(spec["source"], columns, rows, lines, styled)
