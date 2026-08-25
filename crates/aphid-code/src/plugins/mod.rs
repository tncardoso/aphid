//! Plugins the coding harness ships with.

pub mod permissions;
pub mod scripts;

pub use permissions::{AllowAll, Confirmer, Decision, DenyAll, PermissionGate, Permissions, Risk};
