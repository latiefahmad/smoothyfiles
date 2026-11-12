"""Builds crates/rf-app/assets/smoothyfiles.ico (and smoothyfiles.png) from the SVG logo.

Every icon size is rendered from vector (no downscaling): smoothyfiles.svg for 32 px and up,
smoothyfiles-small.svg (no glow / rim light, larger bolt) for 16–24 px, which is what the
taskbar and Explorer show. Rendering uses headless Chrome or Edge (transparent background);
packing uses Pillow. Usage: python tools/make_icon.py
"""
import os
import shutil
import subprocess
import tempfile

from PIL import Image

ASSETS = os.path.join(os.path.dirname(__file__), "..", "crates", "rf-app", "assets")
BIG = os.path.join(ASSETS, "smoothyfiles.svg")
SMALL = os.path.join(ASSETS, "smoothyfiles-small.svg")
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
SMALL_MAX = 24


def browser():
    for p in (
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ):
        if os.path.exists(p):
            return p
    raise SystemExit("Chrome or Edge is needed to render the SVG")


def render(svg_path, size, work, exe):
    """One size, rendered by the browser at exactly size x size, transparent."""
    page = os.path.join(work, f"icon-{size}.html")
    with open(svg_path, encoding="utf-8") as f:
        svg = f.read()
    svg = svg.replace("<svg ", f'<svg width="{size}" height="{size}" ', 1)
    with open(page, "w", encoding="utf-8") as f:
        f.write(f"<!doctype html><html><body style='margin:0;background:transparent'>{svg}</body></html>")
    out = os.path.join(work, f"icon-{size}.png")
    subprocess.run(
        [
            exe,
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--force-device-scale-factor=1",
            f"--user-data-dir={os.path.join(work, 'profile')}",
            "--default-background-color=00000000",
            f"--window-size={size},{size}",
            f"--screenshot={out}",
            "file:///" + page.replace("\\", "/"),
        ],
        check=True,
        capture_output=True,
    )
    return Image.open(out).convert("RGBA").crop((0, 0, size, size))


def main():
    exe = browser()
    work = tempfile.mkdtemp(prefix="rf-icon-")
    try:
        images = [render(SMALL if s <= SMALL_MAX else BIG, s, work, exe) for s in SIZES]
        biggest = images[-1]
        out = os.path.join(ASSETS, "smoothyfiles.ico")
        # each size is its own rendering (Pillow keeps appended images whose size matches)
        # plain BMP frames: every Windows API and tool reads them (some choke on PNG frames)
        biggest.save(out, sizes=[(s, s) for s in SIZES], append_images=images[:-1], bitmap_format="bmp")
        biggest.save(os.path.join(ASSETS, "smoothyfiles.png"))
        print("wrote", os.path.abspath(out))
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
