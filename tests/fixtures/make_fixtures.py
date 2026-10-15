#!/usr/bin/env python3
"""Generates the small sample media files used by the integration tests.

Run through make_fixtures.sh. Needs Pillow for JPEG/PNG/GIF; WAV uses the
standard library. MP3/MP4/OGG are made with ffmpeg (FFMPEG env var or PATH);
without ffmpeg, small structurally valid files are synthesised instead.
"""

import io
import math
import os
import random
import shutil
import struct
import subprocess
import sys
import tempfile
import wave
import zlib

OUT = os.path.dirname(os.path.abspath(__file__))
rng = random.Random(1234)

try:
    from PIL import Image, ImageDraw
except ImportError:
    sys.exit("Pillow is required: pip install pillow (or use a venv)")


def picture(w, h, seed):
    r = random.Random(seed)
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        for x in range(w):
            px[x, y] = (
                (x * 255 // w + r.randint(0, 40)) % 256,
                (y * 255 // h + r.randint(0, 40)) % 256,
                ((x + y) * 128 // (w + h) + r.randint(0, 40)) % 256,
            )
    d = ImageDraw.Draw(img)
    for _ in range(12):
        x0, y0 = r.randint(0, w), r.randint(0, h)
        d.ellipse(
            [x0, y0, x0 + r.randint(10, w // 3), y0 + r.randint(10, h // 3)],
            fill=(r.randint(0, 255), r.randint(0, 255), r.randint(0, 255)),
        )
    return img


def exif_with_thumbnail(thumb_jpeg):
    """Minimal EXIF block whose IFD1 points at an embedded JPEG thumbnail."""
    # TIFF header (little endian), IFD0 with no entries, IFD1 with 2 entries.
    ifd0_off = 8
    ifd1_off = ifd0_off + 2 + 4
    thumb_off = ifd1_off + 2 + 2 * 12 + 4
    tiff = b"II*\x00" + struct.pack("<I", ifd0_off)
    tiff += struct.pack("<H", 0) + struct.pack("<I", ifd1_off)
    tiff += struct.pack("<H", 2)
    tiff += struct.pack("<HHII", 0x0201, 4, 1, thumb_off)
    tiff += struct.pack("<HHII", 0x0202, 4, 1, len(thumb_jpeg))
    tiff += struct.pack("<I", 0)
    tiff += thumb_jpeg
    return b"Exif\x00\x00" + tiff


def make_images():
    img = picture(320, 240, 1)
    tb = io.BytesIO()
    img.resize((80, 60)).save(tb, "JPEG", quality=70)
    img.save(
        os.path.join(OUT, "photo.jpg"),
        "JPEG",
        quality=88,
        exif=exif_with_thumbnail(tb.getvalue()),
    )
    picture(256, 192, 2).save(os.path.join(OUT, "photo2.jpg"), "JPEG", quality=80, progressive=True)
    picture(200, 150, 3).save(os.path.join(OUT, "image.png"), "PNG")
    picture(120, 90, 4).convert("P", palette=Image.ADAPTIVE).save(os.path.join(OUT, "anim.gif"), "GIF")


def make_wav():
    with wave.open(os.path.join(OUT, "sound.wav"), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(8000)
        frames = b"".join(
            struct.pack("<h", int(8000 * math.sin(2 * math.pi * 440 * i / 8000)))
            for i in range(8000 * 3 // 2)
        )
        w.writeframes(frames)


def ffmpeg_path():
    p = os.environ.get("FFMPEG") or shutil.which("ffmpeg")
    return p if p and os.path.exists(p) else None


def run_ffmpeg(ff, args, out):
    subprocess.run(
        [ff, "-hide_banner", "-loglevel", "error", "-y", "-bitexact"] + args + ["-fflags", "+bitexact", out],
        check=True,
    )


def make_with_ffmpeg(ff):
    tone = ["-f", "lavfi", "-i", "sine=frequency=440:duration=2:sample_rate=22050"]
    run_ffmpeg(ff, tone + ["-c:a", "libmp3lame", "-b:a", "32k", "-ac", "1", "-map_metadata", "-1"],
               os.path.join(OUT, "song.mp3"))
    run_ffmpeg(ff, tone + ["-c:a", "libvorbis", "-q:a", "0", "-ac", "1"], os.path.join(OUT, "voice.ogg"))
    run_ffmpeg(
        ff,
        ["-f", "lavfi", "-i", "testsrc=size=96x64:rate=10:duration=2", "-c:v", "mpeg4", "-q:v", "12",
         "-pix_fmt", "yuv420p", "-movflags", "+faststart"],
        os.path.join(OUT, "clip.mp4"),
    )


def synth_mp3():
    # MPEG-1 Layer III, 32 kbit/s, 32 kHz, mono: 144*32000/32000 = 144 bytes per frame.
    hdr = bytes([0xFF, 0xFB, 0x18, 0xC4])
    frames = b"".join(hdr + bytes(140) for _ in range(60))
    with open(os.path.join(OUT, "song.mp3"), "wb") as f:
        f.write(frames)


def ogg_crc(data):
    crc = 0
    for b in data:
        crc ^= b << 24
        for _ in range(8):
            crc = ((crc << 1) ^ 0x04C11DB7) & 0xFFFFFFFF if crc & 0x80000000 else (crc << 1) & 0xFFFFFFFF
    return crc


def ogg_page(payload, seq, flags, granule):
    segs = []
    n = len(payload)
    while n >= 255:
        segs.append(255)
        n -= 255
    segs.append(n)
    hdr = b"OggS" + bytes([0, flags]) + struct.pack("<qII", granule, 0x1234, seq) + b"\0\0\0\0"
    page = hdr + bytes([len(segs)]) + bytes(segs) + payload
    crc = ogg_crc(page)
    return page[:22] + struct.pack("<I", crc) + page[26:]


def synth_ogg():
    pages = [ogg_page(b"\x01vorbis" + bytes(23), 0, 0x02, 0)]
    for i in range(1, 6):
        pages.append(ogg_page(bytes(rng.getrandbits(8) for _ in range(400)), i, 0x04 if i == 5 else 0, i * 1000))
    with open(os.path.join(OUT, "voice.ogg"), "wb") as f:
        f.write(b"".join(pages))


def box(kind, payload):
    return struct.pack(">I", 8 + len(payload)) + kind + payload


def synth_mp4():
    ftyp = box(b"ftyp", b"isom" + struct.pack(">I", 512) + b"isomiso2mp41")
    mvhd = box(b"mvhd", bytes(100))
    moov = box(b"moov", mvhd)
    mdat = box(b"mdat", bytes(rng.getrandbits(8) for _ in range(20000)))
    with open(os.path.join(OUT, "clip.mp4"), "wb") as f:
        f.write(ftyp + moov + mdat)


def main():
    make_images()
    make_wav()
    ff = ffmpeg_path()
    if ff:
        make_with_ffmpeg(ff)
    else:
        print("ffmpeg not found: synthesising minimal MP3/MP4/OGG files", file=sys.stderr)
        synth_mp3()
        synth_ogg()
        synth_mp4()
    total = 0
    for name in sorted(os.listdir(OUT)):
        if name.startswith("make_fixtures"):
            continue
        size = os.path.getsize(os.path.join(OUT, name))
        total += size
        print(f"{name:12} {size:8} bytes")
    print(f"total        {total:8} bytes")


if __name__ == "__main__":
    main()
