//! Where the list of plugins comes from.
//!
//! Two sources, and the relationship between them is the point.
//! [`discover`](super::discover) walks `.aphid/plugins` and turns every file it
//! finds into an entry, so dropping a `.rhai` in there still loads it and
//! nobody has to write anything down. `.aphid/plugins.json` is where you
//! **override**: switch one off, isolate a service for it, configure it, or
//! name a file that lives somewhere else.
//!
//! That is the same relationship `.aphid/plugins/<name>.json` already has with
//! the `.rhai` file it configures. Replacing discovery would mean declaring
//! every plugin twice, which is the sort of bookkeeping that is wrong the first
//! time somebody forgets.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use aphid_agent::rt::{Component, Composition, Entry, Isolate, Resolver};
use serde::Deserialize;
use serde_json::Value;

use super::component::ScriptComponent;
use super::discover::PluginFile;
use super::host::PluginHost;

/// The name of the composition file, beside the plugin directory.
pub const FILE_NAME: &str = "plugins.json";

/// One row of `.aphid/plugins.json`.
///
/// Everything but `id` is optional, because the common override is one field.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// Which plugin this is about. Matches the name discovery gave the file.
    pub id: String,
    /// Where to load it from, if not where discovery would look.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub config: Option<Value>,
    #[serde(default)]
    pub disabled: bool,
    /// Per-service realm assignment: `true` for a realm of this entry's own,
    /// a string for one shared with everyone naming it.
    #[serde(default)]
    pub isolate: BTreeMap<String, IsolateSpec>,
}

/// `true` or a realm name.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum IsolateSpec {
    Own(bool),
    Shared(String),
}

impl IsolateSpec {
    fn resolve(&self) -> Option<Isolate> {
        match self {
            IsolateSpec::Own(true) => Some(Isolate::Local),
            IsolateSpec::Own(false) => None,
            IsolateSpec::Shared(name) => Some(Isolate::Shared(name.clone())),
        }
    }
}

/// Read the composition file beside a plugin directory.
///
/// A missing file is not a problem — it is the ordinary case, and it means
/// "whatever discovery finds, unchanged". A malformed one is worth saying out
/// loud, and worth not guessing about: the rows are ignored and the reason
/// returned.
///
/// # Errors
///
/// A message naming what could not be parsed.
pub fn read(root: &Path) -> Result<Vec<Row>, String> {
    let path = root.join(".aphid").join(FILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

/// Turn discovered files and override rows into one list of entries.
///
/// Order is discovery's, then anything named only in the file — so the list a
/// person reads matches the directory they are looking at, with their own
/// additions after it.
#[must_use]
pub fn compose(files: &[PluginFile], rows: &[Row]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = files
        .iter()
        .map(|file| Entry::new(&file.name, file.path.to_string_lossy()))
        .collect();

    for row in rows {
        let overridden = overlay(row);
        match entries.iter_mut().find(|entry| entry.id == row.id) {
            Some(entry) => {
                if let Some(url) = &row.url {
                    entry.url.clone_from(url);
                }
                entry.config = overridden.config;
                entry.disabled = row.disabled;
                entry.isolate = overridden.isolate;
            }
            // A row naming a file discovery did not find is a plugin from
            // somewhere else, which is one of the reasons to write the file.
            None => {
                if let Some(url) = &row.url {
                    let mut entry = Entry::new(&row.id, url);
                    entry.config = overridden.config;
                    entry.disabled = row.disabled;
                    entry.isolate = overridden.isolate;
                    entries.push(entry);
                }
            }
        }
    }

    entries
}

fn overlay(row: &Row) -> Entry {
    let mut entry = Entry::new(&row.id, row.url.clone().unwrap_or_default());
    entry.config = row.config.clone().unwrap_or(Value::Null);
    entry.isolate = row
        .isolate
        .iter()
        .filter_map(|(key, spec)| spec.resolve().map(|isolate| (key.clone(), isolate)))
        .collect();
    entry
}

/// Compiles an entry's `url` into a mounted script.
pub struct Scripts {
    host: Arc<PluginHost>,
    composition: Composition,
}

impl Scripts {
    #[must_use]
    pub fn new(host: Arc<PluginHost>, composition: &Composition) -> Self {
        Self {
            host,
            composition: composition.clone(),
        }
    }
}

impl Resolver for Scripts {
    fn resolve(&self, entry: &Entry) -> Result<Arc<dyn Component>, String> {
        let plugin = self
            .host
            .plugins()
            .iter()
            .find(|plugin| plugin.name() == entry.id)
            .ok_or_else(|| format!("`{}` did not compile, or was not loaded", entry.id))?;

        Ok(Arc::new(ScriptComponent::new(
            Arc::clone(plugin),
            &self.composition,
        )))
    }
}
