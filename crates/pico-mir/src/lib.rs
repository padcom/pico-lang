//! Mid-level IR, lowering and monomorphization for pico.

pub mod eval;
pub mod fold;
pub mod lower;
pub mod mir;

pub use lower::{check_signatures, lower};
pub use mir::{Program, dump};
