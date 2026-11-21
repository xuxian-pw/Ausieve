"""Render the app icon (design "M3") into multi-size .ico files.

    python assets/make_icons.py

Writes assets/app.ico (normal) and assets/warn.ico (shown in the tray when the
output or capture is failing), plus assets/preview.png for a quick look. The .ico
files are committed, so building the program does not need Python.

Design, in a 64x64 grid: a dark rounded square; a white disc ("all the sound")
with a bite taken out of it; a coral ring sitting in the bite ("the one program
left out"). Sizes of 20 px and below use thicker strokes and a wider gap so the
shapes stay apart on the pixel grid.
"""

from pathlib import Path
from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
SS = 16  # supersampling factor

BG = (0x1A, 0x1A, 0x1A, 255)
NORMAL = {"disc": (255, 255, 255, 255), "ring": (0xF0, 0x99, 0x7B, 255)}
WARN = {"disc": (0xFA, 0xC7, 0x75, 255), "ring": (0xE2, 0x4B, 0x4A, 255)}

# Geometry in 64-unit space.
LARGE = dict(margin=2, radius=14, disc_c=(24, 32), disc_r=15, ring_c=(40, 32), ring_r=13.5, ring_w=4.5, gap=2)
SMALL = dict(margin=0, radius=12, disc_c=(23, 32), disc_r=17, ring_c=(41, 32), ring_r=13, ring_w=7, gap=3.5)

SIZES = [16, 20, 24, 32, 40, 48, 64, 256]


def circle(draw, c, r, fill):
    draw.ellipse([c[0] - r, c[1] - r, c[0] + r, c[1] + r], fill=fill)


def render(size, colors):
    g = SMALL if size <= 20 else LARGE
    n = size * SS
    k = n / 64

    def p(v):
        return v * k

    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    m = p(g["margin"])
    d.rounded_rectangle([m, m, n - 1 - m, n - 1 - m], radius=p(g["radius"]), fill=BG)

    rc = (p(g["ring_c"][0]), p(g["ring_c"][1]))
    outer = p(g["ring_r"] + g["ring_w"] / 2)
    inner = p(g["ring_r"] - g["ring_w"] / 2)

    # Disc minus the bite around the ring.
    mask = Image.new("L", (n, n), 0)
    md = ImageDraw.Draw(mask)
    circle(md, (p(g["disc_c"][0]), p(g["disc_c"][1])), p(g["disc_r"]), 255)
    circle(md, rc, outer + p(g["gap"]), 0)
    img.paste(Image.new("RGBA", (n, n), colors["disc"]), (0, 0), mask)

    # Ring.
    mask = Image.new("L", (n, n), 0)
    md = ImageDraw.Draw(mask)
    circle(md, rc, outer, 255)
    circle(md, rc, inner, 0)
    img.paste(Image.new("RGBA", (n, n), colors["ring"]), (0, 0), mask)

    return img.resize((size, size), Image.BOX)


def write_ico(path, images):
    """ICO with PNG-compressed entries (supported by Windows Vista and later)."""
    import io
    import struct

    blobs = []
    for im in images:
        buf = io.BytesIO()
        im.save(buf, "PNG")
        blobs.append(buf.getvalue())
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for im, b in zip(images, blobs):
        w = im.width % 256
        out += struct.pack("<BBBBHHII", w, w, 0, 0, 1, 32, len(b), offset)
        offset += len(b)
    path.write_bytes(out + b"".join(blobs))


def main():
    sets = {}
    for name, colors in [("app", NORMAL), ("warn", WARN)]:
        imgs = [render(s, colors) for s in SIZES]
        write_ico(HERE / f"{name}.ico", imgs)
        sets[name] = imgs

    # Preview: each size at 1x on light and dark strips, plus the 256 px icon.
    pad = 8
    strip_w = sum(SIZES[:-1]) + pad * len(SIZES)
    w = max(strip_w, 256 * 2 + pad * 3)
    h = pad + 256 + pad + (64 + pad) * 4
    prev = Image.new("RGBA", (w, h), (128, 128, 128, 255))
    prev.alpha_composite(sets["app"][-1], (pad, pad))
    prev.alpha_composite(sets["warn"][-1], (pad * 2 + 256, pad))
    y = pad * 2 + 256
    for name in ["app", "warn"]:
        for bg in [(243, 243, 243, 255), (32, 32, 32, 255)]:
            prev.paste(Image.new("RGBA", (w, 64 + pad), bg), (0, y))
            x = pad
            for im in sets[name][:-1]:
                prev.alpha_composite(im, (x, y + (64 - im.height) // 2 + pad // 2))
                x += im.width + pad
            y += 64 + pad
    prev.save(HERE / "preview.png")


if __name__ == "__main__":
    main()
