//! Scalar fastpath variant — portable baseline used on targets without SIMD
//! intrinsics or when feature detection picks the fallback.
//!
//! No `#[target_feature]` attributes here: every function uses portable Rust
//! and must compile on any supported target. Future hot-path duplicates land
//! in this module first, then are mirrored under `neon` / `sse42` /
//! `avx2_bmi2` with their respective umbrella attributes.

#![allow(dead_code)]

/// Marker constant so this submodule actually contains symbols on every
/// supported target while the migration is in progress. Will be removed once
/// real hot-path functions land in subsequent commits.
pub(crate) const KERNEL_TAG: &str = "scalar";
