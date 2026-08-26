pub mod diff;
pub mod git;
pub mod p4;

pub use diff::{parse_unified_diff, DiffLineKind, UnifiedDiff};
