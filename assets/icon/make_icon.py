"""Draws the Copilot Canvas app icon (assets/icon/copilot-canvas.ico and a
512 px PNG preview): a dark canvas tile with a faint dot grid, three session
cards joined by connectors (working blue, needs-you gold, done green) and the
glowing diamond of the toolbar's mark. Sizes up to 32 px get a simpler
drawing (the diamond and one card) so they stay legible.

Run: python assets/icon/make_icon.py   (needs Pillow)
"""

from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

HERE = Path(__file__).resolve().parent
SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]

# The GUI's palette (canvas-core paint.rs).
BG_TOP = (30, 38, 66)
BG_BOTTOM = (10, 13, 20)
PANEL = (22, 27, 34)
ACCENT = (88, 166, 255)
ATTENTION = (255, 176, 0)
DONE = (110, 240, 190)
PURPLE = (150, 95, 255)
CYAN = (90, 228, 255)
TEXT = (201, 209, 217)
GRID = (70, 84, 112)


def gradient(size, top, bottom):
    """A smooth top-left to bottom-right two-colour gradient."""
    vertical = Image.linear_gradient("L").resize((size, size))
    horizontal = vertical.rotate(90)  # counter-clockwise: dark on the left
    ramp = ImageChops.add(vertical, horizontal, scale=2)
    return Image.composite(Image.new("RGBA", (size, size), bottom + (255,)), Image.new("RGBA", (size, size), top + (255,)), ramp)


def rounded_mask(size, inset, radius):
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle((inset, inset, size - inset, size - inset), radius=radius, fill=255)
    return mask


def diamond(cx, cy, r):
    return [(cx, cy - r), (cx + r, cy), (cx, cy + r), (cx - r, cy)]


def glowing_diamond(size, cx, cy, r, glow):
    """The mark: a cyan-blue-purple diamond with a soft blue glow and a highlight."""
    layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    halo = Image.new("L", (size, size), 0)
    ImageDraw.Draw(halo).polygon(diamond(cx, cy, r * 1.08), fill=255)
    halo = halo.filter(ImageFilter.GaussianBlur(glow))
    layer = Image.composite(Image.new("RGBA", (size, size), ACCENT + (255,)), layer, halo.point(lambda v: int(v * 0.85)))
    shape = Image.new("L", (size, size), 0)
    ImageDraw.Draw(shape).polygon(diamond(cx, cy, r), fill=255)
    fill = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    span = int(2 * r)
    # Top vertex to bottom vertex: the whole cyan-to-purple range shows.
    ramp = Image.linear_gradient("L").resize((span, span))
    fill.paste(Image.composite(Image.new("RGBA", (span, span), PURPLE + (255,)), Image.new("RGBA", (span, span), CYAN + (255,)), ramp), (int(cx - r), int(cy - r)))
    layer = Image.composite(fill, layer, shape)
    # A highlight on the upper-left facet.
    shine = Image.new("L", (size, size), 0)
    ImageDraw.Draw(shine).polygon([(cx, cy - r * 0.86), (cx - r * 0.86, cy), (cx - r * 0.30, cy), (cx, cy - r * 0.30)], fill=120)
    shine = shine.filter(ImageFilter.GaussianBlur(max(1, r * 0.04)))
    layer = Image.composite(Image.new("RGBA", (size, size), (255, 255, 255, 255)), layer, ImageChops.multiply(shine, shape))
    return layer


def card(draw, box, border, s, title_frac=0.62):
    """A session box: panel fill, coloured border, a title bar and a status line."""
    x0, y0, x1, y1 = box
    w = x1 - x0
    draw.rounded_rectangle(box, radius=int(0.11 * w), fill=PANEL + (255,), outline=border + (255,), width=max(2, int(0.045 * w)))
    pad = 0.13 * w
    bar = max(2, int(0.075 * w))
    ty = y0 + (y1 - y0) * 0.36
    draw.rounded_rectangle((x0 + pad, ty - bar / 2, x0 + pad + (w - 2 * pad) * title_frac, ty + bar / 2), radius=bar // 2, fill=TEXT + (235,))
    sy = y0 + (y1 - y0) * 0.68
    dot = bar * 0.95
    draw.ellipse((x0 + pad, sy - dot / 2, x0 + pad + dot, sy + dot / 2), fill=border + (255,))
    draw.rounded_rectangle((x0 + pad + dot * 1.7, sy - bar / 2.6, x0 + pad + (w - 2 * pad) * 0.48, sy + bar / 2.6), radius=bar // 3, fill=(110, 118, 129, 230))


def connector(draw, points, color, width):
    draw.line(points, fill=color + (210,), width=width, joint="curve")
    r = width / 2
    for x, y in (points[0], points[-1]):
        draw.ellipse((x - r, y - r, x + r, y + r), fill=color + (210,))


def bezier(p0, p1, p2, p3, steps=48):
    out = []
    for i in range(steps + 1):
        t = i / steps
        u = 1 - t
        out.append((
            u ** 3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t ** 3 * p3[0],
            u ** 3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t ** 3 * p3[1],
        ))
    return out


def render(size, simple):
    S = 1024
    k = S / 1024
    inset, radius = int(36 * k), int(210 * k)
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    tile = gradient(S, BG_TOP, BG_BOTTOM)
    draw = ImageDraw.Draw(tile)
    if not simple:
        step, dot = 92, 6
        for gy in range(step // 2, S, step):
            for gx in range(step // 2, S, step):
                draw.ellipse((gx - dot, gy - dot, gx + dot, gy + dot), fill=GRID + (110,))
    # A thin light rim so the tile reads on dark taskbars.
    rim = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(rim).rounded_rectangle((inset, inset, S - inset, S - inset), radius=radius, outline=(120, 150, 220, 90), width=int(10 * k))
    tile = Image.alpha_composite(tile, rim)
    img = Image.composite(tile, img, rounded_mask(S, inset, radius))

    layer = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    if simple:
        # A gold "needs you" card peeking out behind the mark.
        card(d, (560, 560, 900, 800), ATTENTION, S, title_frac=0.55)
        img = Image.alpha_composite(img, layer)
        img = Image.alpha_composite(img, glowing_diamond(S, 430, 440, 300, 60))
    else:
        connector(d, bezier((330, 420), (330, 560), (300, 600), (300, 660)), ACCENT, 22)
        connector(d, bezier((500, 330), (600, 330), (600, 520), (660, 520)), ACCENT, 22)
        card(d, (150, 230, 510, 430), ACCENT, S)
        card(d, (590, 450, 900, 640), ATTENTION, S, title_frac=0.55)
        card(d, (160, 650, 520, 850), DONE, S, title_frac=0.7)
        img = Image.alpha_composite(img, layer)
        img = Image.alpha_composite(img, glowing_diamond(S, 760, 250, 150, 46))
        # A loop's lit dot on the green card's corner.
        spark = Image.new("RGBA", (S, S), (0, 0, 0, 0))
        sd = ImageDraw.Draw(spark)
        sd.ellipse((490, 820, 550, 880), fill=DONE + (255,))
        img = Image.alpha_composite(img, spark.filter(ImageFilter.GaussianBlur(10)))
        img = Image.alpha_composite(img, spark)
    # Keep everything inside the tile.
    clip = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    img = Image.composite(img, clip, rounded_mask(S, inset - 2, radius + 2))
    return img.resize((size, size), Image.LANCZOS)


def main():
    images = [render(s, simple=s <= 32) for s in SIZES]
    images[-1].save(HERE / "copilot-canvas.ico", format="ICO", sizes=[(s, s) for s in SIZES], append_images=images[:-1])
    render(512, simple=False).save(HERE / "copilot-canvas.png")
    print("wrote", HERE / "copilot-canvas.ico")


if __name__ == "__main__":
    main()
