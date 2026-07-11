//! Position discipline (spec I13): an input/KV position is **not** an output position.
//! `apply p → KV[p] → logits → optionally sample p+1`. These newtypes make I13 violations a
//! compile error: `InputPos` and `OutputPos` never coerce to each other or to a bare integer.
//!
//! Wire fields named `*_input_pos` deserialize to [`InputPos`]; `*_output_pos` to [`OutputPos`].

/// An input / KV-cache position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputPos(pub i64);

/// A sampled-output position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputPos(pub i64);

impl InputPos {
    pub const NONE: InputPos = InputPos(-1);
    #[inline]
    pub fn get(self) -> i64 {
        self.0
    }
    /// Advance by one input position.
    #[inline]
    pub fn next(self) -> InputPos {
        InputPos(self.0 + 1)
    }
}

impl OutputPos {
    pub const NONE: OutputPos = OutputPos(-1);
    #[inline]
    pub fn get(self) -> i64 {
        self.0
    }
    #[inline]
    pub fn next(self) -> OutputPos {
        OutputPos(self.0 + 1)
    }
}

impl core::fmt::Display for InputPos {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "in:{}", self.0)
    }
}
impl core::fmt::Display for OutputPos {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "out:{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newtypes_do_not_coerce() {
        // The point of I13: these are distinct types. This test documents the intent;
        // the guarantee is enforced by the compiler (InputPos + OutputPos does not typecheck).
        let ip = InputPos(10);
        let op = OutputPos(10);
        assert_eq!(ip.get(), op.get());
        assert_ne!(ip.next(), InputPos(10));
        assert_eq!(ip.next(), InputPos(11));
    }
}
