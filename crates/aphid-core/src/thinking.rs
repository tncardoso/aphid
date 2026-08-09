//! Reasoning effort levels and how they map onto provider wire values.

use compact_str::CompactString;

/// How much reasoning to ask a model for.
#[derive(
    Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingLevel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::XHigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }

    /// Collapse the levels above `High` onto `High`.
    ///
    /// Token-budget providers only distinguish four levels; `xhigh` and `max`
    /// are aphid-side vocabulary for APIs that accept named efforts.
    #[must_use]
    pub const fn clamped(self) -> ThinkingLevel {
        match self {
            ThinkingLevel::XHigh | ThinkingLevel::Max => ThinkingLevel::High,
            other => other,
        }
    }
}

/// A model's configured thinking level, including "not thinking".
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub enum ModelThinkingLevel {
    Off,
    Level(ThinkingLevel),
}

impl ModelThinkingLevel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ModelThinkingLevel::Off => "off",
            ModelThinkingLevel::Level(level) => level.as_str(),
        }
    }

    const fn index(self) -> usize {
        match self {
            ModelThinkingLevel::Off => 0,
            ModelThinkingLevel::Level(ThinkingLevel::Minimal) => 1,
            ModelThinkingLevel::Level(ThinkingLevel::Low) => 2,
            ModelThinkingLevel::Level(ThinkingLevel::Medium) => 3,
            ModelThinkingLevel::Level(ThinkingLevel::High) => 4,
            ModelThinkingLevel::Level(ThinkingLevel::XHigh) => 5,
            ModelThinkingLevel::Level(ThinkingLevel::Max) => 6,
        }
    }
}

impl From<ThinkingLevel> for ModelThinkingLevel {
    fn from(level: ThinkingLevel) -> Self {
        ModelThinkingLevel::Level(level)
    }
}

/// What a model does with one thinking level.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum LevelMapping {
    /// Send the level's canonical name.
    #[default]
    Default,
    /// The model rejects this level.
    Unsupported,
    /// Send this provider-specific value instead.
    Value(CompactString),
}

/// Per-level wire values for one model.
///
/// A fixed array indexed by level rather than a map: seven slots, no hashing,
/// no allocation.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ThinkingLevelMap([LevelMapping; 7]);

impl ThinkingLevelMap {
    /// Every level mapped to its canonical name.
    #[must_use]
    pub fn all_default() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn get(&self, level: ModelThinkingLevel) -> &LevelMapping {
        &self.0[level.index()]
    }

    pub fn set(&mut self, level: ModelThinkingLevel, mapping: LevelMapping) {
        self.0[level.index()] = mapping;
    }

    #[must_use]
    pub fn with(mut self, level: ModelThinkingLevel, mapping: LevelMapping) -> Self {
        self.set(level, mapping);
        self
    }

    /// Whether the model accepts this level at all.
    #[must_use]
    pub fn supports(&self, level: ModelThinkingLevel) -> bool {
        !matches!(self.get(level), LevelMapping::Unsupported)
    }

    /// The value to put on the wire, or `None` when the level is unsupported.
    #[must_use]
    pub fn resolve(&self, level: ModelThinkingLevel) -> Option<&str> {
        match self.get(level) {
            LevelMapping::Default => Some(level.as_str()),
            LevelMapping::Unsupported => None,
            LevelMapping::Value(v) => Some(v.as_str()),
        }
    }
}

/// Token budgets per level, for providers that take a number rather than a name.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ThinkingBudgets([Option<u32>; 4]);

impl ThinkingBudgets {
    /// Tokens always reserved for the answer when reasoning shares the response
    /// ceiling.
    pub const MIN_ANSWER_TOKENS: u32 = 1024;

    #[must_use]
    pub const fn new() -> Self {
        Self([None; 4])
    }

    #[must_use]
    pub fn get(&self, level: ThinkingLevel) -> Option<u32> {
        self.0[level.clamped() as usize]
    }

    pub fn set(&mut self, level: ThinkingLevel, budget: u32) {
        self.0[level.clamped() as usize] = Some(budget);
    }

    #[must_use]
    pub fn with(mut self, level: ThinkingLevel, budget: u32) -> Self {
        self.set(level, budget);
        self
    }

    /// The configured budget, falling back to the built-in ladder.
    #[must_use]
    pub fn resolve(&self, level: ThinkingLevel) -> u32 {
        self.get(level).unwrap_or(match level.clamped() {
            ThinkingLevel::Minimal => 1024,
            ThinkingLevel::Low => 2048,
            ThinkingLevel::Medium => 8192,
            _ => 16384,
        })
    }
}

impl Default for ThinkingBudgets {
    fn default() -> Self {
        Self::new()
    }
}
