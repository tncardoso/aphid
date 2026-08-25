//! The tool table.
//!
//! Laid out for the read path: declarations live in one contiguous `Vec<Tool>`
//! because that is exactly what a request encoder wants, and the handlers sit
//! in a second vector at the same indices.
//!
//! This is the table, not the registry. [`Toolbox`](crate::Toolbox) wraps one
//! so that contributing a tool is revertible and the set can change inside a
//! session.

use std::sync::Arc;

use aphid_core::Tool;

use crate::tool::ToolHandler;

/// Registered tools, split into the half the provider sees and the half that
/// runs.
#[derive(Clone, Default)]
pub struct Tools {
    declarations: Vec<Tool>,
    handlers: Vec<Arc<dyn ToolHandler>>,
}

impl Tools {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. A later registration under the same name replaces the
    /// earlier one, which is how a plugin overrides a built-in.
    pub fn push(&mut self, handler: Arc<dyn ToolHandler>) {
        let declaration = handler.declaration().clone();
        match self.index_of(&declaration.name) {
            Some(index) => {
                self.declarations[index] = declaration;
                self.handlers[index] = handler;
            }
            None => {
                self.declarations.push(declaration);
                self.handlers.push(handler);
            }
        }
    }

    /// Take the handlers out, for handing to a live registry.
    #[must_use]
    pub fn into_handlers(self) -> Vec<Arc<dyn ToolHandler>> {
        self.handlers
    }

    /// Remove a tool by name. Nothing happens if it was not registered.
    pub fn remove(&mut self, name: &str) {
        if let Some(index) = self.index_of(name) {
            self.declarations.remove(index);
            self.handlers.remove(index);
        }
    }

    /// The declarations, contiguous and ready to hand to a request encoder.
    #[must_use]
    pub fn declarations(&self) -> &[Tool] {
        &self.declarations
    }

    /// Tool counts are in the tens, so a scan beats a map and its allocation.
    fn index_of(&self, name: &str) -> Option<usize> {
        self.declarations
            .iter()
            .position(|tool| tool.name.as_str() == name)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn ToolHandler>> {
        self.index_of(name).map(|index| &self.handlers[index])
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.declarations.iter().map(|tool| tool.name.as_str())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.declarations.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }
}

impl std::fmt::Debug for Tools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}
