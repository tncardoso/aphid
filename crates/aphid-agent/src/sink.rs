//! Where a component's user-facing output goes.
//!
//! The `sink` service. A front end provides one — the terminal UI renders
//! notices, the headless printer writes them, the alate gateway forwards them
//! down a socket — and every component that has something to say resolves it
//! rather than being handed one at construction.
//!
//! Nothing here is about scripts. It lives beside the runtime because it is a
//! capability of the runtime, and a Rust component that never touches Rhai
//! still wants somewhere to put a notice.

/// Where a component's `log`, `notify` and `prompt` output goes.
pub trait Sink: Send + Sync + 'static {
    /// Something the user should see. The terminal UI renders these as notices.
    fn notify(&self, source: &str, text: &str);

    /// Something only a developer wants. Defaults to standard error, which is
    /// where the terminal UI is not drawing.
    fn log(&self, source: &str, text: &str) {
        eprintln!("[{source}] {text}");
    }

    /// Text for the model, as if the user had typed it.
    ///
    /// Defaults to doing nothing, because a front end with no prompt queue —
    /// headless, or a caller embedding the agent — has nowhere to put it. The
    /// terminal UI puts it in the same queue a typed line goes to.
    fn prompt(&self, source: &str, text: &str) {
        let _ = (source, text);
    }
}

/// The sink for a host nobody is watching.
#[derive(Copy, Clone, Debug, Default)]
pub struct Silent;

impl Sink for Silent {
    fn notify(&self, _source: &str, _text: &str) {}
    fn log(&self, _source: &str, _text: &str) {}
}
