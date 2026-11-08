#!/usr/bin/env python3
"""Regenerates the small media fixtures used by src-tauri/tests (needs Pillow, piexif, ffmpeg)."""
import os, subprocess, random
from PIL import Image, ImageDraw
import piexif

out = os.path.join(os.path.dirname(__file__), "..", "src-tauri", "tests", "fixtures")
os.makedirs(out, exist_ok=True)

# 400x200 landscape marked "orientation 6": must be rendered as 200x400 portrait.
im = Image.new("RGB", (400, 200), (30, 90, 160))
ImageDraw.Draw(im).rectangle((0, 0, 199, 99), fill=(230, 80, 60))
exif = {
    "0th": {piexif.ImageIFD.Orientation: 6, piexif.ImageIFD.Make: b"Canon", piexif.ImageIFD.Model: b"Canon EOS R5"},
    "Exif": {
        piexif.ExifIFD.DateTimeOriginal: b"2021:07:04 23:30:15",
        piexif.ExifIFD.ISOSpeedRatings: 400,
        piexif.ExifIFD.FNumber: (28, 10),
        piexif.ExifIFD.ExposureTime: (1, 250),
        piexif.ExifIFD.FocalLength: (50, 1),
        piexif.ExifIFD.LensModel: b"RF50mm F1.8",
        piexif.ExifIFD.PixelXDimension: 400, piexif.ExifIFD.PixelYDimension: 200,
    },
    "GPS": {
        piexif.GPSIFD.GPSLatitudeRef: b"S", piexif.GPSIFD.GPSLatitude: ((33, 1), (52, 1), (0, 1)),
        piexif.GPSIFD.GPSLongitudeRef: b"E", piexif.GPSIFD.GPSLongitude: ((151, 1), (12, 1), (0, 1)),
    },
}
im.save(os.path.join(out, "exif_orient6.jpg"), exif=piexif.dump(exif), quality=90)

Image.new("RGBA", (64, 48), (20, 200, 120, 255)).save(os.path.join(out, "plain.png"))

# Fake RAW: opaque bytes with a large real JPEG embedded in the middle.
random.seed(7)
noisy = Image.effect_noise((900, 600), 80).convert("RGB")
noisy.save("/tmp/_prev.jpg", quality=92)
prev = open("/tmp/_prev.jpg", "rb").read()
junk = bytes(random.getrandbits(8) & 0x7F for _ in range(4000))
open(os.path.join(out, "fake_raw.dng"), "wb").write(junk + prev + junk)

open(os.path.join(out, "corrupt.jpg"), "wb").write(b"this is not an image" * 50)

subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=15:duration=2",
                "-pix_fmt", "yuv420p", os.path.join(out, "clip.mp4")], check=True)
print(sorted(os.listdir(out)))
