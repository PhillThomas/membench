#!/usr/bin/env python3
"""Render the banner in README.md and the GitHub social preview (1280x640).

    ./scripts/make-social-preview.py
writes assets/social-preview.png. Upload that same file under the repository's
Settings -> General -> Social preview; the repo and the link card should never
show two different pictures.

The grid is copied verbatim out of README.md, including the machine attribution
the README itself gives, so the image cannot claim a better run than the repo
does. No numbers are invented and none are rounded up.
"""
import re
from PIL import Image, ImageDraw, ImageFont

W, H = 1280, 640
BG_TOP, BG_BOT = (11, 14, 20), (17, 23, 34)
PANEL, PANEL_EDGE, STRIP = (13, 17, 23), (47, 57, 74), (22, 27, 36)
TXT, DIM, FAINT = (230, 237, 243), (139, 148, 158), (110, 118, 129)
GREEN, BLUE, PURPLE = (126, 231, 135), (88, 166, 255), (203, 145, 245)
D = "/usr/share/fonts/truetype/dejavu/"
mono = lambda s: ImageFont.truetype(D + "DejaVuSansMono.ttf", s)
monob = lambda s: ImageFont.truetype(D + "DejaVuSansMono-Bold.ttf", s)
sans = lambda s: ImageFont.truetype(D + "DejaVuSans.ttf", s)
sansb = lambda s: ImageFont.truetype(D + "DejaVuSans-Bold.ttf", s)

# ---------------------------------------------------------------- background
img = Image.new("RGB", (W, H))
px = img.load()
for y in range(H):
    t = y / H
    c = tuple(int(BG_TOP[i] + (BG_BOT[i] - BG_TOP[i]) * t) for i in range(3))
    for x in range(W):
        px[x, y] = c
overlay = Image.new("RGB", (W, H), BG_TOP)
od = ImageDraw.Draw(overlay)
od.ellipse([W - 520, -320, W + 340, 520], fill=(24, 38, 58))
img = Image.blend(img, overlay, 0.55)
d = ImageDraw.Draw(img, "RGBA")

# ---------------------------------------------------------------- terminal
PX, PY, PW, PH = 56, 78, 736, 494
d.rounded_rectangle([PX, PY, PX + PW, PY + PH], radius=14, fill=PANEL, outline=PANEL_EDGE, width=2)
d.rounded_rectangle([PX, PY, PX + PW, PY + 46], radius=14, fill=STRIP)
d.rectangle([PX, PY + 32, PX + PW, PY + 46], fill=STRIP)
for i, x in enumerate((26, 48, 70)):
    d.ellipse([PX + x - 7, PY + 22 - 7, PX + x + 7, PY + 22 + 7], fill=(52, 62, 80))
d.text((PX + 96, PY + 13), "membench run", font=sans(17), fill=(159, 168, 180))

# Verbatim from README.md. A monospace font aligns this block by construction,
# so each line is drawn as one string at one x and nothing is re-flowed; the
# units are then overdrawn in a dimmer colour at their exact offset, which is
# the only way to colour them without moving a column.
LINES = [
    "             Read      Write       Copy   Latency",
    "Memory     29.0 GB/s  29.1 GB/s  13.4 GB/s   126 ns",
    "L1 Cache   1.03 TB/s 658.3 GB/s 438.2 GB/s  0.93 ns",
    "L2 Cache   652.2 GB/s 706.9 GB/s 262.3 GB/s  3.06 ns",
    "L3 Cache   329.9 GB/s 307.6 GB/s  74.7 GB/s 11.36 ns",
]
UNIT = re.compile(r"(?:GB/s|TB/s|ns)")
tx, ty = PX + 34, PY + 74
TBL = 20
f = mono(TBL)
d.text((tx, ty), "$ ", font=f, fill=DIM)
w = d.textlength("$ ", font=f)
d.text((tx + w, ty), "membench run --pages 2m", font=monob(TBL), fill=TXT)

y = ty + 56
maxx = 0
for i, line in enumerate(LINES):
    header = i == 0
    if not header:
        d.text((tx, y), line[:10], font=monob(TBL), fill=BLUE)
    d.text((tx, y), line, font=f, fill=FAINT if header else TXT)
    for m in UNIT.finditer(line):
        if m.start() < 10:
            continue
        ox = tx + d.textlength(line[: m.start()], font=f)
        d.text((ox, y), m.group(), font=f, fill=(121, 136, 128) if not header else FAINT)
    maxx = max(maxx, tx + d.textlength(line, font=f))
    y += 40

y += 22
d.line([PX + 34, y, PX + PW - 34, y], fill=(38, 46, 60), width=1)
y += 20
# Three lines, in the README's own words. The caption is the argument: these
# numbers came off a container with no huge pages on a powersave governor, and
# the tool said so rather than quietly presenting them as a clean bench.
for line, col in [
    ("sample output, copied from the README:", FAINT),
    ("Ryzen 5 PRO 5650G in an LXC container, no huge pages,", DIM),
    ("powersave governor — the machine that produces numbers", DIM),
    ("people misread.", GREEN),
]:
    d.text((PX + 34, y), line, font=sans(19), fill=col)
    maxx = max(maxx, PX + 34 + d.textlength(line, font=sans(19)))
    y += 28

# ---------------------------------------------------------------- right column
RX = 832
rmax = 0
def R(x, y, text, font, fill):
    global rmax
    d.text((x, y), text, font=font, fill=fill)
    rmax = max(rmax, x + d.textlength(text, font=font))

R(RX, 96, "membench", monob(62), TXT)
R(RX, 176, "A Cache & Memory", sansb(27), (203, 213, 224))
R(RX, 212, "benchmark for the", sansb(27), (203, 213, 224))
R(RX, 248, "Linux terminal", sansb(27), (203, 213, 224))
R(RX, 302, "in the shape of AIDA64's window", sans(20), DIM)

bullets = [
    ("inline-assembly kernels, not libc", GREEN),
    ("avx512 / avx2 / sse2 / neon / scalar", PURPLE),
    ("warns when a number can't be trusted", BLUE),
]
y = 356
for text, col in bullets:
    d.ellipse([RX, y + 8, RX + 10, y + 18], fill=col)
    R(RX + 26, y, text, sans(19), (190, 200, 212))
    y += 42

d.line([RX, 496, min(RX + 388, W - 48), 496], fill=(38, 46, 60), width=1)
R(RX, 518, "github.com/PhillThomas/membench", monob(19), BLUE)
R(RX, 550, "Rust  ·  MIT  ·  x86-64 + aarch64", sans(18), FAINT)

print("right edge of longest table row:", int(maxx), "of panel edge", PX + PW - 20)
print("terminal right edge:", int(maxx), "of", PX + PW - 16)
print("right column edge:", int(rmax), "of", W - 48)
assert maxx <= PX + PW - 16, "table overflows the panel"
assert rmax <= W - 48, "right column overflows the canvas"
print("both edges inside their limits")

import os
out = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "assets", "social-preview.png")
img.save(out, optimize=True)
print("wrote", out)
print("right edge", int(maxx), "of", PX + PW - 16, "| right column", int(rmax), "of", W - 48)
