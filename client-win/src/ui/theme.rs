//! BubbleBuffer tokens for the native viewer, taken from
//! client-win/mockups/mockup.css. Change values here, not at call sites.

use egui::{Color32, CornerRadius, FontFamily, FontId};

// ------------------------------------------------------------ fields
/// Calico black: behind the video, letterbox.
pub const VOID: Color32 = Color32::from_rgb(0x10, 0x12, 0x14);
/// App canvas.
pub const FIELD: Color32 = Color32::from_rgb(0x26, 0x28, 0x2D);
/// Duvet: panels, strip.
pub const SURFACE: Color32 = Color32::from_rgb(0x2F, 0x31, 0x38);
/// Rows, inputs.
pub const SURFACE_RAISED: Color32 = Color32::from_rgb(0x30, 0x33, 0x3A);
/// Hover, selected row.
pub const SURFACE_MUTED: Color32 = Color32::from_rgb(0x3B, 0x3E, 0x45);
/// Step rail on flow screens (near black).
pub const RAIL: Color32 = Color32::from_rgb(0x1B, 0x1D, 0x21);

// ------------------------------------------------------------ ink
pub const INK: Color32 = Color32::from_rgb(0xF3, 0xEF, 0xE7);
pub const INK_MUTED: Color32 = Color32::from_rgb(0xC6, 0xBA, 0xAC);
pub const INK_FAINT: Color32 = Color32::from_rgb(0x99, 0x9D, 0xA6);

// ------------------------------------------------------------ threads
pub const THREAD: Color32 = Color32::from_rgb(0x3E, 0x41, 0x48);
pub const THREAD_STRONG: Color32 = Color32::from_rgb(0x57, 0x5B, 0x64);

// ------------------------------------------------------------ accents
/// Bright apricot: the one main action, live signal, selection marker.
pub const SIGNAL: Color32 = Color32::from_rgb(0xF0, 0xA0, 0x64);
pub const SIGNAL_PRESS: Color32 = Color32::from_rgb(0xDE, 0x94, 0x5F);
/// Ink on filled apricot.
pub const SIGNAL_INK: Color32 = Color32::from_rgb(0x28, 0x2A, 0x2F);
/// Recovery seam (reconnecting).
pub const PAW: Color32 = Color32::from_rgb(0xDD, 0xA3, 0x9D);
/// Grounding: hosting, switch checked.
pub const MOSS: Color32 = Color32::from_rgb(0x69, 0x7A, 0x6B);

// ------------------------------------------------------------ semantic
pub const CONSTRUCTIVE: Color32 = Color32::from_rgb(0xCA, 0xD8, 0xCD);
pub const INFORMATION: Color32 = Color32::from_rgb(0xCB, 0xD5, 0xDF);
pub const ATTENTION: Color32 = Color32::from_rgb(0xF0, 0xD2, 0x9C);
pub const DANGER: Color32 = Color32::from_rgb(0xE5, 0xB1, 0xAA);
pub const FOCUS: Color32 = Color32::from_rgb(0x7A, 0xB7, 0xFF);

// ------------------------------------------------------------ chrome over video
/// rgba(38, 40, 45, 0.96)
pub const CHROME_BG: Color32 = Color32::from_rgba_premultiplied(36, 38, 43, 245);
/// Buffer seam shadow: 0 4px 12px rgba(0,0,0,.35).
pub const CHROME_SHADOW: egui::Shadow = egui::Shadow {
    offset: [0, 4],
    blur: 12,
    spread: 0,
    color: Color32::from_black_alpha(89),
};

// ------------------------------------------------------------ shape
// CSS order is top-left, top-right, bottom-right, bottom-left;
// egui is nw, ne, sw, se.
pub const R_COMPACT: CornerRadius = CornerRadius::same(5);
/// Fitted control: 7 11 6 9.
pub const R_CONTROL: CornerRadius = CornerRadius { nw: 7, ne: 11, se: 6, sw: 9 };
/// Fitted field: 6 10 6 8.
pub const R_FIELD: CornerRadius = CornerRadius { nw: 6, ne: 10, se: 6, sw: 8 };
/// Fitted panel: 12 18 10 16.
pub const R_PANEL: CornerRadius = CornerRadius { nw: 12, ne: 18, se: 10, sw: 16 };
/// Edge-attached strip: square at the window edge, 0 0 10 12.
pub const R_STRIP: CornerRadius = CornerRadius { nw: 0, ne: 0, se: 10, sw: 12 };
/// Status pills are the only round shapes.
pub const R_PILL: CornerRadius = CornerRadius::same(255);

// ------------------------------------------------------------ spacing (4 px base)
pub const SP2: f32 = 8.0;
pub const SP3: f32 = 12.0;

// ------------------------------------------------------------ sizes
pub const WINDOW_W: f32 = 1100.0;
pub const WINDOW_H: f32 = 720.0;
pub const TITLEBAR_H: f32 = 36.0;
/// Session strip height (the browser's is 32).
pub const STRIP_H: f32 = 42.0;
/// Collapsed session tab height.
pub const TAB_H: f32 = 26.0;
pub const CONTROL_H: f32 = 36.0;
/// Small buttons: inline row actions. 32 px is the smallest hit target.
pub const CONTROL_SM_H: f32 = 32.0;
/// One line of UI text, and the minimum height of a row of widgets.
pub const LINE_H: f32 = 20.0;
pub const CONTROL_LG_H: f32 = 44.0;
pub const FIELD_H: f32 = 40.0;
pub const RAIL_W: f32 = 300.0;
pub const DETAIL_W: f32 = 400.0;
pub const PANE_MAX_W: f32 = 680.0;
pub const ICON_SM: f32 = 16.0;
pub const ICON: f32 = 18.0;
pub const ICON_LG: f32 = 22.0;

// ------------------------------------------------------------ motion (ms)
pub const T_FAST: f32 = 0.120;

// ------------------------------------------------------------ type
// Families registered by fonts.rs. Bricolage is a static instance at
// wdth 86 / wght 650; Atkinson comes as regular (450) and bold (720).
pub const FAMILY_DISPLAY: &str = "display";
pub const FAMILY_UI: &str = "ui";
pub const FAMILY_UI_BOLD: &str = "ui-bold";
pub const FAMILY_MONO: &str = "mono";

pub fn display(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY_DISPLAY.into()))
}
pub fn body(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY_UI.into()))
}
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY_UI_BOLD.into()))
}
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY_MONO.into()))
}

/// Type scale (px at 1x).
pub mod size {
    /// Panel name (Bricolage 36/0.95).
    pub const PANEL_NAME: f32 = 36.0;
    /// Screen titles (Bricolage 28/1.05).
    pub const TITLE: f32 = 28.0;
    /// "Your PCs" list heading.
    pub const LIST_TITLE: f32 = 30.0;
    pub const ROW_NAME: f32 = 15.0;
    pub const BODY: f32 = 14.0;
    pub const LEAD: f32 = 15.0;
    pub const LABEL: f32 = 13.0;
    pub const SMALL: f32 = 12.0;
    pub const HINT: f32 = 11.5;
    pub const ID: f32 = 12.5;
    pub const ID_LG: f32 = 15.0;
    /// 6-digit comparison number.
    pub const NUMBER: f32 = 40.0;
}
