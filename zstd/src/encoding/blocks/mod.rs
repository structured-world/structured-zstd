//! After Magic_Number and Frame_Header, there are some number of blocks. Each frame must have at least one block,
//! but there is no upper limit on the number of blocks per frame.
//!
//! There are a few different kinds of blocks, and implementations for those kinds are
//! in this module.
mod compressed;

pub(super) use compressed::*;
// The dictionary finalizer derives sequence codes the way blocks do.
#[cfg(feature = "dict-builder")]
pub(crate) use compressed::{
    encode_literal_length, encode_match_len, encode_offset, encode_offset_with_history,
    encode_offset_with_history_fast, uses_fast_offset_codes,
};
