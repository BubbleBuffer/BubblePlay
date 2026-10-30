#!/usr/bin/env python3
"""Derive BubblePlay exports from the approved master mark.

    python branding/build-icons.py            # exports + previews
    python branding/build-icons.py --no-preview
    python branding/build-icons.py --followups  # Windows only; no Android/web favicon writes

Master (never edited here): branding/bubbleplay-icon.svg
Needs: Python 3.10+, Pillow, and a Chrome/Chromium binary for rasterising
(set CHROME=<path> if it is not in a standard location).

Writes
  branding/out/bubbleplay.ico          16..256, each frame rendered from the master tile at its size
  branding/out/bubbleplay-tray.ico     16..48 notification-area frames: the tileless mark, transparent
  branding/out/bubbleplay-tray-dark.ico  the same frames with the outer outline in the tile colour (dark taskbars)
  branding/small/*.png                 exact native-pixel copies of the 16/20/24 ICO frames
  branding/out/tray-32.bgra            premultiplied BGRA candidate for future tray wiring
  branding/out/bubbleplay-tile.svg     the master (ivory tile, full bleed)
  branding/out/bubbleplay-tight.svg    the master without its tile
  worker/public/favicon.svg, favicon-32.png
  android/.../drawable-xxxhdpi/ic_launcher_foreground.png   the master on the 108dp adaptive canvas
  android/.../values/colors.xml, mipmap-anydpi/ic_launcher.xml
  branding/preview/*.png               (unless --no-preview)

Not written, maintained separately:
  android/.../drawable/ic_launcher_monochrome.xml   monochrome layer (old art, in a design round)
"""
from __future__ import annotations

import io
import math
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
BRANDING = ROOT / "branding"
MASTER = BRANDING / "bubbleplay-icon.svg"
OUT = BRANDING / "out"
PREVIEW = BRANDING / "preview"
PUBLIC = ROOT / "worker" / "public"
RES = ROOT / "android" / "app" / "src" / "main" / "res"

SVG_NS = "http://www.w3.org/2000/svg"
ANDROID = "{http://schemas.android.com/apk/res/android}"
AAPT = "{http://schemas.android.com/aapt}"

SAFE_ZONE_DP = 66        # Android adaptive icon safe zone (diameter) on a 108dp canvas, drawn in previews
ADAPTIVE_PX = 432        # adaptive foreground raster: 4 px per dp at xxxhdpi
ICO_SMALL = (16, 20, 24)
ICO_SIZES = (16, 20, 24, 32, 48, 64, 256)
TRAY_SIZES = (16, 20, 24, 32, 40, 48)   # small-icon sizes at 100..300% DPI
TRAY_MARGIN = 1 / 16     # clear margin on each side of a tray frame: 1px at 16px
# The outer outline (monitor silhouette, neck, foot) vanishes on a dark taskbar; the dark-taskbar
# tray icon draws these in the tile colour. Screen edge, face and cursor keep their charcoal.
TRAY_DARK_OUTLINE = ("bezel-outline", "neck", "foot")
SUPERSAMPLE = 1024

# The master's ivory tile is the only <g> with id "background"; the rest is the mark.
BACKGROUND = re.compile(r'<g id="background".*?</g>\n', re.S)


# --------------------------------------------------------------------------- master

def local(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def fmt(value: float) -> str:
    return f"{value:.3f}".rstrip("0").rstrip(".")


def write_text(path: Path, text: str) -> None:
    path.write_text(text, encoding="utf-8", newline="\n")


class Master:
    """The approved master, read once; every derived variant is cut from its text."""

    def __init__(self) -> None:
        self.text = MASTER.read_text(encoding="utf-8")
        tile = next(node for node in ET.parse(MASTER).getroot().iter() if node.get("id") == "tile")
        self.field = tile.get("fill")
        self.art_text, count = BACKGROUND.subn("", self.text, count=1)
        if count != 1:
            raise SystemExit(f"{MASTER} must have one background layer (g#background)")


def tile_svg(m: Master) -> str:
    """The master as it is: the mark on its ivory tile."""
    return m.text


def tight_svg(m: Master) -> str:
    """The mark alone, no tile, master viewBox."""
    return m.art_text


def tray_svg(m: Master, tight_big: Image.Image) -> str:
    """The tileless mark cropped to a square around its own bounds, so it fills a tray frame."""
    left, top, right, bottom = tight_big.getchannel("A").getbbox()
    unit = 1280 / tight_big.width                       # master viewBox units per raster pixel
    side = max(right - left, bottom - top) * unit / (1 - 2 * TRAY_MARGIN)
    cx, cy = (left + right) / 2 * unit, (top + bottom) / 2 * unit
    box = " ".join(fmt(v) for v in (cx - side / 2, cy - side / 2, side, side))
    text, count = re.subn(r'viewBox="0 0 1280 1280"', f'viewBox="{box}"', m.art_text, count=1)
    if count != 1:
        raise SystemExit(f"{MASTER} must keep its 1280 viewBox")
    return text


def tray_dark_svg(m: Master, tray: str) -> str:
    """The tray mark for dark taskbars: its outer outline recoloured to the master's tile colour."""
    for part in TRAY_DARK_OUTLINE:
        tray, count = re.subn(rf'(<path id="{part}"[^>]*?stroke=")#[0-9A-Fa-f]{{6}}(")',
                              rf"\g<1>{m.field}\g<2>", tray, count=1)
        if count != 1:
            raise SystemExit(f"{MASTER}: expected a stroked path#{part}")
    return tray


def svg_doc(view_box: tuple[float, float, float, float], body: str) -> str:
    box = " ".join(fmt(v) for v in view_box)
    return f'<svg xmlns="{SVG_NS}" viewBox="{box}">{body}</svg>\n'


# --------------------------------------------------------------------------- rasterising

def find_chrome() -> str:
    candidates = [os.environ.get("CHROME"), shutil.which("chrome"), shutil.which("chromium"),
                  r"C:\Program Files\Google\Chrome\Application\chrome.exe",
                  r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
                  r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
                  "/usr/bin/chromium", "/usr/bin/google-chrome",
                  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"]
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return candidate
    raise SystemExit("No Chrome/Chromium found; set CHROME=<path to the binary>.")


class Chrome:
    def __init__(self) -> None:
        self.binary = find_chrome()
        self.work = Path(tempfile.mkdtemp(prefix="bubbleplay-icons-"))

    def screenshot(self, url: str, width: int, height: int, transparent: bool) -> Image.Image:
        target = self.work / "shot.png"
        target.unlink(missing_ok=True)
        args = [self.binary, "--headless=new", "--disable-gpu", "--hide-scrollbars", "--no-first-run",
                "--no-sandbox", "--disable-crash-reporter", "--disable-extensions",
                "--force-device-scale-factor=1", f"--user-data-dir={self.work / 'profile'}",
                f"--window-size={width},{height}", f"--screenshot={target}"]
        if transparent:
            args.append("--default-background-color=00000000")
        result = subprocess.run(args + [url], capture_output=True, timeout=120)
        if result.returncode:
            raise RuntimeError(result.stderr.decode("utf-8", errors="replace"))
        with Image.open(target) as image:
            return image.convert("RGBA")

    def svg(self, svg: str, size: int = SUPERSAMPLE) -> Image.Image:
        """Rasterise a square SVG at `size` px with a transparent background."""
        page = self.work / "page.html"
        sized = svg.replace("<svg ", f'<svg width="{size}" height="{size}" ', 1)
        page.write_text(f'<!doctype html><html><body style="margin:0;background:transparent;overflow:hidden">{sized}</body></html>',
                        encoding="utf-8")
        return self.screenshot(page.as_uri(), size, size, True)

    def close(self) -> None:
        shutil.rmtree(self.work, ignore_errors=True)


def downsample(image: Image.Image, size: int) -> Image.Image:
    # Resample premultiplied so transparent edge pixels do not bleed colour.
    premultiplied = image.convert("RGBa").resize((size, size), Image.LANCZOS)
    return premultiplied.convert("RGBA")


# --------------------------------------------------------------------------- Windows

def ico_frame(image: Image.Image) -> bytes:
    size = image.width
    if size >= 256:
        data = io.BytesIO()
        image.save(data, "PNG", optimize=True)
        return data.getvalue()
    # 32-bit BGRA DIB, bottom-up, followed by an (empty) 1-bit AND mask.
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    blue_first = Image.merge("RGBA", (*image.split()[2::-1], image.split()[3]))
    rows = [blue_first.crop((0, y, size, y + 1)).tobytes() for y in reversed(range(size))]
    mask_row = bytes(((size + 31) // 32) * 4)
    return header + b"".join(rows) + mask_row * size


def write_ico(path: Path, frames: list[Image.Image]) -> None:
    blobs = [ico_frame(frame) for frame in frames]
    out = struct.pack("<HHH", 0, 1, len(frames))
    offset = 6 + 16 * len(frames)
    for frame, blob in zip(frames, blobs):
        dimension = 0 if frame.width >= 256 else frame.width
        out += struct.pack("<BBBBHHII", dimension, dimension, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    path.write_bytes(out + b"".join(blobs))


def premultiplied_bgra(image: Image.Image) -> bytes:
    r, g, b, a = image.convert("RGBa").split()
    return Image.merge("RGBA", (b, g, r, a)).tobytes()


# --------------------------------------------------------------------------- Android

def launcher_foreground(m: Master, chrome: Chrome) -> Image.Image:
    """Adaptive foreground: the master's 1280 frame is the 108dp canvas, so the mark keeps its master placement."""
    return chrome.svg(m.art_text, ADAPTIVE_PX)


def write_launcher_resources(m: Master) -> None:
    path = RES / "values" / "colors.xml"
    colours = path.read_text(encoding="utf-8")
    colours, count = re.subn(r'(<color name="launcher_background">)[^<]*(</color>)',
                             rf'\g<1>{m.field}\g<2>', colours)
    if count != 1:
        raise SystemExit("expected one launcher_background colour")
    write_text(path, colours)
    write_text(RES / "mipmap-anydpi" / "ic_launcher.xml",
               '<?xml version="1.0" encoding="utf-8"?>\n'
               '<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">\n'
               '    <background android:drawable="@color/launcher_background" />\n'
               '    <foreground android:drawable="@drawable/ic_launcher_foreground" />\n'
               '    <monochrome android:drawable="@drawable/ic_launcher_monochrome" />\n'
               '</adaptive-icon>\n')


def vd_to_svg(xml: str, colour: str | None = None) -> str:
    """Render-check helper: turn a vector drawable back into SVG,
    so the previews show what the XML says rather than what the master says."""
    root = ET.fromstring(xml.encode("utf-8"))
    defs: list[str] = []

    def convert(node: ET.Element) -> str:
        tag = local(node.tag)
        if tag == "group":
            scale = float(node.get(ANDROID + "scaleX", "1"))
            assert scale == float(node.get(ANDROID + "scaleY", "1"))
            tx, ty = node.get(ANDROID + "translateX", "0"), node.get(ANDROID + "translateY", "0")
            return f'<g transform="translate({tx} {ty}) scale({scale})">' + "".join(convert(c) for c in node) + "</g>"
        assert tag == "path", tag
        fill = node.get(ANDROID + "fillColor")
        for attr in node.findall(AAPT + "attr"):
            gradient = attr[0]
            name = f"g{len(defs)}"
            stops = "".join(f'<stop offset="{i.get(ANDROID + "offset")}" stop-color="#{i.get(ANDROID + "color")[3:]}"/>'
                            for i in gradient)
            defs.append(f'<linearGradient id="{name}" gradientUnits="userSpaceOnUse" '
                        f'x1="{gradient.get(ANDROID + "startX")}" y1="{gradient.get(ANDROID + "startY")}" '
                        f'x2="{gradient.get(ANDROID + "endX")}" y2="{gradient.get(ANDROID + "endY")}">{stops}</linearGradient>')
            fill = f"url(#{name})"
        paint = lambda value: colour or "#" + value[3:]
        attrs = [f'd="{node.get(ANDROID + "pathData")}"',
                 f'fill="{fill if fill and fill.startswith("url") else paint(fill) if fill else "none"}"']
        if node.get(ANDROID + "fillType") == "evenOdd":
            attrs.append('fill-rule="evenodd"')
        if node.get(ANDROID + "strokeColor"):
            attrs += [f'stroke="{paint(node.get(ANDROID + "strokeColor"))}"',
                      f'stroke-width="{node.get(ANDROID + "strokeWidth")}"',
                      f'stroke-linecap="{node.get(ANDROID + "strokeLineCap", "butt")}"',
                      f'stroke-linejoin="{node.get(ANDROID + "strokeLineJoin", "miter")}"']
        return f"<path {' '.join(attrs)}/>"

    body = "".join(convert(child) for child in root)
    return svg_doc((0, 0, 108, 108), ("<defs>" + "".join(defs) + "</defs>" if defs else "") + body)


# --------------------------------------------------------------------------- previews

def hex_rgb(colour: str) -> tuple[int, int, int]:
    colour = colour.lstrip("#")
    return tuple(int(colour[i:i + 2], 16) for i in (0, 2, 4))


def squircle_mask(size: int) -> Image.Image:
    big = size * 4
    mask = Image.new("L", (big, big), 0)
    half = big / 2
    points = []
    for step in range(720):
        angle = math.tau * step / 720
        c, s = math.cos(angle), math.sin(angle)
        points.append((half + half * math.copysign(abs(c) ** 0.5, c), half + half * math.copysign(abs(s) ** 0.5, s)))
    ImageDraw.Draw(mask).polygon(points, fill=255)
    return mask.resize((size, size), Image.LANCZOS)


def circle_mask(size: int) -> Image.Image:
    big = size * 4
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).ellipse((0, 0, big - 1, big - 1), fill=255)
    return mask.resize((size, size), Image.LANCZOS)


def launcher_preview(chrome: Chrome, foreground: Image.Image, monochrome: str, field: str) -> None:
    px = 4                       # pixels per dp
    canvas, visible = 108 * px, 72 * px
    inset = (canvas - visible) // 2
    page = (0x1C, 0x1E, 0x22)
    fg = foreground
    themes = [("#D7E3F7", "#1B3A5C"), ("#2B3A4D", "#C9E1FF")]   # themed-icon pairs (light, dark)
    layers = [(Image.new("RGBA", (canvas, canvas), hex_rgb(field) + (255,)), fg)]
    for background, ink in themes:
        mono = downsample(chrome.svg(vd_to_svg(monochrome, ink)), canvas)
        layers.append((Image.new("RGBA", (canvas, canvas), hex_rgb(background) + (255,)), mono))
    gap = 24
    sheet = Image.new("RGB", (gap + len(layers) * 3 * (canvas + gap), canvas + 2 * gap), page)
    x = gap
    for background, layer in layers:
        full = Image.alpha_composite(background, layer)
        # 1: whole 108dp canvas with the 66dp safe zone and the 72dp viewport drawn in
        guide = full.copy()
        draw = ImageDraw.Draw(guide)
        r = SAFE_ZONE_DP * px / 2
        draw.ellipse((canvas / 2 - r, canvas / 2 - r, canvas / 2 + r, canvas / 2 + r), outline=(220, 40, 40, 255), width=2)
        draw.rectangle((inset, inset, inset + visible, inset + visible), outline=(40, 110, 220, 255), width=2)
        sheet.paste(guide.convert("RGB"), (x, gap))
        x += canvas + gap
        # 2 and 3: the 72dp viewport under a circle and a squircle mask
        viewport = full.crop((inset, inset, inset + visible, inset + visible)).convert("RGB")
        for mask in (circle_mask(visible), squircle_mask(visible)):
            sheet.paste(viewport, (x + inset, gap + inset), mask)
            x += canvas + gap
    sheet.save(PREVIEW / "android-launcher.png")
    Image.alpha_composite(Image.new("RGBA", (canvas, canvas), page + (255,)),
                          downsample(chrome.svg(vd_to_svg(monochrome)), canvas)).convert("RGB").save(
        PREVIEW / "android-monochrome.png")


def windows_preview(frames: dict[int, Image.Image], sizes: tuple[int, ...] = (16, 20, 24, 32, 48),
                    name: str = "windows-ico.png",
                    variants: dict[str, dict[int, Image.Image]] | None = None) -> None:
    """Frames at 1:1 and enlarged, one band per (variant, taskbar colour)."""
    zoom_to = 192
    backgrounds = [("#F3F3F3", "light taskbar"), ("#202020", "dark taskbar")]
    variants = variants or {"": frames}
    bands = [(label, frames_of, colour) for label, frames_of in variants.items() for colour, _ in backgrounds]
    gap = 20
    row = zoom_to + 64 + 3 * gap
    sheet = Image.new("RGB", (gap + len(sizes) * (zoom_to + gap), len(bands) * row), (90, 90, 90))
    for band, (label, frames_of, colour) in enumerate(bands):
        dark = colour == "#202020"
        ImageDraw.Draw(sheet).rectangle((0, band * row, sheet.width, (band + 1) * row), fill=hex_rgb(colour))
        for index, image in enumerate(frames_of[size] for size in sizes):
            x = gap + index * (zoom_to + gap)
            y = band * row + gap
            backdrop = Image.new("RGBA", image.size, hex_rgb(colour) + (255,))
            flat = Image.alpha_composite(backdrop, image).convert("RGB")
            caption = f"{label} {image.width}px / 1:1".strip()
            ImageDraw.Draw(sheet).text((x + 64, y + 4), caption, fill=(210, 210, 210) if dark else (110, 110, 110))
            sheet.paste(flat, (x, y))                                         # 1:1
            scale = zoom_to // image.width
            sheet.paste(flat.resize((image.width * scale, image.height * scale), Image.NEAREST), (x, y + 64 + gap))
    sheet.save(PREVIEW / name)


# --------------------------------------------------------------------------- main

def main() -> None:
    previews = "--no-preview" not in sys.argv
    followups = "--followups" in sys.argv
    m = Master()
    OUT.mkdir(exist_ok=True)
    PREVIEW.mkdir(exist_ok=True)
    chrome = Chrome()
    try:
        tile, tight = tile_svg(m), tight_svg(m)
        write_text(OUT / "bubbleplay-tile.svg", tile)
        write_text(OUT / "bubbleplay-tight.svg", tight)
        tight_big = chrome.svg(tight)

        # Each ICO frame is the master tile rendered at its own size (the 16/20/24
        # frames keep the ivory field, as the approved small frames do).
        frames = {size: chrome.svg(tile, size) for size in ICO_SIZES}
        small = BRANDING / "small"
        small.mkdir(exist_ok=True)
        for size in ICO_SMALL:
            frames[size].save(small / f"{size}.png", optimize=True)
        write_ico(OUT / "bubbleplay.ico", [frames[size] for size in sorted(frames)])
        (OUT / "tray-32.bgra").write_bytes(premultiplied_bgra(downsample(tight_big, 32)))
        # The notification area gets the mark without its tile, cropped to fill each frame.
        tray = tray_svg(m, tight_big)
        tray_frames = {size: chrome.svg(tray, size) for size in TRAY_SIZES}
        write_ico(OUT / "bubbleplay-tray.ico", [tray_frames[size] for size in TRAY_SIZES])
        tray_dark = tray_dark_svg(m, tray)
        tray_dark_frames = {size: chrome.svg(tray_dark, size) for size in TRAY_SIZES}
        write_ico(OUT / "bubbleplay-tray-dark.ico", [tray_dark_frames[size] for size in TRAY_SIZES])

        if not followups:
            write_text(PUBLIC / "favicon.svg", tile)
            frames[32].save(PUBLIC / "favicon-32.png", optimize=True)

            foreground = launcher_foreground(m, chrome)
            foreground.save(RES / "drawable-xxxhdpi" / "ic_launcher_foreground.png", optimize=True)
            # The approved foreground is the PNG above; drop the vector it replaced so the two cannot disagree.
            (RES / "drawable" / "ic_launcher_foreground.xml").unlink(missing_ok=True)
            write_launcher_resources(m)
            # ic_launcher_monochrome.xml is not regenerated: that layer still carries the old art (design round).

        if previews:
            if not followups:
                monochrome = (RES / "drawable" / "ic_launcher_monochrome.xml").read_text(encoding="utf-8")
                launcher_preview(chrome, foreground, monochrome, m.field)
            windows_preview(frames)
            windows_preview(tray_frames, TRAY_SIZES, "windows-tray.png",
                            {"light icon": tray_frames, "dark icon": tray_dark_frames})
    finally:
        chrome.close()
    print("icons written")


if __name__ == "__main__":
    main()
