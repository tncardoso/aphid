//! Model descriptions: identity, capability, limits and price.

use std::ops::BitOr;

use compact_str::CompactString;

use crate::compat::Compat;
use crate::json::Json;
use crate::provider::{Api, ProviderId};
use crate::thinking::ThinkingLevelMap;
use crate::usage::{Cost, Usage};

/// What a model accepts as input.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct InputModalities(u8);

impl InputModalities {
    pub const TEXT: InputModalities = InputModalities(1 << 0);
    pub const IMAGE: InputModalities = InputModalities(1 << 1);

    #[must_use]
    pub const fn contains(self, other: InputModalities) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for InputModalities {
    type Output = InputModalities;

    fn bitor(self, rhs: Self) -> Self {
        InputModalities(self.0 | rhs.0)
    }
}

/// Price per million tokens, in US dollars.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct ModelCostRates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// A pricing tier that kicks in above an input-size threshold.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ModelCostTier {
    /// Applies when total input tokens exceed this count.
    pub input_tokens_above: u32,
    pub rates: ModelCostRates,
}

/// What a model charges.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelCost {
    pub rates: ModelCostRates,
    /// Request-wide tiers, highest matching threshold wins.
    pub tiers: Vec<ModelCostTier>,
}

impl ModelCost {
    /// Flat pricing with no tiers.
    #[must_use]
    pub fn flat(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Self {
        Self {
            rates: ModelCostRates {
                input,
                output,
                cache_read,
                cache_write,
            },
            tiers: Vec::new(),
        }
    }

    /// The rates that apply to a request of this input size.
    #[must_use]
    pub fn rates_for(&self, input_tokens: u32) -> &ModelCostRates {
        self.tiers
            .iter()
            .filter(|t| input_tokens > t.input_tokens_above)
            .max_by_key(|t| t.input_tokens_above)
            .map_or(&self.rates, |t| &t.rates)
    }

    /// Price a completed request.
    #[must_use]
    pub fn cost_of(&self, usage: &Usage) -> Cost {
        const PER_MILLION: f64 = 1_000_000.0;
        let rates = self.rates_for(usage.billed_input());
        let input = f64::from(usage.input) * rates.input / PER_MILLION;
        let output = f64::from(usage.output) * rates.output / PER_MILLION;
        let cache_read = f64::from(usage.cache_read) * rates.cache_read / PER_MILLION;
        let cache_write = f64::from(usage.cache_write) * rates.cache_write / PER_MILLION;
        Cost {
            input,
            output,
            cache_read,
            cache_write,
            total: input + output + cache_read + cache_write,
        }
    }
}

/// Everything aphid needs to know to talk to one model.
#[derive(Clone, Debug)]
pub struct Model {
    pub id: CompactString,
    pub name: String,
    pub api: Api,
    pub provider: ProviderId,
    pub base_url: String,
    /// Whether the model reasons at all. When false, thinking options are
    /// ignored rather than rejected.
    pub reasoning: bool,
    pub thinking_levels: ThinkingLevelMap,
    pub input: InputModalities,
    pub cost: ModelCost,
    pub context_window: u32,
    pub max_tokens: u32,
    /// Default sampling parameters, overridden per request.
    pub sampling_params: Option<Json>,
    /// Extra headers this model requires.
    pub headers: Vec<(CompactString, String)>,
    pub compat: Compat,
}

impl Model {
    /// Tokens left for a response given how much input a request carries.
    ///
    /// Providers should clamp their `max_tokens` to this so a long context
    /// cannot produce a request that is rejected outright.
    #[must_use]
    pub fn available_output_tokens(&self, input_tokens: u32, safety: u32) -> u32 {
        self.context_window
            .saturating_sub(input_tokens)
            .saturating_sub(safety)
            .min(self.max_tokens)
    }
}
