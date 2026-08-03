#!/usr/bin/env python3
"""Build the looping demo GIF for the README from Telegram screenshots.

Screenshots vary in size between devices and Telegram clients, so every frame
is scaled to one width and padded to one canvas — a GIF with mismatched frame
sizes jumps around as it loops.

Frames are held, then faded out to the chat background and in to the next.
Fade frames change every pixel, so they cost far more than held ones — the
defaults (3 steps, a 64-colour palette, no dithering) are what keep the file
near 1 MB instead of 4. `--fade 0` drops back to hard cuts at about 460 KB.

Usage:
    python3 scripts/build-demo-gif.py [--seconds 3] [--width 800] [--fade 0.45]

Input:  docs/img/frames/*.png|jpg, in filename order.
        The name after the leading number becomes the caption:
            1-price-comparison.png  ->  "Price comparison"
            2-photo-search.png      ->  "Photo search"
Output: docs/img/scout-demo.gif
"""

import argparse
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw, ImageFont

ROOT = pathlib.Path(__file__).resolve().parent.parent
FRAMES = ROOT / "docs" / "img" / "frames"
OUTPUT = ROOT / "docs" / "img" / "scout-demo.gif"

CAPTION_TEXT = (236, 240, 243)
CAPTION_HEIGHT = 44
# Fallback only; the real background is sampled from the screenshots.
BACKGROUND = (20, 24, 31)


def sample_background(images: list) -> tuple:
    """The chat background, taken from the screenshots themselves.

    Padding must be invisible: frames are rarely the same height, and a
    guessed colour turns every short frame into a visible letterbox.
    """
    from collections import Counter

    votes = Counter()
    for img in images:
        w, h = img.size
        for pt in ((1, 1), (w - 2, 1), (1, h - 2), (w - 2, h - 2)):
            votes[img.getpixel(pt)] += 1
    return votes.most_common(1)[0][0] if votes else BACKGROUND


def caption_from(path: pathlib.Path) -> str:
    """`1-price-comparison.png` -> `Price comparison`."""
    stem = re.sub(r"^\d+[-_. ]*", "", path.stem)
    text = stem.replace("-", " ").replace("_", " ").strip()
    # Only the first letter — capitalize() would lowercase "eBay", "bol.com".
    return text[:1].upper() + text[1:] if text else path.stem


def load_font(size: int) -> ImageFont.ImageFont:
    for candidate in (
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ):
        if pathlib.Path(candidate).exists():
            try:
                return ImageFont.truetype(candidate, size)
            except OSError:
                continue
    return ImageFont.load_default()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--seconds", type=float, default=3.0, help="seconds per frame")
    ap.add_argument("--width", type=int, default=800, help="output width in px")
    ap.add_argument("--no-captions", action="store_true")
    ap.add_argument("--fade", type=float, default=0.45, help="fade seconds; 0 disables")
    ap.add_argument("--fade-steps", type=int, default=3, help="blended frames per half-fade")
    args = ap.parse_args()

    if not shutil.which("ffmpeg"):
        sys.exit("ffmpeg not found — brew install ffmpeg")

    shots = sorted(
        p for p in FRAMES.glob("*") if p.suffix.lower() in {".png", ".jpg", ".jpeg"}
    )
    if not shots:
        sys.exit(f"no screenshots in {FRAMES} — see docs/img/README.md for the shot list")

    font = load_font(20)
    opened = [Image.open(s).convert("RGB") for s in shots]
    background = sample_background(opened)
    # A caption bar slightly lighter than the chat, so it reads as chrome.
    caption_bar = tuple(min(255, c + 14) for c in background)

    scaled = []
    for shot, img in zip(shots, opened):
        height = round(img.height * args.width / img.width)
        scaled.append((shot, img.resize((args.width, height), Image.LANCZOS)))

    # One canvas for every frame: the tallest screenshot sets the height.
    canvas_h = max(img.height for _, img in scaled)
    if not args.no_captions:
        canvas_h += CAPTION_HEIGHT

    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp)
        built = []
        for i, (shot, img) in enumerate(scaled):
            frame = Image.new("RGB", (args.width, canvas_h), background)
            top = CAPTION_HEIGHT if not args.no_captions else 0
            # Centred vertically in whatever space is left over.
            frame.paste(img, (0, top + (canvas_h - top - img.height) // 2))

            if not args.no_captions:
                draw = ImageDraw.Draw(frame)
                draw.rectangle([0, 0, args.width, CAPTION_HEIGHT], fill=caption_bar)
                text = caption_from(shot)
                box = draw.textbbox((0, 0), text, font=font)
                draw.text(
                    ((args.width - (box[2] - box[0])) // 2, (CAPTION_HEIGHT - (box[3] - box[1])) // 2 - 2),
                    text,
                    font=font,
                    fill=CAPTION_TEXT,
                )
            built.append(frame)
            frame.save(tmp / f"{i:03d}.png")
            print(f"  frame {i + 1}: {shot.name}" + ("" if args.no_captions else f"  “{caption_from(shot)}”"))

        # Timeline: hold, fade out to the chat background, fade in to the
        # next. The last fades back into the first, so the loop has no seam.
        #
        # Fading *through* the background rather than crossfading directly is
        # both softer to watch and far smaller: a direct blend ghosts two
        # screens of text over each other, and that entropy defeats GIF's
        # inter-frame compression (measured at 3.9 MB). Fades to a near-flat
        # dark frame compress to almost nothing.
        steps = args.fade_steps if args.fade > 0 else 0
        blank = Image.new("RGB", (args.width, canvas_h), background)
        timeline = []  # (path, seconds on screen)
        for i, frame in enumerate(built):
            timeline.append((tmp / f"{i:03d}.png", args.seconds))
            if not steps:
                continue
            nxt = built[(i + 1) % len(built)]
            half = args.fade / (2 * steps)
            for s in range(1, steps + 1):          # out
                path = tmp / f"x{i:03d}o{s:02d}.png"
                Image.blend(frame, blank, s / steps).save(path)
                timeline.append((path, half))
            for s in range(1, steps):              # in (final step is the held frame)
                path = tmp / f"x{i:03d}i{s:02d}.png"
                Image.blend(blank, nxt, s / steps).save(path)
                timeline.append((path, half))

        # ffmpeg's concat demuxer takes a duration per entry, which a plain
        # -framerate cannot express: holds are seconds, fade steps are tens
        # of milliseconds.
        listing = tmp / "timeline.txt"
        with listing.open("w") as fh:
            for path, secs in timeline:
                fh.write(f"file '{path}'\nduration {secs:.3f}\n")
            fh.write(f"file '{timeline[-1][0]}'\n")  # concat needs a final repeat

        palette = tmp / "palette.png"
        demux = ["-f", "concat", "-safe", "0", "-i", str(listing)]
        subprocess.run(
            ["ffmpeg", "-y", "-loglevel", "error", *demux,
             "-vf", "palettegen=stats_mode=diff:max_colors=64", str(palette)],
            check=True,
        )
        subprocess.run(
            ["ffmpeg", "-y", "-loglevel", "error", *demux, "-i", str(palette),
             # Screenshots are flat UI colour, so dithering adds only noise —
             # and noise in a crossfade destroys inter-frame compression
             # (measured: 4.3 MB dithered vs a fraction of that without).
             "-lavfi", "paletteuse=dither=none:diff_mode=rectangle",
             "-loop", "0", str(OUTPUT)],
            check=True,
        )
        if steps:
            print(f"  + {len(built)} crossfades, {steps} steps over {args.fade}s")

    size_kb = OUTPUT.stat().st_size / 1024
    print(f"\n{OUTPUT.relative_to(ROOT)} — {len(shots)} frames, {size_kb:.0f} KB")
    if size_kb > 5000:
        print("  warning: over 5 MB; try --width 360 or fewer frames")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
