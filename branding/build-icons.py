#!/usr/bin/env python3
"""Derive BubblePlay exports from the master mark and hinted small-frame geometry.

    python branding/build-icons.py            # exports + previews
    python branding/build-icons.py --no-preview
    python branding/build-icons.py --followups  # Windows + login only; no Android/web favicon writes

Master (never edited here): branding/bubbleplay-icon.svg
Needs: Python 3.10+, Pillow, and a Chrome/Chromium binary for rasterising
(set CHROME=<path> if it is not in a standard location).

Writes
  branding/out/bubbleplay.ico          16/20/24 hinted tight (no field), 32..256 tile
  branding/small/*.png                 exact native-pixel copies of the small ICO frames
  branding/out/tray-32.bgra            premultiplied BGRA candidate for future tray wiring
  branding/out/*.svg                   the derived variants the rasters come from
  worker/public/favicon.svg, favicon-32.png
  worker/public/login.html, relay/auth.go  inline login mark between BRAND-MARK markers
  android/.../values/colors.xml, mipmap-anydpi/ic_launcher.xml
  android/.../drawable/ic_launcher_foreground.xml, ic_launcher_monochrome.xml
  branding/preview/*.png               (unless --no-preview)
"""
from __future__ import annotations

import functools
import http.server
import io
import math
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import xml.etree.ElementTree as ET
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
BRANDING = ROOT / "branding"
MASTER = BRANDING / "bubbleplay-icon.svg"
OUT = BRANDING / "out"
PREVIEW = BRANDING / "preview"
PUBLIC = ROOT / "worker" / "public"
RES = ROOT / "android" / "app" / "src" / "main" / "res"

SVG_NS = "http://www.w3.org/2000/svg"
MILK = "#F3EFE7"
FLAT_EAR = "#DE945F"

# Derived-variant tuning (master units, viewBox 1280).
TILE_PADDING = 0.10      # free space each side of the composition on the tile
TILE_RADIUS = 0.22       # corner radius of the milk tile, fraction of its side
KEYLINE = 30             # milk keyline outside the charcoal outline, small sizes only
SAFE_ZONE_DP = 66        # Android adaptive icon safe zone (diameter) on a 108dp canvas
SAFE_ZONE_FILL = 0.97    # share of the safe zone the composition may use
MONO_GAP = 30            # gap between head outline and pointer in the monochrome layer
ICO_TIGHT = (16, 20, 24)
ICO_TILE = (32, 48, 64, 256)
SUPERSAMPLE = 1024

# The login page is served with `style-src 'self'`, so the inline mark can use
# presentation attributes only (no style attribute, no <style>).
LOGIN_MARK_PX = 64
LOGIN_MARK_GAP_PX = 18   # space under the mark, baked into its viewBox


# --------------------------------------------------------------------------- master

def local(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def fmt(value: float) -> str:
    return f"{value:.3f}".rstrip("0").rstrip(".")


def translation(node: ET.Element) -> tuple[float, float]:
    text = node.get("transform")
    if not text:
        return (0.0, 0.0)
    match = re.fullmatch(r"matrix\(1 0 0 1 (-?[\d.]+) (-?[\d.]+)\)", text.strip())
    if not match:
        raise SystemExit(f"unsupported transform on #{node.get('id')}: {text}")
    return (float(match[1]), float(match[2]))


def polyline(d: str) -> list[tuple[float, float]]:
    """Vertices of an M/L/H/V path (the outline and the pointer)."""
    points: list[tuple[float, float]] = []
    x = y = 0.0
    for command, args in re.findall(r"([MLHVZ])([^MLHVZ]*)", d):
        numbers = [float(n) for n in re.findall(r"-?[\d.]+", args)]
        if command in "ML":
            for i in range(0, len(numbers), 2):
                x, y = numbers[i], numbers[i + 1]
                points.append((x, y))
        elif command == "H":
            x = numbers[-1]
            points.append((x, y))
        elif command == "V":
            y = numbers[-1]
            points.append((x, y))
    return points


class Master:
    def __init__(self) -> None:
        root = ET.parse(MASTER).getroot()
        by_id = {node.get("id"): node for node in root.iter() if node.get("id")}
        self.by_id = by_id
        self.cat = by_id["cat"]
        self.pointer = by_id["pointer"]
        self.field = by_id["field"].get("fill")
        gradient = by_id["ear-highlight"]
        self.gradient_stops = [(float(s.get("offset")), s.get("stop-color")) for s in gradient]
        assert [gradient.get(k) for k in ("x1", "y1", "x2", "y2")] == ["0", "0", "1", "1"]
        self.cat_shift = translation(self.cat)
        self.outline = by_id["path-1"]
        self.outline_width = float(self.outline.get("stroke-width"))
        self.pointer_path = by_id["path-muqvh7oy-d-dd1e9a4f"]
        self.pointer_width = float(self.pointer_path.get("stroke-width"))
        # Bounding boxes in master space, strokes included.
        tx, ty = self.cat_shift
        xs, ys = zip(*polyline(self.outline.get("d")))
        half = self.outline_width / 2
        self.head_box = (min(xs) + tx - half, min(ys) + ty - half, max(xs) + tx + half, max(ys) + ty + half)
        xs, ys = zip(*polyline(self.pointer_path.get("d")))
        half = self.pointer_width / 2
        self.pointer_box = (min(xs) - half, min(ys) - half, max(xs) + half, max(ys) + half)
        self.box = (
            min(self.head_box[0], self.pointer_box[0]), min(self.head_box[1], self.pointer_box[1]),
            max(self.head_box[2], self.pointer_box[2]), max(self.head_box[3], self.pointer_box[3]),
        )

    # -- SVG serialisation (clean: no editor metadata, no data-name, no ids)
    def svg_node(self, node: ET.Element, flat_ear: bool = False) -> str:
        tag = local(node.tag)
        if tag == "g":
            inner = "".join(self.svg_node(child, flat_ear) for child in node)
            shift = translation(node)
            if shift == (0.0, 0.0):
                return inner
            return f'<g transform="translate({fmt(shift[0])} {fmt(shift[1])})">{inner}</g>'
        assert tag == "path", tag
        attrs = [f'd="{node.get("d")}"']
        shift = translation(node)
        if shift != (0.0, 0.0):
            attrs.append(f'transform="translate({fmt(shift[0])} {fmt(shift[1])})"')
        fill = node.get("fill", "none")
        if flat_ear and fill.startswith("url("):
            fill = FLAT_EAR
        attrs.append(f'fill="{fill}"')
        stroke = node.get("stroke", "none")
        if stroke != "none":
            attrs.append(f'stroke="{stroke}"')
            attrs.append(f'stroke-width="{fmt(float(node.get("stroke-width")))}"')
            for name in ("stroke-linecap", "stroke-linejoin"):
                value = node.get(name)
                if value and value not in ("butt", "miter"):
                    attrs.append(f'{name}="{value}"')
        return f"<path {' '.join(attrs)}/>"

    def defs(self) -> str:
        stops = "".join(f'<stop offset="{fmt(o)}" stop-color="{c}"/>' for o, c in self.gradient_stops)
        return f'<defs><linearGradient id="ear-highlight" x1="0" y1="0" x2="1" y2="1">{stops}</linearGradient></defs>'

    def artwork(self, pointer: bool = True, flat_ear: bool = False) -> str:
        body = self.svg_node(self.cat, flat_ear)
        if pointer:
            body += self.svg_node(self.pointer, flat_ear)
        return ("" if flat_ear else self.defs()) + body

    def keyline(self) -> str:
        tx, ty = self.cat_shift
        return (f'<path transform="translate({fmt(tx)} {fmt(ty)})" d="{self.outline.get("d")}" fill="none" '
                f'stroke="{MILK}" stroke-width="{fmt(self.outline_width + 2 * KEYLINE)}" stroke-linejoin="round"/>')


def svg_doc(view_box: tuple[float, float, float, float], body: str, extra: str = "") -> str:
    box = " ".join(fmt(v) for v in view_box)
    return f'<svg xmlns="{SVG_NS}" viewBox="{box}"{extra}>{body}</svg>\n'


def tile_geometry(m: Master) -> tuple[float, float, float]:
    width, height = m.box[2] - m.box[0], m.box[3] - m.box[1]
    side = max(width, height) / (1 - 2 * TILE_PADDING)
    return ((m.box[0] + m.box[2]) / 2 - side / 2, (m.box[1] + m.box[3]) / 2 - side / 2, side)


def tile_svg(m: Master, extra_bottom: float = 0.0, extra: str = "") -> str:
    """The mark on a rounded milk tile, cropped to the composition."""
    x, y, side = tile_geometry(m)
    radius = side * TILE_RADIUS
    field = (f'<rect x="{fmt(x)}" y="{fmt(y)}" width="{fmt(side)}" height="{fmt(side)}" '
             f'rx="{fmt(radius)}" fill="{m.field}"/>')
    return svg_doc((x, y, side, side + extra_bottom), field + m.artwork(), extra)


def tight_svg(m: Master) -> str:
    """Whole mark, no field, milk keyline: small taskbar sizes."""
    x0, y0, x1, y1 = m.box
    side = max(x1 - x0, y1 - y0) + 2 * KEYLINE
    view = ((x0 + x1) / 2 - side / 2, (y0 + y1) / 2 - side / 2, side, side)
    pointer_line = (f'<path d="{m.pointer_path.get("d")}" fill="none" stroke="{MILK}" '
                    f'stroke-width="{fmt(m.pointer_width + 2 * KEYLINE)}" stroke-linejoin="round"/>')
    return svg_doc(view, m.keyline() + pointer_line + m.artwork())


def bare_svg(m: Master) -> str:
    """Whole composition, no field, master viewBox (used for measuring)."""
    return svg_doc((0, 0, 1280, 1280), m.artwork())


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


def small_frame(size: int) -> Image.Image:
    """Hand-hinted native-pixel variants of the approved cat and pointer.

    Coordinates are pixel centres, drawn without antialiasing or resampling.
    Small-size optical corrections keep the eyes, smile and pointer separate;
    the lower grey facet and cheek blush are omitted at notification-area sizes.
    The one-pixel opaque milk keyline provides contrast on dark taskbars.
    """
    geometry = {
        16: {
            "head": [(1, 4), (4, 1), (5, 1), (7, 3), (9, 1), (10, 1), (12, 4), (12, 8), (10, 11), (3, 11), (1, 9)],
            "left": [(2, 4), (4, 2), (5, 2), (7, 4)],
            "inner": [(3, 4), (4, 3), (5, 3), (6, 4)],
            "right": [(8, 3), (9, 2), (10, 2), (11, 3), (11, 4)],
            "eyes": [[(3, 6), (4, 5), (5, 6)], [(8, 6), (9, 5), (10, 6)]],
            "nose": (7, 7), "mouth": [(5, 8), (6, 9), (7, 8), (8, 9), (9, 8)],
            "pointer": [(10, 8), (10, 14), (12, 12), (14, 12)],
        },
        20: {
            "head": [(2, 5), (5, 1), (6, 1), (9, 4), (12, 1), (13, 1), (15, 5), (15, 10), (13, 14), (4, 14), (2, 12)],
            "left": [(3, 5), (5, 2), (6, 2), (8, 5)],
            "inner": [(4, 5), (5, 3), (6, 3), (7, 5)],
            "right": [(10, 4), (12, 2), (13, 2), (14, 5)],
            "eyes": [[(4, 8), (6, 6), (8, 8)], [(10, 8), (12, 6), (14, 8)]],
            "nose": (9, 9), "mouth": [(6, 10), (6, 11), (7, 12), (8, 12), (9, 11), (10, 12), (11, 12), (12, 11), (12, 10)],
            "pointer": [(13, 11), (13, 18), (15, 16), (18, 16)],
        },
        24: {
            "head": [(2, 6), (6, 1), (7, 1), (10, 5), (14, 1), (15, 1), (18, 6), (18, 12), (15, 17), (5, 17), (2, 14)],
            "left": [(3, 6), (6, 2), (7, 2), (9, 6)],
            "inner": [(4, 6), (6, 3), (7, 3), (8, 6)],
            "right": [(11, 5), (14, 2), (15, 2), (17, 6)],
            "eyes": [[(5, 9), (7, 7), (8, 7), (10, 9)], [(12, 9), (14, 7), (15, 7), (17, 9)]],
            "nose": (11, 11), "mouth": [(8, 12), (8, 13), (9, 14), (10, 14), (11, 13), (12, 14), (13, 14), (14, 13), (14, 12)],
            "pointer": [(15, 13), (15, 22), (18, 19), (22, 19)],
        },
    }[size]
    image = Image.new("RGBA", (size, size))
    draw = ImageDraw.Draw(image)
    ink, face, apricot = "#282A2F", "#FFFAF1", "#F0A064"
    draw.polygon(geometry["head"], fill=face, outline=ink)
    draw.polygon(geometry["left"], fill=ink)
    draw.polygon(geometry["inner"], fill=face)
    draw.polygon(geometry["right"], fill=apricot)
    for eye in geometry["eyes"]:
        draw.line(eye, fill=ink, width=1)
    draw.point(geometry["nose"], fill=apricot)
    draw.line(geometry["mouth"], fill=ink, width=1)
    draw.polygon(geometry["pointer"], fill=apricot, outline=ink)
    keyline = Image.new("RGBA", image.size, MILK)
    keyline.putalpha(image.getchannel("A").filter(ImageFilter.MaxFilter(3)))
    return Image.alpha_composite(keyline, image)


# --------------------------------------------------------------------------- Android

def argb(colour: str) -> str:
    return "#FF" + colour.lstrip("#").upper()


def enclosing_circle(image: Image.Image, units_per_px: float) -> tuple[float, float, float]:
    """Approximate minimum enclosing circle of the opaque pixels (Badoiu-Clarkson)."""
    alpha = image.getchannel("A")
    width, height = alpha.size
    data = alpha.load()
    points = []
    for y in range(height):
        row = [x for x in range(width) if data[x, y] > 8]
        if row:
            points += [(row[0], y), (row[-1] + 1, y), (row[0], y + 1), (row[-1] + 1, y + 1)]
    cx = sum(p[0] for p in points) / len(points)
    cy = sum(p[1] for p in points) / len(points)
    for i in range(1, 4000):
        fx, fy = max(points, key=lambda p: (p[0] - cx) ** 2 + (p[1] - cy) ** 2)
        cx += (fx - cx) / (i + 1)
        cy += (fy - cy) / (i + 1)
    radius = max(math.hypot(p[0] - cx, p[1] - cy) for p in points)
    return (cx * units_per_px, cy * units_per_px, radius * units_per_px)


def vd_header(comment: str) -> str:
    return ('<?xml version="1.0" encoding="utf-8"?>\n'
            f"<!-- {comment}\n     Generated by branding/build-icons.py from branding/bubbleplay-icon.svg; do not edit by hand. -->\n"
            '<vector xmlns:android="http://schemas.android.com/apk/res/android"\n'
            '    xmlns:aapt="http://schemas.android.com/aapt"\n'
            '    android:width="108dp"\n    android:height="108dp"\n'
            '    android:viewportWidth="108"\n    android:viewportHeight="108">\n')


def vd_group_open(indent: str, scale: float | None = None, shift: tuple[float, float] = (0, 0)) -> str:
    attrs = []
    if scale is not None:
        attrs += [f'android:scaleX="{scale:.6f}"', f'android:scaleY="{scale:.6f}"']
    attrs += [f'android:translateX="{fmt(shift[0])}"', f'android:translateY="{fmt(shift[1])}"']
    return f"{indent}<group\n" + "\n".join(f"{indent}    {a}" for a in attrs) + ">\n"


def user_space_gradient(points: list[tuple[float, float]]) -> tuple[float, float, float, float]:
    """SVG objectBoundingBox gradient (0,0)->(1,1) as user-space start/end.

    Its colour bands run parallel to the box's other diagonal, so the
    user-space vector is the box diagonal projected on that band normal.
    """
    xs, ys = zip(*points)
    x0, y0, w, h = min(xs), min(ys), max(xs) - min(xs), max(ys) - min(ys)
    k = 2 * w * h / (w * w + h * h)
    return (x0, y0, x0 + h * k, y0 + w * k)


def vd_node(m: Master, node: ET.Element, indent: str) -> str:
    tag = local(node.tag)
    shift = translation(node)
    if tag == "g":
        inner_indent = indent + "    " if shift != (0.0, 0.0) else indent
        inner = "".join(vd_node(m, child, inner_indent) for child in node)
        if shift == (0.0, 0.0):
            return inner
        return vd_group_open(indent, None, shift) + inner + f"{indent}</group>\n"
    if shift != (0.0, 0.0):
        clone = ET.Element(node.tag, {k: v for k, v in node.attrib.items() if k != "transform"})
        return vd_group_open(indent, None, shift) + vd_node(m, clone, indent + "    ") + f"{indent}</group>\n"
    attrs = [f'android:pathData="{node.get("d")}"']
    fill = node.get("fill", "none")
    gradient = ""
    if fill.startswith("url("):
        x0, y0, x1, y1 = user_space_gradient(polyline(node.get("d")))
        items = "".join(f'{indent}                <item android:offset="{fmt(o)}" android:color="{argb(c)}" />\n'
                        for o, c in m.gradient_stops)
        gradient = (f'{indent}    <aapt:attr name="android:fillColor">\n'
                    f'{indent}        <gradient\n{indent}            android:type="linear"\n'
                    f'{indent}            android:startX="{fmt(x0)}"\n{indent}            android:startY="{fmt(y0)}"\n'
                    f'{indent}            android:endX="{fmt(x1)}"\n{indent}            android:endY="{fmt(y1)}">\n'
                    f'{items}{indent}        </gradient>\n{indent}    </aapt:attr>\n')
    elif fill != "none":
        attrs.append(f'android:fillColor="{argb(fill)}"')
    stroke = node.get("stroke", "none")
    if stroke != "none":
        attrs.append(f'android:strokeColor="{argb(stroke)}"')
        attrs.append(f'android:strokeWidth="{fmt(float(node.get("stroke-width")))}"')
        for name, attr in (("stroke-linecap", "strokeLineCap"), ("stroke-linejoin", "strokeLineJoin")):
            value = node.get(name)
            if value and value not in ("butt", "miter"):
                attrs.append(f'android:{attr}="{value}"')
    body = f"{indent}<path\n" + "\n".join(f"{indent}    {a}" for a in attrs)
    return body + (f">\n{gradient}{indent}</path>\n" if gradient else " />\n")


def launcher_transform(m: Master, chrome: Chrome) -> tuple[float, tuple[float, float]]:
    measure = 640
    cx, cy, radius = enclosing_circle(downsample(chrome.svg(bare_svg(m)), measure), 1280 / measure)
    scale = SAFE_ZONE_DP * SAFE_ZONE_FILL / (2 * radius)
    return scale, (54 - cx * scale, 54 - cy * scale)


def foreground_xml(m: Master, scale: float, shift: tuple[float, float]) -> str:
    return (vd_header("Adaptive icon foreground: the BubblePlay mark inside the 66dp safe zone.")
            + vd_group_open("    ", scale, shift)
            + vd_node(m, m.cat, "        ") + vd_node(m, m.pointer, "        ")
            + "    </group>\n</vector>\n")


def monochrome_xml(m: Master, scale: float, shift: tuple[float, float]) -> str:
    """Single-colour layer: head outline (opened where the pointer sits), dark
    ear with its inset as a hole, eyes, nose, smile, solid pointer."""
    ink = "#FFFFFFFF"
    indent = "            "
    points = polyline(m.outline.get("d"))
    tx, ty = m.cat_shift
    pointer = polyline(m.pointer_path.get("d"))
    pointer_left = min(p[0] for p in pointer) - tx          # in cat space
    bottom_end = pointer_left - m.pointer_width / 2 - MONO_GAP
    first, last = points[0], points[-1]
    start = (first[0] + (last[0] - first[0]) * 0.12, first[1] + (last[1] - first[1]) * 0.12)
    outline = f"M{fmt(start[0])} {fmt(start[1])}" + "".join(f"L{fmt(x)} {fmt(y)}" for x, y in points[:-1])
    outline += f"L{fmt(bottom_end)} {fmt(last[1])}"

    def stroke(d: str, width: float, cap: str, join: str) -> str:
        lines = [f'android:pathData="{d}"', f'android:strokeColor="{ink}"', f'android:strokeWidth="{fmt(width)}"']
        if cap != "butt":
            lines.append(f'android:strokeLineCap="{cap}"')
        if join != "miter":
            lines.append(f'android:strokeLineJoin="{join}"')
        return f"{indent}<path\n" + "\n".join(f"{indent}    {line}" for line in lines) + " />\n"

    def from_master(element_id: str) -> str:
        node = m.by_id[element_id]
        return stroke(node.get("d"), float(node.get("stroke-width")),
                      node.get("stroke-linecap", "butt"), node.get("stroke-linejoin", "miter"))

    ear = m.by_id["ear-left"].get("d") + m.by_id["ear-inset-left"].get("d")
    body = stroke(outline, m.outline_width, "butt", "bevel")
    body += f'{indent}<path\n{indent}    android:pathData="{ear}"\n{indent}    android:fillColor="{ink}"\n{indent}    android:fillType="evenOdd" />\n'
    body += from_master("eye-left") + from_master("eye-right") + from_master("smile")
    body += f'{indent}<path\n{indent}    android:pathData="{m.by_id["nose"].get("d")}"\n{indent}    android:fillColor="{ink}" />\n'
    pointer_d = m.pointer_path.get("d")
    pointer_xml = (f'        <path\n            android:pathData="{pointer_d}"\n            android:fillColor="{ink}"\n'
                   f'            android:strokeColor="{ink}"\n            android:strokeWidth="{fmt(m.pointer_width)}"\n'
                   f'            android:strokeLineJoin="round" />\n')
    return (vd_header("Themed-icon layer: one colour, tinted by the launcher.")
            + vd_group_open("    ", scale, shift)
            + vd_group_open("        ", None, m.cat_shift) + body + "        </group>\n"
            + pointer_xml + "    </group>\n</vector>\n")


ANDROID = "{http://schemas.android.com/apk/res/android}"
AAPT = "{http://schemas.android.com/aapt}"


def vd_to_svg(xml: str, colour: str | None = None) -> str:
    """Render-check helper: turn the generated vector drawable back into SVG,
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


# --------------------------------------------------------------------------- web

def write_login_mark(m: Master) -> None:
    _, _, side = tile_geometry(m)
    gap = side * LOGIN_MARK_GAP_PX / LOGIN_MARK_PX
    extra = (f' width="{LOGIN_MARK_PX}" height="{LOGIN_MARK_PX + LOGIN_MARK_GAP_PX}"'
             ' role="img" aria-label="BubblePlay mark"')
    mark = tile_svg(m, gap, extra).strip()
    pattern = re.compile(r"(<!-- BRAND-MARK:start[^>]*-->\n).*?([ \t]*<!-- BRAND-MARK:end -->)", re.S)
    identity = tile_svg(m, extra=' width="26" height="26" aria-hidden="true"').strip()
    # Retain the established icon container. Its SVG stroke rule must not leak
    # into filled artwork; this inner group resets inheritance without CSP styles.
    identity = identity.replace('aria-hidden="true">', 'aria-hidden="true"><g stroke="none">', 1)
    identity = identity.replace("</svg>", "</g></svg>")
    # Both marks live in the same document; their gradient IDs must be distinct.
    identity = identity.replace("ear-highlight", "identity-ear-highlight")
    identity_pattern = re.compile(
        r"[ \t]*(?:<!-- BRAND-IDENTITY:start -->\s*.*?<!-- BRAND-IDENTITY:end -->|"
        r'<span class="signal-cut" aria-hidden="true">\s*<svg\b.*?</svg>\s*</span>)', re.S)
    for path in (PUBLIC / "login.html", ROOT / "relay" / "auth.go"):
        html = path.read_text(encoding="utf-8")
        if not pattern.search(html):
            raise SystemExit(f"{path} has no BRAND-MARK markers")
        html = pattern.sub(lambda match: f"{match[1]}    {mark}\n{match[2]}", html)
        html, count = identity_pattern.subn(
            lambda _: f'    <!-- BRAND-IDENTITY:start -->\n    <span class="signal-cut" aria-hidden="true">{identity}</span>\n    <!-- BRAND-IDENTITY:end -->', html)
        if count != 1:
            raise SystemExit(f"{path} must have one brand identity")
        path.write_text(html, encoding="utf-8", newline="\n")


def write_launcher_resources() -> None:
    path = RES / "values" / "colors.xml"
    colours = path.read_text(encoding="utf-8")
    colours, count = re.subn(r'(<color name="launcher_background">)[^<]*(</color>)',
                             rf'\g<1>{MILK}\g<2>', colours)
    if count != 1:
        raise SystemExit("expected one launcher_background colour")
    path.write_text(colours, encoding="utf-8", newline="\n")
    (RES / "mipmap-anydpi" / "ic_launcher.xml").write_text(
        '<?xml version="1.0" encoding="utf-8"?>\n'
        '<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">\n'
        '    <background android:drawable="@color/launcher_background" />\n'
        '    <foreground android:drawable="@drawable/ic_launcher_foreground" />\n'
        '    <monochrome android:drawable="@drawable/ic_launcher_monochrome" />\n'
        '</adaptive-icon>\n', encoding="utf-8", newline="\n")


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


def launcher_preview(chrome: Chrome, foreground: str, monochrome: str) -> None:
    px = 4                       # pixels per dp
    canvas, visible = 108 * px, 72 * px
    inset = (canvas - visible) // 2
    page = (0x1C, 0x1E, 0x22)
    fg = downsample(chrome.svg(vd_to_svg(foreground)), canvas)
    themes = [("#D7E3F7", "#1B3A5C"), ("#2B3A4D", "#C9E1FF")]   # themed-icon pairs (light, dark)
    layers = [(Image.new("RGBA", (canvas, canvas), hex_rgb(MILK) + (255,)), fg)]
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


def windows_preview(frames: dict[int, Image.Image]) -> None:
    zoom_to = 192
    backgrounds = [("#F3F3F3", "light taskbar"), ("#202020", "dark taskbar")]
    items = [frames[size] for size in (16, 20, 24, 32, 48)]
    gap = 20
    row = zoom_to + 64 + 3 * gap
    sheet = Image.new("RGB", (gap + len(items) * (zoom_to + gap), 2 * row), (90, 90, 90))
    for band, (colour, _) in enumerate(backgrounds):
        ImageDraw.Draw(sheet).rectangle((0, band * row, sheet.width, (band + 1) * row), fill=hex_rgb(colour))
        for index, image in enumerate(items):
            x = gap + index * (zoom_to + gap)
            y = band * row + gap
            backdrop = Image.new("RGBA", image.size, hex_rgb(colour) + (255,))
            flat = Image.alpha_composite(backdrop, image).convert("RGB")
            ImageDraw.Draw(sheet).text((x + 64, y + 4), f"{image.width}px / 1:1", fill=(110, 110, 110) if band == 0 else (210, 210, 210))
            sheet.paste(flat, (x, y))                                         # 1:1
            scale = zoom_to // image.width
            sheet.paste(flat.resize((image.width * scale, image.height * scale), Image.NEAREST), (x, y + 64 + gap))
    sheet.save(PREVIEW / "windows-ico.png")


LOGIN_CSP = ("default-src 'none'; img-src 'self'; style-src 'self'; font-src 'self'; script-src 'unsafe-inline'; "
             "connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'")


class LoginHandler(http.server.SimpleHTTPRequestHandler):
    """Serves worker/public the way the relay serves the login page: `/` is
    login.html with the gate's CSP (copied from worker/src/gate.js)."""

    def log_message(self, *args) -> None:
        pass

    def do_GET(self) -> None:
        if self.path.split("?")[0] == "/__auth/methods":
            body = b'{"totp":false,"approval":["host"],"hostOnline":true}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path.split("?")[0] == "/go-login":
            source = (ROOT / "relay" / "auth.go").read_text(encoding="utf-8")
            page = source.split("const relayLoginPage = `", 1)[1].split("`", 1)[0]
            page = page.replace("{{NONCE}}", "branding-preview").replace("{{FACTOR}}", "").replace("{{APPROVAL}}", "")
            body = page.encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Security-Policy", LOGIN_CSP.replace("style-src 'self'", "style-src 'self' 'nonce-branding-preview'").replace("script-src 'unsafe-inline'", "script-src 'nonce-branding-preview'"))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path.split("?")[0] == "/":
            body = (PUBLIC / "login.html").read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Security-Policy", LOGIN_CSP)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        super().do_GET()


def login_preview(chrome: Chrome) -> None:
    handler = functools.partial(LoginHandler, directory=str(PUBLIC))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        url = f"http://127.0.0.1:{server.server_address[1]}/"
        chrome.screenshot(url, 1280, 800, False).convert("RGB").save(PREVIEW / "login-desktop.png")
        chrome.screenshot(url, 500, 900, False).convert("RGB").save(PREVIEW / "login-narrow.png")
        chrome.screenshot(url + "go-login", 1280, 800, False).convert("RGB").save(PREVIEW / "login-go-desktop.png")
        chrome.screenshot(url + "go-login", 500, 900, False).convert("RGB").save(PREVIEW / "login-go-narrow.png")
    finally:
        server.shutdown()


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
        (OUT / "bubbleplay-tile.svg").write_text(tile, encoding="utf-8", newline="\n")
        (OUT / "bubbleplay-tight.svg").write_text(tight, encoding="utf-8", newline="\n")
        tile_big, tight_big = chrome.svg(tile), chrome.svg(tight)

        frames = {size: small_frame(size) for size in ICO_TIGHT}
        small = BRANDING / "small"
        small.mkdir(exist_ok=True)
        for size, image in frames.items():
            image.save(small / f"{size}.png", optimize=True)
        frames |= {size: downsample(tile_big, size) for size in ICO_TILE}
        write_ico(OUT / "bubbleplay.ico", [frames[size] for size in sorted(frames)])
        tray = downsample(tight_big, 32)
        (OUT / "tray-32.bgra").write_bytes(premultiplied_bgra(tray))

        if not followups:
            (PUBLIC / "favicon.svg").write_text(tile, encoding="utf-8", newline="\n")
            frames[32].save(PUBLIC / "favicon-32.png", optimize=True)
        write_login_mark(m)

        if not followups:
            scale, shift = launcher_transform(m, chrome)
            foreground, monochrome = foreground_xml(m, scale, shift), monochrome_xml(m, scale, shift)
            (RES / "drawable" / "ic_launcher_foreground.xml").write_text(foreground, encoding="utf-8", newline="\n")
            (RES / "drawable" / "ic_launcher_monochrome.xml").write_text(monochrome, encoding="utf-8", newline="\n")
            write_launcher_resources()
            print(f"launcher scale {scale:.5f}, head width {(m.head_box[2] - m.head_box[0]) * scale:.1f}dp")

        if previews:
            frames[48] = frames.get(48) or downsample(tile_big, 48)
            if not followups:
                launcher_preview(chrome, foreground, monochrome)
            windows_preview(frames)
            login_preview(chrome)
    finally:
        chrome.close()
    print("icons written")


if __name__ == "__main__":
    main()
