//! Hash-chain match finder used by `Lazy2`.
//!
//! Will host:
//! - Chain walk (donor `ZSTD_HcFindBestMatch`)
//! - `insert_position` / `insert_position_no_rebase` (donor
//!   `ZSTD_insertAndFindFirstIndex_internal`)
//! - Lazy match selection (`pick_lazy_match`,
//!   `start_matching_lazy`)
//! - Speculative tail check (donor `zstd_lazy.c:714`,
//!   `MEM_read32(match+ml-3) == MEM_read32(ip+ml-3)`) — pending fix.
//!
//! Empty in #111 Phase 1.0 (scaffold).

#![allow(dead_code)]
