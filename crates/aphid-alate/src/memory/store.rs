//! The facts on disk.
//!
//! One file for each path, so `/projects/aphid` is `memory/projects/aphid.md`,
//! and one bullet for each fact:
//!
//! ```text
//! # /projects/aphid
//!
//! - 2026-08-11 — The plugin API stays as small as it can be.
//! - 2026-08-11 — Docs are in simplified technical English; comments are not.
//! ```
//!
//! There is no index. For the hundreds of facts one agent writes, reading the
//! whole tree costs a fraction of a millisecond, and an index that can disagree
//! with the files is worse than a walk that cannot. A memory that outgrows a
//! walk wants a database, not an index bolted on here.
//!
//! Recall weighs a word by how rare it is across the memory, so a question that
//! shares one unusual word with a fact ranks it above one that shares three
//! ordinary ones. Equal answers come back newest first.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{Error, Hit};

/// How many facts a memory must hold before a common word is ignored.
///
/// Below this, "the ceiling" would throw away the only word a three-fact memory
/// has in common with the question.
const CEILING_FROM: usize = 8;

/// The share of the facts a word may reach before it stops telling one from
/// another. A ceiling on how far a word may reach, for that reason: a
/// question holding "the" would otherwise drag the whole memory into the answer.
const CEILING: f64 = 0.5;

/// The shortest word worth matching on.
const MIN_WORD: usize = 2;

/// Facts kept as markdown under one directory.
pub struct Memory {
    root: PathBuf,
}

/// One fact, as read off the disk.
struct Entry {
    path: String,
    fact: String,
    /// The date the bullet carries, for ordering. Facts written before dates
    /// were recorded sort last, which is where the oldest belong anyway.
    date: String,
}

impl Memory {
    /// Open the memory under `root`, creating the directory if it is absent.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created.
    pub fn open(root: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(root).map_err(|source| Error::io(root, source))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Write one fact under one path.
    ///
    /// # Errors
    ///
    /// Fails when the path is not one a memory may hold, or when the file
    /// cannot be written.
    pub fn store(&mut self, path: &str, fact: &str) -> Result<(), Error> {
        let path = super::normalise(path)?;
        let file = self.file_for(&path);

        let fact = one_line(fact);
        if fact.is_empty() {
            return Err(Error::Fact("a fact cannot be empty".to_owned()));
        }

        // Read, add, write whole: the file is a few kilobytes, and going
        // through `write_atomically` means a daemon killed mid-write leaves the
        // memory it had rather than half of it.
        let mut text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                format!("# {path}\n\n")
            }
            Err(source) => return Err(Error::io(&file, source)),
        };
        if !text.ends_with('\n') {
            text.push('\n');
        }

        let today = chrono::Utc::now().format("%Y-%m-%d");
        text.push_str(&format!("- {today} — {fact}\n"));

        aphid_core::catalog::write_atomically(&file, &text)
            .map_err(|source| Error::io(&file, source))?;
        Ok(())
    }

    /// The facts that answer `query`, best first.
    ///
    /// An empty query answers with the newest facts, which is what "what do you
    /// know" means when nobody has narrowed it down.
    ///
    /// # Errors
    ///
    /// Fails when `path` is not one a memory may hold, or when the tree cannot
    /// be read.
    pub fn recall(
        &mut self,
        query: &str,
        path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Hit>, Error> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let under = path.map(super::normalise).transpose()?;
        let mut entries = self.entries()?;

        if let Some(under) = &under {
            entries.retain(|entry| {
                entry.path == *under || entry.path.starts_with(&format!("{under}/"))
            });
        }

        // Newest first, so it is both the answer to an empty query and the way
        // equal scores are broken below.
        entries.sort_by(|left, right| right.date.cmp(&left.date));

        let words = keep_words(query, &entries);
        if words.is_empty() {
            return Ok(entries
                .into_iter()
                .take(limit)
                .map(|entry| Hit {
                    path: entry.path,
                    fact: entry.fact,
                    score: 1.0,
                })
                .collect());
        }

        let total: f64 = words.values().sum();
        let mut hits: Vec<Hit> = entries
            .into_iter()
            .filter_map(|entry| {
                let held = tokens(&entry.fact);
                let matched: f64 = words
                    .iter()
                    .filter(|(word, _)| held.contains(*word))
                    .map(|(_, weight)| *weight)
                    .sum();
                (matched > 0.0).then(|| Hit {
                    path: entry.path,
                    fact: entry.fact,
                    score: matched / total,
                })
            })
            .collect();

        // A stable sort, over a list already in date order, so two facts that
        // answer equally well come back newest first.
        hits.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }

    /// Every path that holds a fact, in order.
    ///
    /// # Errors
    ///
    /// Fails when the tree cannot be read.
    pub fn paths(&self) -> Result<Vec<String>, Error> {
        let mut paths: Vec<String> = self
            .entries()?
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// The file one wiki path is kept in.
    fn file_for(&self, path: &str) -> PathBuf {
        let mut file = self.root.clone();
        for segment in path.trim_start_matches('/').split('/') {
            file.push(segment);
        }
        file.set_extension("md");
        file
    }

    /// Every fact in the tree.
    fn entries(&self) -> Result<Vec<Entry>, Error> {
        let mut files = Vec::new();
        collect(&self.root, &mut files)?;
        files.sort();

        let mut entries = Vec::new();
        for file in files {
            let text = match std::fs::read_to_string(&file) {
                Ok(text) => text,
                // A file that went while we walked is not worth failing over.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(Error::io(&file, source)),
            };
            let path = self.path_of(&file);
            for line in text.lines() {
                let Some(bullet) = line.trim().strip_prefix("- ") else {
                    continue;
                };
                let (date, fact) = split_date(bullet.trim());
                if fact.is_empty() {
                    continue;
                }
                entries.push(Entry {
                    path: path.clone(),
                    fact: fact.to_owned(),
                    date: date.to_owned(),
                });
            }
        }
        Ok(entries)
    }

    /// The wiki path a file holds, which is its place in the tree without the
    /// extension.
    fn path_of(&self, file: &Path) -> String {
        let relative = file.strip_prefix(&self.root).unwrap_or(file);
        let mut path = String::from("/");
        let parts: Vec<String> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        for (at, part) in parts.iter().enumerate() {
            if at > 0 {
                path.push('/');
            }
            if at + 1 == parts.len() {
                path.push_str(part.strip_suffix(".md").unwrap_or(part));
            } else {
                path.push_str(part);
            }
        }
        path
    }
}

/// Every `.md` file under `dir`, however deep.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(Error::io(dir, source)),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            collect(&path, out)?;
        } else if path.extension().is_some_and(|kind| kind == "md") {
            out.push(path);
        }
    }
    Ok(())
}

/// The query's words, each with the weight it carries, common ones dropped.
///
/// The weight is the inverse of how many facts hold the word, so a question
/// that shares one rare word with a fact ranks it above one that shares three
/// ordinary ones.
fn keep_words(query: &str, entries: &[Entry]) -> HashMap<String, f64> {
    let asked = tokens(query);
    if asked.is_empty() || entries.is_empty() {
        return HashMap::new();
    }

    let held: Vec<Vec<String>> = entries.iter().map(|entry| tokens(&entry.fact)).collect();
    let total = entries.len() as f64;
    let ceiling = entries.len() >= CEILING_FROM;

    let mut words = HashMap::new();
    for word in asked {
        let count = held.iter().filter(|fact| fact.contains(&word)).count();
        if count == 0 {
            continue;
        }
        if ceiling && count as f64 / total > CEILING {
            continue;
        }
        words.insert(word, (total / count as f64).ln() + 1.0);
    }
    words
}

/// The words of a text, folded so that `Memory` and `memory` are one word.
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= MIN_WORD)
        .map(str::to_lowercase)
        .collect()
}

/// A bullet split into the date it opens with and the rest.
fn split_date(bullet: &str) -> (&str, &str) {
    // `YYYY-MM-DD — `, which is what `store` writes. Anything else is all fact,
    // because a memory somebody edited by hand still has to be readable.
    let Some((head, tail)) = bullet.split_once(" — ") else {
        return ("", bullet);
    };
    let dated = head.len() == 10
        && head.chars().enumerate().all(|(at, c)| {
            if at == 4 || at == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        });
    if dated {
        (head, tail.trim())
    } else {
        ("", bullet)
    }
}

/// One fact on one line.
///
/// A bullet is a line, so a fact that arrived with newlines in it would become
/// several facts, most of them nonsense. Folding is the honest repair.
fn one_line(fact: &str) -> String {
    fact.split_whitespace().collect::<Vec<_>>().join(" ")
}
