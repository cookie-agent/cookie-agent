#!/usr/bin/env python3
"""Developer-only renderer for TUI preview dumps.

Turns the cell dump written by the ignored `render_preview` test in
`crates/tui/src/ui/transcript/tests/render_preview.rs` (one JSON array per
row of `[symbol, fg, bg, modifier]` cells) into a PNG, so a transcript design
change can be looked at without a terminal. Requires Pillow.

    PREVIEW_EVENTS=path/to/events.jsonl PREVIEW_OUT=/tmp/preview.jsonl \\
        cargo test -p cookie_agent_tui render_preview -- --ignored
    scripts/tui_preview_png.py /tmp/preview.jsonl /tmp/preview.png

Pass `--dark` for a dump rendered with `PREVIEW_DARK=1`: it only changes the
terminal default colours used for cells whose colour is `Reset`.
"""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

CELL_WIDTH = 9
CELL_HEIGHT = 19
FONT_SIZE = 15

# xterm-ish values for the named ANSI colours ratatui prints.
NAMED = {
    "Black": (0, 0, 0),
    "Red": (205, 49, 49),
    "Green": (13, 188, 121),
    "Yellow": (229, 229, 16),
    "Blue": (36, 114, 200),
    "Magenta": (188, 63, 188),
    "Cyan": (17, 168, 205),
    "Gray": (229, 229, 229),
    "DarkGray": (102, 102, 102),
    "LightRed": (241, 76, 76),
    "LightGreen": (35, 209, 139),
    "LightYellow": (245, 245, 67),
    "LightBlue": (59, 142, 234),
    "LightMagenta": (214, 112, 214),
    "LightCyan": (41, 184, 219),
    "White": (255, 255, 255),
}

RGB = re.compile(r"Rgb\((\d+), (\d+), (\d+)\)")


def colour(value: str, default: tuple[int, int, int]) -> tuple[int, int, int]:
    """A ratatui `Color` Debug string as RGB; `Reset` and unknowns use `default`."""
    match = RGB.fullmatch(value)
    if match:
        return tuple(int(part) for part in match.groups())
    return NAMED.get(value, default)


def load_font(directory: Path, name: str) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(str(directory / name), FONT_SIZE)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("dump", type=Path, help="cell dump written by render_preview")
    parser.add_argument("png", type=Path, help="PNG to write")
    parser.add_argument("--dark", action="store_true", help="dark terminal defaults")
    parser.add_argument(
        "--font-dir",
        type=Path,
        default=Path("/usr/share/fonts/truetype/dejavu"),
        help="directory holding DejaVuSansMono{,-Bold,-Oblique}.ttf",
    )
    args = parser.parse_args()

    rows = [json.loads(line) for line in args.dump.read_text().splitlines() if line]
    regular = load_font(args.font_dir, "DejaVuSansMono.ttf")
    bold = load_font(args.font_dir, "DejaVuSansMono-Bold.ttf")
    italic = load_font(args.font_dir, "DejaVuSansMono-Oblique.ttf")
    default_fg, default_bg = (
        ((230, 230, 230), (20, 20, 20)) if args.dark else ((30, 30, 30), (255, 255, 255))
    )

    width, height = len(rows[0]), len(rows)
    image = Image.new("RGB", (width * CELL_WIDTH, height * CELL_HEIGHT), default_bg)
    draw = ImageDraw.Draw(image)
    for y, row in enumerate(rows):
        for x, (symbol, fg, bg, modifier) in enumerate(row):
            foreground, background = colour(fg, default_fg), colour(bg, default_bg)
            if "REVERSED" in modifier:
                foreground, background = background, foreground
            left, top = x * CELL_WIDTH, y * CELL_HEIGHT
            draw.rectangle(
                [left, top, left + CELL_WIDTH - 1, top + CELL_HEIGHT - 1], fill=background
            )
            if symbol.strip():
                font = bold if "BOLD" in modifier else italic if "ITALIC" in modifier else regular
                draw.text((left, top + 1), symbol, font=font, fill=foreground)
            if "UNDERLINED" in modifier:
                underline = top + CELL_HEIGHT - 2
                draw.line([left, underline, left + CELL_WIDTH - 1, underline], fill=foreground)
    image.save(args.png)


if __name__ == "__main__":
    main()
