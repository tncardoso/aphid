//! The transcript one request sends, when a listener shaped it.
//!
//! The stored transcript is never touched: a shaped request is built into a
//! scratch transcript that lives for one call to the backend. Copying costs one
//! pass over the messages the request carries, which is what the encoder reads
//! anyway.

use aphid_core::{ContentInput, ContentRef, MessageId, Role, Tool, Transcript};

use crate::events::{History, RequestShape};

/// Build what a request with this shape sends.
///
/// `run_start` is where the current run begins; [`History::Run`] sends nothing
/// before it.
pub(crate) fn shaped(src: &Transcript, run_start: usize, shape: &RequestShape) -> Transcript {
    let mut out = Transcript::new();

    let has_system = src.get(0).is_some_and(|first| first.role() == Role::System);
    match &shape.system {
        Some(text) => {
            out.push_system(text);
        }
        None => {
            if has_system && let Some(id) = src.id_at(0) {
                src.compact_into(&[id], &mut out);
            }
        }
    }

    for (role, text) in &shape.prefix {
        match role {
            Role::System => out.push_system(text),
            _ => out.push_user(text),
        };
    }

    let first = usize::from(has_system);
    let from = match shape.history {
        History::All => first,
        History::Run => run_start.max(first),
    };

    let mut prefix = shape.prompt_prefix.as_deref();
    for index in from..src.len() {
        let Some(id) = src.id_at(index) else { break };
        let message = src.message(id);
        if let Some(text) = prefix
            && message.role() == Role::User
        {
            push_prefixed(&mut out, src, id, text);
            prefix = None;
            continue;
        }
        src.compact_into(&[id], &mut out);
    }

    out
}

/// Copy one user message with `prefix` in front of its text.
///
/// The prefix joins the first text part rather than becoming a part of its
/// own, so a message with no images still goes out as one plain string.
fn push_prefixed(out: &mut Transcript, src: &Transcript, id: MessageId, prefix: &str) {
    let message = src.message(id);
    let mut joined: Option<String> = None;
    let mut rest: Vec<ContentInput<'_>> = Vec::new();
    for part in message.content() {
        match part {
            ContentRef::Text(text) if joined.is_none() => {
                joined = Some(format!("{prefix}\n\n{}", text.text()));
            }
            ContentRef::Text(text) => rest.push(ContentInput::Text(text.text())),
            ContentRef::Image(image) => rest.push(ContentInput::Image {
                data: image.data(),
                mime: image.mime(),
            }),
            ContentRef::Thinking(_) | ContentRef::ToolCall(_) => {}
        }
    }
    let joined = joined.unwrap_or_else(|| prefix.to_owned());
    let mut parts = vec![ContentInput::Text(&joined)];
    parts.extend(rest);
    out.push_user_parts(&parts);
}

/// The tools a request offers, less the ones the shape leaves out.
pub(crate) fn tools(mut declarations: Vec<Tool>, shape: &RequestShape) -> Vec<Tool> {
    if !shape.exclude_tools.is_empty() {
        declarations.retain(|tool| !shape.exclude_tools.iter().any(|name| *name == tool.name));
    }
    declarations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(transcript: &Transcript, index: usize) -> (Role, String) {
        let message = transcript.get(index).expect("a message");
        let text = message
            .content()
            .filter_map(|part| match part {
                ContentRef::Text(text) => Some(text.text().to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|");
        (message.role(), text)
    }

    fn all(transcript: &Transcript) -> Vec<(Role, String)> {
        (0..transcript.len()).map(|i| text(transcript, i)).collect()
    }

    fn two_runs() -> Transcript {
        let mut t = Transcript::new();
        t.push_system("base");
        t.push_user("first");
        t.push_system("a note");
        t.push_user("second");
        t
    }

    #[test]
    fn the_default_shape_copies_everything() {
        let src = two_runs();
        let out = shaped(&src, 3, &RequestShape::default());
        assert_eq!(all(&out), all(&src));
    }

    #[test]
    fn the_run_history_starts_at_the_run() {
        let shape = RequestShape {
            history: History::Run,
            ..RequestShape::default()
        };
        let out = shaped(&two_runs(), 3, &shape);
        assert_eq!(
            all(&out),
            vec![
                (Role::System, "base".to_owned()),
                (Role::User, "second".to_owned())
            ]
        );
    }

    #[test]
    fn the_system_prompt_is_replaced_and_the_prefix_follows_it() {
        let shape = RequestShape {
            history: History::Run,
            system: Some("fresh".to_owned()),
            prefix: vec![(Role::User, "context".to_owned())],
            prompt_prefix: Some("<chat></chat>".to_owned()),
            ..RequestShape::default()
        };
        let out = shaped(&two_runs(), 3, &shape);
        assert_eq!(
            all(&out),
            vec![
                (Role::System, "fresh".to_owned()),
                (Role::User, "context".to_owned()),
                (Role::User, "<chat></chat>\n\nsecond".to_owned()),
            ]
        );
    }

    #[test]
    fn only_the_first_user_message_is_prefixed() {
        let mut src = two_runs();
        src.push_user("third");
        let shape = RequestShape {
            history: History::Run,
            prompt_prefix: Some("P".to_owned()),
            ..RequestShape::default()
        };
        let out = shaped(&src, 3, &shape);
        assert_eq!(text(&out, 1), (Role::User, "P\n\nsecond".to_owned()));
        assert_eq!(text(&out, 2), (Role::User, "third".to_owned()));
    }

    #[test]
    fn excluded_tools_are_left_out() {
        let tool = |name: &str| Tool::new(name, "", serde_json::json!({}));
        let shape = RequestShape {
            exclude_tools: vec!["zoom".to_owned()],
            ..RequestShape::default()
        };
        let kept = tools(vec![tool("read"), tool("zoom")], &shape);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "read");
    }
}
