//! Hash / chain / BT tables used by the encoder match finders.
//!
//! Will host the shared `MatchTable` abstraction plus the concrete
//! storage layouts:
//!
//! - hash table (main, indexed by `ZSTD_hashPtr`-equivalent)
//! - chain table (dual-purpose: HC chain links or BT pointer pairs)
//! - hash3 side table (HC3 short-match lookup)
//! - arena allocator (cwksp parity — single `Box<[u8]>` bumped per
//!   frame so per-frame `Vec` reallocation disappears from the hot
//!   path)
//!
//! Empty in #111 Phase 1.0 (scaffold). Subsequent Phase 1 commits will
//! move table storage out of `super::match_generator` here.

#![allow(dead_code)]
