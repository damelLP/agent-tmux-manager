"""Pure lifecycle-to-capture mapping; mutations below are explicitly Synthetic.

Native replays preserve capture rows and SGR. Runtime draft insertion, selection
movement without a captured selected frame, and uncaptured viewport sizes are
synthetic, not claims about a real harness's wrapping or resize algorithm.
"""
from dataclasses import replace
import re

from screens import cell_width, render_screen

MODALS = {"permission", "folder-trust", "folder-trust-selected", "hooks-review",
          "hooks-list", "worktree", "update"}


def select_screen(catalog, mode, frame, variant, columns, rows):
    candidates = [s for s in catalog if s["mode"] == mode and s["state"] == frame]
    if not candidates:
        raise ValueError(f"no captured scene: {mode}/{frame}")
    if mode == "codex" and frame in ("fresh", "clear-complete") and rows < 25:
        # U5's logo needs 25 rows with its header/editor/footer. P1's compact
        # fresh layout keeps those anchors without the logo or startup help.
        candidates = [s for s in candidates if s["variant"] == "p1-compact"]
    # A native viewport wins over a requested variant at another size. Within
    # it prefer that variant, then the original U5 source, then catalog order.
    return min(candidates, key=lambda s: (
        (s["columns"], s["rows"]) != (columns, rows),
        s["variant"] != variant, s["variant"] != "u5"))


def visible(text):
    return "".join(c if c.isprintable() or c == "\n" else repr(c)[1:-1] for c in text)


def wrap(text, columns):
    lines = []
    for line in text.split("\n"):
        chunk, width = "", 0
        for char in line:
            size = cell_width(char)
            if width + size > columns:
                lines.append(chunk)
                chunk, width = "", 0
            chunk += char
            width += size
        lines.append(chunk)
    return lines


def editor_slot(lines, mode):
    if mode == "pi":
        rules = [i for i, line in enumerate(lines)
                 if re.fullmatch(r"─+(?: [\u2800-\u28ff] Working ─+)?", line.rstrip())]
        if len(rules) < 2:
            raise ValueError("pi capture missing editor rules")
        return rules[-2] + 1, rules[-1], ""
    marker = "❯" if mode == "claude" else "›"
    slots = [i for i, line in enumerate(lines) if line.startswith(marker)]
    if not slots:
        if mode == "codex" and not any(line.strip() for line in lines):
            # P1's clearing gap has no editor; retain input until it returns.
            return None
        raise ValueError("capture missing active editor")
    return slots[-1], slots[-1] + 1, "❯\u00a0" if mode == "claude" else "› "


def vary_values(line, mode, frame):
    """Synthetic equal-width scalar mutations; never applied to user input."""
    replacements = {
        "claude": {"2.1.325": "9.8.765", "Haiku 5.5": "Opus 9.99", "✶ Levitating…": "✳ Generating…"},
        "codex": {"0.167.0": "0.987.6", "GPT-6-Luna": "GPT-9-Dawn", "GPT-6.1-Sol": "GPT-9.2-Sky"},
        "pi": {"1.5.0": "9.8.7", "gpt-6-luna": "gpt-9-dawn", "⠇ Working": "⠏ Working"},
    }[mode]
    for old, new in replacements.items():
        line = line.replace(old, new)
    if mode == "pi" and frame not in ("fresh", "clear-complete"):
        line = line.replace("$0.000", "$4.321")
    return line


def render_state(catalog, assets, mode, frame, draft, variant, columns, rows, selection, folder=None):
    minimum = {"codex": (80, 19), "pi": (86, 19)}.get(mode, (2, 2))
    if columns < minimum[0] or rows < minimum[1]:
        raise ValueError(f"{mode} viewport {columns}x{rows} requires at least {minimum[0]}x{minimum[1]}")
    if mode == "claude" and frame == "folder-trust":
        frame = "folder-trust-selected" if selection == 2 else frame
        variant = f"p1-{columns}"
    if mode == "codex" and draft == "/new" and frame not in MODALS:
        frame, draft = "clear-menu", ""
    elif mode != "pi" and draft and frame in ("fresh", "clear-complete"):
        drafts = [s for s in catalog if s["mode"] == mode and s["state"] == "draft"
                  and (s["columns"], s["rows"]) == (columns, rows)]
        if drafts:
            frame = "draft"
    spec = select_screen(catalog, mode, frame, variant, columns, rows)
    result = render_screen(spec, assets)
    lines = list(result.lines)
    styled = list(result.styled_lines) if result.styled_lines is not None else None
    if variant == "values":
        lines = [vary_values(line, mode, frame) for line in lines]
        if styled is not None:
            styled = [vary_values(line, mode, frame) for line in styled]
    if frame in ("folder-trust", "folder-trust-selected") and folder is not None:
        for i, line in enumerate(lines):
            if line.lstrip().startswith(("/fixture/", "/tmp/u8a-fixture/")):
                lines[i] = line[:len(line) - len(line.lstrip())] + visible(folder).replace("\n", "\\n")
                if styled is not None:
                    styled[i] = lines[i]
    if frame in MODALS:
        # Selection mutations retain all surrounding capture text. Claude trust
        # has actual selected captures; never substitute its negative wording.
        if selection != 1 and frame not in ("folder-trust-selected", "hooks-list", "update"):
            marker = "❯" if mode == "claude" else "›"
            for i, line in enumerate(lines):
                if f"{marker} 1." in line or f"  {selection}." in line:
                    lines[i] = line.replace(f"{marker} 1.", "  1.").replace(
                        f"  {selection}.", f"{marker} {selection}.")
                    if styled is not None:
                        styled[i] = lines[i]
    elif draft or (mode == "claude" and frame == "interrupted"):
        slot = editor_slot(lines, mode)
        if slot is not None:
            start, end, prefix = slot
            inserted = wrap(prefix + visible(draft), columns)
            # Preserve SGR for an exact captured draft; only synthetic editor
            # substitutions lose styling.
            if [line.rstrip(" ") for line in lines[start:end]] != [line.rstrip(" ") for line in inserted]:
                lines[start:end] = inserted
                if styled is not None:
                    styled[start:end] = inserted
    # Synthetic reflow preserves complete footer tokens and equal-width pi
    # rules. Never clip a right-aligned footer through a warning/model label.
    for i, line in enumerate(lines):
        fitted = line
        if mode == "pi" and re.fullmatch(r"─+(?: [\u2800-\u28ff] Working ─+)?", line):
            label = line.rstrip("─")
            fitted = label + "─" * (columns - cell_width(label))
        elif mode == "pi" and i == len(lines) - 1 and cell_width(line) > columns:
            left, gap, right = line.rstrip().partition("  ")
            right = right.strip()
            fitted = left + " " * max(2, columns - cell_width(left + right)) + right if gap else line
        elif mode == "codex" and "← for agents" in line and cell_width(line) > columns:
            fitted = "  " + " ".join(line.split())
        if fitted != line:
            lines[i] = fitted
            if styled is not None:
                styled[i] = fitted
    # Remove redundant blank padding first. Preserve editor blanks and one
    # separator per history block; only then crop old transcript from the top.
    while len(lines) > rows:
        slot = editor_slot(lines, mode) if frame not in MODALS and mode == "pi" else None
        protected = set(range(slot[0], slot[1])) if slot else set()
        blank = next((i for i in range(len(lines) - 1)
                      if i not in protected and not lines[i].strip() and not lines[i + 1].strip()), 0)
        del lines[blank]
        if styled is not None:
            del styled[blank]
    for i, line in enumerate(lines):
        if cell_width(line) > columns:
            lines[i] = wrap(line, columns)[0]
            if styled is not None:
                styled[i] = lines[i]
    # Do not pad native replays: their original viewport may omit blank EOF rows.
    if (columns, rows) != (result.columns, result.rows) or draft:
        padding = [""] * (rows - len(lines))
        lines += padding
        if styled is not None:
            styled += padding
    return replace(result, columns=columns, rows=rows, lines=tuple(lines),
                   styled_lines=tuple(styled) if styled is not None else None)
