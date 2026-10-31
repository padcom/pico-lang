//! Mid-level IR, lowering and monomorphization for ppl.

pub mod lower;
pub mod mir;

pub use lower::lower;
pub use mir::{Program, dump};
