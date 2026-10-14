//! Embedded fonts: Bricolage Grotesque (display), Atkinson Hyperlegible Next
//! (ui, ui-bold) and Fragment Mono (mono). All OFL; the licence texts sit
//! next to the files in src/ui/fonts/, and README.txt there records how the
//! static TTF instances were cut from the variable woff2 sources in
//! mockups/fonts/. egui 0.35 reads TTF/OTF only, so no woff2 is embedded.

use std::collections::BTreeMap;
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily, TextStyle};

use super::theme::{self, size};

static DISPLAY: &[u8] = include_bytes!("fonts/bricolage-grotesque-display-wdth86-wght650-opsz28.ttf");
static UI: &[u8] = include_bytes!("fonts/atkinson-hyperlegible-next-ui-wght450.ttf");
static UI_BOLD: &[u8] = include_bytes!("fonts/atkinson-hyperlegible-next-ui-bold-wght720.ttf");
static MONO: &[u8] = include_bytes!("fonts/fragment-mono.ttf");

/// Register the families named in theme.rs (display, ui, ui-bold, mono)
/// and make "ui" the default proportional family and "mono" the default
/// monospace family.
pub fn install(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();

    // egui's own fonts stay behind ours, so symbols and anything we do not
    // ship still render.
    let prop_fallback: Vec<String> = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mono_fallback: Vec<String> = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();

    let faces: [(&str, &'static [u8]); 4] = [
        (theme::FAMILY_DISPLAY, DISPLAY),
        (theme::FAMILY_UI, UI),
        (theme::FAMILY_UI_BOLD, UI_BOLD),
        (theme::FAMILY_MONO, MONO),
    ];
    for (name, bytes) in faces {
        defs.font_data.insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }

    let chain = |first: &str, fallback: &[String]| -> Vec<String> {
        let mut v = vec![first.to_owned()];
        v.extend(fallback.iter().cloned());
        v
    };

    for name in [theme::FAMILY_DISPLAY, theme::FAMILY_UI, theme::FAMILY_UI_BOLD] {
        defs.families.insert(FontFamily::Name(name.into()), chain(name, &prop_fallback));
    }
    defs.families.insert(FontFamily::Name(theme::FAMILY_MONO.into()), chain(theme::FAMILY_MONO, &mono_fallback));
    defs.families.insert(FontFamily::Proportional, chain(theme::FAMILY_UI, &prop_fallback));
    defs.families.insert(FontFamily::Monospace, chain(theme::FAMILY_MONO, &mono_fallback));

    ctx.set_fonts(defs);

    ctx.all_styles_mut(|style| {
        style.text_styles = BTreeMap::from([
            (TextStyle::Small, theme::body(size::SMALL)),
            (TextStyle::Body, theme::body(size::BODY)),
            (TextStyle::Button, theme::bold(size::BODY)),
            (TextStyle::Heading, theme::display(size::TITLE)),
            (TextStyle::Monospace, theme::mono(size::ID)),
        ]);
    });
}
