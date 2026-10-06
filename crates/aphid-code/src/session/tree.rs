//! The shape of a session: its messages as a tree, read without replaying them.
//!
//! A [`Tree`] is what a listing or a tree view needs: who said what, in which
//! branch, and where the session was left. Reading one skims each line for
//! its id, parent, role and the start of its text. Tool arguments and images
//! are never decoded, so a list of every session in a workspace stays cheap.
//!
//! A [`TreeView`] is the same tree by turns, one prompt and what came of it,
//! which is how a person reads a conversation. It is plain data, so a daemon
//! can send it to a client that has no access to the file.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use aphid_core::{Role, Timestamp};
use serde::{Deserialize, Serialize};

use super::format::{HeadRecord, Header, LabelRecord};

/// How much of a message's text a node keeps.
const PREVIEW: usize = 240;

/// One message, as a tree view needs it.
#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    pub parent: Option<String>,
    pub role: Role,
    pub ts: Timestamp,
    /// The start of the message's text, on one line.
    pub preview: String,
    /// How many tool calls an assistant message makes.
    pub tool_calls: u16,
    /// Indexes of the messages that follow this one, oldest first.
    pub children: Vec<usize>,
}

impl Node {
    /// Whether a branch can start right after this message: a system prompt,
    /// or an assistant message that ends its turn. A fork between a tool call
    /// and its result would leave a call with no answer.
    #[must_use]
    pub fn ends_turn(&self) -> bool {
        match self.role {
            Role::System => true,
            Role::Assistant => self.tool_calls == 0,
            Role::User | Role::ToolResult => false,
        }
    }
}

/// A session's messages as a tree.
#[derive(Clone, Debug)]
pub struct Tree {
    pub header: Header,
    /// Every message, in the order it was written.
    pub nodes: Vec<Node>,
    /// Where the session continues.
    pub head: Option<String>,
    /// The last name given to each branch, by the node it starts at.
    pub labels: HashMap<String, String>,
    index: HashMap<String, usize>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Skim {
    Session(Box<Header>),
    Message(SkimRecord),
    Head(HeadRecord),
    Label(LabelRecord),
}

#[derive(Deserialize)]
struct SkimRecord {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    role: Role,
    ts: Timestamp,
    #[serde(default)]
    content: Vec<SkimBlock>,
}

/// A content block, keeping only what a preview shows. Fields that are not
/// named here (image data, tool arguments) are skipped by the parser.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SkimBlock {
    Text {
        text: String,
    },
    ToolCall {},
    #[serde(other)]
    Other,
}

impl Tree {
    /// Skim a session file.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or has no header. Lines that do not
    /// parse are skipped, as [`super::store::read`] does.
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let mut header = None;
        let mut nodes: Vec<Node> = Vec::new();
        let mut head = None;
        let mut labels = HashMap::new();
        let mut index = HashMap::new();

        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Skim>(&line) {
                Ok(Skim::Session(found)) => header = Some(*found),
                Ok(Skim::Message(record)) => {
                    let (id, parent) = match record.id {
                        Some(id) => (id, record.parent),
                        // Written before sessions were trees: a line, in order.
                        None => (
                            nodes.len().to_string(),
                            nodes.last().map(|last| last.id.clone()),
                        ),
                    };
                    let mut preview = String::new();
                    let mut tool_calls = 0u16;
                    for block in &record.content {
                        match block {
                            SkimBlock::Text { text } if preview.is_empty() => {
                                preview = one_line(text, PREVIEW);
                            }
                            SkimBlock::ToolCall {} => tool_calls = tool_calls.saturating_add(1),
                            SkimBlock::Text { .. } | SkimBlock::Other => {}
                        }
                    }
                    head = Some(id.clone());
                    index.insert(id.clone(), nodes.len());
                    nodes.push(Node {
                        id,
                        parent,
                        role: record.role,
                        ts: record.ts,
                        preview,
                        tool_calls,
                        children: Vec::new(),
                    });
                }
                Ok(Skim::Head(moved)) => head = moved.at,
                Ok(Skim::Label(label)) => {
                    labels.insert(label.node, label.text);
                }
                Err(_) => continue,
            }
        }

        for child in 0..nodes.len() {
            if let Some(parent) = nodes[child]
                .parent
                .as_deref()
                .and_then(|parent| index.get(parent).copied())
            {
                nodes[parent].children.push(child);
            }
        }

        let header = header.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} has no session header", path.display()),
            )
        })?;
        Ok(Self {
            header,
            nodes,
            head,
            labels,
            index,
        })
    }

    /// The message with this id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Node> {
        self.index.get(id).map(|index| &self.nodes[*index])
    }

    fn parent_of(&self, index: usize) -> Option<usize> {
        self.nodes[index]
            .parent
            .as_deref()
            .and_then(|parent| self.index.get(parent).copied())
    }

    /// The session's name: the label of its first branch, or its first prompt.
    #[must_use]
    pub fn title(&self) -> String {
        self.view().title
    }

    /// The tree by turns.
    #[must_use]
    pub fn view(&self) -> TreeView {
        // A turn starts at a prompt. Everything up to the next prompt is the
        // agent's answer to it.
        let turn_of = |mut index: usize| -> Option<usize> {
            for _ in 0..=self.nodes.len() {
                if self.nodes[index].role == Role::User {
                    return Some(index);
                }
                index = self.parent_of(index)?;
            }
            None
        };

        let mut turns = Vec::new();
        let mut at = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if node.role != Role::User {
                continue;
            }
            let parent = self
                .parent_of(index)
                .and_then(turn_of)
                .map(|parent| self.nodes[parent].id.clone());

            // The answer: follow the newest message that is not a prompt.
            let mut reply = String::new();
            let mut tool_calls = 0u16;
            let mut cursor = index;
            let mut end = None;
            loop {
                let next = self.nodes[cursor]
                    .children
                    .iter()
                    .rev()
                    .copied()
                    .find(|child| self.nodes[*child].role != Role::User);
                let Some(next) = next else { break };
                let child = &self.nodes[next];
                tool_calls = tool_calls.saturating_add(child.tool_calls);
                if child.role == Role::Assistant && !child.preview.is_empty() {
                    reply.clone_from(&child.preview);
                }
                cursor = next;
            }
            if cursor != index && self.nodes[cursor].ends_turn() {
                end = Some(self.nodes[cursor].id.clone());
            }

            at.insert(node.id.clone(), turns.len());
            turns.push(Turn {
                id: node.id.clone(),
                parent,
                prompt: node.preview.clone(),
                reply,
                tool_calls,
                end,
                label: self.labels.get(&node.id).cloned(),
                ts: node.ts,
                on_head: false,
                is_head: false,
                running: false,
            });
        }

        // The turn the head is in, and every turn above it.
        let head_turn = self
            .head
            .as_deref()
            .and_then(|head| self.index.get(head).copied())
            .and_then(turn_of)
            .map(|index| self.nodes[index].id.clone());
        let mut cursor = head_turn.clone();
        let mut first = true;
        while let Some(id) = cursor {
            let Some(&turn) = at.get(&id) else { break };
            turns[turn].on_head = true;
            turns[turn].is_head = first;
            first = false;
            cursor = turns[turn].parent.clone();
        }

        let title = turns
            .iter()
            .find(|turn| turn.parent.is_none())
            .map(|turn| turn.label.clone().unwrap_or_else(|| turn.prompt.clone()))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "(empty)".to_owned());

        TreeView {
            session: self.header.id.clone(),
            title,
            started: self.header.started,
            head: self.head.clone(),
            turns,
        }
    }
}

/// A session by turns: what a tree view draws.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeView {
    /// The session's id.
    pub session: String,
    pub title: String,
    pub started: Timestamp,
    /// The message the session continues from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Every turn, in the order its prompt was written. A parent always comes
    /// before its children.
    pub turns: Vec<Turn>,
}

/// One prompt and the agent's answer to it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// The prompt's message id.
    pub id: String,
    /// The turn this one follows; `None` for a first prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub prompt: String,
    /// The start of the agent's last words in the turn.
    #[serde(default)]
    pub reply: String,
    #[serde(default)]
    pub tool_calls: u16,
    /// The message that ends the turn, where a branch can continue from. `None`
    /// while the turn has no final answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    /// The name given to the branch that starts here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub ts: Timestamp,
    /// On the branch the session continues.
    #[serde(default)]
    pub on_head: bool,
    /// The turn the session continues from.
    #[serde(default)]
    pub is_head: bool,
    /// A run is adding to it now. Set by whoever knows, not by the file.
    #[serde(default)]
    pub running: bool,
}

impl TreeView {
    /// The turn with this prompt id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Turn> {
        self.turns.iter().find(|turn| turn.id == id)
    }

    /// The turns that follow `parent`; `None` for the first prompts.
    pub fn children<'a>(&'a self, parent: Option<&'a str>) -> impl Iterator<Item = &'a Turn> {
        self.turns
            .iter()
            .filter(move |turn| turn.parent.as_deref() == parent)
    }

    /// Where the branch the head is on starts: the first turn above the head
    /// that has a sibling, or the first turn of all. A name for the current
    /// branch goes there.
    #[must_use]
    pub fn branch_start(&self) -> Option<&Turn> {
        let head = self.turns.iter().find(|turn| turn.is_head)?;
        self.branch_start_of(&head.id)
    }

    /// Where the branch that holds the turn `id` starts: the first turn above
    /// it, itself included, that has a sibling, or the first turn of all.
    #[must_use]
    pub fn branch_start_of(&self, id: &str) -> Option<&Turn> {
        let mut turn = self.get(id)?;
        loop {
            let siblings = self.children(turn.parent.as_deref()).count();
            if siblings > 1 {
                return Some(turn);
            }
            match turn.parent.as_deref().and_then(|parent| self.get(parent)) {
                Some(parent) => turn = parent,
                None => return Some(turn),
            }
        }
    }

    /// What to call a turn: its branch's name, or its prompt.
    #[must_use]
    pub fn name(turn: &Turn) -> &str {
        turn.label.as_deref().unwrap_or(&turn.prompt)
    }
}

/// `text` on one line, cut to `limit` characters.
fn one_line(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for (count, ch) in text
        .split_whitespace()
        .flat_map(|word| std::iter::once(' ').chain(word.chars()))
        .skip(1)
        .enumerate()
    {
        if count == limit {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}
