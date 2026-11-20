//! Bounded aggregate review, distinct from the full-patch status preview.

pub mod budget;
#[cfg(feature = "runtime")]
pub mod collect;
pub mod model;
pub mod patch;
pub mod state;
pub mod viewed;

#[cfg(test)]
mod tests;
