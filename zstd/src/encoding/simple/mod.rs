//! "Simple" (donor `ZSTD_fast`) match-finder backend used by the
//! `Fastest` / `CompressionLevel::Level(1)` path.
//!
//! Donor parity: the active matcher is the donor-shape
//! [`fast_kernel`] + [`fast_matcher`] pair — a single-pass kernel
//! reading a flat `Vec<u8>` history with a `Vec<u32>` hash table
//! indexed by `hash_ptr<MLS>` (multiply-shift over the first MLS
//! bytes). Replaces the legacy SuffixStore-based MatchGenerator
//! that lived here through #111 Phase 1c and was removed in
//! #198 phase 1b.

pub(crate) mod fast_kernel;
pub(crate) mod fast_matcher;
