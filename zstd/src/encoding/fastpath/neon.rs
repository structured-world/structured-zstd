//! AArch64 NEON fastpath variant. Every hot-path function in this module is
//! marked `#[target_feature(enable = "neon")]` so that the standard-library
//! NEON intrinsics (which themselves carry that attribute) inline directly
//! into the call graph instead of going through the function-call ABI barrier.
//!
//! NEON is part of the AArch64 baseline ISA — the attribute is therefore
//! redundant for correctness but mandatory for inline behavior under the Rust
//! ABI rules.

#![cfg(all(target_arch = "aarch64", target_endian = "little"))]
#![allow(dead_code)]

pub(crate) const KERNEL_TAG: &str = "neon";
