//! Wire-protocol and provider identity.

use std::fmt;
use std::str::FromStr;

use compact_str::CompactString;

/// The wire protocol a model speaks.
///
/// Only the OpenAI Chat Completions shape is implemented today. `Custom` keeps
/// OpenAI-compatible third-party endpoints — llama.cpp, vLLM, SGLang — nameable
/// without a code change.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Api {
    OpenAiCompletions,
    Custom(CompactString),
}

impl Api {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Api::OpenAiCompletions => "openai-completions",
            Api::Custom(name) => name.as_str(),
        }
    }
}

impl fmt::Display for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Api {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "openai-completions" => Api::OpenAiCompletions,
            other => Api::Custom(CompactString::new(other)),
        })
    }
}

/// Identifies a provider.
///
/// An open set rather than an enum: providers are configuration, and users add
/// their own.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ProviderId(CompactString);

impl ProviderId {
    pub const DEEPSEEK: ProviderId = ProviderId(CompactString::const_new("deepseek"));

    #[must_use]
    pub fn new(id: impl Into<CompactString>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}
