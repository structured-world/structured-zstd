//! Optimal-parser cost model.
//!
//! Hosts the price tables (`HcOptState`), the `bit_weight` /
//! `frac_weight` functions that translate symbol counts to bit-costs, and
//! the per-symbol price helpers (`literal_price`, `lit_length_price`,
//! `offset_price_unrep`, `match_length_price`) consumed by the optimal
//! parser DP body in [`crate::encoding::opt`].
//!
//! Donor parity: mirrors `zstd_opt.c` price functions and stat handling.
//! All arithmetic is raw (`+`/`-`/`*`) guarded by `debug_assert!`; donor
//! never uses saturating ops on this path.
//!
//! Empty in #111 Phase 1.0 (scaffold). Phase 2 will move the
//! corresponding code out of `super::match_generator`.

// Allow the empty module during the multi-commit Phase 1 split so each
// intermediate revision compiles cleanly while the contents migrate over.
#![allow(dead_code)]
