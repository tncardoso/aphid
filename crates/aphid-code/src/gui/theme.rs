//! The colors of the graphical interfaces, and the theme they install.
//!
//! Both front ends are built on `gpui-component`, which keeps one [`Theme`] in
//! a global. Its components read that global rather than the colors at each
//! call, so a dialog, a sidebar or a Markdown block comes out in the colors of
//! aphid only because this module put them there.
//!
//! The palette is the one the coding interface has always drawn with. It lives
//! here now so that the alate front end draws with the same one.

use gpui::{App, Hsla, Rgba, px, rgb, rgba};
use gpui_component::theme::{Theme, ThemeMode};

/// The page behind everything.
pub const BACKGROUND: u32 = 0x101419;
/// A surface that sits on the page: a drawer, a card, a side panel.
pub const PANEL: u32 = 0x171c22;
/// A surface that sits on a panel: a selected row, a button, a text box.
pub const PANEL_RAISED: u32 = 0x1e252d;
/// The line between two surfaces.
pub const BORDER: u32 = 0x303943;
/// Prose.
pub const TEXT: u32 = 0xd9e2ec;
/// Prose that is there to be read second: a timestamp, a token count.
pub const MUTED: u32 = 0x84919e;
/// The green of the wordmark. What the eye is meant to go to.
pub const ACCENT: u32 = 0x80c96b;
/// The bubble a person's own message is drawn in.
pub const USER: u32 = 0x243525;
/// A tool that failed, and an answer that refuses.
pub const DANGER: u32 = 0xe06c75;

/// Text on top of [`ACCENT`], which is bright enough to need dark ink.
const ON_ACCENT: u32 = 0x0d150d;
/// A link, and nothing else.
const LINK: u32 = 0x75a7e8;
/// What a modal lays over the window it covers.
const OVERLAY: u32 = 0x00000099;

/// Install the theme.
///
/// Call this once, at the start of the `App` closure and before any window
/// opens. It initializes `gpui-component` — which is what registers the
/// globals its dialogs, lists and text views need — and then writes the aphid
/// palette over the theme it chose.
///
/// The mode is forced to dark. The interfaces have one palette and no light
/// one, so following the appearance of the system would give a theme that is
/// half ours. Nothing in `gpui-component` reads the appearance again after
/// `init`, so what is written here stands for the life of the process.
pub fn init(cx: &mut App) {
    gpui_component::init(cx);
    Theme::change(ThemeMode::Dark, None, cx);
    apply(cx);
}

/// Write the palette over the theme in the global.
///
/// Separate from [`init`] because it is also the answer to anything that
/// re-applies a theme configuration: run this after it, and the colors are
/// ours again.
fn apply(cx: &mut App) {
    let theme = Theme::global_mut(cx);

    // The interfaces name a font by family and let the platform find it, which
    // is what the coding interface already did.
    theme.font_family = "system-ui".into();
    theme.mono_font_family = "monospace".into();
    theme.font_size = px(14.);
    theme.mono_font_size = px(13.);

    let colors = &mut theme.colors;

    colors.background = solid(BACKGROUND);
    colors.foreground = solid(TEXT);
    colors.border = solid(BORDER);
    colors.ring = solid(ACCENT);
    colors.caret = solid(ACCENT);
    colors.selection = solid(USER);
    colors.overlay = translucent(OVERLAY);

    // A panel is the one surface above the page, so everything that is a card,
    // a popover, a drawer or a bar takes the same color. Depth comes from the
    // border, not from a stack of five greys.
    colors.muted = solid(PANEL);
    colors.muted_foreground = solid(MUTED);
    colors.popover = solid(PANEL);
    colors.popover_foreground = solid(TEXT);
    colors.group_box = solid(PANEL);
    colors.group_box_foreground = solid(TEXT);
    colors.title_bar = solid(PANEL);
    colors.title_bar_border = solid(BORDER);
    colors.accordion = solid(PANEL);
    colors.accordion_hover = solid(PANEL_RAISED);

    colors.accent = solid(PANEL_RAISED);
    colors.accent_foreground = solid(TEXT);
    colors.secondary = solid(PANEL_RAISED);
    colors.secondary_hover = solid(BORDER);
    colors.secondary_active = solid(BORDER);
    colors.secondary_foreground = solid(TEXT);
    colors.input = solid(PANEL_RAISED);

    colors.primary = solid(ACCENT);
    colors.primary_hover = solid(ACCENT);
    colors.primary_active = solid(ACCENT);
    colors.primary_foreground = solid(ON_ACCENT);

    colors.danger = solid(DANGER);
    colors.danger_hover = solid(DANGER);
    colors.danger_active = solid(DANGER);
    colors.danger_foreground = solid(BACKGROUND);

    colors.link = solid(LINK);
    colors.link_hover = solid(LINK);
    colors.link_active = solid(LINK);

    colors.list = solid(BACKGROUND);
    colors.list_even = solid(BACKGROUND);
    colors.list_head = solid(PANEL);
    colors.list_hover = solid(PANEL);
    colors.list_active = solid(PANEL_RAISED);
    colors.list_active_border = solid(ACCENT);

    colors.sidebar = solid(PANEL);
    colors.sidebar_foreground = solid(TEXT);
    colors.sidebar_border = solid(BORDER);
    colors.sidebar_accent = solid(PANEL_RAISED);
    colors.sidebar_accent_foreground = solid(TEXT);
    colors.sidebar_primary = solid(ACCENT);
    colors.sidebar_primary_foreground = solid(ON_ACCENT);

    colors.tab_bar = solid(PANEL);
    colors.tab = solid(PANEL);
    colors.tab_foreground = solid(MUTED);
    colors.tab_active = solid(PANEL_RAISED);
    colors.tab_active_foreground = solid(TEXT);

    colors.scrollbar = translucent(0x00000000);
    colors.scrollbar_thumb = solid(BORDER);
    colors.scrollbar_thumb_hover = solid(MUTED);

    colors.skeleton = solid(PANEL_RAISED);
    colors.drag_border = solid(ACCENT);
    colors.drop_target = solid(PANEL_RAISED);
}

/// An `0xRRGGBB` constant as an opaque color.
fn solid(color: u32) -> Hsla {
    let color: Rgba = rgb(color);
    color.into()
}

/// An `0xRRGGBBAA` constant as a color that keeps its alpha.
fn translucent(color: u32) -> Hsla {
    let color: Rgba = rgba(color);
    color.into()
}
