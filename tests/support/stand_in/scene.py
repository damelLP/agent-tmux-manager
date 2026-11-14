"""Pure lifecycle-to-capture mapping; mutations below are explicitly Synthetic.

Native replays preserve capture rows and SGR. Runtime draft insertion, selection
movement without a captured selected frame, and uncaptured viewport sizes are
synthetic, not claims about a real harness's wrapping or resize algorithm.
"""
from dataclasses import replace

from screens import cell_width, render_screen

MODALS = {"permission", "folder-trust", "folder-trust-selected", "hooks-review",
          "hooks-list", "worktree", "update"}


def select_screen(catalog, mode, frame, variant, columns, rows):
    candidates = [s for s in catalog if s["mode"] == mode and s["state"] == frame]
    if not candidates:
        raise ValueError(f"no captured scene: {mode}/{frame}")
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
        rules = [i for i, line in enumerate(lines) if line.startswith("─" * 10)]
        if len(rules) < 2:
            raise ValueError("pi capture missing editor rules")
        return rules[-2] + 1, rules[-1], ""
    marker = "❯" if mode == "claude" else "›"
    slots = [i for i, line in enumerate(lines) if line.startswith(marker)]
    if not slots:
        raise ValueError("capture missing active editor")
    return slots[-1], slots[-1] + 1, "❯\u00a0" if mode == "claude" else "› "


def render_state(catalog, assets, mode, frame, draft, variant, columns, rows, selection, folder=None):
    if min(columns, rows) < 2:
        raise ValueError("viewport too small")
    if mode == "claude" and frame == "folder-trust":
        frame = "folder-trust-selected" if selection == 2 else frame
        variant = f"p1-{columns}"
    if mode == "codex" and draft == "/new" and frame not in MODALS:
        frame, draft = "clear-menu", ""
    elif draft and frame in ("fresh", "clear-complete"):
        drafts = [s for s in catalog if s["mode"] == mode and s["state"] == "draft"
                  and (s["columns"], s["rows"]) == (columns, rows)]
        if drafts:
            frame = "draft"
    spec = select_screen(catalog, mode, frame, variant, columns, rows)
    result = render_screen(spec, assets)
    lines = list(result.lines)
    styled = list(result.styled_lines) if result.styled_lines is not None else None
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
        start, end, prefix = editor_slot(lines, mode)
        inserted = wrap(prefix + visible(draft), columns)
        # Preserve original SGR (including normal/dim/cursor cells) for an exact
        # captured draft. Otherwise only the synthetic editor loses its styling.
        if [line.rstrip(" ") for line in lines[start:end]] != [line.rstrip(" ") for line in inserted]:
            lines[start:end] = inserted
            if styled is not None:
                styled[start:end] = inserted
    # Synthetic viewport fit: consume blank padding before dropping transcript,
    # and clip long chrome rather than causing terminal auto-wrap/scroll.
    while len(lines) > rows:
        blank = next((i for i in reversed(range(len(lines))) if not lines[i].strip()), 0)
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
