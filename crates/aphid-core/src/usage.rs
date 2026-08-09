//! Token and cost accounting for a request.

use std::ops::{Add, AddAssign};

/// Money spent on a single request, in US dollars.
#[derive(Copy, Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

impl AddAssign for Cost {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_read += rhs.cache_read;
        self.cache_write += rhs.cache_write;
        self.total += rhs.total;
    }
}

impl Add for Cost {
    type Output = Cost;

    fn add(mut self, rhs: Self) -> Self {
        self += rhs;
        self
    }
}

/// Tokens consumed by a request, plus what they cost.
#[derive(Copy, Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_write: u32,
    /// Subset of `cache_write` written with long retention. Only some providers
    /// report this split.
    pub cache_write_1h: Option<u32>,
    /// Reasoning tokens where the provider breaks them out. This is a *subset*
    /// of `output`, not an addition to it.
    pub reasoning: Option<u32>,
    pub total_tokens: u64,
    pub cost: Cost,
}

impl Usage {
    /// Tokens billed as input, across fresh and cached reads.
    #[must_use]
    pub const fn billed_input(&self) -> u32 {
        self.input + self.cache_read + self.cache_write
    }
}

impl AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_read += rhs.cache_read;
        self.cache_write += rhs.cache_write;
        self.cache_write_1h = sum_options(self.cache_write_1h, rhs.cache_write_1h);
        self.reasoning = sum_options(self.reasoning, rhs.reasoning);
        self.total_tokens += rhs.total_tokens;
        self.cost += rhs.cost;
    }
}

impl Add for Usage {
    type Output = Usage;

    fn add(mut self, rhs: Self) -> Self {
        self += rhs;
        self
    }
}

/// `None` means "this provider does not report the field", so it must not turn
/// an otherwise-reported total into `None`.
fn sum_options(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0) + y.unwrap_or(0)),
    }
}
