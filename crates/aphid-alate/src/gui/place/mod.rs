//! Keeping the window above the others, and putting it where it belongs.
//!
//! Everything here is **best effort and fails in silence**. A companion that
//! refused to open because a window manager would not answer would be worse
//! than one that opens in the wrong corner, and the corner is something the
//! person at the keyboard can see and fix. So each platform tries, and a
//! platform that cannot try does nothing.
//!
//! What each one can do differs, and it is not a matter of effort:
//!
//! - **X11** takes both. `_NET_WM_STATE_ABOVE` keeps the window over the
//!   others and `ConfigureWindow` moves it, which is what makes the console
//!   drop from the top of the screen whatever the window manager decided at
//!   map time.
//! - **macOS** takes the level and the spaces. It honours the frame a window
//!   is created with, so there is nothing to move afterwards.
//! - **Wayland** takes neither, by the design of the protocol: no client
//!   places its own surface. The window carries the app id
//!   [`super::window::APP_ID`] so a rule in the compositor can do it, and the
//!   documentation says how.
//!
//! ## Why the window is found by process id and not by handle
//!
//! GPUI has `impl HasWindowHandle for Window`, and on macOS it gives the
//! `NSView` this uses. On X11 it does **not**: `X11Window::window_handle` is
//! `unimplemented!()` (`platform/linux/x11/window.rs:315`), so asking for the
//! handle panics rather than failing. What GPUI does publish is
//! `_NET_WM_PID` on every window it creates (`window.rs:483`), so the window is
//! found the way any other X11 tool would find it: walk `_NET_CLIENT_LIST` from
//! the root and take the windows whose pid is ours.

use gpui::{Bounds, Pixels, Window};

#[cfg(target_os = "macos")]
mod mac;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod x11;

/// Put the window where `bounds` says, and ask for it to stay on top.
///
/// Call it after the window is on screen, not while it is being created: X11
/// publishes a window in `_NET_CLIENT_LIST` when the window manager takes it
/// over, which is at map time and not before.
pub fn place(window: &Window, bounds: Bounds<Pixels>) {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        let _ = window;
        x11::place(origin_of(bounds, window.scale_factor()));
    }
    #[cfg(target_os = "macos")]
    {
        let _ = bounds;
        mac::float(window);
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
    {
        let _ = (window, bounds);
    }
}

/// Where the window goes, in the pixels the display actually has.
///
/// GPUI works in logical pixels and X11 in device ones, so a window placed by
/// its logical origin on a display scaled by two lands half way across it.
#[must_use]
fn origin_of(bounds: Bounds<Pixels>, scale: f32) -> (i32, i32) {
    let scale = if scale > 0. { scale } else { 1. };
    (
        (f32::from(bounds.origin.x) * scale).round() as i32,
        (f32::from(bounds.origin.y) * scale).round() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Point, Size, px};

    fn at(x: f32, y: f32) -> Bounds<Pixels> {
        Bounds {
            origin: Point { x: px(x), y: px(y) },
            size: Size {
                width: px(720.),
                height: px(56.),
            },
        }
    }

    #[test]
    fn an_unscaled_display_moves_the_window_to_the_logical_origin() {
        assert_eq!(origin_of(at(440., 0.), 1.), (440, 0));
    }

    #[test]
    fn a_scaled_display_moves_it_to_the_pixels_that_display_has() {
        assert_eq!(origin_of(at(440., 12.), 2.), (880, 24));
        assert_eq!(origin_of(at(100., 100.), 1.5), (150, 150));
    }

    #[test]
    fn a_scale_that_makes_no_sense_is_treated_as_one() {
        // What a display that has not reported yet can answer with. Moving the
        // window to zero would be worse than leaving it where it is.
        assert_eq!(origin_of(at(440., 0.), 0.), (440, 0));
    }

    #[test]
    fn a_second_monitor_keeps_its_offset() {
        // X11 has one coordinate space for every screen, so the origin of a
        // window on the second monitor is past the width of the first.
        assert_eq!(origin_of(at(1920. + 440., 0.), 1.), (2360, 0));
    }
}
