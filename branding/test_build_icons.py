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
        tile = icons.hex_rgb("#F2EBDD")
        with Image.open(icons.OUT / "bubbleplay.ico") as ico:
            self.assertEqual(ico.ico.sizes(), {(n, n) for n in (16, 20, 24, 32, 48, 64, 256)})
            for size in (16, 20, 24):
                image = ico.ico.getimage((size, size)).convert("RGBA")
                self.assertLessEqual(set(image.getchannel("A").getdata()), {0, 255})
                last = size - 1
                for corner in ((0, 0), (last, 0), (0, last), (last, last)):
                    self.assertIn(image.getpixel(corner)[3], (0, 255))
                # Charcoal, milk and apricot never reach a solid pixel at 16-24 px (nearest channel
                # distance is 20 or more), so only the ivory tile is asserted for these frames.
                opaque = [pixel[:3] for pixel in image.getdata() if pixel[3] == 255]
                near_tile = sum(max(abs(a - b) for a, b in zip(colour, tile)) <= 8 for colour in opaque)
                self.assertGreaterEqual(near_tile, 3 if size == 24 else 1)
                with Image.open(icons.BRANDING / "small" / f"{size}.png") as png:
                    self.assertEqual(image.tobytes(), png.convert("RGBA").tobytes())

    def test_rendered_launcher_stays_in_safe_circle(self):
        master_hash = hashlib.sha256(icons.MASTER.read_bytes()).hexdigest()
        with Image.open(icons.RES / "drawable-xxxhdpi/ic_launcher_foreground.png") as foreground:
            alpha = foreground.convert("RGBA").getchannel("A")
        width, height = alpha.size
        self.assertEqual(width, height)
        radius = 33 / 108 * width
        for index, value in enumerate(alpha.getdata()):
            if value > 8:
                x, y = index % width, index // width
                self.assertLessEqual((x + 0.5 - width / 2) ** 2 + (y + 0.5 - height / 2) ** 2, radius ** 2)
        mono = (icons.RES / "drawable/ic_launcher_monochrome.xml").read_text()
        root = icons.ET.fromstring(mono)
        paints = {node.get(attr) for node in root.iter() for attr in
                  (icons.ANDROID + "fillColor", icons.ANDROID + "strokeColor") if node.get(attr)}
        self.assertEqual(paints, {"#FFFFFFFF"})
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
