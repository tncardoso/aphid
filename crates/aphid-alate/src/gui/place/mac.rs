//! The macOS half: a floating window that follows you between spaces.
//!
//! There is nothing to move here. macOS honours the frame a window is created
//! with, so `WindowOptions` already put the window where it belongs and the
//! only thing left is what a window manager decides on X11: that it stays over
//! the ordinary windows, and that switching desktops does not leave it behind.
//!
//! GPUI has an `NSStatusItem` of its own (`platform/mac/status_item.rs`) and a
//! window level it sets for pop-ups, but `mod mac` is private, so neither is
//! reachable from here. What is reachable is the `NSView`, through the window
//! handle — which on this platform, unlike X11, is implemented.

use gpui::Window;
use objc2_app_kit::{NSFloatingWindowLevel, NSView, NSWindowCollectionBehavior};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Keep the window over the others, on every space.
pub fn float(window: &Window) {
    // `Window` has a `window_handle` of its own, returning GPUI's own handle
    // type, so the trait's method is named outright rather than called on the
    // window: the inherent one wins otherwise.
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    // SAFETY: the handle is the `NSView` GPUI made for this window, and this
    // runs on the main thread — the only one GPUI draws or lays out on, and the
    // only one AppKit allows either from.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let Some(window) = view.window() else {
        // A view that is not in a window yet. The caller tries again on a later
        // frame, as it does for a window X11 has not listed.
        return;
    };
    window.setLevel(NSFloatingWindowLevel);
    window.setCollectionBehavior(
        // Follow the person between desktops rather than staying on the one it
        // was opened in, which is what a companion is for.
        NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::Stationary,
    );
}
