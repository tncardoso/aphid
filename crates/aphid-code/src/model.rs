//! The models `/model` and `--model` can choose between.

use aphid_core::catalog::{self, ModelEntry};
use aphid_core::{Model, ModelThinkingLevel, ThinkingLevel};

/// The models aphid knows about.
///
/// One source: the user's own `~/.aphid/models.json`. The coding agent ships no
/// default models, so a fresh install is empty until the user runs
/// `aphid models add`.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    models: Vec<Model>,
    diagnostics: Vec<String>,
}

/// Why a name did not resolve to exactly one model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// Nothing matched. Carries every id, so the caller can list them.
    Unknown { candidates: Vec<String> },
    /// Several matched. Carries the ones that did.
    Ambiguous { matches: Vec<String> },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Unknown { candidates } => {
                write!(f, "no such model. Available: {}", candidates.join(", "))
            }
            ResolveError::Ambiguous { matches } => {
                write!(f, "ambiguous, matches: {}", matches.join(", "))
            }
        }
    }
}

impl std::error::Error for ResolveError {}

impl Catalog {
    /// The models in `~/.aphid/models.json`.
    ///
    /// Never fails. A catalog file that cannot be read leaves a diagnostic and
    /// an empty catalog behind — a typo in a config file should not stop the
    /// agent from starting, but it must not fall back to a model the user did
    /// not configure.
    #[must_use]
    pub fn new() -> Self {
        let Some(path) = catalog::config_path() else {
            return Self::from_parts(&[]);
        };
        match catalog::load(&path) {
            Ok(config) => Self::from_parts(config.models()),
            Err(error) => {
                let mut catalog = Self::from_parts(&[]);
                catalog.diagnostics.push(error.to_string());
                catalog
            }
        }
    }

    /// Build a catalog from config entries.
    ///
    /// The seam tests use, so none of them has to move `$HOME`. An entry that
    /// cannot become a [`Model`] is reported and skipped, not fatal. A duplicate
    /// id replaces the earlier entry and keeps its place, so the first model in
    /// the file stays the default.
    #[must_use]
    pub fn from_parts(entries: &[ModelEntry]) -> Self {
        let mut catalog = Self {
            models: Vec::new(),
            diagnostics: Vec::new(),
        };
        for entry in entries {
            match Model::try_from(entry) {
                Ok(model) => match catalog.models.iter_mut().find(|held| held.id == model.id) {
                    Some(held) => *held = model,
                    None => catalog.models.push(model),
                },
                Err(error) => catalog.diagnostics.push(error.to_string()),
            }
        }
        catalog
    }

    #[must_use]
    pub fn models(&self) -> &[Model] {
        &self.models
    }

    /// What went wrong loading the catalog, if anything. Worth surfacing: a
    /// model that silently does not exist is worse than one that reports why.
    #[must_use]
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// The model used when none was asked for: the first configured model.
    #[must_use]
    pub fn default_model(&self) -> Option<Model> {
        self.models.first().cloned()
    }

    /// Resolve a user-supplied name.
    ///
    /// Tried in order: the exact id, then a trailing segment (`pro` matches
    /// `deepseek-v4-pro`), then a prefix. The first stage to match anything
    /// decides — so an exact id is never shadowed by a prefix.
    ///
    /// # Errors
    ///
    /// Fails when nothing matched, or when more than one thing did.
    pub fn resolve(&self, name: &str) -> Result<Model, ResolveError> {
        let name = name.trim();

        if let Some(model) = self.models.iter().find(|model| model.id == name) {
            return Ok(model.clone());
        }

        let suffix = format!("-{name}");
        let by_segment: Vec<&Model> = self
            .models
            .iter()
            .filter(|model| model.id.ends_with(&suffix))
            .collect();
        if let Some(model) = single(&by_segment)? {
            return Ok(model);
        }

        let by_prefix: Vec<&Model> = self
            .models
            .iter()
            .filter(|model| model.id.starts_with(name))
            .collect();
        if let Some(model) = single(&by_prefix)? {
            return Ok(model);
        }

        Err(ResolveError::Unknown {
            candidates: self.ids(),
        })
    }

    /// Position of a model in the catalog, for a picker's selected row.
    #[must_use]
    pub fn position(&self, id: &str) -> Option<usize> {
        self.models.iter().position(|model| model.id == id)
    }

    /// The next model after `id`, wrapping. What Ctrl-P cycles through.
    #[must_use]
    pub fn next_after(&self, id: &str) -> Option<Model> {
        if self.models.is_empty() {
            return None;
        }
        let at = self
            .position(id)
            .map_or(0, |at| (at + 1) % self.models.len());
        self.models.get(at).cloned()
    }

    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.models
            .iter()
            .map(|model| model.id.to_string())
            .collect()
    }
}

/// `Ok(None)` means "nothing matched at this stage, try the next one".
fn single(matches: &[&Model]) -> Result<Option<Model>, ResolveError> {
    match matches {
        [] => Ok(None),
        [only] => Ok(Some((*only).clone())),
        many => Err(ResolveError::Ambiguous {
            matches: many.iter().map(|model| model.id.to_string()).collect(),
        }),
    }
}

/// Fit a thinking level to what a model can actually serve.
///
/// Models map aphid's ladder differently, and some serve none of it. Returns the
/// level to use, or `None` when the model cannot think at all — with a note when
/// the answer differs from what was asked for.
#[must_use]
pub fn clamp_thinking(
    model: &Model,
    wanted: Option<ThinkingLevel>,
) -> (Option<ThinkingLevel>, Option<String>) {
    let Some(wanted) = wanted else {
        return (None, None);
    };

    if !model.reasoning {
        return (
            None,
            Some(format!("{} does not support thinking", model.id)),
        );
    }

    if model
        .thinking_levels
        .supports(ModelThinkingLevel::Level(wanted))
    {
        return (Some(wanted), None);
    }

    // Step down until the model has something to offer.
    let ladder = [
        ThinkingLevel::Max,
        ThinkingLevel::XHigh,
        ThinkingLevel::High,
        ThinkingLevel::Medium,
        ThinkingLevel::Low,
        ThinkingLevel::Minimal,
    ];
    let fallback = ladder
        .into_iter()
        .filter(|level| *level <= wanted)
        .find(|level| {
            model
                .thinking_levels
                .supports(ModelThinkingLevel::Level(*level))
        });

    match fallback {
        Some(level) => (
            Some(level),
            Some(format!(
                "{} does not support thinking {}, using {}",
                model.id,
                wanted.as_str(),
                level.as_str()
            )),
        ),
        None => (
            None,
            Some(format!(
                "{} does not support thinking {}",
                model.id,
                wanted.as_str()
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal hand-written entry, the shape a user would actually type.
    fn entry(id: &str) -> ModelEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "base_url": "http://localhost:8080/v1",
            "context_window": 32768,
            "max_tokens": 4096,
        }))
        .expect("a valid entry")
    }

    /// An entry that can think, for the thinking-clamp tests.
    fn reasoning_entry(id: &str) -> ModelEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "base_url": "http://localhost:8080/v1",
            "context_window": 32768,
            "max_tokens": 4096,
            "reasoning": true,
        }))
        .expect("a valid entry")
    }

    /// A small config-only catalog. Tests must not read the real
    /// `~/.aphid/models.json`, or they would pass or fail depending on whose
    /// machine they run on.
    fn catalog() -> Catalog {
        Catalog::from_parts(&[entry("deepseek-v4-flash"), entry("deepseek-v4-pro")])
    }

    #[test]
    fn an_exact_id_resolves() {
        let catalog = catalog();
        assert_eq!(
            catalog.resolve("deepseek-v4-pro").unwrap().id,
            "deepseek-v4-pro"
        );
    }

    #[test]
    fn a_trailing_segment_resolves() {
        let catalog = catalog();
        assert_eq!(catalog.resolve("pro").unwrap().id, "deepseek-v4-pro");
        assert_eq!(catalog.resolve("flash").unwrap().id, "deepseek-v4-flash");
    }

    #[test]
    fn an_unknown_name_lists_the_candidates() {
        let catalog = catalog();
        let error = catalog.resolve("gpt-9").unwrap_err();
        let ResolveError::Unknown { candidates } = error else {
            panic!("expected Unknown, got {error:?}");
        };
        assert!(candidates.contains(&"deepseek-v4-pro".to_owned()));
    }

    #[test]
    fn an_ambiguous_prefix_is_refused() {
        let catalog = catalog();
        // Every configured model shares this prefix.
        let error = catalog.resolve("deepseek").unwrap_err();
        let ResolveError::Ambiguous { matches } = error else {
            panic!("expected Ambiguous, got {error:?}");
        };
        assert!(matches.len() >= 2);
    }

    #[test]
    fn cycling_wraps_around_the_catalog() {
        let catalog = catalog();
        let first = catalog.default_model().expect("a non-empty catalog");
        let mut id = first.id.to_string();
        for _ in 0..catalog.models().len() {
            id = catalog
                .next_after(&id)
                .expect("a next model")
                .id
                .to_string();
        }
        assert_eq!(id, first.id, "a full cycle returns to the start");
    }

    #[test]
    fn an_empty_catalog_has_no_default_model() {
        let catalog = Catalog::from_parts(&[]);
        assert!(catalog.models().is_empty());
        assert!(catalog.default_model().is_none());
        assert!(catalog.next_after("anything").is_none());
    }

    #[test]
    fn configured_models_keep_file_order() {
        let catalog = Catalog::from_parts(&[entry("local-llama"), entry("second")]);
        assert_eq!(catalog.models().len(), 2);
        assert_eq!(catalog.resolve("local-llama").unwrap().id, "local-llama");
        assert!(catalog.diagnostics().is_empty());

        // The first configured model is the default.
        assert_eq!(catalog.default_model().unwrap().id, "local-llama");
    }

    #[test]
    fn a_duplicate_entry_replaces_the_earlier_one() {
        let mut replacement = entry("duplicate");
        replacement.base_url = "http://localhost:9999/v1".to_owned();

        let catalog = Catalog::from_parts(&[entry("duplicate"), replacement.clone()]);
        assert_eq!(
            catalog.models().len(),
            1,
            "it replaced rather than appended"
        );

        let replaced = catalog.resolve("duplicate").unwrap();
        assert_eq!(replaced.base_url, "http://localhost:9999/v1");
        assert_eq!(
            catalog.position("duplicate"),
            Some(0),
            "and it kept its place, so the default model is unchanged"
        );
    }

    #[test]
    fn a_broken_entry_is_reported_and_skipped() {
        // A missing `base_url` cannot be guessed, but it must not take the rest
        // of the catalog down with it.
        let mut broken = entry("no-endpoint");
        broken.base_url = String::new();

        let catalog = Catalog::from_parts(&[broken, entry("fine")]);
        assert_eq!(catalog.diagnostics().len(), 1);
        assert!(catalog.diagnostics()[0].contains("no-endpoint"));
        assert!(catalog.resolve("no-endpoint").is_err());
        assert!(
            catalog.resolve("fine").is_ok(),
            "the entries after a broken one still load"
        );
    }

    #[test]
    fn thinking_is_clamped_to_what_the_model_serves() {
        let catalog = Catalog::from_parts(&[reasoning_entry("flash")]);
        let model = catalog.resolve("flash").unwrap();

        // A reasoning model with default thinking levels maps the whole ladder,
        // so nothing is clamped.
        let (level, note) = clamp_thinking(&model, Some(ThinkingLevel::High));
        assert_eq!(level, Some(ThinkingLevel::High));
        assert!(note.is_none());

        // No request, no level and nothing to say about it.
        let (level, note) = clamp_thinking(&model, None);
        assert!(level.is_none());
        assert!(note.is_none());
    }
}
