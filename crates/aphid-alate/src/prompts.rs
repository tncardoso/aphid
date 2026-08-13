//! Instruction text built into the binary.
//!
//! Kept as files rather than inline strings so the prose can be read and
//! edited on its own, without wading through Rust escapes.

/// The system prompt an alate uses in place of [`aphid_code`]'s default,
/// because "a coding agent working in a terminal" is not what a resident
/// agent is.
pub const SYSTEM: &str = include_str!("prompts/system.txt");
