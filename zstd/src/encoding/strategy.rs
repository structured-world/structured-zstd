//! Const-generic encoder strategy dispatch.
//!
//! Replaces the runtime `match self.parse_mode` ladder with a `Strategy`
//! trait whose associated `const`s let the compiler monomorphise the
//! entire encoder pipeline per-level. Each strategy carries:
//!
//! - `MIN_MATCH` — minimum match length the parser will produce
//! - `ACCURATE_PRICE` / `FAVOR_SMALL_OFFSETS` — cost-model flags
//! - `USE_HASH3` — compile-time gate equivalent to donor `static
//!   (mls==3)` short-match probe inside `ZSTD_insertBtAndGetAllMatches`
//! - `USE_BT` / `MAX_SEARCH_DEPTH` — match-finder shape
//! - `OPT_LEVEL` — donor `optLevel` (0 = btopt, 2 = btultra/btultra2)
//! - `type Matcher: MatchFinder` — concrete finder implementation
//!
//! Empty in #111 Phase 1.0 (scaffold). The actual trait lands in
//! Phase 3 once the per-mode code paths have been extracted into the
//! dedicated submodules in Phases 1-2.

#![allow(dead_code)]
