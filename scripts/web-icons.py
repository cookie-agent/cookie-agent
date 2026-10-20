"""Draws the web page's home-screen icons from the logo
(docs/site/assets/logo.svg) into crates/web/assets/icons/.

The logo is plain geometry, so it is drawn here with Pillow rather than
through an SVG renderer: a cookie (a circle with three bites), its circuit
lines, and its chips. Run it after changing the logo:

    python3 scripts/web-icons.py
"""

import os
import shutil

from PIL import Image, ImageDraw

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "crates", "web", "assets", "icons")
CREAM = (251, 244, 230, 255)
DOUGH = (236, 184, 110, 255)
CHIP = (94, 58, 38, 255)
BITES = [(64, 14, 7.5), (72.6, 23.9, 8), (82.2, 32.4, 7)]
LINES = [
    [(35.2, 28.8), (35.2, 41.3), (49.8, 54)],
    [(49.8, 34.6), (49.8, 54)],
    [(49.8, 54), (57.4, 46.4), (70.5, 46.4)],
    [(49.8, 54), (53.5, 58.8), (67.3, 58.8), (67.3, 69.7)],
    [(25.7, 47.7), (25.7, 58.8), (34.4, 58.8), (34.4, 70.5)],
    [(49.8, 54), (34.4, 70.5)],
    [(51.1, 71.7), (51.1, 79.7)],
]
CHIPS = [
    (49.8, 54, 4.8),
    (35.2, 28.8, 3.9),
    (49.8, 34.6, 3.5),
    (70.5, 46.4, 4.7),
    (25.7, 47.7, 4.8),
    (34.4, 70.5, 4.7),
    (67.3, 69.7, 4.7),
    (51.1, 71.7, 3.4),
]


def draw(size, background):
    """The logo at `size` pixels on `background` (None for transparent)."""
    scale = size * 4 / 100  # drawn four times larger, then reduced
    big = size * 4
    image = Image.new("RGBA", (big, big), background or (0, 0, 0, 0))

    def circle(target, x, y, r, fill):
        target.ellipse([(x - r) * scale, (y - r) * scale, (x + r) * scale, (y + r) * scale], fill=fill)

    cookie = Image.new("L", (big, big), 0)
    mask = ImageDraw.Draw(cookie)
    circle(mask, 50, 50, 36, 255)
    for x, y, r in BITES:
        circle(mask, x, y, r, 0)
    image.paste(Image.new("RGBA", (big, big), DOUGH), (0, 0), cookie)
    pen = ImageDraw.Draw(image)
    width = 0.9
    for line in LINES:
        points = [(x * scale, y * scale) for x, y in line]
        pen.line(points, fill=CHIP, width=max(1, round(width * scale)), joint="curve")
        for x, y in line:
            circle(pen, x, y, width / 2, CHIP)
    for x, y, r in CHIPS:
        circle(pen, x, y, r, CHIP)
    image = image.resize((size, size), Image.LANCZOS)
    # A palette of the logo's few colours and their antialiased edges keeps
    # the files small: they count toward the page's size budget.
    return image.quantize(colors=48, method=Image.Quantize.FASTOCTREE)


def main():
    os.makedirs(OUT, exist_ok=True)
    for size in (192, 512):
        draw(size, None).save(os.path.join(OUT, "icon-%d.png" % size), optimize=True)
        # A maskable icon fills its square; the cookie (36% of the side from
        # the centre) is inside the 40% safe zone.
        draw(size, CREAM).save(os.path.join(OUT, "maskable-%d.png" % size), optimize=True)
    draw(180, CREAM).save(os.path.join(OUT, "apple-touch-icon.png"), optimize=True)
    shutil.copyfile(os.path.join(ROOT, "docs", "site", "assets", "logo.svg"), os.path.join(OUT, "icon.svg"))


main()
