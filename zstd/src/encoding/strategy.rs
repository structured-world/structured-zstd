//! Encoder strategy enum + (eventual) const-generic strategy dispatch.
//!
//! In Phase 1 of the #111 rewrite this file hosts only the
//! [`HcParseMode`] enum — the dispatch tag that selects between the
//! Lazy2 / BtOpt / BtUltra / BtUltra2 code paths in `match_generator`
//! and `cost_model`. Hosting it here breaks what would otherwise be a
//! reverse dependency from `cost_model` back into the monolith
//! (`super::match_generator`) and gives both consumers a neutral place
//! to import from.
//!
//! Phase 3 will replace the runtime `match` over [`HcParseMode`] with a
//! `Strategy` trait whose associated `const`s let the compiler
//! monomorphise the entire encoder pipeline per-level. Each strategy
//! carries:
//!
//! - `MIN_MATCH` — minimum match length the parser will produce
//! - `ACCURATE_PRICE` / `FAVOR_SMALL_OFFSETS` — cost-model flags
//! - `USE_HASH3` — compile-time gate equivalent to donor `static
//!   (mls==3)` short-match probe inside `ZSTD_insertBtAndGetAllMatches`
//! - `USE_BT` / `MAX_SEARCH_DEPTH` — match-finder shape
//! - `OPT_LEVEL` — donor `optLevel` (0 = btopt, 2 = btultra/btultra2)
//! - `type Matcher: MatchFinder` — concrete finder implementation

#![allow(dead_code)]

/// Runtime dispatch tag selecting which optimal-parser / match-finder
/// pipeline `HcMatchGenerator` should execute. The cost-model profile
/// and the matcher's `start_matching` body both branch on this enum
/// until the Phase 3 `Strategy` trait replaces the runtime match.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum HcParseMode {
    Lazy2,
    BtOpt,
    BtUltra,
    BtUltra2,
}
