Fonts embedded by src/ui/fonts.rs
==================================

Sources (same files as worker/public/fonts, copied to client-win/mockups/fonts/):
  atkinson-hyperlegible-next.woff2   variable, wght 200-800 (default 400)
  bricolage-grotesque.woff2          variable, opsz 12-96 (default 96), wght 200-800 (default 800), wdth 75-100 (default 100)
  fragment-mono.woff2                static, weight 400

egui 0.35 (epaint, skrifa) reads TTF/OTF only, so each face is a static
instance cut with fontTools (varLib.instancer), then re-saved as plain sfnt
(the instancer keeps the woff2 flavour otherwise, which egui cannot read).

Instances in this folder:
  atkinson-hyperlegible-next-ui-wght450.ttf            ui       wght 450
  atkinson-hyperlegible-next-ui-bold-wght720.ttf       ui-bold  wght 720
  bricolage-grotesque-display-wdth86-wght650-opsz28.ttf display wdth 86, wght 650, opsz 28
  fragment-mono.ttf                                     mono     as is (woff2 converted to TTF)

Commands (run in the scratchpad venv with fonttools and brotli installed,
from the mockups/fonts folder, output here):

  python -m fontTools.varLib.instancer atkinson-hyperlegible-next.woff2 wght=450 -o atkinson-hyperlegible-next-ui-wght450.ttf
  python -m fontTools.varLib.instancer atkinson-hyperlegible-next.woff2 wght=720 -o atkinson-hyperlegible-next-ui-bold-wght720.ttf
  python -m fontTools.varLib.instancer bricolage-grotesque.woff2 wght=650 wdth=86 opsz=28 -o bricolage-grotesque-display-wdth86-wght650-opsz28.ttf
  # then, for each instance: load with TTFont(path), set flavor = None, save(path)
  # fragment-mono: TTFont('fragment-mono.woff2'), flavor = None, save('fragment-mono.ttf')

Notes:
  - Bricolage has an opsz axis that the mockups do not set. The browser's
    automatic optical sizing would use the font-size in px (28 for screen
    titles, 36 for panel names). The instance pins opsz 28 for the heading
    style. Panel names at 36 px use the same instance.
  - The instancer's --update-name-table option fails on these fonts, so the
    name table keeps the variable font's names. The Bricolage name table
    still reads "Bricolage Grotesque 96pt ExtraBold". egui does not use the
    names.
  - Licences: OFL-atkinson.txt, OFL-bricolage.txt, OFL-fragment.txt (copies
    of client-win/mockups/fonts/OFL-*.txt).
