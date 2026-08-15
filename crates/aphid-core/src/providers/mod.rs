//! Concrete provider descriptions.
//!
//! These are plain constructors, not a catalog format: model metadata is
//! written out as constants so startup costs nothing and the numbers are
//! reviewable in a diff.
//!
//! A catalog format does exist, alongside rather than instead: [`crate::catalog`]
//! reads the user's own `~/.aphid/models.json`, and [`crate::models_dev`] fills
//! one in from models.dev. These constants back the `raw` and `agent` front
//! ends and the tests; the coding agent reads only the configured catalogue.

pub mod deepseek;
