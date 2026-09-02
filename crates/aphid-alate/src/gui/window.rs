//! Where the window sits, and how big it is.
//!
//! GPUI sets a window's origin only when it is created: `WindowBounds` carries
//! one, and `Window::resize` changes the size and never the place. That single
//! fact shapes both modes.
//!
//! The quake console therefore grows **downwards from a fixed origin at the top
//! of the screen**, so expanding and collapsing it is a resize and the window
//! stays where it was. Changing *mode* is the one thing that has to close the
//! window and open another — which is what the desktop companion this borrows
//! from does too. The connection survives it, because the connection is held by
//! the model and not by the window.

use gpui::{
    Bounds, Pixels, Point, Size, TitlebarOptions, WindowBounds, WindowKind, WindowOptions, px,
};

use super::config::Mode;

/// How wide the quake console is.
const QUAKE_WIDTH: f32 = 720.;
/// How tall it is with the transcript hidden: one bar of status.
const QUAKE_BAR: f32 = 56.;
/// How tall it is with the transcript shown.
const QUAKE_HEIGHT: f32 = 560.;
/// How wide the companion column is.
const COMPANION_WIDTH: f32 = 420.;
/// How far the companion keeps from the top and bottom of the screen, so it
/// does not fight a panel or a dock for the last pixel.
const COMPANION_MARGIN: f32 = 24.;

/// The screen to place a window on.
///
/// The display the person is already looking at when there is one, and the
/// primary otherwise. The desktop companion this borrows from always took the
/// display at index zero, which puts the console on the wrong monitor for
/// anybody with two.
///
/// The fallback is a screen that does not exist, for the case where no display
/// reports at all: a window somewhere beats no window.
#[must_use]
pub fn screen(cx: &mut gpui::App) -> Bounds<Pixels> {
    cx.active_window()
        .and_then(|window| window.update(cx, |_, window, _| window.bounds()).ok())
        .or_else(|| cx.primary_display().map(|display| display.bounds()))
        .unwrap_or(Bounds {
            origin: Point {
                x: px(0.),
                y: px(0.),
            },
            size: Size {
                width: px(1280.),
                height: px(800.),
            },
        })
}

/// The window this mode wants on this display.
///
/// `expanded` is only asked of the quake console; the companion is one height.
#[must_use]
pub fn bounds(mode: Mode, expanded: bool, display: Bounds<Pixels>) -> Bounds<Pixels> {
    match mode {
        Mode::Quake => {
            let width = px(QUAKE_WIDTH).min(display.size.width);
            let height = px(if expanded { QUAKE_HEIGHT } else { QUAKE_BAR });
            Bounds {
                // Centred across the screen and flush with its top, which is
                // the origin every later resize grows from.
                origin: Point {
                    x: display.origin.x + (display.size.width - width) / 2.,
                    y: display.origin.y,
                },
                size: Size {
                    width,
                    height: height.min(display.size.height),
                },
            }
        }
        Mode::Companion => {
            let width = px(COMPANION_WIDTH).min(display.size.width);
            let margin = px(COMPANION_MARGIN);
            let height = (display.size.height - margin - margin).max(px(QUAKE_BAR));
            Bounds {
                origin: Point {
                    x: display.origin.x + display.size.width - width,
                    y: display.origin.y + margin,
                },
                size: Size { width, height },
            }
        }
    }
}

/// The size a resize should ask for, without moving the window.
///
/// Only the quake console has two of them. The companion answers with the size
/// it already has, so a toggle there is about what is drawn and not about how
/// much room it takes.
#[must_use]
pub fn size_of(mode: Mode, expanded: bool, display: Bounds<Pixels>) -> Size<Pixels> {
    bounds(mode, expanded, display).size
}

/// How to open the window for this mode.
///
/// `WindowKind::Normal` and not `PopUp`, which would seem the natural choice
/// for a console that drops over other windows: a pop-up becomes a
/// `_NET_WM_WINDOW_TYPE_NOTIFICATION` under X11 and a non-activating panel on
/// macOS, and neither of those takes the keyboard. A window nobody can type in
/// is no use to a companion whose whole point is the text box.
///
/// The `app_id` is what a Wayland compositor matches a rule against, since
/// Wayland lets no client place its own windows. It is the same string in both
/// modes so that one rule covers them.
#[must_use]
pub fn options(mode: Mode, bounds: Bounds<Pixels>) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("alate".into()),
            appears_transparent: true,
            ..Default::default()
        }),
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        is_movable: mode == Mode::Companion,
        is_resizable: false,
        is_minimizable: false,
        display_id: None,
        window_background: gpui::WindowBackgroundAppearance::Opaque,
        app_id: Some(APP_ID.to_owned()),
        window_min_size: Some(Size {
            width: px(320.),
            height: px(QUAKE_BAR),
        }),
        window_decorations: Some(gpui::WindowDecorations::Client),
        tabbing_identifier: None,
    }
}

/// What a compositor knows this window as.
pub const APP_ID: &str = "com.embornal.aphid.alate";

#[cfg(test)]
mod tests {
    use super::*;

    /// A display that is not the machine's, so the geometry can be checked
    /// without a screen. The offset is deliberate: a second monitor does not
    /// start at zero, and a window placed as though it did lands on the first.
    fn display() -> Bounds<Pixels> {
        Bounds {
            origin: Point {
                x: px(1920.),
                y: px(0.),
            },
            size: Size {
                width: px(1600.),
                height: px(1200.),
            },
        }
    }

    #[test]
    fn the_quake_console_is_centred_and_flush_with_the_top() {
        let bounds = bounds(Mode::Quake, false, display());
        assert_eq!(bounds.size.width, px(QUAKE_WIDTH));
        assert_eq!(bounds.size.height, px(QUAKE_BAR));
        assert_eq!(bounds.origin.y, px(0.));
        assert_eq!(bounds.origin.x, px(1920. + (1600. - 720.) / 2.));
    }

    #[test]
    fn expanding_the_quake_console_grows_it_downwards_from_the_same_origin() {
        let collapsed = bounds(Mode::Quake, false, display());
        let expanded = bounds(Mode::Quake, true, display());
        assert_eq!(collapsed.origin, expanded.origin, "the origin never moves");
        assert!(expanded.size.height > collapsed.size.height);
        assert_eq!(expanded.size.height, px(QUAKE_HEIGHT));
    }

    #[test]
    fn the_companion_stands_against_the_right_edge() {
        let bounds = bounds(Mode::Companion, false, display());
        assert_eq!(bounds.size.width, px(COMPANION_WIDTH));
        assert_eq!(
            bounds.origin.x + bounds.size.width,
            px(1920. + 1600.),
            "flush with the right edge of that display"
        );
        assert_eq!(bounds.origin.y, px(COMPANION_MARGIN));
        assert_eq!(bounds.size.height, px(1200. - 2. * COMPANION_MARGIN));
    }

    #[test]
    fn the_companion_ignores_the_expanded_switch() {
        assert_eq!(
            bounds(Mode::Companion, false, display()),
            bounds(Mode::Companion, true, display())
        );
    }

    #[test]
    fn a_screen_smaller_than_the_window_is_not_overflowed() {
        let small = Bounds {
            origin: Point {
                x: px(0.),
                y: px(0.),
            },
            // Narrower than either window wants to be, and shorter than the
            // expanded console.
            size: Size {
                width: px(380.),
                height: px(320.),
            },
        };
        let quake = bounds(Mode::Quake, true, small);
        assert_eq!(quake.size.width, px(380.));
        assert!(quake.size.height <= px(320.));
        let companion = bounds(Mode::Companion, false, small);
        assert_eq!(companion.size.width, px(380.));
        assert_eq!(companion.origin.x, px(0.), "flush with both edges at once");
    }

    #[test]
    fn a_resize_asks_for_the_size_the_bounds_would_have_had() {
        let display = display();
        assert_eq!(
            size_of(Mode::Quake, true, display),
            bounds(Mode::Quake, true, display).size
        );
    }
}
