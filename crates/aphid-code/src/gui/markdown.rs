//! A small, UI-neutral Markdown document used by the GPUI renderer.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Text(String),
    Heading { level: u8, text: String },
    Code { language: String, text: String },
    Quote(String),
    ListItem { depth: usize, text: String },
    Rule,
    Image { url: String, alt: String },
    Link { url: String, text: String },
    Table(String),
}

#[derive(Copy, Clone)]
enum Open {
    Text,
    Heading(u8),
    Code,
    Quote,
    ListItem(usize),
    Table,
}

#[must_use]
pub fn parse(source: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut open = Vec::<Open>::new();
    let mut text = String::new();
    let mut code_language = String::new();
    let mut list_depth = 0usize;
    let mut image: Option<(String, String)> = None;
    let mut link: Option<(String, String)> = None;

    let parser = Parser::new_ext(source, Options::all());
    for event in parser {
        match event {
            Event::Start(Tag::Paragraph) => open.push(Open::Text),
            Event::Start(Tag::Heading { level, .. }) => {
                open.push(Open::Heading(heading_level(level)));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                code_language = match kind {
                    CodeBlockKind::Indented => String::new(),
                    CodeBlockKind::Fenced(language) => language.into_string(),
                };
                open.push(Open::Code);
            }
            Event::Start(Tag::BlockQuote(_)) => open.push(Open::Quote),
            Event::Start(Tag::List(_)) => list_depth += 1,
            Event::Start(Tag::Item) => open.push(Open::ListItem(list_depth)),
            Event::Start(Tag::Table(_)) => open.push(Open::Table),
            Event::Start(Tag::TableRow) => {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
            }
            Event::Start(Tag::TableCell) => {
                if !text.is_empty() && !text.ends_with(['\n', ' ']) {
                    text.push_str(" │ ");
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                image = Some((dest_url.into_string(), String::new()));
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                if !text.trim().is_empty() {
                    blocks.push(Block::Text(std::mem::take(&mut text)));
                }
                link = Some((dest_url.into_string(), String::new()));
            }
            Event::End(TagEnd::Paragraph)
            | Event::End(TagEnd::Heading(_))
            | Event::End(TagEnd::CodeBlock)
            | Event::End(TagEnd::BlockQuote(_))
            | Event::End(TagEnd::Item)
            | Event::End(TagEnd::Table) => {
                if let Some(kind) = open.pop() {
                    let value = text.trim_end().to_owned();
                    text.clear();
                    let block = match kind {
                        Open::Text => Block::Text(value),
                        Open::Heading(level) => Block::Heading { level, text: value },
                        Open::Code => Block::Code {
                            language: std::mem::take(&mut code_language),
                            text: value,
                        },
                        Open::Quote => Block::Quote(value),
                        Open::ListItem(depth) => Block::ListItem { depth, text: value },
                        Open::Table => Block::Table(value),
                    };
                    blocks.push(block);
                }
            }
            Event::End(TagEnd::List(_)) => list_depth = list_depth.saturating_sub(1),
            Event::End(TagEnd::Image) => {
                if let Some((url, alt)) = image.take() {
                    blocks.push(Block::Image { url, alt });
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some((url, text)) = link.take() {
                    blocks.push(Block::Link { url, text });
                }
            }
            Event::Text(value) | Event::Code(value) => {
                if let Some((_, alt)) = &mut image {
                    alt.push_str(&value);
                } else if let Some((_, label)) = &mut link {
                    label.push_str(&value);
                } else {
                    text.push_str(&value);
                }
            }
            Event::SoftBreak => text.push(' '),
            Event::HardBreak => text.push('\n'),
            Event::Rule => blocks.push(Block::Rule),
            Event::TaskListMarker(checked) => {
                text.push_str(if checked { "[x] " } else { "[ ] " });
            }
            Event::Html(value) | Event::InlineHtml(value) => text.push_str(&value),
            Event::FootnoteReference(value) => {
                text.push('[');
                text.push_str(&value);
                text.push(']');
            }
            Event::InlineMath(value) | Event::DisplayMath(value) => text.push_str(&value),
            Event::Start(_) | Event::End(_) => {}
        }
    }

    if !text.trim().is_empty() {
        blocks.push(Block::Text(text));
    }
    blocks
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_rich_blocks_and_remote_images_separate() {
        let blocks = parse(
            "# Result\n\n- one\n  - two\n\n```rust\nlet x = 1;\n```\n\n[docs](https://example.com/docs)\n\n![chart](https://example.com/chart.png)",
        );

        assert!(matches!(blocks[0], Block::Heading { level: 1, .. }));
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::ListItem { depth: 2, .. }))
        );
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::Code { language, .. } if language == "rust"))
        );
        assert!(blocks.iter().any(|block| matches!(block, Block::Image { url, alt } if url == "https://example.com/chart.png" && alt == "chart")));
        assert!(blocks.iter().any(|block| matches!(block, Block::Link { url, text } if url == "https://example.com/docs" && text == "docs")));
    }
}
