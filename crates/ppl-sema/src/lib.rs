//! Name resolution, type checking and typed HIR for ppl.

pub mod format;
pub mod hir;
pub mod resolve;
pub mod ty;
pub mod typeck;

pub use resolve::resolve;
pub use ty::{FloatTy, IntTy, Ty};
pub use typeck::{FnSig, Types, check};
