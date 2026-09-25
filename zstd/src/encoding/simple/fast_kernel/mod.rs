//! Upstream zstd-shape Fast strategy block compressor — flat hash-table +
//! tight per-block loop, ported from `lib/compress/zstd_fast.c`. See
//! [`kernel::compress_block_fast`] for the entry point, which
//! [`super::fast_matcher::FastKernelMatcher`] (the Simple backend) drives
//! per block.

pub(crate) mod count;
pub(crate) mod hash_table;
pub(crate) mod kernel;
