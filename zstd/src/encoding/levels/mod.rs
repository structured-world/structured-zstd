pub(crate) mod config;
mod fastest;
pub(crate) use fastest::{BlockInput, compress_block_encoded, compress_block_encoded_borrowed};
