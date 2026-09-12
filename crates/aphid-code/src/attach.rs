//! Files that ride with a message.
//!
//! A file is attached by naming it in the text: `@src/main.rs`. The marker is
//! ordinary text, so nothing has to be drawn for it, it is stored in the
//! session with the rest of the message, and a resumed conversation reads
//! exactly as it was written. What makes it more than text is the [`Store`]:
//! the bytes [`read`] took, keyed by the path the file list gave. [`search`]
//! finds the markers in the text at the moment the message goes out, and
//! [`compose`] turns them into the parts the model receives.
//!
//! The alternative — a chip in the editor, one buffer character per attachment
//! — was not taken. `ratatui_textarea` has no per-range style and does not
//! expose where a buffer offset lands on the screen, so a drawn badge would
//! mean replacing the widget's rendering with one of our own. Text costs
//! nothing and reads the same in the session file.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::sync::Arc;

use aphid_core::ContentInput;

use crate::tools::Workspace;
use crate::tools::truncate;

/// The largest image that may be attached, in bytes.
///
/// Ten megabytes: well below every endpoint's own limit — the request body is
/// what a provider measures — and four times what a screenshot weighs.
pub const MAX_IMAGE_BYTES: u64 = 10_000_000;

/// How much of a file is looked at before it is called text.
const SNIFF_BYTES: usize = 16;

/// What the first bytes of a file say it is.
enum Signature {
    /// One of the four formats every vision endpoint takes.
    Supported(&'static str),
    /// An image in a format that is not carried.
    Unsupported(&'static str),
    /// Not an image.
    Other,
}

/// What a file became when it was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// The text of the file, wrapped so the model knows where it came from.
    Text {
        content: String,
        /// The file was longer than [`truncate`]'s cap, and `content` says so.
        truncated: bool,
    },
    /// The bytes of an image, exactly as they were on disk.
    ///
    /// `Arc` so a clone of a [`Prompt`] is a pointer copy rather than a
    /// megabyte one.
    Image { data: Arc<[u8]>, mime: &'static str },
}

/// The files a draft names, by path, as they were read.
///
/// Filled when a file is attached, kept for as long as the draft is, and
/// dropped when the message goes out. The text decides *what* is sent; this
/// decides what the bytes are.
pub type Store = BTreeMap<String, Body>;

/// One place in the text where a file is named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention {
    /// The byte offset just after the marker, where the part that carries the
    /// file starts.
    pub after: usize,
    /// The path the marker names, as it is keyed in the store.
    pub path: String,
}

/// One piece of a message, in the order the model receives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Text(String),
    Image { data: Arc<[u8]>, mime: &'static str },
}

/// One message on its way to the agent.
///
/// Holds what a run needs: the text, and the files its markers named, already
/// cut into the order the model reads them in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    /// The line as it was written, markers and all. What the pane shows and
    /// what a session stores, which is not quite the parts: a file's own
    /// content is a part but not words anybody typed.
    line: String,
    parts: Vec<Part>,
}

impl Prompt {
    /// A prompt of text alone, which is what almost every message is.
    pub fn text(text: impl Into<String>) -> Self {
        let line = text.into();
        Self {
            parts: vec![Part::Text(line.clone())],
            line,
        }
    }

    /// The line as the user wrote it.
    #[must_use]
    pub fn line(&self) -> &str {
        &self.line
    }

    /// The parts, in order, for a caller that has to look at them.
    #[must_use]
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// The pieces a run needs, borrowing from this prompt.
    #[must_use]
    pub fn inputs(&self) -> Vec<ContentInput<'_>> {
        self.parts
            .iter()
            .map(|part| match part {
                Part::Text(text) => ContentInput::Text(text),
                Part::Image { data, mime } => ContentInput::Image { data, mime },
            })
            .collect()
    }
}

/// Read `path` for a message, without sending it anywhere yet.
///
/// Runs off the interface's own thread: it reads a whole file.
///
/// # Errors
///
/// Every failure is a sentence for the user: a path the workspace refuses, a
/// directory, a file too large to attach, an image in a format that is not
/// carried, or a file that is neither text nor one of those images.
pub fn read(workspace: &Workspace, path: &str) -> Result<Body, String> {
    let resolved = workspace.resolve_read(path)?;
    let meta =
        std::fs::metadata(&resolved).map_err(|error| format!("could not read {path}: {error}"))?;
    if meta.is_dir() {
        return Err(format!("{path} is a directory"));
    }

    // The head is read first so that an oversized video is refused by its
    // length rather than after it has been pulled into memory.
    let mut head = [0u8; SNIFF_BYTES];
    let mut file = std::fs::File::open(&resolved)
        .map_err(|error| format!("could not read {path}: {error}"))?;
    let found = file
        .read(&mut head)
        .map_err(|error| format!("could not read {path}: {error}"))?;
    let head = &head[..found];

    match signature(head) {
        Signature::Supported(mime) => {
            if meta.len() > MAX_IMAGE_BYTES {
                return Err(format!(
                    "{path} is {}; the most that can be attached is {}",
                    size(meta.len() as usize),
                    size(MAX_IMAGE_BYTES as usize)
                ));
            }
            let bytes = std::fs::read(&resolved)
                .map_err(|error| format!("could not read {path}: {error}"))?;
            Ok(Body::Image {
                data: bytes.into(),
                mime,
            })
        }
        Signature::Unsupported(kind) => Err(format!(
            "{path} is a {kind} image; only PNG, JPEG, GIF and WebP can be attached"
        )),
        Signature::Other if head.contains(&0) => Err(binary_message(path)),
        Signature::Other => {
            let bytes = std::fs::read(&resolved)
                .map_err(|error| format!("could not read {path}: {error}"))?;
            let Ok(text) = String::from_utf8(bytes) else {
                return Err(binary_message(path));
            };
            Ok(wrap(path, &text))
        }
    }
}

/// What the first bytes of a file say it is.
///
/// The content is what counts: every endpoint detects the format itself and
/// ignores the name, so a `screenshot.png` that is really a bitmap would be
/// refused at the far end rather than here.
fn signature(head: &[u8]) -> Signature {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Signature::Supported("image/png");
    }
    if head.starts_with(b"\xff\xd8\xff") {
        return Signature::Supported("image/jpeg");
    }
    if head.starts_with(b"GIF8") {
        return Signature::Supported("image/gif");
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return Signature::Supported("image/webp");
    }
    if head.starts_with(b"BM") {
        return Signature::Unsupported("BMP");
    }
    if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        return Signature::Unsupported("TIFF");
    }
    Signature::Other
}

/// The text of a file, in a wrapper that names it and caps it.
fn wrap(path: &str, text: &str) -> Body {
    let capped = truncate::head(text);
    let mut content = format!("<file path=\"{path}\">\n");
    content.push_str(&capped.text);
    if let Some(notice) = capped.notice() {
        content.push_str(&notice);
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("</file>");
    Body::Text {
        content,
        truncated: capped.truncated,
    }
}

fn binary_message(path: &str) -> String {
    format!("{path} is not text, and it is not an image that can be attached")
}

/// Every place in `text` where a file the store holds is named.
///
/// A marker is an `@` followed by a path, and the longest path wins, so
/// `@a.png.bak` is not a marker for `a.png` with three letters after it. The
/// scan is by the paths the store holds rather than by words, so a path with a
/// space in it needs no quoting.
#[must_use]
pub fn search(text: &str, store: &Store) -> Vec<Mention> {
    let paths = by_length(store);
    if paths.is_empty() {
        return Vec::new();
    }

    let mut found = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let Some(sign) = text[at..].find('@') else {
            break;
        };
        let marker = at + sign;
        // The byte after the `@`, which is where a path may begin. Not every
        // offset after an `@` is a character boundary; `find` gives one.
        let start = marker + 1;
        let rest = &text[start..];
        match paths.iter().find(|path| rest.starts_with(path.as_str())) {
            Some(path) => {
                let after = start + path.len();
                found.push(Mention {
                    after,
                    path: path.clone(),
                });
                at = after;
            }
            None => at = start,
        }
    }
    found
}

/// The marker the cursor at character `col` would take with it, as the
/// character range `[start, end)` of the line.
///
/// `back` is the Backspace case, where the cursor may sit just after the marker
/// or anywhere inside it. A Delete sits at its head or inside it, never after.
#[must_use]
pub fn mention_at(line: &str, col: usize, store: &Store, back: bool) -> Option<(usize, usize)> {
    let paths = by_length(store);
    let chars: Vec<char> = line.chars().collect();

    for start in 0..chars.len() {
        if chars[start] != '@' {
            continue;
        }
        for path in &paths {
            let path: Vec<char> = path.chars().collect();
            if !chars[start + 1..].starts_with(&path) {
                continue;
            }
            let end = start + 1 + path.len();
            let holds = if back {
                start < col && col <= end
            } else {
                start <= col && col < end
            };
            if holds {
                return Some((start, end));
            }
        }
    }
    None
}

/// Cut `text` into the parts a model reads: each file directly after the words
/// that name it.
///
/// A marker whose path the store does not hold is left as text, which is what
/// makes a marker typed by hand harmless.
#[must_use]
pub fn compose(text: &str, store: &Store) -> Prompt {
    let found = search(text, store);
    if found.is_empty() {
        return Prompt::text(text);
    }

    let mut parts = Vec::new();
    let mut cut = 0;
    for mention in found {
        let Some(body) = store.get(&mention.path) else {
            continue;
        };
        // The words that name the file include the marker itself: the model
        // reads "look at @shot.png", and the picture follows it.
        let words = &text[cut..mention.after];
        if !words.is_empty() {
            parts.push(Part::Text(words.to_owned()));
        }
        parts.push(match body {
            Body::Text { content, .. } => Part::Text(content.clone()),
            Body::Image { data, mime } => Part::Image {
                data: Arc::clone(data),
                mime,
            },
        });
        cut = mention.after;
    }
    if cut < text.len() {
        parts.push(Part::Text(text[cut..].to_owned()));
    }
    if parts.is_empty() {
        parts.push(Part::Text(text.to_owned()));
    }
    Prompt {
        line: text.to_owned(),
        parts,
    }
}

/// The paths of the store, longest first, so that the longest match wins.
fn by_length(store: &Store) -> Vec<String> {
    let mut paths: Vec<String> = store.keys().cloned().collect();
    paths.sort_by_key(|path| std::cmp::Reverse(path.len()));
    paths
}

/// A count of bytes the way the status line writes one.
fn size(count: usize) -> String {
    crate::tui::scrollback::bytes(count)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// Bytes that say PNG, which is all the signature looks at.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0123456789";

    /// A workspace of its own, holding the named files.
    fn workspace(files: &[(&str, &[u8])]) -> Workspace {
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let root = std::env::temp_dir().join(format!(
            "aphid-attach-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create the workspace");
        for (path, bytes) in files {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("create the parent");
            }
            std::fs::write(&full, bytes).expect("write the file");
        }
        Workspace::new(root)
    }

    fn store(entries: &[(&str, Body)]) -> Store {
        entries
            .iter()
            .map(|(path, body)| ((*path).to_owned(), body.clone()))
            .collect()
    }

    fn image() -> Body {
        Body::Image {
            data: Arc::from(&[1u8, 2, 3][..]),
            mime: "image/png",
        }
    }

    fn text(content: &str) -> Body {
        Body::Text {
            content: content.to_owned(),
            truncated: false,
        }
    }

    // ---- reading ---------------------------------------------------------

    #[test]
    fn a_text_file_is_wrapped_so_the_model_knows_where_it_came_from() {
        let workspace = workspace(&[("src/main.rs", b"fn main() {}\n")]);
        let body = read(&workspace, "src/main.rs").expect("reads");

        let Body::Text { content, truncated } = body else {
            panic!("a text file is text");
        };
        assert!(!truncated);
        assert_eq!(
            content,
            "<file path=\"src/main.rs\">\nfn main() {}\n</file>"
        );
    }

    #[test]
    fn a_long_text_file_is_capped_and_the_cap_is_in_the_text() {
        let long: String = (0..truncate::MAX_LINES + 500)
            .map(|n| format!("line {n}\n"))
            .collect();
        let workspace = workspace(&[("long.txt", long.as_bytes())]);
        let body = read(&workspace, "long.txt").expect("reads");

        let Body::Text { content, truncated } = body else {
            panic!("a text file is text");
        };
        assert!(truncated);
        assert!(content.contains("lines shown"), "{content}");
        assert!(content.ends_with("\n</file>"), "{content}");
        assert!(!content.contains("line 1500"), "the tail is dropped");
    }

    #[test]
    fn the_bytes_decide_the_format_and_not_the_name() {
        let workspace = workspace(&[("note.txt", PNG), ("shot.png", b"not a picture")]);

        assert_eq!(
            read(&workspace, "note.txt").expect("reads"),
            Body::Image {
                data: Arc::from(PNG),
                mime: "image/png",
            },
            "a picture called .txt is a picture"
        );
        assert!(
            matches!(read(&workspace, "shot.png"), Ok(Body::Text { .. })),
            "and words called .png are words"
        );
    }

    #[test]
    fn an_image_in_a_format_that_is_not_carried_is_refused() {
        let workspace = workspace(&[("shot.bmp", b"BM\x00\x00\x00\x00")]);
        let error = read(&workspace, "shot.bmp").expect_err("refused");

        assert!(error.contains("BMP"), "{error}");
        assert!(error.contains("PNG, JPEG, GIF and WebP"), "{error}");
    }

    #[test]
    fn an_image_above_the_cap_is_refused_before_it_is_read() {
        let mut big = PNG.to_vec();
        big.resize(12_000_000, 0);
        let workspace = workspace(&[("huge.png", &big)]);
        let error = read(&workspace, "huge.png").expect_err("refused");

        assert!(error.contains("12.0 MB"), "{error}");
        assert!(error.contains("10.0 MB"), "{error}");
    }

    #[test]
    fn a_binary_file_and_a_directory_are_refused_with_a_reason() {
        let workspace = workspace(&[("blob.bin", b"\x00\x01\x02\x03"), ("dir/keep", b"x")]);

        let error = read(&workspace, "blob.bin").expect_err("refused");
        assert!(error.contains("not text"), "{error}");

        let error = read(&workspace, "dir").expect_err("refused");
        assert!(error.contains("is a directory"), "{error}");

        let error = read(&workspace, "gone.rs").expect_err("refused");
        assert!(!error.is_empty());
    }

    // ---- finding the markers --------------------------------------------

    #[test]
    fn a_marker_is_found_by_its_path() {
        let store = store(&[("src/main.rs", text("x"))]);
        let found = search("explain @src/main.rs please", &store);

        assert_eq!(
            found,
            [Mention {
                after: "explain @src/main.rs".len(),
                path: "src/main.rs".to_owned(),
            }]
        );
    }

    #[test]
    fn a_marker_the_store_does_not_hold_is_plain_text() {
        let store = store(&[("src/main.rs", text("x"))]);
        assert!(search("explain @src/lib.rs", &store).is_empty());
        assert!(search("mail me at @home", &store).is_empty());
    }

    #[test]
    fn the_longest_path_wins() {
        let store = store(&[("a.png", text("short")), ("a.png.bak", text("long"))]);
        let found = search("see @a.png.bak", &store);

        assert_eq!(
            found.iter().map(|m| m.path.as_str()).collect::<Vec<_>>(),
            ["a.png.bak"]
        );
    }

    #[test]
    fn one_path_in_two_places_is_two_markers() {
        let store = store(&[("a.png", text("x"))]);
        let found = search("compare @a.png with @a.png", &store);
        assert_eq!(found.len(), 2);
    }

    // ---- taking a marker back with one key -------------------------------

    #[test]
    fn backspace_at_a_marker_takes_the_whole_of_it() {
        let store = store(&[("shots/old.png", text("x"))]);
        let line = "look at @shots/old.png";
        let end = line.chars().count();

        for col in [end, end - 1, end - 5, 9] {
            assert_eq!(
                mention_at(line, col, &store, true),
                Some((8, end)),
                "inside or just after, at column {col}"
            );
        }
        assert!(mention_at(line, 0, &store, true).is_none(), "before it");
    }

    #[test]
    fn delete_at_a_marker_takes_the_whole_of_it() {
        let store = store(&[("old.png", text("x"))]);
        let line = "see @old.png now";
        let start = 4;

        assert_eq!(mention_at(line, start, &store, false), Some((start, 12)));
        assert_eq!(
            mention_at(line, start + 4, &store, false),
            Some((start, 12))
        );
        assert!(
            mention_at(line, 12, &store, false).is_none(),
            "a delete just after the marker belongs to what follows"
        );
    }

    // ---- the message -----------------------------------------------------

    #[test]
    fn the_image_follows_the_words_that_name_it() {
        let store = store(&[("a.png", image()), ("b.png", image())]);
        let prompt = compose("compare @a.png with @b.png", &store);

        let parts = prompt.parts();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], Part::Text("compare @a.png".to_owned()));
        assert!(matches!(parts[1], Part::Image { .. }));
        assert_eq!(parts[2], Part::Text(" with @b.png".to_owned()));
        assert!(matches!(parts[3], Part::Image { .. }));
    }

    #[test]
    fn a_text_file_follows_its_marker_as_its_own_part() {
        let store = store(&[("src/main.rs", text("<file/>"))]);
        let prompt = compose("explain @src/main.rs", &store);

        assert_eq!(
            prompt.parts(),
            [
                Part::Text("explain @src/main.rs".to_owned()),
                Part::Text("<file/>".to_owned()),
            ]
        );
    }

    #[test]
    fn a_message_without_a_marker_is_one_part() {
        let store = store(&[("a.png", image())]);
        let prompt = compose("just words", &store);
        assert_eq!(prompt.parts(), [Part::Text("just words".to_owned())]);
        assert_eq!(
            compose("just words", &Store::new()).inputs().len(),
            1,
            "and an empty store is a plain prompt"
        );
    }

    #[test]
    fn the_inputs_are_the_parts_in_order() {
        let store = store(&[("a.png", image())]);
        let prompt = compose("see @a.png", &store);
        let inputs = prompt.inputs();

        assert_eq!(inputs.len(), 2);
        assert!(matches!(inputs[0], ContentInput::Text("see @a.png")));
        assert!(matches!(
            inputs[1],
            ContentInput::Image {
                mime: "image/png",
                ..
            }
        ));
    }
}
