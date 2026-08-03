#!/usr/bin/env python3
"""Build the looping demo GIF for the README from Telegram screenshots.

Screenshots vary in size between devices and Telegram clients, so every frame
is scaled to one width and padded to one canvas — a GIF with mismatched frame
sizes jumps around as it loops.

Usage:
    python3 scripts/build-demo-gif.py [--seconds 3] [--width 420]

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
            frame.save(tmp / f"{i:03d}.png")
            print(f"  frame {i + 1}: {shot.name}" + ("" if args.no_captions else f"  “{caption_from(shot)}”"))

        # Two passes: a palette built from every frame, then the encode. A
        # per-frame palette makes flat UI colours shimmer as the loop runs.
        palette = tmp / "palette.png"
        rate = f"1/{args.seconds}"
        common = ["-y", "-loglevel", "error", "-framerate", rate, "-i", str(tmp / "%03d.png")]
        subprocess.run(
            ["ffmpeg", *common, "-vf", "palettegen=stats_mode=diff", str(palette)],
            check=True,
        )
        subprocess.run(
            [
                "ffmpeg", *common, "-i", str(palette),
                "-lavfi", "paletteuse=dither=bayer:bayer_scale=3",
                "-loop", "0", str(OUTPUT),
            ],
            check=True,
        )

    size_kb = OUTPUT.stat().st_size / 1024
    print(f"\n{OUTPUT.relative_to(ROOT)} — {len(shots)} frames, {size_kb:.0f} KB")
    if size_kb > 5000:
        print("  warning: over 5 MB; try --width 360 or fewer frames")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
