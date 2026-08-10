//! The models `/model` and `--model` can choose between.

use aphid_core::{Model, ModelThinkingLevel, ThinkingLevel, providers::deepseek};

/// The models aphid knows about.
///
/// Today that is whatever `aphid-core` ships providers for. Adding a provider
/// there extends `/model` with no change here.
#[derive(Clone, Debug)]
pub struct Catalog {
    models: Vec<Model>,
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
    #[must_use]
    pub fn new() -> Self {
        Self {
            models: deepseek::models(),
        }
    }

    #[must_use]
    pub fn models(&self) -> &[Model] {
        &self.models
    }

    /// The model used when none was asked for.
    ///
    /// # Panics
    ///
    /// Panics if the catalog is empty, which would mean `aphid-core` ships no
    /// providers at all.
    #[must_use]
    pub fn default_model(&self) -> Model {
        self.models.first().expect("a non-empty catalog").clone()
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

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
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

    #[test]
    fn an_exact_id_resolves() {
        let catalog = Catalog::new();
        assert_eq!(
            catalog.resolve("deepseek-v4-pro").unwrap().id,
            "deepseek-v4-pro"
        );
    }

    #[test]
    fn a_trailing_segment_resolves() {
        let catalog = Catalog::new();
        assert_eq!(catalog.resolve("pro").unwrap().id, "deepseek-v4-pro");
        assert_eq!(catalog.resolve("flash").unwrap().id, "deepseek-v4-flash");
    }

    #[test]
    fn an_unknown_name_lists_the_candidates() {
        let catalog = Catalog::new();
        let error = catalog.resolve("gpt-9").unwrap_err();
        let ResolveError::Unknown { candidates } = error else {
            panic!("expected Unknown, got {error:?}");
        };
        assert!(candidates.contains(&"deepseek-v4-pro".to_owned()));
    }

    #[test]
    fn an_ambiguous_prefix_is_refused() {
        let catalog = Catalog::new();
        // Every deepseek model shares this prefix.
        let error = catalog.resolve("deepseek").unwrap_err();
        let ResolveError::Ambiguous { matches } = error else {
            panic!("expected Ambiguous, got {error:?}");
        };
        assert!(matches.len() >= 2);
    }

    #[test]
    fn cycling_wraps_around_the_catalog() {
        let catalog = Catalog::new();
        let first = catalog.default_model();
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
    fn thinking_is_clamped_to_what_the_model_serves() {
        let catalog = Catalog::new();
        let model = catalog.resolve("flash").unwrap();

        // DeepSeek maps the whole ladder, so nothing is clamped.
        let (level, note) = clamp_thinking(&model, Some(ThinkingLevel::High));
        assert_eq!(level, Some(ThinkingLevel::High));
        assert!(note.is_none());

        // No request, no level and nothing to say about it.
        let (level, note) = clamp_thinking(&model, None);
        assert!(level.is_none());
        assert!(note.is_none());
    }
}
