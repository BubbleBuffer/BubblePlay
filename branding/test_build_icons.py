"""Verify generated export contracts, including rendered Android safe-zone fit."""
import hashlib
import importlib.util
import unittest
from pathlib import Path

from PIL import Image

spec = importlib.util.spec_from_file_location("build_icons", Path(__file__).with_name("build-icons.py"))
icons = importlib.util.module_from_spec(spec)
spec.loader.exec_module(icons)


class IconExportsTest(unittest.TestCase):
    def test_ico_sizes_and_small_frame_transparency(self):
        with Image.open(icons.OUT / "bubbleplay.ico") as ico:
            self.assertEqual(ico.ico.sizes(), {(n, n) for n in (16, 20, 24, 32, 48, 64, 256)})
            for size in (16, 20, 24):
                image = ico.ico.getimage((size, size)).convert("RGBA")
                self.assertEqual(set(image.getchannel("A").getdata()), {0, 255})
                opaque = [pixel[:3] for pixel in image.getdata() if pixel[3] == 255]
                self.assertEqual(set(opaque), {icons.hex_rgb(colour) for colour in
                                              ("#282A2F", "#FFFAF1", "#F0A064", icons.MILK)})
                for colour in ("#282A2F", "#FFFAF1", "#F0A064"):
                    self.assertGreaterEqual(opaque.count(icons.hex_rgb(colour)), 3)
                with Image.open(icons.BRANDING / "small" / f"{size}.png") as png:
                    self.assertEqual(image.tobytes(), png.convert("RGBA").tobytes())

    def test_rendered_launcher_stays_in_safe_circle(self):
        master_hash = hashlib.sha256(icons.MASTER.read_bytes()).hexdigest()
        chrome = icons.Chrome()
        try:
            foreground = (icons.RES / "drawable/ic_launcher_foreground.xml").read_text()
            rendered = chrome.svg(icons.vd_to_svg(foreground), 1080)
            alpha = rendered.getchannel("A")
            for y in range(1080):
                for x in range(1080):
                    if alpha.getpixel((x, y)) > 8:
                        self.assertLessEqual((x + 0.5 - 540) ** 2 + (y + 0.5 - 540) ** 2, 330 ** 2)
            mono = (icons.RES / "drawable/ic_launcher_monochrome.xml").read_text()
            root = icons.ET.fromstring(mono)
            paints = {node.get(attr) for node in root.iter() for attr in
                      (icons.ANDROID + "fillColor", icons.ANDROID + "strokeColor") if node.get(attr)}
            self.assertEqual(paints, {"#FFFFFFFF"})
        finally:
            chrome.close()
        self.assertEqual(hashlib.sha256(icons.MASTER.read_bytes()).hexdigest(), master_hash)

    def test_relays_share_the_inline_mark(self):
        pattern = icons.re.compile(r"<!-- BRAND-MARK:start -->\s*(.*?)\s*<!-- BRAND-MARK:end -->", icons.re.S)
        worker = pattern.search((icons.PUBLIC / "login.html").read_text()).group(1)
        relay = pattern.search((icons.ROOT / "relay/auth.go").read_text()).group(1)
        self.assertEqual(worker, relay)
        self.assertIn('width="64"', worker)

    def test_login_identity_is_inline_and_shared(self):
        pattern = icons.re.compile(r"<!-- BRAND-IDENTITY:start -->\s*(.*?)\s*<!-- BRAND-IDENTITY:end -->", icons.re.S)
        pages = [(icons.PUBLIC / "login.html").read_text(), (icons.ROOT / "relay/auth.go").read_text()]
        identities = [pattern.search(page).group(1) for page in pages]
        self.assertEqual(identities[0], identities[1])
        container = icons.ET.fromstring(identities[0])
        root = container.find(f"{{{icons.SVG_NS}}}svg")
        self.assertEqual(root.get("width"), "26")
        self.assertEqual(root.get("aria-hidden"), "true")
        for page in pages:
            self.assertNotIn('d="M9 4H4v16h16v-5M15 4h5v5"', page)
        for node in root.iter():
            self.assertNotIn("style", node.attrib)
            self.assertNotIn(icons.local(node.tag), ("image", "script", "style"))


if __name__ == "__main__":
    unittest.main()
