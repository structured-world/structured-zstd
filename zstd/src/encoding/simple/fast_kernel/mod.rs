//! Donor-shape Fast strategy block compressor — flat hash-table +
//! tight per-block loop, ported from `lib/compress/zstd_fast.c`. See
//! [`kernel::compress_block_fast`] for the entry point.
//!
//! This module is the first commit of the donor-port branch: the
//! kernel is implemented and unit-tested in isolation but not yet
//! wired into [`super::MatchGenerator`] / `MatcherStorage`. The
//! follow-up commit on the same branch replaces the `SuffixStore`-
//! based Fast strategy path with a `MatcherStorage::FastKernel`
//! arm that invokes [`compress_block_fast`] per block. Until then
//! every item below appears unused — the `#[allow(dead_code)]` on
//! each sub-module exists solely to keep the baseline `cargo clippy
//! -D warnings` build green during the staged rollout. It is
//! removed in the wiring commit.
#![allow(dead_code, unused_imports)]

pub(crate) mod count;
pub(crate) mod hash_table;
pub(crate) mod kernel;

pub(crate) use hash_table::FastHashTable;
pub(crate) use kernel::{FastBlockResult, compress_block_fast};
