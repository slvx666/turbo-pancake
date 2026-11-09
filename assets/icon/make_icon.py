"""Draws the turbo-pancake application icon (original artwork, no third-party assets).

    python assets/icon/make_icon.py

Writes turbo-pancake.ico (16-256 px, executables), turbo-pancake-64.png (window icon)
and turbo-pancake.png (256 px) next to this file.
Rounded square with a warm gradient; a white speech bubble with three voice bars.
Every size is rendered from a 1024 px supersampled drawing; the small sizes use a
slightly bolder bubble and no tail detail so they stay readable at 16 px.
"""

from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
SS = 1024  # supersampled canvas
TOP = (255, 145, 64)  # warm orange
BOTTOM = (226, 59, 104)  # raspberry
BAR = (233, 84, 92)


def gradient(size: int) -> Image.Image:
    img = Image.new("RGBA", (size, size))
    px = img.load()
    for y in range(size):
        for x in range(size):
            t = (0.65 * y + 0.35 * x) / size
            px[x, y] = tuple(round(TOP[i] + (BOTTOM[i] - TOP[i]) * t) for i in range(3)) + (255,)
    return img


def draw(small: bool) -> Image.Image:
    s = SS
    base = gradient(256).resize((s, s), Image.BICUBIC)
    mask = Image.new("L", (s, s), 0)
    margin = round(s * (0.02 if small else 0.04))
    ImageDraw.Draw(mask).rounded_rectangle(
        (margin, margin, s - margin, s - margin), radius=round(s * 0.22), fill=255
    )
    icon = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    icon.paste(base, (0, 0), mask)

    d = ImageDraw.Draw(icon)
    # speech bubble
    bx0, by0 = s * (0.17 if small else 0.2), s * (0.2 if small else 0.22)
    bx1, by1 = s * (0.83 if small else 0.8), s * (0.7 if small else 0.68)
    d.rounded_rectangle((bx0, by0, bx1, by1), radius=s * 0.16, fill="white")
    # tail (bottom left)
    tx = bx0 + s * 0.12
    d.polygon(
        [(tx, by1 - s * 0.05), (tx + s * 0.16, by1 - s * 0.02), (tx - s * 0.04, by1 + s * 0.14)],
        fill="white",
    )
    # three voice bars, centred in the bubble
    cy = (by0 + by1) / 2
    w = s * (0.085 if small else 0.07)
    gap = s * (0.07 if small else 0.065)
    heights = (0.16, 0.28, 0.16) if small else (0.14, 0.26, 0.18)
    cx = (bx0 + bx1) / 2
    for i, h in enumerate(heights):
        x = cx + (i - 1) * (w + gap)
        hh = s * h / 2
        d.rounded_rectangle((x - w / 2, cy - hh, x + w / 2, cy + hh), radius=w / 2, fill=BAR)
    return icon


def main() -> None:
    big, small = draw(False), draw(True)
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    frames = [(small if n <= 32 else big).resize((n, n), Image.LANCZOS) for n in sizes]
    frames[-1].save(HERE / "turbo-pancake.png")
    # Window icon of the client (title bar, taskbar): small, decoded at start.
    frames[sizes.index(64)].save(HERE / "turbo-pancake-64.png")
    # BMP frames at every size: every Windows icon API loads them.
    frames[-1].save(
        HERE / "turbo-pancake.ico",
        sizes=[(n, n) for n in sizes],
        append_images=frames[:-1],
        bitmap_format="bmp",
    )
    # A preview sheet for review (not used by the build).
    sheet = Image.new("RGBA", (sum(sizes) + 10 * len(sizes), 256), (40, 40, 46, 255))
    x = 0
    for f in frames:
        sheet.paste(f, (x, 256 - f.height), f)
        x += f.width + 10
    sheet.save(HERE / "preview.png")
    print("wrote", HERE / "turbo-pancake.ico")


if __name__ == "__main__":
    main()
