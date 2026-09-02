//! The creature's body: a thread that draws it, and the image that reaches the
//! window.
//!
//! The drawing happens on a thread of its own because a frame ends in
//! `wait_for` — a fence the CPU sits on until the GPU is done — and sitting on
//! that on the thread GPUI draws with would stutter the text box for the sake
//! of an ornament.
//!
//! The window drives the clock rather than the thread: it asks for a frame when
//! it wants one, and the thread answers with pixels. That is what makes the
//! rhythm the window's business — thirty frames a second while something is
//! happening, ten while nothing is, and none at all while the window is
//! collapsed — without the thread knowing anything about windows.

mod gpu;

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use gpui::{App, RenderImage, Window};

use super::config::Familiar;
use super::emote::Emote;

/// How often to ask for a frame while something is happening.
pub const FAST: std::time::Duration = std::time::Duration::from_millis(33);
/// How often while nothing is.
pub const SLOW: std::time::Duration = std::time::Duration::from_millis(100);

/// The width and height each familiar is drawn at.
///
/// Multiples of 64, so every row of pixels is a multiple of 256 bytes and comes
/// back from the GPU with no padding.
///
/// One size for each familiar and not one for each place it appears: a Blade
/// context is built once and belongs to a thread, so drawing the collapsed bar
/// at its own smaller size would mean tearing that down and building it again
/// on every toggle. GPUI scales the one image into whatever the panel is.
#[must_use]
pub fn size_of(familiar: Familiar) -> (u32, u32) {
    match familiar {
        Familiar::Sap => (256, 256),
        Familiar::Drift => (192, 192),
    }
}

/// What the window asks the thread for.
enum Order {
    Draw {
        time: f32,
        emote: Emote,
        previous: Emote,
        blend: f32,
    },
    /// Put the context down and end, while the application is still up. See
    /// [`Body::stop`] for why that matters.
    Stop,
}

/// What the thread answers with.
enum Answer {
    Frame(Vec<u8>),
    /// There is no body, and this is why. Said once, and the thread ends.
    Trouble(String),
}

/// The handle the window holds.
pub struct Body {
    orders: Sender<Order>,
    answers: Receiver<Answer>,
    /// Kept so that a deliberate stop can wait for the context to be put down
    /// before another is built.
    thread: Option<std::thread::JoinHandle<()>>,
    width: u32,
    height: u32,
    /// The image last published, kept so it can be dropped when the next one
    /// replaces it. Without that every frame leaks a texture into GPUI's atlas.
    image: Option<Arc<RenderImage>>,
    /// Why there is no creature, when there is none.
    trouble: Option<String>,
    /// Whether a frame has been asked for and not yet answered. One at a time,
    /// so a slow GPU makes the creature slow instead of making a queue.
    waiting: bool,
}

impl Body {
    /// Start drawing this familiar at this size.
    ///
    /// Never fails: a machine with no device to draw on gets a body with
    /// [`Body::trouble`] set, and the window says so where the creature would
    /// have been. The panel is an ornament; the gateway client is the function.
    #[must_use]
    pub fn start(familiar: Familiar, width: u32, height: u32) -> Self {
        let (orders, wishes) = channel::<Order>();
        let (replies, answers) = channel::<Answer>();
        let thread = std::thread::Builder::new()
            .name(format!("alate-{}", familiar.label()))
            .spawn(move || paint(familiar, width, height, &wishes, &replies))
            .ok();
        Self {
            orders,
            answers,
            thread,
            width,
            height,
            image: None,
            trouble: None,
            waiting: false,
        }
    }

    /// Ask for a frame, unless one is already being drawn.
    pub fn ask(&mut self, time: f32, emote: Emote, previous: Emote, blend: f32) {
        if self.waiting || self.trouble.is_some() {
            return;
        }
        let sent = self.orders.send(Order::Draw {
            time,
            emote,
            previous,
            blend,
        });
        if sent.is_err() {
            self.trouble = Some("the alate stopped drawing".to_owned());
        } else {
            self.waiting = true;
        }
    }

    /// Take whatever the thread has drawn, and hand it to GPUI.
    ///
    /// Returns whether the image changed, which is what tells the window
    /// whether the frame is worth a repaint.
    pub fn collect(&mut self, window: &mut Window, cx: &mut App) -> bool {
        let mut newest = None;
        loop {
            match self.answers.try_recv() {
                // Only the last one is worth drawing; the others are already
                // behind the clock.
                Ok(Answer::Frame(bytes)) => {
                    self.waiting = false;
                    newest = Some(bytes);
                }
                Ok(Answer::Trouble(reason)) => {
                    self.waiting = false;
                    self.trouble = Some(reason);
                    return true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.waiting = false;
                    if self.trouble.is_none() {
                        self.trouble = Some("the alate stopped drawing".to_owned());
                    }
                    return true;
                }
            }
        }
        let Some(bytes) = newest else { return false };
        let Some(buffer) = image::RgbaImage::from_raw(self.width, self.height, bytes) else {
            return false;
        };
        // The bytes are BGRA and the type says RGBA, which is what GPUI's own
        // `RenderImage` expects: it reads the buffer as BGRA whatever the image
        // crate calls it.
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        if let Some(old) = self.image.replace(image) {
            // Not housekeeping: without this every frame is a new texture in
            // the atlas and none of them is ever freed.
            cx.drop_image(old, Some(window));
        }
        true
    }

    /// The image to draw, when there is one.
    #[must_use]
    pub fn image(&self) -> Option<Arc<RenderImage>> {
        self.image.clone()
    }

    /// Why there is no creature.
    #[must_use]
    pub fn trouble(&self) -> Option<&str> {
        self.trouble.as_deref()
    }
}

impl Body {
    /// End the drawing thread and wait for it to put its context down.
    ///
    /// Call this whenever a body is being replaced while the application is
    /// still running — changing familiar is the one case. It is **not** called
    /// when the process is closing: see [`paint`] for what happens then and
    /// why the difference matters.
    pub fn stop(&mut self) {
        let _ = self.orders.send(Order::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        // Deliberately not a stop. Dropping the sender ends the thread, which
        // then leaves its context alone; the image is left to GPUI, which drops
        // it with the window.
    }
}

/// The thread: build a context, then draw what is asked for.
///
/// ## Why it sometimes walks away from its own context
///
/// Dropping the context calls `vkDestroyDevice`. Doing that **while GPUI is
/// tearing down the device it draws windows with** segfaults inside the driver,
/// which is what closing the window used to do here: the view is dropped, this
/// channel closes, and this thread races the shutdown with a device to destroy.
///
/// So the two ways of ending are told apart. [`Order::Stop`] is a stop asked
/// for while the application is up — changing familiar — and the context is put
/// down properly. A channel that simply closes is the process going away, and
/// then the context is left where it is: there is nothing to reclaim that the
/// kernel will not reclaim a moment later anyway.
fn paint(
    familiar: Familiar,
    width: u32,
    height: u32,
    wishes: &Receiver<Order>,
    replies: &Sender<Answer>,
) {
    let mut painter = match gpu::Painter::new(familiar, width, height) {
        Ok(painter) => painter,
        Err(reason) => {
            let _ = replies.send(Answer::Trouble(reason));
            return;
        }
    };
    let closing = loop {
        match wishes.recv() {
            Ok(Order::Draw {
                time,
                emote,
                previous,
                blend,
            }) => match painter.frame(time, emote, previous, blend) {
                Ok(bytes) => {
                    if replies.send(Answer::Frame(bytes)).is_err() {
                        break true;
                    }
                }
                Err(reason) => {
                    let _ = replies.send(Answer::Trouble(reason));
                    break false;
                }
            },
            Ok(Order::Stop) => break false,
            Err(_) => break true,
        }
    };
    if closing {
        std::mem::forget(painter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_width_is_a_multiple_of_sixty_four() {
        // Which is what makes every row of pixels a multiple of 256 bytes, and
        // the readback the image with no padding in it.
        for familiar in [Familiar::Sap, Familiar::Drift] {
            let (width, height) = size_of(familiar);
            assert_eq!(width % 64, 0, "{familiar:?}");
            assert!(height > 0);
        }
    }
}
