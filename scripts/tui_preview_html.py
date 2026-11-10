#!/usr/bin/env python3
"""Developer-only renderer for TUI preview dumps, as an HTML page.

Like `scripts/tui_preview_png.py`, it turns the cell dump the ignored
`render_preview` test writes (one JSON array per row of
`[symbol, fg, bg, modifier]` cells) into a picture of the terminal, but as a
page in JetBrains Mono with colour emoji, for a browser to photograph
(`scripts/web-screenshots.sh --tui` does). Box-drawing cells are stretched a
little so the gutters join as they do in a terminal.

    scripts/tui_preview_html.py dump.jsonl out.html [--dark] \\
        [--replace REGEX=TEXT]... [--blank-scrollbar]

`--replace` overwrites the first match of REGEX in each row with TEXT,
padded with spaces to the match's length, or with the next character of
TEXT after a `|` (`'Model\\[base\\] =GPT-5 |─'` pads with `─`): it hides what
only the test harness shows, such as its stand-in model's name. With
`--blank-scrollbar` the scrollbar column and the rule beside the header are
blanked above the composer.
"""

from __future__ import annotations

import argparse
import html
import json
import re
from pathlib import Path

RGB = re.compile(r"Rgb\((\d+), (\d+), (\d+)\)")
FONTS = Path(__file__).resolve().parent.parent / "crates/web/assets/fonts"
EDGE = 18


def colour(value: str, default: str) -> str:
    match = RGB.fullmatch(value)
    return "rgb(%s,%s,%s)" % match.groups() if match else default


def is_box(symbol: str) -> bool:
    return len(symbol) == 1 and 0x2500 <= ord(symbol) <= 0x259F


def replace(row: list, pattern: str, text: str) -> None:
    """Overwrites the first match of `pattern` in `row`, one cell per
    character of the matched text."""
    fill = " "
    if "|" in text:
        text, fill = text.rsplit("|", 1)
    line = "".join(cell[0] if len(cell[0]) == 1 else "\0" for cell in row)
    match = re.search(pattern, line)
    if not match:
        return
    start, end = match.span()
    new = text.ljust(end - start, fill)[: end - start]
    after = row[end] if end < len(row) else row[end - 1]
    for offset, symbol in enumerate(new):
        cell = list(row[start + offset])
        if offset >= len(text) and fill != " ":
            # Padding continues the line after the match.
            cell = [symbol, after[1], after[2], after[3]]
        else:
            cell = [symbol, cell[1], cell[2], "NONE" if fill == " " else cell[3]]
        row[start + offset] = cell


def blank_scrollbar(rows: list) -> None:
    composer = max(
        index for index, row in enumerate(rows) if "".join(c[0] for c in row).lstrip().startswith("╭")
    )
    for row in rows[:composer]:
        for x in range(len(row) - EDGE, len(row)):
            if is_box(row[x][0]):
                row[x] = [" ", row[x][1], rows[composer - 1][2][2], "NONE"]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("dump", type=Path)
    parser.add_argument("html", type=Path)
    parser.add_argument("--dark", action="store_true")
    parser.add_argument("--replace", action="append", default=[], metavar="REGEX=TEXT")
    parser.add_argument("--blank-scrollbar", action="store_true")
    args = parser.parse_args()

    rows = [json.loads(line) for line in args.dump.read_text().splitlines() if line]
    for rule in args.replace:
        pattern, text = rule.split("=", 1)
        for row in rows:
            replace(row, pattern, text)
    if args.blank_scrollbar:
        blank_scrollbar(rows)

    default_fg, default_bg = ("#e4e4e7", "#0e0e10") if args.dark else ("#18181b", "#ffffff")
    lines = []
    for row in rows:
        cells, skip = [], 0
        for symbol, fg, bg, modifier in row:
            if skip:
                skip -= 1
                continue
            foreground, background = colour(fg, default_fg), colour(bg, default_bg)
            if "REVERSED" in modifier:
                foreground, background = background, foreground
            style = "color:%s;background:%s" % (foreground, background)
            if "BOLD" in modifier:
                style += ";font-weight:700"
            if "ITALIC" in modifier:
                style += ";font-style:italic"
            if "UNDERLINED" in modifier:
                style += ";text-decoration:underline"
            if "DIM" in modifier:
                style += ";opacity:.6"
            wide = len(symbol) == 1 and ord(symbol) >= 0x1F000
            if wide:
                skip = 1
            kind = ' class="w"' if wide else ' class="b"' if is_box(symbol) else ""
            cells.append('<span%s style="%s">%s</span>' % (kind, style, html.escape(symbol or " ")))
        lines.append('<div class="r">%s</div>' % "".join(cells))

    args.html.write_text(
        """<!doctype html><meta charset="utf-8"><style>
@font-face{font-family:Mono;src:url("%(fonts)s/JetBrainsMono-Regular.woff2")}
@font-face{font-family:Mono;font-weight:700;src:url("%(fonts)s/JetBrainsMono-Bold.woff2")}
html,body{margin:0;background:%(bg)s}
.t{display:inline-block;padding:10px 12px;background:%(bg)s;font:15px/20px Mono,monospace;font-variant-ligatures:none}
.r{height:20px;white-space:pre}
.r span{display:inline-block;width:9px;height:20px;vertical-align:top;overflow:visible}
.r span.b{transform:scaleY(1.12)}
.r span.w{width:18px;font:13px/20px "Noto Color Emoji",emoji,sans-serif;text-align:center}
</style><div class="t">%(rows)s</div>
"""
        % {"fonts": FONTS.as_uri(), "bg": default_bg, "rows": "\n".join(lines)}
    )


if __name__ == "__main__":
    main()
