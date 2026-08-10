//! Concrete provider descriptions.
//!
//! These are plain constructors, not a catalog format: model metadata is
//! written out as constants so startup costs nothing and the numbers are
//! reviewable in a diff.
//!
//! A catalog format does exist, alongside rather than instead: [`crate::catalog`]
//! reads the user's own `~/.aphid/models.json`, and [`crate::models_dev`] fills
//! one in from models.dev. These constants stay the floor that needs no
//! configuration to work.

pub mod deepseek;
