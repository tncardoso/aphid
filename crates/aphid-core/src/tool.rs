//! Tool definitions offered to a model.

use compact_str::CompactString;

use crate::json::Json;

/// How hard to push the provider to constrain sampling to a tool's schema.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Strictness {
    /// Use constrained sampling where the endpoint supports it.
    Prefer,
    /// Fail rather than send an unconstrained request.
    Require,
}

/// Grammar dialects a provider may accept.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum GrammarFormat {
    OpenAiLark,
    OpenAiRegex,
}

/// The same intended language, encoded for each dialect that can express it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct GrammarVariants {
    pub lark: Option<String>,
    pub regex: Option<String>,
}

impl GrammarVariants {
    #[must_use]
    pub fn get(&self, format: GrammarFormat) -> Option<&str> {
        match format {
            GrammarFormat::OpenAiLark => self.lark.as_deref(),
            GrammarFormat::OpenAiRegex => self.regex.as_deref(),
        }
    }
}

/// Provider-side constrained sampling for one tool.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConstrainedSampling {
    /// Constrain output to the tool's JSON Schema.
    JsonSchema { strict: Strictness },
    /// Constrain output to a grammar.
    Grammar { variants: GrammarVariants },
}

/// A tool the model may call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Tool {
    pub name: CompactString,
    pub description: String,
    /// A JSON Schema document describing the arguments.
    ///
    /// Kept as raw JSON: it is authored once and forwarded verbatim, so there is
    /// nothing to gain from a typed schema tree in the core.
    pub parameters: Json,
    /// `None` leaves sampling unconstrained.
    pub constrained_sampling: Option<ConstrainedSampling>,
}

impl Tool {
    #[must_use]
    pub fn new(
        name: impl Into<CompactString>,
        description: impl Into<String>,
        parameters: Json,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            constrained_sampling: None,
        }
    }

    #[must_use]
    pub fn constrained(mut self, constrained_sampling: ConstrainedSampling) -> Self {
        self.constrained_sampling = Some(constrained_sampling);
        self
    }
}
