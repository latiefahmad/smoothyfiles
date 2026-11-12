"""Makes crates/rf-app/assets/splash.gif, the animated picture the installer and the updater show.

The scene (scene.html) is latief.id/smoothyfiles's 3-D hero logo (the same shapes, materials, light and
bloom) with the name in Bricolage Grotesque, rendered frame by frame in headless Chrome, then:
the background outside the moving logo is frozen (bloom changes it by a few levels each frame,
which would make the GIF rewrite every pixel), one 128-colour palette, ordered dithering (stable
from frame to frame), 15 frames a second. About 1.3 MB; the loop is seamless.

    python tools/splash/make_splash.py --three <path to node_modules/three> --font <Bricolage .woff2>

Needs node, Chrome, ffmpeg and Pillow. three comes from the site (smoothyfilescom/node_modules/three);
the font from Google Fonts (Bricolage Grotesque, OFL) or the site's build.
"""

import argparse
import glob
import os
import shutil
import subprocess
import sys
import tempfile
import time

from PIL import Image, ImageChops

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
W, H, RENDER_FPS, OUT_FPS = 520, 300, 20, 15
CHROME = r"C:\Program Files\Google\Chrome\Application\chrome.exe"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--three", required=True)
    ap.add_argument("--font", required=True)
    ap.add_argument("--out", default=os.path.join(ROOT, "crates", "rf-app", "assets", "splash.gif"))
    ap.add_argument("--port", type=int, default=8765)
    a = ap.parse_args()

    work = tempfile.mkdtemp(prefix="rf-splash-")
    shutil.copy(os.path.join(HERE, "scene.html"), os.path.join(work, "index.html"))
    shutil.copy(os.path.join(HERE, "server.js"), work)
    shutil.copy(a.font, os.path.join(work, "bricolage.woff2"))
    server = subprocess.Popen(["node", os.path.join(work, "server.js"), os.path.abspath(a.three), str(a.port)], stdout=subprocess.PIPE, text=True)
    server.stdout.readline()  # "listening"
    chrome = subprocess.Popen([
        CHROME, "--headless=new", f"--user-data-dir={os.path.join(work, 'profile')}", "--no-first-run",
        "--enable-unsafe-swiftshader", "--use-angle=swiftshader", f"--window-size={W * 2},{H * 2}",
        "--remote-debugging-port=0", f"http://127.0.0.1:{a.port}/?scale=2&fps={RENDER_FPS}",
    ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        server.wait(timeout=600)  # it ends when the page says it is done
    finally:
        chrome.kill()
        time.sleep(1)

    frames = sorted(glob.glob(os.path.join(work, "frames", "f*.png")))
    ims = [Image.open(f).convert("RGB").resize((W, H), Image.LANCZOS) for f in frames]
    # the part that really moves (more than 8 levels), with a margin
    ref, box = ims[0], None
    for im in ims[1:]:
        b = ImageChops.difference(ref, im).convert("L").point(lambda v: 255 if v > 8 else 0).getbbox()
        if b:
            box = b if box is None else (min(box[0], b[0]), min(box[1], b[1]), max(box[2], b[2]), max(box[3], b[3]))
    m = 10
    box = (max(0, box[0] - m), max(0, box[1] - m), min(W, box[2] + m), min(H, box[3] + m))
    still = os.path.join(work, "still")
    os.makedirs(still)
    for i, im in enumerate(ims):
        out = ref.copy()
        out.paste(im.crop(box), box[:2])
        out.save(os.path.join(still, f"f{i:03d}.png"))

    seq = os.path.join(still, "f%03d.png")
    pal = os.path.join(work, "palette.png")
    subprocess.check_call(["ffmpeg", "-v", "error", "-y", "-framerate", str(RENDER_FPS), "-i", seq, "-vf", "palettegen=max_colors=128:stats_mode=full", pal])
    subprocess.check_call([
        "ffmpeg", "-v", "error", "-y", "-framerate", str(RENDER_FPS), "-i", seq, "-i", pal,
        "-lavfi", f"fps={OUT_FPS}[a];[a][1:v]paletteuse=dither=bayer:bayer_scale=3:diff_mode=rectangle", "-loop", "0", a.out,
    ])
    print(a.out, os.path.getsize(a.out), "bytes,", len(ims), "frames rendered")
    shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
