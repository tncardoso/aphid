//! What an update asked the runtime to do.

/// The effects one update wants performed, in the order it asked for them.
///
/// A list and not one effect, because one message often means two things:
/// switch the model *and* say so in the pane. [`Cmd::none`] allocates nothing,
/// which is what almost every message returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cmd<E> {
    effects: Vec<E>,
}

impl<E> Cmd<E> {
    /// Nothing to do. The model changed and that is all.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            effects: Vec::new(),
        }
    }

    #[must_use]
    pub fn one(effect: E) -> Self {
        Self {
            effects: vec![effect],
        }
    }

    #[must_use]
    pub fn batch(effects: impl IntoIterator<Item = E>) -> Self {
        Self {
            effects: effects.into_iter().collect(),
        }
    }

    pub fn push(&mut self, effect: E) {
        self.effects.push(effect);
    }

    /// What was asked for, for a test to read.
    #[must_use]
    pub fn effects(&self) -> &[E] {
        &self.effects
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    #[must_use]
    pub fn into_effects(self) -> Vec<E> {
        self.effects
    }
}

impl<E> From<E> for Cmd<E> {
    fn from(effect: E) -> Self {
        Self::one(effect)
    }
}

#[cfg(test)]
mod tests {
    use super::Cmd;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Effect {
        Say(&'static str),
        Quit,
    }

    #[test]
    fn nothing_is_the_common_answer() {
        let cmd: Cmd<Effect> = Cmd::none();
        assert!(cmd.is_empty());
        assert_eq!(cmd.effects(), []);
    }

    #[test]
    fn the_order_asked_for_is_the_order_kept() {
        let mut cmd = Cmd::one(Effect::Say("first"));
        cmd.push(Effect::Say("second"));
        cmd.push(Effect::Quit);

        assert_eq!(
            cmd.into_effects(),
            [Effect::Say("first"), Effect::Say("second"), Effect::Quit]
        );
    }
}
