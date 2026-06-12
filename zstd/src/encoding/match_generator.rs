//! Matching algorithm used find repeated parts in the original data
//!
//! The Zstd format relies on finden repeated sequences of data and compressing these sequences as instructions to the decoder.
//! A sequence basically tells the decoder "Go back X bytes and copy Y bytes to the end of your decode buffer".
//!
//! The task here is to efficiently find matches in the already encoded data for the current suffix of the not yet encoded data.

use alloc::vec::Vec;
// SIMD/CRC intrinsics now live in `crate::encoding::fastpath::*` where they
// sit under per-CPU `#[target_feature]` umbrellas; no architecture-specific
// intrinsic imports remain in this file.
use super::CompressionLevel;
use super::Matcher;
use super::Sequence;
use super::blocks::encode_offset_with_history;
use super::bt::BtMatcher;
#[cfg(test)]
use super::cost_model::HC_MAX_LIT;
use super::cost_model::{
    HC_BITCOST_MULTIPLIER, HC_FORMAT_MINMATCH, HC_OPT_NUM, HC_PREDEF_THRESHOLD, HcOptState,
    HcOptimalCostProfile,
};
#[cfg(test)]
use super::cost_model::{HC_BLOCKSIZE_MAX, HC_MAX_LL, HC_MAX_ML, HC_MAX_OFF, HcOptPriceType};
use super::dfast::DfastMatchGenerator;
// FAST_HASH_FILL_STEP test-only re-export was tied to the legacy
// SuffixStore MatchGenerator's interleaved hash-fill stride. The
// donor-shape Fast kernel walks ip0 with kSearchStrength step-skip
// acceleration instead, so the constant has no consumer in the
// remaining live test set today.
#[cfg(test)]
use super::match_table::helpers::INCOMPRESSIBLE_SKIP_STEP;
use super::match_table::helpers::MIN_MATCH_LEN;
#[cfg(test)]
use super::match_table::helpers::common_prefix_len;
#[cfg(test)]
use super::opt::ldm::HcRawSeq;
use super::opt::ldm::{HcOptLdmState, HcRawSeqStore};
use super::opt::types::{
    HcCandidateQuery, HcOptimalNode, HcOptimalPlanBuffers, HcOptimalPlanState, HcOptimalSequence,
    MatchCandidate,
};
use super::row::RowMatchGenerator;
use super::simple::fast_matcher::{FAST_LEVEL_1_HASH_LOG, FAST_LEVEL_1_MLS, FastKernelMatcher};
#[cfg(all(
    test,
    feature = "std",
    target_arch = "aarch64",
    target_endian = "little"
))]
use std::arch::is_aarch64_feature_detected;
#[cfg(all(test, feature = "std", target_arch = "x86_64"))]
use std::arch::is_x86_feature_detected;

pub(crate) const DFAST_MIN_MATCH_LEN: usize = 5;
// Bytes the dfast short hash reads (donor `mls = 5`). Seeding / lookahead
// guards use it so a position is only short-hashed once its full 5-byte key
// is in range.
pub(crate) const DFAST_SHORT_HASH_LOOKAHEAD: usize = 5;
pub(crate) const ROW_MIN_MATCH_LEN: usize = 5;
// Donor `clevels.h:31` at level 3 large-input bucket sets
// `hashLog = 17` (the long-hash table) and `chainLog = 16` (the
// short-hash table — donor names this `chainTable` even though for
// dfast it's used as a plain single-slot hash). Each table holds one
// `U32` per slot; the donor overwrites on collision and recovers
// compression quality via the inline `_search_next_long` retry
// (after a short-hash hit, probes `hashLong[hl1]` at `ip + 1` and
// keeps the longer match).
//
// We mirror that storage layout: single `u32` per bucket (no
// `[u32; N]` array), `long_hash` sized `1 << DFAST_HASH_BITS` and
// `short_hash` one bit smaller via `DFAST_SHORT_HASH_BITS_DELTA`.
// Two-table footprint at Level 3: `2^17 × 4 + 2^16 × 4 = 768 KiB`,
// exact upstream parity. The `_search_next_long` retry lives in
// `DfastMatchGenerator::hash_candidate` (called via
// `best_match`). Earlier revisions kept a
// 4-slot bucket per hash position; that paid 4× the donor memory
// without measurable ratio gain once the retry was in place.
//
// `dfast_hash_bits_for_window` still clamps the runtime long-hash
// value to `[MIN_WINDOW_LOG, DFAST_HASH_BITS]`, so this const is the
// upper bound rather than a fixed default.
pub(crate) const DFAST_HASH_BITS: usize = 17;
/// Difference between `long_hash_bits` and `short_hash_bits` —
/// donor `hashLog - chainLog` is 1 at every dfast level (`clevels.h`
/// level 2: 16-15=1; level 3: 17-16=1). The short hash is one bit
/// smaller than the long hash so the per-bucket footprint matches
/// donor sizing exactly.
pub(crate) const DFAST_SHORT_HASH_BITS_DELTA: usize = 1;
/// Sentinel value for an empty slot in the dfast hash tables. Real
/// positions are stored as `(abs_pos - position_base + 1) as u32`, so
/// `0` is reserved as the "empty" marker and a true relative offset
/// of `0` never appears in the table. Mirrors the LDM table's
/// `LdmEntry.offset == 0` convention (see `encoding/ldm/table.rs`)
/// so both rebasing structures share
/// one sentinel scheme.
pub(crate) const DFAST_EMPTY_SLOT: u32 = 0;

/// Guard band reserved above the high-water mark before triggering a
/// rebase on the Dfast hash tables. When the next insert would push a
/// relative offset above `u32::MAX - DFAST_REBASE_GUARD_BAND`, the
/// table calls `reduce(GUARD_BAND)` to shift every slot down and
/// advance `position_base` so future inserts stay inside the `u32`
/// window. Same scheme as `encoding/ldm/table.rs`.
pub(crate) const DFAST_REBASE_GUARD_BAND: u32 = 1u32 << 30;
pub(crate) const DFAST_SKIP_SEARCH_STRENGTH: usize = 6;
pub(crate) const DFAST_SKIP_STEP_GROWTH_INTERVAL: usize = 1 << DFAST_SKIP_SEARCH_STRENGTH;
pub(crate) const DFAST_MAX_SKIP_STEP: usize = 8;
pub(crate) const DFAST_INCOMPRESSIBLE_SKIP_STEP: usize = 16;
pub(crate) const ROW_HASH_BITS: usize = 20;
pub(crate) const ROW_LOG: usize = 5;
pub(crate) const ROW_SEARCH_DEPTH: usize = 16;
pub(crate) const ROW_TARGET_LEN: usize = 48;
pub(crate) const ROW_TAG_BITS: usize = 8;
pub(crate) const ROW_EMPTY_SLOT: u32 = u32::MAX;
pub(crate) const ROW_HASH_KEY_LEN: usize = 4;
// HASH_MIX_PRIME now lives in `crate::encoding::fastpath::scalar`; the four
// per-CPU `hash_mix_u64` variants share it via that module.
// HC_PRIME3BYTES / HC_PRIME4BYTES moved to match_table::storage
// alongside the hash helpers in Phase 1e Stage A. Only the test
// module references the constants directly (production code goes
// through `MatchTable::hash_value_with_mls`).
#[cfg(test)]
use super::match_table::storage::{HC_PRIME3BYTES, HC_PRIME4BYTES};

// HC_HASH_LOG / HC_CHAIN_LOG / HC3_HASH_LOG / HC_EMPTY live on the
// shared storage module so MatchTable methods can reference them
// without pulling in this module. Re-imported here so existing
// macros / configs / tests keep their unqualified names.
#[cfg(test)]
use super::match_table::storage::HC_EMPTY;
use super::match_table::storage::HC3_HASH_LOG;
// HC_HASH_LOG / HC_CHAIN_LOG feed the test-only `HC_CONFIG` default.
#[cfg(test)]
use super::match_table::storage::{HC_CHAIN_LOG, HC_HASH_LOG};
// HC3_MAX_OFFSET moved to encoding::bt alongside the hash3 candidate
// probe macro that consumes it; the macro references it via the
// fully-qualified `$crate::encoding::bt::HC3_MAX_OFFSET` path so this
// module no longer needs a local import.
const HC_SEARCH_DEPTH: usize = 16;
// HC_MIN_MATCH_LEN moved to encoding::hc; re-imported here so
// existing references compile unchanged.
use super::hc::HC_MIN_MATCH_LEN;
const HC_OPT_MIN_MATCH_LEN: usize = HC_FORMAT_MINMATCH;
const HC_TARGET_LEN: usize = 48;

// MAX_HC_SEARCH_DEPTH moved to encoding::hc alongside chain_candidates.
use super::hc::MAX_HC_SEARCH_DEPTH;

// `Strategy` and `StrategyTag` live in `crate::encoding::strategy`.
// The driver carries a `StrategyTag` field set at `reset()` and
// dispatches each block into a monomorphised `compress_block::<S>`
// per concrete strategy.

/// Bundled tuning knobs for the hash-chain matcher. Using a typed config
/// instead of positional `usize` args eliminates parameter-order hazards.
#[derive(Copy, Clone, PartialEq, Eq)]
struct HcConfig {
    hash_log: usize,
    chain_log: usize,
    search_depth: usize,
    target_len: usize,
    /// Binary-tree finder hash width (donor `mls = BOUNDED(4, minMatch, 6)`),
    /// carried explicitly per level so it is NOT inferred from `target_len`
    /// (a `target_length` override must not silently flip the finder between
    /// 5- and 4-byte hashing). Only the BT body reads it; HC/lazy levels keep
    /// it at 4 (their `hash_position` is always 4-byte). 5 for the
    /// minMatch=5 BT levels (btlazy2 + btopt L16), 4 elsewhere.
    search_mls: usize,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) struct RowConfig {
    pub(crate) hash_bits: usize,
    pub(crate) row_log: usize,
    pub(crate) search_depth: usize,
    pub(crate) target_len: usize,
    /// Donor `cParams.minMatch` for the row matcher: the regular-search
    /// acceptance floor (a row candidate must extend to >= `mls` bytes).
    /// The C-like advanced API surfaces this as the row min-match knob.
    /// `ROW_MIN_MATCH_LEN` (5) is the default; the row hash key width stays
    /// 4 bytes (an internal detail), so this only tunes the acceptance
    /// floor, not the candidate hash distribution.
    pub(crate) mls: usize,
}

// Only used as the default HashChain config when the test-only parse×search
// override pairs a level with a backend its native row doesn't populate.
#[cfg(test)]
const HC_CONFIG: HcConfig = HcConfig {
    hash_log: HC_HASH_LOG,
    chain_log: HC_CHAIN_LOG,
    search_depth: HC_SEARCH_DEPTH,
    target_len: HC_TARGET_LEN,
    search_mls: 4,
};

/// Base HashChain config synthesized when a public-parameter strategy
/// override ([`super::parameters`]) routes a level to the HC / BT
/// backend whose native level row didn't populate `hc` (e.g. forcing
/// `Strategy::Lazy2` onto a level the table resolves to Fast). Mirrors
/// the mid-band lazy defaults; the per-knob overrides then refine it.
const HC_OVERRIDE_DEFAULT: HcConfig = HcConfig {
    hash_log: super::match_table::storage::HC_HASH_LOG,
    chain_log: super::match_table::storage::HC_CHAIN_LOG,
    search_depth: HC_SEARCH_DEPTH,
    target_len: HC_TARGET_LEN,
    search_mls: 4,
};

const BTULTRA2_HC_CONFIG: HcConfig = HcConfig {
    hash_log: 24,
    chain_log: 24,
    search_depth: 512,
    target_len: 256,
    search_mls: 4,
};

const BTULTRA2_HC_CONFIG_L22: HcConfig = HcConfig {
    hash_log: 25,
    chain_log: 27,
    search_depth: 512,
    target_len: 999,
    search_mls: 4,
};

const BTULTRA2_HC_CONFIG_L22_256K: HcConfig = HcConfig {
    hash_log: 19,
    chain_log: 19,
    search_depth: 1 << 13,
    target_len: 999,
    search_mls: 4,
};

const BTULTRA2_HC_CONFIG_L22_128K: HcConfig = HcConfig {
    hash_log: 17,
    chain_log: 18,
    search_depth: 1 << 11,
    target_len: 999,
    search_mls: 4,
};

const BTULTRA2_HC_CONFIG_L22_16K: HcConfig = HcConfig {
    hash_log: 15,
    chain_log: 15,
    search_depth: 1 << 10,
    target_len: 999,
    search_mls: 4,
};

// Default Row config: only used by tests and the test-only parse×search
// override (production greedy L5 carries its own `ROW_L5`).
#[cfg(test)]
const ROW_CONFIG: RowConfig = RowConfig {
    hash_bits: ROW_HASH_BITS,
    row_log: ROW_LOG,
    search_depth: ROW_SEARCH_DEPTH,
    target_len: ROW_TARGET_LEN,
    mls: ROW_MIN_MATCH_LEN,
};

// Level-5 greedy is the ONLY strategy routed to the Row backend
// (`StrategyTag::backend`: greedy -> Row; lazy / btopt / btultra* ->
// HashChain), so it is the only level whose `row:` field is read. The donor
// `clevels.h` default row (srcSize > 256 KB) for level 5 is searchLog=3,
// targetLength=2, from which the row matcher derives:
//   rowLog       = clamp(searchLog, 4, 6) = 4
//   search_depth = 1 << min(searchLog, rowLog) = 8   (= nbAttempts)
//   target_len   = targetLength = 2                  (nice-match early-out)
// The shared `ROW_CONFIG` (row_log=5, search_depth=16, target_len=48) ran a
// level-12-grade search here: 16 slots per row, never early-exiting until a
// 48-byte match. That exhaustive walk was the dominant cost in greedy L5's
// encode-speed regression vs FFI. `hash_bits` stays at the `ROW_HASH_BITS`
// cap (the row table size is a separate budget).
const ROW_L5: RowConfig = RowConfig {
    hash_bits: ROW_HASH_BITS,
    row_log: 4,
    search_depth: 8,
    target_len: 2,
    mls: ROW_MIN_MATCH_LEN,
};

// Donor `clevels.h` unbounded defaults for the lazy band, verified via
// `ZSTD_getCParams(level, 0, 0)`:
//   L6  { w21 c18 h19 s3 mml5 t4  lazy  } → rowLog 4, depth 1<<3 = 8
//   L7  { w21 c19 h20 s4 mml5 t8  lazy  } → rowLog 4, depth 16
//   L8  { w21 c19 h20 s4 mml5 t16 lazy2 } → rowLog 4, depth 16
//   L9  { w22 c20 h21 s4 mml5 t16 lazy2 } → rowLog 4, depth 16
//   L10 { w22 c21 h22 s5 mml5 t16 lazy2 } → rowLog 5, depth 32
//   L11 { w22 c21 h22 s6 mml5 t16 lazy2 } → rowLog 6, depth 64
//   L12 { w22 c22 h23 s6 mml5 t32 lazy2 } → rowLog 6, depth 64
// `rowLog = clamp(searchLog, 4, 6)`, `depth = 1 << min(searchLog, rowLog)`
// (same derivation as `ROW_L5` above). `hash_bits` carries the donor
// `hashLog`; the hinted-source clamp in `configure` caps it by the window
// exactly like the donor `ZSTD_adjustCParams` path.
const ROW_L6: RowConfig = RowConfig {
    hash_bits: 19,
    row_log: 4,
    search_depth: 8,
    target_len: 4,
    mls: ROW_MIN_MATCH_LEN,
};
const ROW_L7: RowConfig = RowConfig {
    hash_bits: 20,
    row_log: 4,
    search_depth: 16,
    target_len: 8,
    mls: ROW_MIN_MATCH_LEN,
};
const ROW_L8: RowConfig = RowConfig {
    hash_bits: 20,
    row_log: 4,
    search_depth: 16,
    target_len: 16,
    mls: ROW_MIN_MATCH_LEN,
};
const ROW_L9: RowConfig = RowConfig {
    hash_bits: 21,
    row_log: 4,
    search_depth: 16,
    target_len: 16,
    mls: ROW_MIN_MATCH_LEN,
};
const ROW_L10: RowConfig = RowConfig {
    hash_bits: 22,
    row_log: 5,
    search_depth: 32,
    target_len: 16,
    mls: ROW_MIN_MATCH_LEN,
};
const ROW_L11: RowConfig = RowConfig {
    hash_bits: 22,
    row_log: 6,
    search_depth: 64,
    target_len: 16,
    mls: ROW_MIN_MATCH_LEN,
};
const ROW_L12: RowConfig = RowConfig {
    hash_bits: 23,
    row_log: 6,
    search_depth: 64,
    target_len: 32,
    mls: ROW_MIN_MATCH_LEN,
};

/// Per-level Double-Fast hash sizing, mirroring the donor `clevels.h` columns
/// (config-driven, not a hardcoded constant): `long_hash_log` =
/// `cParams.hashLog` (the long 8-byte hash table), `short_hash_log` =
/// `cParams.chainLog` (the short hash table dfast repurposes as its
/// secondary index). Only the Dfast backend reads it, so non-dfast level
/// rows carry `dfast: None`. `minMatch` stays the donor-fixed `5`
/// (`DFAST_MIN_MATCH_LEN`, used in const contexts).
#[derive(Copy, Clone, PartialEq, Eq)]
struct DfastConfig {
    long_hash_log: u8,
    short_hash_log: u8,
}

// Donor clevels.h default row (srcSize > 256 KB): L3 {hashLog 17, chainLog 16},
// L4 {hashLog 18, chainLog 18}.
const DFAST_L3: DfastConfig = DfastConfig {
    long_hash_log: 17,
    short_hash_log: 16,
};
const DFAST_L4: DfastConfig = DfastConfig {
    long_hash_log: 18,
    short_hash_log: 18,
};

/// Per-level Fast-strategy tuning, only consumed by the `FastKernelMatcher`
/// (Simple backend): `hash_log` = donor `cParams.hashLog`, `mls` = donor
/// `cParams.minMatch` (4..=8), `step_size` = donor `stepSize`. Carried as
/// `LevelParams.fast` (`Some` only on Fast level rows; `None` elsewhere).
#[derive(Copy, Clone, PartialEq, Eq)]
struct FastConfig {
    hash_log: u32,
    mls: u32,
    step_size: usize,
}

const FAST_L1: FastConfig = FastConfig {
    hash_log: 14,
    mls: 7,
    step_size: 2,
};
const FAST_L2: FastConfig = FastConfig {
    hash_log: 16,
    mls: 6,
    step_size: 2,
};

/// Resolved tuning parameters for a compression level. The
/// [`StrategyTag`] is the single source of truth for the backend
/// family and the compile-time strategy consts; the runtime
/// [`BackendTag`] used by the driver dispatcher is derived via
/// [`StrategyTag::backend`] so the two cannot drift.
#[derive(Copy, Clone, PartialEq, Eq)]
struct LevelParams {
    strategy_tag: super::strategy::StrategyTag,
    /// Decoupled search-method axis. Independent of `strategy_tag`'s
    /// parse half: a level can pair any parse (greedy / lazy depth via
    /// `lazy_depth`) with any search backend here. Defaults to the
    /// historical pairing (`strategy_tag.search()`) but is overridable
    /// per level so the parse×search matrix can be swept and tuned.
    search: super::strategy::SearchMethod,
    window_log: u8,
    lazy_depth: u8,
    /// Per-strategy tuning. Exactly one is `Some` on each level row, matching
    /// `strategy_tag`'s backend, so the table self-documents which knobs a
    /// level actually consumes (the others are `None`, not dead placeholders):
    /// `fast` for the Fast/Simple backend, `dfast` for Double-Fast, `hc` for
    /// the HashChain (lazy / btopt / btultra*) backend, `row` for the Row
    /// (greedy L5) backend.
    fast: Option<FastConfig>,
    dfast: Option<DfastConfig>,
    hc: Option<HcConfig>,
    row: Option<RowConfig>,
}

impl LevelParams {
    /// Backend family (storage variant) for the driver dispatcher.
    /// Derived from the decoupled `search` axis so a level can route to
    /// a different search backend than its `strategy_tag` historically
    /// implied.
    fn backend(&self) -> super::strategy::BackendTag {
        self.search.backend()
    }

    /// Parse mode derived from the decoupled `search` axis: the binary-tree
    /// search path carries `ParseMode::Optimal`; every other search backend
    /// derives greedy/lazy/lazy2 from `lazy_depth`. Reading `search` (not the
    /// strategy tag) keeps the parse×search decoupling complete even when a
    /// level whose tag is `Bt*` is overridden to a non-BT search backend.
    fn parse(&self) -> super::strategy::ParseMode {
        match self.search {
            super::strategy::SearchMethod::BinaryTree => super::strategy::ParseMode::Optimal,
            _ => super::strategy::ParseMode::from_lazy_depth(self.lazy_depth),
        }
    }

    /// Cheap fingerprint pre-splitter level, the C-like `blockSplitterLevel`
    /// knob. Mirrors the donor `splitLevels[]` table indexed by strategy in
    /// `ZSTD_optimalBlockSize` (`{0,0,1,2,2,3,3,4,4,4}` over fast..btultra2):
    /// fast=0, dfast=1, greedy=2, lazy=2, lazy2=3, btlazy2=3,
    /// btopt/btultra/btultra2=4. We collapse the donor `lazy2` and `btlazy2`
    /// strategies into the hash-chain `Lazy` tag, distinguished here by
    /// `lazy_depth` (the level table runs both at depth 2), so depth 2 routes
    /// to split level 3 to match the donor. `split_level == 0` routes to the
    /// cheap from-borders heuristic; `1..=4` to byChunks with internal
    /// sampling level `split_level - 1`. The `savings >= 3` gate in
    /// `optimal_block_size` keeps incompressible data and the first full block
    /// whole, so homogeneous frames are not over-split.
    fn pre_split(&self) -> Option<u8> {
        match self.strategy_tag {
            super::strategy::StrategyTag::Fast => Some(0),
            super::strategy::StrategyTag::Dfast => Some(1),
            super::strategy::StrategyTag::Greedy => Some(2),
            // The lazy2 / btlazy2 band (Lazy at lazy_depth >= 2, and Btlazy2)
            // uses the rate-1 full-scan chunk splitter (4), NOT the rate-5
            // sampler (3). The rate-5 sampler combined with the larger
            // hash_log is sensitive enough to register a phantom statistical
            // transition on perfectly homogeneous but periodic input (e.g. a
            // repeating log-line stream whose period does not divide the 8 KB
            // chunk size): the sampled bytes land on a different phase in each
            // chunk, so two identical-distribution chunks look different and
            // the block is split at 8 KB, then re-split on every window,
            // cascading a large stream into hundreds of tiny blocks whose
            // per-block headers dwarf the payload. The rate-1 scan reads every
            // byte, so it sees periodic data as uniform and declines to split,
            // while still finding genuine content boundaries (measured better
            // ratio on the real decode corpus, and no longer expands a
            // periodic stream vs a single full block). lazy/greedy keep the
            // coarse samplers (lower hash_log => not sensitive enough to
            // alias here).
            super::strategy::StrategyTag::Lazy => {
                if self.lazy_depth >= 2 {
                    Some(4)
                } else {
                    Some(2)
                }
            }
            super::strategy::StrategyTag::Btlazy2 => Some(4),
            super::strategy::StrategyTag::BtOpt
            | super::strategy::StrategyTag::BtUltra
            | super::strategy::StrategyTag::BtUltra2 => Some(4),
        }
    }
}

/// Apply the public-parameter per-knob overrides (#27) onto the
/// level-resolved [`LevelParams`], in place. Runs in [`Matcher::reset`]
/// after the level params are computed and before backend selection, so
/// a strategy override re-routes the backend uniformly. An all-`None`
/// override is a no-op the caller skips via
/// [`super::parameters::ParamOverrides::is_empty`], keeping the default
/// level geometry byte-identical.
fn apply_param_overrides(params: &mut LevelParams, ov: &super::parameters::ParamOverrides) {
    use super::strategy::SearchMethod;

    // 1. Strategy override re-derives tag / search / lazy depth.
    if let Some(strategy) = ov.strategy {
        let tag = strategy.tag();
        params.strategy_tag = tag;
        params.search = tag.search();
        params.lazy_depth = strategy.lazy_depth();
    }

    // 2. Ensure the active backend's config row exists (synthesize a
    //    default when a strategy override moved off the native row).
    match params.search {
        SearchMethod::Fast => {
            params.fast.get_or_insert(FAST_L1);
        }
        SearchMethod::DoubleFast => {
            params.dfast.get_or_insert(DFAST_L3);
        }
        SearchMethod::RowHash => {
            params.row.get_or_insert(ROW_L5);
        }
        SearchMethod::HashChain | SearchMethod::BinaryTree => {
            // A `Btlazy2` strategy override moved off a non-HC row needs the
            // BT 5-byte finder hash (donor minMatch 5); other synthesized HC
            // rows keep the 4-byte default. An explicit `min_match` override
            // below refines this further.
            params.hc.get_or_insert(HcConfig {
                search_mls: if matches!(params.strategy_tag, super::strategy::StrategyTag::Btlazy2)
                {
                    5
                } else {
                    HC_OVERRIDE_DEFAULT.search_mls
                },
                ..HC_OVERRIDE_DEFAULT
            });
        }
    }

    // 3. window_log (bounds-checked at <= 30 by the builder).
    if let Some(window_log) = ov.window_log {
        params.window_log = window_log;
    }

    // 4. Per-backend numeric knobs map into the active config, mirroring
    //    the donor `cParams` -> matcher translation documented on each
    //    config struct.
    match params.search {
        SearchMethod::Fast => {
            if let Some(fast) = params.fast.as_mut() {
                if let Some(hash_log) = ov.hash_log {
                    fast.hash_log = hash_log;
                }
                if let Some(min_match) = ov.min_match {
                    fast.mls = min_match;
                }
            }
        }
        SearchMethod::DoubleFast => {
            if let Some(dfast) = params.dfast.as_mut() {
                // hashLog -> long table, chainLog -> short table (the
                // dfast secondary index). Both bounds-checked <= 30, so
                // the `u8` casts are lossless.
                if let Some(hash_log) = ov.hash_log {
                    dfast.long_hash_log = hash_log as u8;
                }
                if let Some(chain_log) = ov.chain_log {
                    dfast.short_hash_log = chain_log as u8;
                }
            }
        }
        SearchMethod::RowHash => {
            if let Some(row) = params.row.as_mut() {
                // Row hash-table width override (mirrors dfast `long_hash_log`
                // / hc `hash_log`). Row has no separate chain table — the
                // per-row depth comes from `search_log` below — so only
                // `hash_log` maps here; `chain_log` has no Row analogue.
                if let Some(hash_log) = ov.hash_log {
                    row.hash_bits = hash_log as usize;
                }
                if let Some(search_log) = ov.search_log {
                    // Donor: rowLog = clamp(searchLog, 4, 6);
                    //        nbAttempts = 1 << min(searchLog, rowLog).
                    let row_log = (search_log as usize).clamp(4, 6);
                    row.row_log = row_log;
                    row.search_depth = 1usize << (search_log as usize).min(row_log);
                }
                if let Some(target_length) = ov.target_length {
                    row.target_len = target_length as usize;
                }
                if let Some(min_match) = ov.min_match {
                    row.mls = min_match as usize;
                }
            }
        }
        SearchMethod::HashChain | SearchMethod::BinaryTree => {
            if let Some(hc) = params.hc.as_mut() {
                if let Some(hash_log) = ov.hash_log {
                    hc.hash_log = hash_log as usize;
                }
                if let Some(chain_log) = ov.chain_log {
                    hc.chain_log = chain_log as usize;
                }
                if let Some(search_log) = ov.search_log {
                    hc.search_depth = 1usize << search_log;
                }
                if let Some(target_length) = ov.target_length {
                    hc.target_len = target_length as usize;
                }
                if let Some(min_match) = ov.min_match {
                    // Donor `mls = BOUNDED(4, cParams.minMatch, 6)`: a BT
                    // min_match override maps into the finder hash width. Only
                    // the BT body reads `search_mls`; HC/lazy keep 4-byte
                    // hashing regardless, so this is a no-op for them.
                    hc.search_mls = (min_match as usize).clamp(4, 6);
                }
            }
        }
    }
}

/// Map the resolved runtime strategy to the donor LDM strategy ordinal
/// (1..=9) that [`super::ldm::params::LdmParams::adjust_for`] expects.
/// The collapsed `Lazy` tag splits on `lazy_depth` (lazy = 4, lazy2 = 5).
#[cfg(feature = "hash")]
fn ldm_strategy_ordinal(tag: super::strategy::StrategyTag, lazy_depth: u8) -> u32 {
    use super::strategy::StrategyTag;
    match tag {
        StrategyTag::Fast => 1,
        StrategyTag::Dfast => 2,
        StrategyTag::Greedy => 3,
        StrategyTag::Lazy => {
            if lazy_depth >= 2 {
                5
            } else {
                4
            }
        }
        // Donor `ZSTD_btlazy2` ordinal.
        StrategyTag::Btlazy2 => 6,
        StrategyTag::BtOpt => 7,
        StrategyTag::BtUltra => 8,
        StrategyTag::BtUltra2 => 9,
    }
}

/// `ceil(log2(size))` of a source-size hint, with a zero hint floored to
/// [`MIN_WINDOW_LOG`]. This is the single quantization every hint-dependent
/// matcher parameter is derived from: the window-log cap, the HC / Fast hash
/// and chain widths, the Dfast / Row table widths, the L22 config buckets, and
/// the Fast attach-vs-copy cutoff. Two hints sharing this value resolve to the
/// identical matcher shape, which is why it (not the raw byte count) keys the
/// primed-dictionary snapshot — see [`PrimedKey`]. Operates on the full `u64`
/// so callers comparing a hint against a cutoff get the same bucketed decision
/// here and at the driver, with no `as usize` truncation on 32-bit targets.
pub(crate) fn source_size_ceil_log(size: u64) -> u8 {
    if size == 0 {
        MIN_WINDOW_LOG
    } else {
        (64 - (size - 1).leading_zeros()) as u8
    }
}

/// Donor `ZSTD_shouldAttachDict` cutoff for the Fast strategy, as a ceil-log
/// bucket: 8 KiB = `2^13`, and `bucket <= 13` is exactly `hint <= 8192` because
/// the bucket is monotone in the hint. A hint at or below this (or unknown,
/// `None`) ATTACHES the dictionary (a separate immutable table); a larger hint
/// COPIES it into the live table. Shared by `reset` (which records the mode in
/// the primed-snapshot key) and `prime_with_dictionary` (which acts on it).
const FAST_ATTACH_DICT_CUTOFF_LOG: u8 = 13;

/// Dfast counterpart of [`FAST_ATTACH_DICT_CUTOFF_LOG`]: donor
/// `ZSTD_dictMatchState` attach cutoff for the double-fast strategy is 16 KiB
/// (`2^14`), so small / unknown-size inputs ATTACH (separate immutable dict
/// long+short tables + dual-probe in `start_matching_fast_loop`) and larger
/// known-size inputs COPY (re-prime the dict into the live tables, where the
/// dense scan matches it as window history). The attach build also self-gates
/// on `use_fast_loop` inside `skip_matching_for_dict_attach` — only the
/// fast-loop levels (L3 / Default / L0) carry the dual-probe.
const DFAST_ATTACH_DICT_CUTOFF_LOG: u8 = 14;

/// `ZSTD_dictMatchState` attach cutoff for the Row (greedy/lazy) strategy is
/// 32 KiB (`2^15`, donor `attachDictSizeCutoffs`): small / unknown-size inputs
/// ATTACH the dict into the separate immutable row index (bounded dual-probe in
/// `row_candidate_rl`), larger known-size inputs dense-COPY into the live rows.
const ROW_ATTACH_DICT_CUTOFF_LOG: u8 = 15;

// Source-size cap for the dfast hash bits when a size hint is present: a tiny
// input needs no larger hash than its window. The donor `cParams.hashLog` /
// `chainLog` (from `DfastConfig`) caps it from above at the call site.
fn dfast_hash_bits_for_window(max_window_size: usize) -> usize {
    let window_log = (usize::BITS - 1 - max_window_size.leading_zeros()) as usize;
    window_log.max(MIN_WINDOW_LOG as usize)
}

fn row_hash_bits_for_window(max_window_size: usize) -> usize {
    // Donor `ZSTD_adjustCParams_internal` cap: `hashLog <= windowLog + 1`.
    // The `+ 1` is load-bearing for L12, whose donor hashLog (23) exceeds
    // its windowLog (22) — a plain `windowLog` cap would shrink the L12
    // table on EVERY hinted reset and split primed snapshots between
    // hinted and unhinted frames that resolve to the identical geometry.
    // No constant upper clamp: the old `ROW_HASH_BITS` (20) ceiling
    // predates the lazy band moving onto Row (L9-12 carry donor hashLog
    // 21-23).
    let window_log = (usize::BITS - 1 - max_window_size.leading_zeros()) as usize;
    (window_log + 1).max(MIN_WINDOW_LOG as usize)
}

/// `floor(log2(window))` for the HashChain table-log cap (donor
/// `ZSTD_adjustCParams_internal`). The caller clamps the level's `hash_log` /
/// `chain_log` from above with this so a small hinted input doesn't allocate the
/// full level's tables.
fn hc_hash_bits_for_window(max_window_size: usize) -> usize {
    let window_log = (usize::BITS - 1 - max_window_size.leading_zeros()) as usize;
    window_log.max(MIN_WINDOW_LOG as usize)
}

/// Parameter table for numeric compression levels 1–22.
///
/// Each entry maps a zstd compression level to the best-available matcher
/// backend and tuning knobs. High levels map to dedicated parse modes:
/// btopt (16-17), btultra (18), btultra2 (19-22) — matching donor
/// `clevels.h` (level 19 is `ZSTD_btultra2`, not plain btultra).
///
/// Index 0 = level 1, index 21 = level 22.
#[rustfmt::skip]
const LEVEL_TABLE: [LevelParams; 22] = [
    // Exactly one of fast/dfast/hc/row is Some per row, matching the strategy
    // backend; the rest are None (not dead placeholders).
    // Lvl  Strategy       wlog  lazy  per-strategy config
    // ---  -------------- ----  ----  -------------------
    /* 1 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Fast, search: super::strategy::SearchMethod::Fast, window_log: 19, lazy_depth: 0, fast: Some(FAST_L1), dfast: None, hc: None, row: None },
    /* 2 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Fast, search: super::strategy::SearchMethod::Fast, window_log: 20, lazy_depth: 0, fast: Some(FAST_L2), dfast: None, hc: None, row: None },
    /* 3 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Dfast, search: super::strategy::SearchMethod::DoubleFast, window_log: 21, lazy_depth: 1, fast: None, dfast: Some(DFAST_L3), hc: None, row: None },
    /* 4 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Dfast, search: super::strategy::SearchMethod::DoubleFast, window_log: 21, lazy_depth: 1, fast: None, dfast: Some(DFAST_L4), hc: None, row: None },
    // target_len column for L5..=L15 matches donor cParams.targetLength
    // from clevels.h table[0] (default — srcSize > 256 KB). Donor uses
    // it as the lazy outer loop's `sufficient_len` (nice-match) threshold.
    // Inflating it above donor forces the chain walk to complete
    // search_depth iterations instead of breaking on the first
    // long-enough match — the dominant cost in the L5..=L15 speed
    // regression vs FFI (see lazy_band_target_len_matches_donor_default_table).
    /* 5 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Greedy, search: super::strategy::SearchMethod::RowHash, window_log: 21, lazy_depth: 0, fast: None, dfast: None, hc: None, row: Some(ROW_L5) },
    // L6-12: the donor runs the lazy/lazy2 strategies on the ROW-based
    // match finder by default (`ZSTD_resolveRowMatchFinderMode`: row mode
    // is on for greedy..lazy2 whenever SIMD is available) — a bounded
    // SIMD tag scan per row instead of a pointer-chasing hash-chain walk.
    // Our HashChain walk on these levels was ~75% of L10 wall time on the
    // 1 MiB corpus (dependent chain-table loads). Same `RowConfig`
    // derivation as `ROW_L5` above, donor values per level in the
    // `ROW_L6..ROW_L12` comment block.
    /* 6 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Lazy, search: super::strategy::SearchMethod::RowHash, window_log: 21, lazy_depth: 1, fast: None, dfast: None, hc: None, row: Some(ROW_L6) },
    /* 7 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Lazy, search: super::strategy::SearchMethod::RowHash, window_log: 21, lazy_depth: 1, fast: None, dfast: None, hc: None, row: Some(ROW_L7) },
    /* 8 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Lazy, search: super::strategy::SearchMethod::RowHash, window_log: 21, lazy_depth: 2, fast: None, dfast: None, hc: None, row: Some(ROW_L8) },
    /* 9 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Lazy, search: super::strategy::SearchMethod::RowHash, window_log: 22, lazy_depth: 2, fast: None, dfast: None, hc: None, row: Some(ROW_L9) },
    /*10 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Lazy, search: super::strategy::SearchMethod::RowHash, window_log: 22, lazy_depth: 2, fast: None, dfast: None, hc: None, row: Some(ROW_L10) },
    /*11 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Lazy, search: super::strategy::SearchMethod::RowHash, window_log: 22, lazy_depth: 2, fast: None, dfast: None, hc: None, row: Some(ROW_L11) },
    /*12 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Lazy, search: super::strategy::SearchMethod::RowHash, window_log: 22, lazy_depth: 2, fast: None, dfast: None, hc: None, row: Some(ROW_L12) },
    // L13-15: reference uses btlazy2 (binary-tree finder) with searchLog 4/5/6
    // (search_depth 16/32/64) and targetLength 32. We run the hash-chain Lazy
    // parser here, so we mirror the reference search budget rather than inflate
    // it: matching the table keeps speed near the reference and makes per-level
    // perf divergences comparable. The binary-tree finder that would let a
    // smaller searchLog find longer matches (and re-establish a strict ratio
    // ladder above L12) is tracked separately; until it lands these levels sit
    // close to L12 on hash-chain inputs by design.
    /*13 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Btlazy2, search: super::strategy::SearchMethod::BinaryTree, window_log: 22, lazy_depth: 2, fast: None, dfast: None, hc: Some(HcConfig { hash_log: 22, chain_log: 22, search_depth: 16, target_len: 32, search_mls: 5 }), row: None },
    /*14 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Btlazy2, search: super::strategy::SearchMethod::BinaryTree, window_log: 22, lazy_depth: 2, fast: None, dfast: None, hc: Some(HcConfig { hash_log: 23, chain_log: 22, search_depth: 32, target_len: 32, search_mls: 5 }), row: None },
    /*15 */ LevelParams { strategy_tag: super::strategy::StrategyTag::Btlazy2, search: super::strategy::SearchMethod::BinaryTree, window_log: 22, lazy_depth: 2, fast: None, dfast: None, hc: Some(HcConfig { hash_log: 23, chain_log: 23, search_depth: 64, target_len: 32, search_mls: 5 }), row: None },
    /*16 */ LevelParams { strategy_tag: super::strategy::StrategyTag::BtOpt, search: super::strategy::SearchMethod::BinaryTree, window_log: 22, lazy_depth: 2, fast: None, dfast: None, hc: Some(HcConfig { hash_log: 22, chain_log: 22, search_depth: 32, target_len: 48, search_mls: 5 }), row: None },
    /*17 */ LevelParams { strategy_tag: super::strategy::StrategyTag::BtOpt, search: super::strategy::SearchMethod::BinaryTree, window_log: 23, lazy_depth: 2, fast: None, dfast: None, hc: Some(HcConfig { hash_log: 22, chain_log: 23, search_depth: 32, target_len: 64, search_mls: 4 }), row: None },
    /*18 */ LevelParams { strategy_tag: super::strategy::StrategyTag::BtUltra, search: super::strategy::SearchMethod::BinaryTree, window_log: 23, lazy_depth: 2, fast: None, dfast: None, hc: Some(HcConfig { hash_log: 22, chain_log: 23, search_depth: 64, target_len: 64, search_mls: 4 }), row: None },
    /*19 */ LevelParams { strategy_tag: super::strategy::StrategyTag::BtUltra2, search: super::strategy::SearchMethod::BinaryTree, window_log: 23, lazy_depth: 2, fast: None, dfast: None, hc: Some(HcConfig { hash_log: 22, chain_log: 24, search_depth: 128, target_len: 256, search_mls: 4 }), row: None },
    /*20 */ LevelParams { strategy_tag: super::strategy::StrategyTag::BtUltra2, search: super::strategy::SearchMethod::BinaryTree, window_log: 25, lazy_depth: 2, fast: None, dfast: None, hc: Some(HcConfig { hash_log: 23, chain_log: 25, search_depth: 128, target_len: 256, search_mls: 4 }), row: None },
    /*21 */ LevelParams { strategy_tag: super::strategy::StrategyTag::BtUltra2, search: super::strategy::SearchMethod::BinaryTree, window_log: 26, lazy_depth: 2, fast: None, dfast: None, hc: Some(BTULTRA2_HC_CONFIG), row: None },
    /*22 */ LevelParams { strategy_tag: super::strategy::StrategyTag::BtUltra2, search: super::strategy::SearchMethod::BinaryTree, window_log: 27, lazy_depth: 2, fast: None, dfast: None, hc: Some(BTULTRA2_HC_CONFIG_L22), row: None },
];

/// Donor `minSrcSize` assumption when building a dictionary's prepared cParams
/// with an unknown source (`zstd_compress.c` `ZSTD_adjustCParams_internal`,
/// `ZSTD_cpm_createCDict`: `if (dictSize && srcSize == UNKNOWN) srcSize =
/// minSrcSize` where `minSrcSize = (1<<9) + 1`). Used by [`cdict_table_logs`].
const DICT_MIN_SRC_SIZE: u64 = 513;

/// Donor `ZSTD_dictAndWindowLog` (`zstd_compress.c`): the window log large
/// enough to address both the source and the dictionary, used when downsizing
/// the hash / chain logs for a dictionary-bearing compress. `window_log` is the
/// (already source-clamped) compress window; `src_size` / `dict_size` are the
/// assumed source and the dictionary length.
fn dict_and_window_log(window_log: u8, src_size: u64, dict_size: u64) -> u32 {
    if dict_size == 0 {
        return window_log as u32;
    }
    let window_size: u64 = 1u64 << window_log;
    let dict_and_window = dict_size.saturating_add(window_size);
    if window_size >= dict_size.saturating_add(src_size) {
        // Window already covers source + dictionary.
        window_log as u32
    } else {
        // ceil(log2(dictAndWindowSize)) = highbit32(x - 1) + 1.
        source_size_ceil_log(dict_and_window) as u32
    }
}

/// Donor `ZSTD_createCDict` table geometry: the `(hash_log, chain_log)` a
/// dictionary's prepared match-finder tables get, mirroring
/// `ZSTD_adjustCParams_internal` under `ZSTD_cpm_createCDict`. A dictionary
/// supplies the long matches, so donor downsizes the table widths toward the
/// dict-and-window log (assuming a `minSrcSize` source) while the live window
/// stays source-sized. `window_log` is the resolved compress window; `hash_log`
/// / `chain_log` are the level's own widths; `uses_bt` selects the binary-tree
/// `cycleLog` (`chainLog - 1`) vs the hash-chain one (`chainLog`).
fn cdict_table_logs(
    window_log: u8,
    hash_log: usize,
    chain_log: usize,
    uses_bt: bool,
    dict_size: usize,
) -> (usize, usize) {
    let dict_size = dict_size as u64;
    // createCDict assumes a minSrcSize source when the real size is unknown.
    let src_size = DICT_MIN_SRC_SIZE;
    // Source-size window resize (donor caps windowLog by ceil_log2(src+dict)).
    let tsize = src_size.saturating_add(dict_size);
    let resized_window_log = (window_log as u32)
        .min(source_size_ceil_log(tsize) as u32)
        .max(1);
    let daw = dict_and_window_log(resized_window_log as u8, src_size, dict_size);
    // `ZSTD_cycleLog(chainLog, strategy)`: chainLog - 1 for binary-tree finders.
    let cycle_log = (chain_log as u32).saturating_sub(uses_bt as u32);
    let new_hash_log = if hash_log as u32 > daw + 1 {
        (daw + 1) as usize
    } else {
        hash_log
    };
    let new_chain_log = if cycle_log > daw {
        chain_log.saturating_sub((cycle_log - daw) as usize)
    } else {
        chain_log
    };
    (new_hash_log, new_chain_log)
}

/// Smallest window_log the encoder will use regardless of source size.
pub(crate) const MIN_WINDOW_LOG: u8 = 10;
/// Conservative floor for source-size-hinted window tuning.
///
/// Hinted windows below 16 KiB (`window_log < 14`) currently regress C-FFI
/// interoperability on certain compressed-block patterns. Keep hinted
/// windows at 16 KiB or larger until that compatibility gap is closed.
const MIN_HINTED_WINDOW_LOG: u8 = 14;

/// Adjust level parameters for a known source size.
///
/// This derives a cap from `ceil(log2(src_size))`, then clamps it to
/// [`MIN_HINTED_WINDOW_LOG`] (16 KiB). A zero-byte size hint is treated as
/// [`MIN_WINDOW_LOG`] for the raw ceil-log step and then promoted to the hinted
/// floor. This keeps tables bounded for small inputs while preserving the
/// encoder's baseline minimum supported window.
/// For the HC backend, `hash_log` and `chain_log` are reduced
/// proportionally.
fn adjust_params_for_source_size(mut params: LevelParams, src_size: u64) -> LevelParams {
    // Derive a source-size-based cap from ceil(log2(src_size)), then
    // clamp first to MIN_WINDOW_LOG (baseline encoder minimum) and then to
    // MIN_HINTED_WINDOW_LOG (16 KiB hinted floor). For tiny or zero hints we
    // therefore keep a 16 KiB effective minimum window in hinted mode.
    // Raw ceil(log2(src_size)) drives the internal table sizes. The
    // advertised `window_log` is separately floored at MIN_HINTED_WINDOW_LOG
    // (a decoder-interop requirement on the wire format), but the hash /
    // chain table widths are internal and never appear in the frame, so they
    // can track the actual source size below that floor.
    let raw_src_log = source_size_ceil_log(src_size);
    let src_log = raw_src_log.max(MIN_WINDOW_LOG).max(MIN_HINTED_WINDOW_LOG);
    if src_log < params.window_log {
        params.window_log = src_log;
    }
    // Internal match-finder tables are sized from `table_log` — the RAW
    // source log (floored only at the baseline `MIN_WINDOW_LOG`), NOT the
    // wire `window_log` floor. The table widths never appear in the frame, so
    // for small inputs they can track the actual source size and avoid
    // zeroing a window-sized table per frame; large inputs keep the level's
    // widths. The cap is applied with the same per-backend headroom the
    // level table uses, so the load factor (and match quality) is unchanged.
    // The Row / Dfast backends derive their table widths from the source in
    // `reset` (their `set_hash_bits` recomputes there), so they are not
    // adjusted here.
    let table_log = raw_src_log.max(MIN_WINDOW_LOG);
    let backend = params.backend();
    if backend == super::strategy::BackendTag::HashChain {
        let hc = params
            .hc
            .as_mut()
            .expect("HashChain level row carries an HcConfig");
        if (table_log + 2) < hc.hash_log as u8 {
            hc.hash_log = (table_log + 2) as usize;
        }
        if (table_log + 1) < hc.chain_log as u8 {
            hc.chain_log = (table_log + 1) as usize;
        }
    } else if backend == super::strategy::BackendTag::Simple {
        let fast = params
            .fast
            .as_mut()
            .expect("Fast level row carries a FastConfig");
        let fast_cap = (table_log + 1) as u32;
        if fast_cap < fast.hash_log {
            fast.hash_log = fast_cap;
        }
    }
    params
}

fn level22_btultra2_params_for_source_size(source_size: Option<u64>) -> LevelParams {
    let mut hc = match source_size {
        Some(size) if size <= 16 * 1024 => BTULTRA2_HC_CONFIG_L22_16K,
        Some(size) if size <= 128 * 1024 => BTULTRA2_HC_CONFIG_L22_128K,
        Some(size) if size <= 256 * 1024 => BTULTRA2_HC_CONFIG_L22_256K,
        _ => BTULTRA2_HC_CONFIG_L22,
    };
    let mut window_log = match source_size {
        Some(size) if size <= 16 * 1024 => 14,
        Some(size) if size <= 128 * 1024 => 17,
        Some(size) if size <= 256 * 1024 => 18,
        _ => 27,
    };
    if let Some(size) = source_size
        && size > 256 * 1024
    {
        let src_log = source_size_ceil_log(size);
        window_log = window_log.min(src_log.max(MIN_WINDOW_LOG));
        let adjusted_table_log = window_log as usize + 1;
        hc.hash_log = hc.hash_log.min(adjusted_table_log);
        hc.chain_log = hc.chain_log.min(adjusted_table_log);
    }
    LevelParams {
        strategy_tag: super::strategy::StrategyTag::BtUltra2,
        search: super::strategy::SearchMethod::BinaryTree,
        window_log,
        lazy_depth: 2,
        fast: None,
        dfast: None,
        hc: Some(hc),
        row: None,
    }
}

/// Estimated steady-state heap footprint of a one-shot compression context
/// at `level` (window history + match-finder tables + block staging), in
/// bytes. Computed from the same per-level tuning table the encoder
/// resolves at frame start, so the estimate tracks the real allocations;
/// it is an upper-bound style budget figure, not an exact accounting.
pub fn estimated_compression_workspace_bytes(level: CompressionLevel) -> usize {
    let params = resolve_level_params(level, None);
    let window = 1usize << params.window_log;
    let tables = params.fast.map(|f| 4usize << f.hash_log).unwrap_or(0)
        + params
            .dfast
            .map(|d| (4usize << d.long_hash_log) + (4usize << d.short_hash_log))
            .unwrap_or(0)
        + params
            .hc
            .map(|h| {
                // The btopt/btultra cascade also allocates the HC3
                // short-match side table (`HC3_HASH_LOG`); HC-mode lazy
                // levels leave it sized to zero.
                let hash3 = if matches!(params.search, super::strategy::SearchMethod::BinaryTree) {
                    4usize << super::match_table::storage::HC3_HASH_LOG
                } else {
                    0
                };
                (4usize << h.hash_log) + (4usize << h.chain_log) + hash3
            })
            .unwrap_or(0)
        + params
            .row
            .map(|r| (4usize << r.hash_bits) + (2usize << r.hash_bits))
            .unwrap_or(0);
    // Block staging: literal + sequence buffers plus the compressed-block
    // scratch, each bounded by the 128 KiB block size.
    let staging = 3 * (128 * 1024);
    window + tables + staging
}

/// Resolve a [`CompressionLevel`] to internal tuning parameters,
/// optionally adjusted for a known source size.
fn resolve_level_params(level: CompressionLevel, source_size: Option<u64>) -> LevelParams {
    if matches!(level, CompressionLevel::Level(22)) {
        return level22_btultra2_params_for_source_size(source_size);
    }
    let params = match level {
        CompressionLevel::Uncompressed => LevelParams {
            strategy_tag: super::strategy::StrategyTag::Fast,
            search: super::strategy::SearchMethod::Fast,
            // Uncompressed frames emit raw blocks and never reference
            // history; advertising a larger window only inflates
            // decoder-side buffer reservation. Stay at 17 (128 KiB).
            window_log: 17,
            lazy_depth: 0,
            // Beyond-donor: hash_log=14 (vs donor's 13) for 2× fewer
            // collisions on structured corpora. Donor's "base for negative"
            // row has targetLength=1 → step_size = 1 + 0 + 1 = 2.
            fast: Some(FastConfig {
                hash_log: 14,
                mls: 6,
                step_size: 2,
            }),
            dfast: None,
            hc: None,
            row: None,
        },
        CompressionLevel::Fastest => {
            // Only the Fast-specific cParams
            // (fast_hash_log / fast_mls / fast_step_size) align
            // with Uncompressed / negative-base row. window_log
            // stays at LEVEL_TABLE[0]'s value (19) — Fastest still
            // does real compression on a full window, unlike
            // Uncompressed which clamps to 17.
            let mut p = LEVEL_TABLE[0];
            p.fast = Some(FastConfig {
                hash_log: 14,
                mls: 6,
                step_size: 2,
            });
            p
        }
        CompressionLevel::Default => LEVEL_TABLE[2],
        CompressionLevel::Better => LEVEL_TABLE[6],
        CompressionLevel::Best => LEVEL_TABLE[10],
        CompressionLevel::Level(n) => {
            if n > 0 {
                let idx = (n as usize).min(CompressionLevel::MAX_LEVEL as usize) - 1;
                LEVEL_TABLE[idx]
            } else if n == 0 {
                // Level 0 = default, matching C zstd semantics.
                LEVEL_TABLE[CompressionLevel::DEFAULT_LEVEL as usize - 1]
            } else {
                // Negative levels — donor sets
                // targetLength = -level (clampedCompressionLevel),
                // yielding step_size = (-level) + 1 since
                // !(targetLength) = 0 when targetLength > 0.
                // So L-1..L-7 get step_size 2..8. Acceleration
                // gradient comes from larger step skipping more
                // positions per iter (faster, worse ratio).
                // Clamp to donor's MIN_LEVEL before negating so
                // i32::MIN can't overflow on `-n`.
                let clamped = n.max(CompressionLevel::MIN_LEVEL);
                let target_length = (-clamped) as usize;
                let step_size = target_length + 1;
                // Donor row-0 ("base for negative", clevels.h srcSize>256KB):
                // hashLog=13, minMatch=7. The 32 KiB hash table (2^13 * 4B)
                // is L1d-resident on contemporary cores, so every probe is an
                // L1 hit; hashLog=14 (64 KiB) overflows a 32 KiB L1d and turns
                // each probe into an L2 access. minMatch=7 (vs 6) skips
                // short-distance 6-byte matches: fewer sequences, less
                // extension/emit work, and parity with the donor's negative
                // ladder on both ratio and throughput.
                LevelParams {
                    strategy_tag: super::strategy::StrategyTag::Fast,
                    search: super::strategy::SearchMethod::Fast,
                    window_log: 19,
                    lazy_depth: 0,
                    fast: Some(FastConfig {
                        hash_log: 13,
                        mls: 7,
                        step_size,
                    }),
                    dfast: None,
                    hc: None,
                    row: None,
                }
            }
        }
    };
    if let Some(size) = source_size {
        adjust_params_for_source_size(params, size)
    } else {
        params
    }
}

/// The cheap fingerprint pre-splitter level for a compression level (the
/// C-like `blockSplitterLevel`), resolved through the same per-level
/// `LevelParams` table as every other tuning knob. `None` keeps the whole
/// 128 KiB block. The frame loop reads this instead of hardcoding the
/// level→split mapping at the call site.
pub(crate) fn level_pre_split(level: CompressionLevel) -> Option<usize> {
    // Named presets are pure aliases: resolve to their numeric level and
    // read the split knob from the same `LevelParams` table as everything
    // else. `Uncompressed` (raw blocks) has no numeric equivalent.
    let numeric = level.numeric_level()?;
    resolve_level_params(CompressionLevel::Level(numeric), None)
        .pre_split()
        .map(usize::from)
}

/// Backend storage for [`MatchGeneratorDriver`]. Exactly one match-finder
/// state lives in the driver at a time — the active variant. Backend
/// transitions in [`Matcher::reset`] drain the current variant's allocations
/// into the shared `vec_pool` and then replace `storage` with a freshly
/// constructed variant for the new backend.
///
/// Replaces the prior pattern of four parallel fields (`match_generator`,
/// `dfast_match_generator: Option<…>`, `row_match_generator: Option<…>`,
/// `hc_match_generator: Option<…>`) + an `active_backend: BackendTag`
/// discriminator: the parallel layout kept drained inner structures
/// allocated across backend switches, and every per-frame/per-slice
/// driver operation had to dispatch on `active_backend` to pick the
/// right field. A single enum collapses the storage and makes the
/// dispatcher pattern-match on the storage variant directly — same
/// number of arms, but `storage.backend()` is now the canonical source
/// of truth and dead variants are dropped when the active backend
/// changes.
#[derive(Clone)]
enum MatcherStorage {
    /// Donor `ZSTD_fast` family. Constructed by
    /// [`MatchGeneratorDriver::new`] as the initial variant and
    /// re-selected by [`Matcher::reset`] for any [`CompressionLevel`]
    /// that `resolve_level_params` maps to [`StrategyTag::Fast`]
    /// (`Uncompressed`, `Fastest`, `Level(1)`, and any non-positive
    /// `Level(n)` not equal to `0`).
    Simple(FastKernelMatcher),
    /// Donor `ZSTD_dfast` family — two-table hash chain. Selected for
    /// any level that resolves to [`StrategyTag::Dfast`] in
    /// `resolve_level_params` (`Default`, `Level(0)`, `Level(2)`,
    /// `Level(3)`).
    Dfast(DfastMatchGenerator),
    /// Donor `ZSTD_greedy` family with row hashing. Selected for any
    /// level that resolves to [`StrategyTag::Greedy`] (currently
    /// `Level(4)` only).
    Row(RowMatchGenerator),
    /// Donor `ZSTD_lazy2` and the BT-based optimal modes
    /// (`btopt` / `btultra` / `btultra2`). Selected for any level that
    /// resolves to [`StrategyTag::Lazy`], [`StrategyTag::BtOpt`],
    /// [`StrategyTag::BtUltra`], or [`StrategyTag::BtUltra2`]
    /// (`Better`, `Best`, `Level(5..=22)`, and any `Level(n)` with
    /// `n > MAX_LEVEL` — `resolve_level_params` clamps positive
    /// numeric levels at `MAX_LEVEL = 22` via
    /// `Level(n).clamp(1, MAX_LEVEL)`, so `Level(23..=i32::MAX)` all
    /// land on `BtUltra2` here). The [`HcMatchGenerator`]'s internal
    /// [`HcBackend`] discriminator decides whether BT scratch is
    /// allocated.
    HashChain(HcMatchGenerator),
}

impl MatcherStorage {
    /// Heap bytes the active backend variant holds (tables, history, scratch).
    fn heap_size(&self) -> usize {
        match self {
            Self::Simple(m) => m.heap_size(),
            Self::Dfast(m) => m.heap_size(),
            Self::Row(m) => m.heap_size(),
            Self::HashChain(m) => m.heap_size(),
        }
    }

    /// [`super::strategy::BackendTag`] family of the active variant.
    fn backend(&self) -> super::strategy::BackendTag {
        use super::strategy::BackendTag;
        match self {
            Self::Simple(_) => BackendTag::Simple,
            Self::Dfast(_) => BackendTag::Dfast,
            Self::Row(_) => BackendTag::Row,
            Self::HashChain(_) => BackendTag::HashChain,
        }
    }
}

/// This is the default implementation of the `Matcher` trait. It allocates and reuses the buffers when possible.
pub struct MatchGeneratorDriver {
    vec_pool: Vec<Vec<u8>>,
    /// Active match-finder state. Exactly one backend lives here at a
    /// time; [`Matcher::reset`] drains the previous variant into
    /// `vec_pool` before swapping in a freshly constructed variant for
    /// the new backend. `storage.backend()` is the canonical source of
    /// truth for the parse family; `strategy_tag` carries the
    /// compile-time strategy chosen at the last `reset()`.
    storage: MatcherStorage,
    // Compile-time strategy tag resolved at `reset()` from the
    // requested `CompressionLevel`'s `LevelParams`. The driver's
    // hot-block dispatcher in `blocks/compressed.rs` matches on
    // this tag to enter the corresponding `Strategy`
    // monomorphisation (`compress_block::<S>`).
    strategy_tag: super::strategy::StrategyTag,
    // Decoupled search-method axis resolved at `reset()` from
    // `LevelParams.search`. The per-block dispatcher routes on this
    // (not on `strategy_tag`) so a level's parse and search backend can
    // be chosen independently. The `BinaryTree` arm still consults
    // `strategy_tag` to pick the opt `Strategy` ZST.
    search: super::strategy::SearchMethod,
    // Decoupled parse-mode axis resolved at `reset()` from
    // `LevelParams::parse()`. Independent of `search`: greedy / lazy /
    // lazy2 can run on any non-opt search backend. The backends still
    // read their own `lazy_depth` (kept in sync at `reset()`); this is
    // the authoritative parse selector for the dispatcher.
    parse: super::strategy::ParseMode,
    /// Test-only per-level recipe override applied in `reset()` before
    /// backend selection. Lets the parse×search matrix be exercised
    /// without editing `LEVEL_TABLE`; never compiled into production.
    #[cfg(test)]
    config_override: Option<(super::strategy::SearchMethod, super::strategy::ParseMode)>,
    /// Fine-grained per-knob overrides from the public
    /// [`super::parameters::CompressionParameters`] surface (#27).
    /// `None` (or an all-`None` [`super::parameters::ParamOverrides`])
    /// keeps the resolved level geometry byte-identical to plain
    /// level-based compression. Applied in [`Matcher::reset`] after the
    /// level params are resolved, before backend selection. Persists
    /// across resets (it is frame configuration, not a one-shot) until
    /// the caller changes it.
    param_overrides: Option<super::parameters::ParamOverrides>,
    slice_size: usize,
    base_slice_size: usize,
    // Frame header window size must stay at the configured live-window budget.
    // Dictionary retention expands internal matcher capacity only.
    reported_window_size: usize,
    // Tracks currently retained bytes that originated from primed dictionary
    // history and have not been evicted yet.
    dictionary_retained_budget: usize,
    // Source size hint for next frame (set via set_source_size_hint, cleared on reset).
    source_size_hint: Option<u64>,
    // Dictionary content size for the next frame (set via set_dictionary_size_hint,
    // consumed on reset). When present on a binary-tree / hash-chain backend, the
    // match-finder hash/chain tables are sized from the DICTIONARY (donor CDict
    // economics: a loaded dictionary supplies the long matches, so the live tables
    // can shrink to the dict's size tier) while the eviction window stays
    // source-sized. Mirrors donor `ZSTD_getCParamRowSize`, which picks the cParams
    // table column from `dictSize` for a dictionary-bearing compress.
    dictionary_size_hint: Option<usize>,
    // Normalized `ceil_log2` bucket of the frame's source-size hint, captured at
    // `reset` (where `source_size_hint` is consumed) via [`source_size_ceil_log`].
    // `None` means the frame was unhinted. Drives `prime_with_dictionary`'s donor
    // `ZSTD_shouldAttachDict` mode for the Simple/Fast backend: `None` (unknown)
    // or `<= FAST_ATTACH_DICT_CUTOFF_LOG` → attach (separate dict table, 2-cursor
    // `compress_block_fast_dict`); larger → copy (dictionary primed into the live
    // table, 4-cursor `compress_block_fast`). The primed-snapshot key is the
    // resolved shape ([`reset_shape`](Self::reset_shape)), not this bucket.
    reset_size_log: Option<u8>,
    // Hint-resolved matcher shape from the last `reset`: the [`LevelParams`], the
    // active backend's applied Dfast/Row hash-table width (`0` for HC/Fast), the
    // Fast attach-vs-copy mode, and the active LDM override (#27). Combined with
    // the frame's level into the [`PrimedKey`] that keys the primed snapshot, so
    // it is only restored into a reset that resolved the identical matcher AND
    // LDM configuration. `None` before the first `reset`.
    reset_shape: Option<(
        LevelParams,
        usize,
        bool,
        Option<super::parameters::LdmOverride>,
    )>,
    // One-shot borrowed block range `[start, end)` staged by the borrowed
    // Fast frame path (`set_borrowed_block`) for the NEXT
    // `start_matching` / `skip_matching_with_hint`. `Some` routes that
    // call to the Simple backend's borrowed scan instead of the owned
    // committed-block path; consumed (reset to `None`) by the routed
    // call. Always `None` on the owned streaming path.
    borrowed_pending: Option<(usize, usize)>,
    /// CDict-equivalent: snapshot of the post-prime matcher state taken
    /// once after the first dictionary prime — the backend `storage`
    /// (hash tables + dictionary history + offset history + window) plus
    /// the driver-level `dictionary_retained_budget`, the only two pieces
    /// `prime_with_dictionary` writes. Subsequent frames restore this
    /// (a table memcpy) instead of re-hashing every dictionary position,
    /// mirroring donor `ZSTD_compressBegin_usingCDict` copying the
    /// precomputed `cdict->matchState`. Invalidated when the dictionary
    /// changes; keyed by the [`PrimedKey`] resolved matcher shape so a snapshot
    /// is only restored into a reset that produces the same matcher — see
    /// `restore_primed_dictionary`.
    primed: Option<(MatcherStorage, usize, PrimedKey)>,
}

/// Identity of the matcher configuration a primed snapshot was captured under:
/// the FULLY RESOLVED matcher shape, not the raw source-size hint.
///
/// `reset()` resolves the hint into a [`LevelParams`] (window_log cap, the
/// HC/Fast table and search geometry, the parse depth/target-length that get
/// baked into the restored `storage`) plus, for the Dfast/Row backends, a
/// table-width derived from the hint's ceil-log bucket. The mapping from hint
/// to resolved shape is many-to-one: the source-size adjustment is monotone in
/// `ceil_log2(hint)`, and Level 22 additionally collapses several buckets onto
/// one donor tier (its `<= 16/128/256 KiB` thresholds). Keying on the raw hint
/// (or even its ceil-log bucket) therefore over-keys — two hints that resolve
/// to the identical matcher would each force a full re-prime. Keying on the
/// resolved (`params`, `table_bits`) pair restores across them.
///
/// `table_bits` is the hint-dependent hash-table width the ACTIVE backend
/// applied (`set_hash_bits` value for Dfast/Row; `0` for HC/Fast, whose widths
/// already live in `params`). The snapshot is only ever captured on the COPY
/// path (a hinted, above-cutoff frame), so `table_bits` is always the resolved
/// Dfast/Row value there, never the unhinted default.
///
/// `level` is kept alongside the resolved `params` because some stored matcher
/// state is derived from the level DIRECTLY, not through `params`: e.g. Dfast's
/// `use_fast_loop` is true for L3 but false for L4, yet L3 and L4 resolve to
/// byte-identical `params`. Without `level` a snapshot captured at L3 could be
/// restored into an L4 reset, installing the wrong `use_fast_loop`.
///
/// `fast_attach` records the Fast backend's attach-vs-copy mode
/// ([`FAST_ATTACH_DICT_CUTOFF_LOG`]) because that cutoff (8 KiB) falls INSIDE a
/// single resolved shape: an 8192- and an 8193-byte Level 1 hint both clamp to
/// window_log 14 with identical `params`/`table_bits`, yet 8192 attaches (a
/// separate dict table) while 8193 copies into the live table — two different
/// `storage` shapes. The frame compressor only captures/restores snapshots on
/// the copy path today, but keying on the mode keeps the snapshot identity
/// self-sufficient rather than relying on that external gate.
///
/// Restoring a snapshot whose key differs would reinstate the old `storage`
/// (and its `max_window_size` / table dimensions / parse params / dict-table
/// shape) under a reset that resolved a different shape — the encoder could
/// then search past the frame header's window and emit an undecodable match.
/// All fields must match before a restore is allowed.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PrimedKey {
    level: super::CompressionLevel,
    params: LevelParams,
    table_bits: usize,
    fast_attach: bool,
    /// Fine-grained LDM override (#27) active at capture time. The
    /// snapshot's cloned `storage` carries `BtMatcher::ldm_producer`,
    /// which is configured from this override; restoring a snapshot
    /// captured under a different LDM configuration (enable flip or
    /// changed knobs) would reinstate a stale producer. `params` already
    /// pins `window_log` / `strategy_tag` (the rest of the producer's
    /// identity), so folding the override completes the LDM identity.
    /// `None` = LDM off, matching `ParamOverrides::ldm`.
    ldm: Option<super::parameters::LdmOverride>,
}

impl MatchGeneratorDriver {
    /// `slice_size` sets the base block allocation size used for matcher input chunks.
    /// `max_slices_in_window` determines the initial window capacity at construction
    /// time. Effective window sizing is recalculated on every [`reset`](Self::reset)
    /// from the resolved compression level and optional source-size hint.
    pub(crate) fn new(slice_size: usize, max_slices_in_window: usize) -> Self {
        // Validate inputs before deriving window_log_init. Three
        // failure modes need explicit guards:
        //
        // 1. Zero args → `max_window_size = 0` → silent 1-byte
        //    degenerate window (useless).
        // 2. Multiplication overflow on `slice_size *
        //    max_slices_in_window` → wraps silently in release.
        // 3. `next_power_of_two` overflow when the product is
        //    above `1 << (usize::BITS - 1)` → modern Rust PANICS
        //    on overflow (older Rust returned 0).
        //
        // Catch all three at construction with a clear domain-
        // specific message via `assert!` + `checked_mul` +
        // `checked_next_power_of_two`, rather than letting either
        // mode produce a silent degenerate matcher OR a generic
        // panic deep in `FastKernelMatcher::with_params`.
        assert!(
            slice_size > 0,
            "MatchGeneratorDriver::new requires slice_size > 0 (got 0)",
        );
        assert!(
            max_slices_in_window > 0,
            "MatchGeneratorDriver::new requires max_slices_in_window > 0 (got 0)",
        );
        let max_window_size = max_slices_in_window
            .checked_mul(slice_size)
            .expect("MatchGeneratorDriver::new: slice_size * max_slices_in_window overflows usize");
        // Derive an effective window_log for the initial-state matcher.
        // `MatchGeneratorDriver::new` runs BEFORE any reset, so it has
        // no LevelParams to consult — we initialise to whatever
        // window_log fits the caller's requested max_window_size
        // (round up to the next power of two via `next_power_of_two`'s
        // log). Reset() overwrites all three params from the resolved
        // LevelParams.
        //
        // `checked_next_power_of_two` returns `None` if the next power
        // of two would overflow `usize`. Modern Rust's
        // `next_power_of_two` PANICS on overflow rather than returning
        // 0 (the panic message is generic and unhelpful), so use the
        // checked variant to surface the failure with a clear,
        // domain-specific error.
        let next_pow2 = max_window_size.checked_next_power_of_two().expect(
            "MatchGeneratorDriver::new: max_window_size too large for \
             next_power_of_two without overflow",
        );
        let window_log_init = next_pow2.trailing_zeros() as u8;
        Self {
            vec_pool: Vec::new(),
            storage: MatcherStorage::Simple(FastKernelMatcher::with_params(
                window_log_init,
                FAST_LEVEL_1_HASH_LOG,
                FAST_LEVEL_1_MLS,
                2, // donor default step_size (targetLength=0 → step=2)
            )),
            strategy_tag: super::strategy::StrategyTag::Fast,
            search: super::strategy::SearchMethod::Fast,
            parse: super::strategy::ParseMode::Greedy,
            #[cfg(test)]
            config_override: None,
            param_overrides: None,
            slice_size,
            base_slice_size: slice_size,
            // Report the ROUNDED-UP window size that the matcher
            // actually carries (via `window_log_init = log2(next_pow2)`
            // → matcher's `max_window_size = 1 << window_log_init =
            // next_pow2`). For non-power-of-two `slice_size *
            // max_slices_in_window` inputs, the unrounded value
            // would under-report the active backend's window until
            // the first `reset()` overwrites both sides from the
            // resolved LevelParams.
            reported_window_size: next_pow2,
            reset_size_log: None,
            reset_shape: None,
            dictionary_retained_budget: 0,
            source_size_hint: None,
            dictionary_size_hint: None,
            borrowed_pending: None,
            primed: None,
        }
    }

    fn level_params(level: CompressionLevel, source_size: Option<u64>) -> LevelParams {
        resolve_level_params(level, source_size)
    }

    /// Install the public-parameter per-knob overrides (#27) applied at
    /// the next [`Matcher::reset`]. `None` (or an all-`None` set) restores
    /// plain level-based geometry. Persists across resets until changed.
    pub(crate) fn set_param_overrides(
        &mut self,
        overrides: Option<super::parameters::ParamOverrides>,
    ) {
        self.param_overrides = overrides;
    }

    /// Active backend family derived from the storage variant. Single
    /// source of truth — no separate runtime tag to drift against.
    fn active_backend(&self) -> super::strategy::BackendTag {
        self.storage.backend()
    }

    fn simple_mut(&mut self) -> &mut FastKernelMatcher {
        match &mut self.storage {
            MatcherStorage::Simple(m) => m,
            _ => panic!("simple backend must be initialized by reset() before use"),
        }
    }

    /// Reclaim the per-block input buffer that the Simple backend
    /// just spent inside `start_matching` / `skip_matching_with_hint`.
    ///
    /// `FastKernelMatcher::take_recycled_space` returns the cleared
    /// (capacity-retained) `Vec<u8>` from the last
    /// `extend_history_with_pending`. We push it onto `vec_pool`
    /// as-is (with `len = 0`); `get_next_space()` is responsible for
    /// resizing the buffer back to `slice_size` on its next pop. The
    /// pushed length is irrelevant — only the capacity matters, and
    /// `extend_history_with_pending` preserves it. Without this
    /// recycle path, the Simple backend would allocate a new
    /// `Vec<u8>` per block — a measurable hot-path cost when blocks
    /// are small (~128 KiB) and processed at hundreds of MiB/s.
    fn recycle_simple_space(&mut self) {
        if let Some(space) = self.simple_mut().take_recycled_space() {
            // `space` is already cleared (len = 0) by
            // `extend_history_with_pending`; capacity is retained.
            // Leaving `len = 0` here avoids the cost of zero-filling
            // the entire allocation — `get_next_space()` resizes the
            // popped buffer up to `slice_size` on demand, so the
            // length the pool holds is irrelevant. This matters most
            // after a small-source-size hint has shrunk `slice_size`
            // mid-frame: the recycled buffer can be much larger than
            // the current `slice_size`, and zero-filling 128 KiB+ on
            // every block would erase the perf win the recycle path
            // is meant to deliver.
            self.vec_pool.push(space);
        }
    }

    /// Register a caller-owned input buffer as the Simple backend's
    /// borrowed one-shot match window. Only valid on the Simple (Fast)
    /// backend; the one-shot frame path gates on that before calling.
    ///
    /// # Safety
    /// Same contract as [`FastKernelMatcher::set_borrowed_window`]: the
    /// buffer must stay live and unmodified until the window is cleared,
    /// and must be cleared before the buffer is dropped or the matcher is
    /// reused for another frame.
    pub(crate) unsafe fn set_borrowed_window(&mut self, buffer: &[u8]) {
        // SAFETY: forwarded contract — caller upholds liveness/clear.
        match self.active_backend() {
            super::strategy::BackendTag::Simple => unsafe {
                self.simple_mut().set_borrowed_window(buffer)
            },
            super::strategy::BackendTag::Dfast => unsafe {
                self.dfast_matcher_mut().set_borrowed_window(buffer)
            },
            other => unreachable!("borrowed window only for Simple/Dfast backends, got {other:?}"),
        }
    }

    /// Clear the borrowed one-shot window, returning the active backend
    /// to the owned `history` path.
    pub(crate) fn clear_borrowed_window(&mut self) {
        match self.active_backend() {
            super::strategy::BackendTag::Simple => self.simple_mut().clear_borrowed_window(),
            super::strategy::BackendTag::Dfast => self.dfast_matcher_mut().clear_borrowed_window(),
            _ => {}
        }
        self.borrowed_pending = None;
    }

    /// Stage the borrowed block range `[block_start, block_end)` for the
    /// NEXT `start_matching` / `skip_matching_with_hint`, which the
    /// borrowed Fast frame path uses in place of `commit_space`. While
    /// staged, those trait calls route to the Simple backend's borrowed
    /// scan/skip (consuming the stage) instead of the owned committed
    /// block. See [`Matcher::start_matching`] /
    /// [`Matcher::skip_matching_with_hint`] on this type.
    pub(crate) fn set_borrowed_block(&mut self, block_start: usize, block_end: usize) {
        assert!(
            matches!(
                self.active_backend(),
                super::strategy::BackendTag::Simple | super::strategy::BackendTag::Dfast
            ),
            "borrowed block staging is only valid for the Simple (Fast) / Dfast backends",
        );
        assert!(
            block_start <= block_end,
            "borrowed block range must satisfy start <= end (start={block_start} end={block_end})",
        );
        self.borrowed_pending = Some((block_start, block_end));
        // Make the range visible to `get_last_space()` immediately: the
        // emit pipeline reads `get_last_space().len()` in
        // `collect_block_parts` BEFORE `start_matching` consumes the
        // stage, so the staged block (not the whole borrowed window) must
        // be reported now to keep the literal-buffer reservation right.
        match self.active_backend() {
            super::strategy::BackendTag::Simple => self
                .simple_mut()
                .stage_borrowed_block(block_start, block_end),
            super::strategy::BackendTag::Dfast => self
                .dfast_matcher_mut()
                .stage_borrowed_block(block_start, block_end),
            _ => unreachable!(),
        }
    }

    #[cfg(test)]
    fn dfast_matcher(&self) -> &DfastMatchGenerator {
        match &self.storage {
            MatcherStorage::Dfast(m) => m,
            _ => panic!("dfast backend must be initialized by reset() before use"),
        }
    }

    fn dfast_matcher_mut(&mut self) -> &mut DfastMatchGenerator {
        match &mut self.storage {
            MatcherStorage::Dfast(m) => m,
            _ => panic!("dfast backend must be initialized by reset() before use"),
        }
    }

    #[cfg(test)]
    fn row_matcher(&self) -> &RowMatchGenerator {
        match &self.storage {
            MatcherStorage::Row(m) => m,
            _ => panic!("row backend must be initialized by reset() before use"),
        }
    }

    fn row_matcher_mut(&mut self) -> &mut RowMatchGenerator {
        match &mut self.storage {
            MatcherStorage::Row(m) => m,
            _ => panic!("row backend must be initialized by reset() before use"),
        }
    }

    #[cfg(test)]
    fn hc_matcher(&self) -> &HcMatchGenerator {
        match &self.storage {
            MatcherStorage::HashChain(m) => m,
            _ => panic!("hash chain backend must be initialized by reset() before use"),
        }
    }

    fn hc_matcher_mut(&mut self) -> &mut HcMatchGenerator {
        match &mut self.storage {
            MatcherStorage::HashChain(m) => m,
            _ => panic!("hash chain backend must be initialized by reset() before use"),
        }
    }

    /// Shrink the active backend's `max_window_size` by the bytes
    /// reclaimed from the dictionary-retention budget. Returns `true`
    /// iff any reclamation happened — the caller uses that as the
    /// gate for [`Self::trim_after_budget_retire`] (which is a no-op
    /// otherwise: with `max_window_size` unchanged the backend's
    /// `trim_to_window` cannot find anything to evict, so calling it
    /// just runs an extra `match` ladder + a single early-out check
    /// per slice commit).
    #[must_use]
    fn retire_dictionary_budget(&mut self, evicted_bytes: usize) -> bool {
        let reclaimed = evicted_bytes.min(self.dictionary_retained_budget);
        if reclaimed == 0 {
            return false;
        }
        self.dictionary_retained_budget -= reclaimed;
        match self.active_backend() {
            super::strategy::BackendTag::Simple => {
                let matcher = self.simple_mut();
                // `reclaimed` can exceed the CURRENT `max_window_size`: the
                // retained dict budget is tracked independently and the
                // window may already have been shrunk by a prior eviction,
                // so the floor at 0 is the correct clamp, not a masked bug.
                matcher.max_window_size = matcher.max_window_size.saturating_sub(reclaimed);
            }
            super::strategy::BackendTag::Dfast => {
                let matcher = self.dfast_matcher_mut();
                // `reclaimed` can exceed the CURRENT `max_window_size`: the
                // retained dict budget is tracked independently and the
                // window may already have been shrunk by a prior eviction,
                // so the floor at 0 is the correct clamp, not a masked bug.
                matcher.max_window_size = matcher.max_window_size.saturating_sub(reclaimed);
            }
            super::strategy::BackendTag::Row => {
                let matcher = self.row_matcher_mut();
                // `reclaimed` can exceed the CURRENT `max_window_size`: the
                // retained dict budget is tracked independently and the
                // window may already have been shrunk by a prior eviction,
                // so the floor at 0 is the correct clamp, not a masked bug.
                matcher.max_window_size = matcher.max_window_size.saturating_sub(reclaimed);
            }
            super::strategy::BackendTag::HashChain => {
                let matcher = self.hc_matcher_mut();
                // See the Simple arm: `reclaimed` may exceed the current
                // window, so saturating to 0 is the correct clamp.
                matcher.table.max_window_size =
                    matcher.table.max_window_size.saturating_sub(reclaimed);
            }
        }
        true
    }

    fn trim_after_budget_retire(&mut self) {
        loop {
            let mut evicted_bytes = 0usize;
            match self.active_backend() {
                super::strategy::BackendTag::Simple => {
                    // FastKernelMatcher owns its history as a single
                    // flat `Vec<u8>` (donor's flat-buffer layout)
                    // rather than the legacy per-block `WindowEntry`
                    // stack. There are no per-block Vec allocations
                    // to recycle into `vec_pool` — `trim_to_window`
                    // drains the oldest bytes in-place and returns
                    // the count for the dictionary-budget loop's
                    // termination check.
                    let MatcherStorage::Simple(m) = &mut self.storage else {
                        unreachable!("active_backend() == Simple proven above");
                    };
                    evicted_bytes += m.trim_to_window();
                }
                super::strategy::BackendTag::Dfast => {
                    // Dfast doesn't retain input Vecs — `history` is the
                    // only byte store, so there is no per-block buffer
                    // to push back through a callback. Eviction byte
                    // count is derived from the `window_size` delta
                    // before/after; the Dfast variant of
                    // `trim_to_window` takes no closure, sidestepping
                    // an unused-`impl FnMut` monomorphization that
                    // would otherwise contractually never fire.
                    let dfast = self.dfast_matcher_mut();
                    let pre = dfast.window_size;
                    dfast.trim_to_window();
                    evicted_bytes += pre - dfast.window_size;
                }
                super::strategy::BackendTag::Row => {
                    // Row keeps bytes only in the contiguous `history` mirror
                    // (block buffers are returned to the pool per block in
                    // `add_data`), so derive the eviction count from the
                    // `window_size` delta, mirroring the Dfast / HashChain arms.
                    let row = self.row_matcher_mut();
                    let pre = row.window_size;
                    row.trim_to_window();
                    evicted_bytes += pre - row.window_size;
                }
                super::strategy::BackendTag::HashChain => {
                    // HC keeps bytes only in the contiguous `history` mirror
                    // (no per-block Vecs to recycle since the window<->history
                    // dedup), so derive the eviction count from the
                    // `window_size` delta, mirroring the Dfast arm above.
                    let table = &mut self.hc_matcher_mut().table;
                    let pre = table.window_size;
                    table.trim_to_window();
                    evicted_bytes += pre - table.window_size;
                }
            }
            if evicted_bytes == 0 {
                break;
            }
            // The loop's invariant is "the backend's previous
            // `max_window_size` shrink had downstream bytes left to
            // evict" — that's what `evicted_bytes != 0` proves at
            // this point. `dictionary_retained_budget` is NOT
            // guaranteed to be positive here: the outer
            // `retire_dictionary_budget` call may have already
            // drained it to zero by reclaiming the last retained
            // bytes, while the backend still has bytes above the
            // freshly-shrunk window cap waiting for this loop to
            // evict. The return value of the retire call below is
            // therefore intentionally discarded — the loop's
            // termination is driven by `evicted_bytes == 0`, not by
            // whether the budget has more bytes left to reclaim.
            let _ = self.retire_dictionary_budget(evicted_bytes);
        }
    }

    fn skip_matching_for_dictionary_priming(&mut self) {
        match self.active_backend() {
            super::strategy::BackendTag::Simple => {
                // Donor `ZSTD_shouldAttachDict` mode selection for the Fast
                // strategy (cutoff 8 KB): small / unknown-size inputs ATTACH
                // (index dict positions into a SEPARATE immutable table; the
                // dual-probe 2-cursor `compress_block_fast_dict` then prefers
                // recent-input matches and falls back to the dict — the path
                // that wins small/unknown). Large known-size inputs COPY (prime
                // dict into the live table; the 4-cursor `compress_block_fast`
                // matches against it as window history — the path that already
                // matches/beats the donor on large corpora). The dispatch in
                // `start_matching` keys off `dict_table.is_some()`, which only
                // the attach path populates. See [`FAST_ATTACH_DICT_CUTOFF_LOG`].
                let attach = self
                    .reset_size_log
                    .is_none_or(|log| log <= FAST_ATTACH_DICT_CUTOFF_LOG);
                if attach {
                    self.simple_mut().skip_matching_for_dict_prime();
                } else {
                    self.simple_mut().skip_matching_with_hint(Some(false));
                }
                self.recycle_simple_space();
            }
            super::strategy::BackendTag::Dfast => {
                // Donor `ZSTD_dictMatchState` mode selection for dfast (cutoff
                // 16 KiB): small / unknown-size inputs ATTACH (build the
                // separate immutable dict long+short tables; the dual-probe
                // `start_matching_fast_loop` searches live + dict, the path that
                // avoids the per-frame dict re-prime that dominates small
                // `compress-dict`). Larger known-size inputs COPY (re-prime the
                // dict into the live tables via `skip_matching_dense`, where the
                // dense scan matches it as window history). `skip_matching_for_dict_attach`
                // self-gates on `use_fast_loop` (only fast-loop levels carry the
                // dual-probe; general-path levels fall back to the dense copy).
                let attach = self
                    .reset_size_log
                    .is_none_or(|log| log <= DFAST_ATTACH_DICT_CUTOFF_LOG);
                if attach {
                    self.dfast_matcher_mut().skip_matching_for_dict_attach();
                } else {
                    self.dfast_matcher_mut().invalidate_dict_cache();
                    self.dfast_matcher_mut().skip_matching_dense();
                }
            }
            super::strategy::BackendTag::Row => {
                // Donor `ZSTD_RowFindBestMatch` `dictMatchState`: small /
                // unknown-size inputs ATTACH (build the separate immutable dict
                // row index; the bounded dual-probe in `row_candidate_rl`
                // searches live + dict, avoiding the per-frame dict re-index),
                // larger known-size inputs COPY (dense re-prime into the live
                // rows).
                let attach = self
                    .reset_size_log
                    .is_none_or(|log| log <= ROW_ATTACH_DICT_CUTOFF_LOG);
                if attach {
                    self.row_matcher_mut().prime_dict_attach_current_block();
                } else {
                    self.row_matcher_mut().invalidate_dict_cache();
                    self.row_matcher_mut().skip_matching_with_hint(Some(false));
                }
            }
            super::strategy::BackendTag::HashChain => {
                let table = &mut self.hc_matcher_mut().table;
                if table.uses_bt {
                    // BT / optimal levels: keep the dict in history for the dms
                    // but do NOT insert it into the live tree (donor separate
                    // dictMatchState). Lazy-HC levels still index the dict into
                    // the live chain (they have no dms).
                    table.skip_matching_dict_bt();
                } else {
                    self.hc_matcher_mut().skip_matching(Some(false));
                }
            }
        }
    }
}

impl Matcher for MatchGeneratorDriver {
    fn supports_dictionary_priming(&self) -> bool {
        true
    }

    fn set_source_size_hint(&mut self, size: u64) {
        self.source_size_hint = Some(size);
    }

    fn set_dictionary_size_hint(&mut self, size: usize) {
        self.dictionary_size_hint = Some(size);
    }

    /// Heap bytes this driver owns: the active backend's tables/history, the
    /// recycled input-buffer pool, and the primed-dictionary snapshot (a cloned
    /// backend kept for CDict-equivalent reuse). The inline struct itself is
    /// accounted by the owner's `size_of`.
    fn heap_size(&self) -> usize {
        let pool: usize = self.vec_pool.capacity() * core::mem::size_of::<Vec<u8>>()
            + self.vec_pool.iter().map(Vec::capacity).sum::<usize>();
        let snapshot = self
            .primed
            .as_ref()
            .map_or(0, |(storage, _, _)| storage.heap_size());
        pool + self.storage.heap_size() + snapshot
    }

    fn clear_param_overrides(&mut self) {
        self.param_overrides = None;
    }

    fn reset(&mut self, level: CompressionLevel) {
        let hint = self.source_size_hint.take();
        let dict_hint = self.dictionary_size_hint.take();
        // Snapshot the hint's normalized ceil-log bucket for the primed-snapshot
        // key and prime_with_dictionary's attach/copy mode decision (the hint is
        // consumed here, but priming happens just after reset). Storing the
        // bucket rather than the raw bytes means two hints that resolve to the
        // same matcher shape share one snapshot instead of each re-priming.
        self.reset_size_log = hint.map(source_size_ceil_log);
        let hinted = hint.is_some();
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut params = Self::level_params(level, hint);
        // Test-only: apply a parse×search override so the matrix can be
        // exercised without editing `LEVEL_TABLE`. Mutating `params` here
        // (before `next_backend`) flows the override through storage
        // selection, `configure`, and the `self.search`/`self.parse`
        // writes uniformly. Consumed with `take()` so it is one-shot: the
        // synthetic pairing applies to exactly this `reset()`, and a later
        // reset on the same driver falls back to the level's real config.
        #[cfg(test)]
        if let Some((search, parse)) = self.config_override.take() {
            params.search = search;
            params.lazy_depth = parse.lazy_depth();
            // The matrix sweep can pair a level with a backend its native
            // row doesn't populate (e.g. greedy L5, which carries only `row`,
            // run on HashChain). Synthesize a default config for the
            // overridden backend so its `configure` arm has something to read.
            use super::strategy::SearchMethod;
            match search {
                SearchMethod::Fast => {
                    params.fast.get_or_insert(FAST_L1);
                }
                SearchMethod::DoubleFast => {
                    params.dfast.get_or_insert(DFAST_L3);
                }
                SearchMethod::RowHash => {
                    params.row.get_or_insert(ROW_CONFIG);
                }
                SearchMethod::HashChain | SearchMethod::BinaryTree => {
                    params.hc.get_or_insert(HC_CONFIG);
                }
            }
        }
        // Public-parameter overrides (#27): apply the per-knob set on top
        // of the level-resolved params. A strategy override re-routes the
        // backend, so this must precede `next_backend` selection. The
        // all-`None` case is skipped so default level geometry stays
        // byte-identical to plain level-based compression.
        if let Some(ov) = self.param_overrides
            && !ov.is_empty()
        {
            apply_param_overrides(&mut params, &ov);
            // `Self::level_params(level, hint)` applied the source-size cap
            // for the LEVEL's native backend. If a strategy override moved
            // the frame onto a different backend, `apply_param_overrides`
            // synthesized that backend's DEFAULT config (FAST_L1 /
            // HC_OVERRIDE_DEFAULT) with full-size table logs AFTER that cap
            // ran. Re-apply the hint cap so a tiny hinted frame doesn't
            // allocate the new backend's full-size tables. An explicit
            // `window_log` override is the user's hard request and must
            // survive the re-cap, so restore it afterwards.
            if let Some(hint_size) = hint {
                params = adjust_params_for_source_size(params, hint_size);
                if let Some(window_log) = ov.window_log {
                    params.window_log = window_log;
                }
            }
        }
        // Dictionary-driven table sizing — parity with donor `ZSTD_createCDict`
        // (`ZSTD_getCParams_internal(level, UNKNOWN, dictSize, ZSTD_cpm_createCDict)`
        // → `ZSTD_adjustCParams_internal`). A loaded dictionary supplies the
        // long-distance matches, so donor sizes the prepared match-finder tables
        // to the DICTIONARY (assuming a `minSrcSize` source), not the live
        // window: it downsizes `hashLog`/`chainLog` toward the dict-and-window
        // log while leaving the frame's eviction `window_log` source-derived so
        // the dictionary bytes stay referenceable (`ZSTD_resetCCtx_byCopyingCDict`
        // copies the small CDict tables but keeps the source window). We apply
        // the same downsizing to the level's own hc geometry and cap (min) so a
        // dict never inflates the level tables. Only the binary-tree / hash-chain
        // backend reads `hc.{hash,chain}_log`; Simple/Dfast/Row derive their
        // widths from the source window in their `reset` arms.
        // A zero-length dictionary is "no dictionary": running the CDict sizing
        // path for `Some(0)` is not a no-op — `cdict_table_logs(.., 0)` still
        // collapses the HC/BT tables toward the 513-byte donor tier via
        // `DICT_MIN_SRC_SIZE`, tanking ratio/perf on the next frame. Priming
        // already treats empty content as empty, so skip the downsizing here too.
        if let Some(dict_size) = dict_hint.filter(|&size| size > 0) {
            // Derive the dict-tier geometry from the level's FULL (un-source-capped)
            // hc widths. `Self::level_params(level, hint)` already source-capped
            // `params.hc`; feeding those capped widths into `cdict_table_logs` and
            // then `.min()`-ing would double-cap, so on a small hinted source with a
            // large dictionary the prepared tables collapse below what the dict needs
            // — defeating the `ZSTD_createCDict` geometry this mirrors. Take the
            // un-hinted base widths instead and assign the result directly:
            // `cdict_table_logs` only ever downsizes, so it never exceeds the base
            // level geometry, while the eviction `window_log` stays source-derived so
            // the dictionary bytes remain referenceable. Active public-parameter
            // overrides (#27) are applied to the base too, so a strategy override
            // that routes onto HashChain/BinaryTree still gets dict-tier sizing and
            // explicit hash/chain overrides feed through as the geometry ceiling.
            let mut base_params = Self::level_params(level, None);
            if let Some(ov) = self.param_overrides
                && !ov.is_empty()
            {
                apply_param_overrides(&mut base_params, &ov);
            }
            if let (Some(hc), Some(base_hc)) = (params.hc.as_mut(), base_params.hc) {
                let uses_bt = matches!(
                    params.strategy_tag,
                    super::strategy::StrategyTag::Btlazy2
                        | super::strategy::StrategyTag::BtOpt
                        | super::strategy::StrategyTag::BtUltra
                        | super::strategy::StrategyTag::BtUltra2
                );
                let (dict_hash_log, dict_chain_log) = cdict_table_logs(
                    params.window_log,
                    base_hc.hash_log,
                    base_hc.chain_log,
                    uses_bt,
                    dict_size,
                );
                hc.hash_log = dict_hash_log;
                hc.chain_log = dict_chain_log;
            }
        }
        let next_backend = params.backend();
        let max_window_size = 1usize << params.window_log;
        self.dictionary_retained_budget = 0;
        // Drop any frame-local borrowed staging so it can't leak across a
        // reset and misroute the next start/skip into borrowed dispatch.
        self.borrowed_pending = None;
        if self.active_backend() != next_backend {
            // Drain the outgoing backend's allocations into the shared
            // pool. The `match &mut self.storage { ... }` block runs to
            // completion before the assignment below replaces the
            // variant, so the inner state we just drained is dropped
            // with the old variant.
            match &mut self.storage {
                MatcherStorage::Simple(_m) => {
                    // FastKernelMatcher owns a flat Vec<u8> history
                    // and a Vec<u32> hash table — both drop with the
                    // variant assignment below, no per-block buffers
                    // to recycle into the driver pools. The
                    // assignment-replace path collapses to a noop
                    // pre-pass for this backend.
                }
                MatcherStorage::Dfast(m) => {
                    // Drop the long / short hash table allocations
                    // before calling `m.reset`. Without this prepass,
                    // `DfastMatchGenerator::reset` would `fill` both
                    // tables with `DFAST_EMPTY_SLOT` sentinels — wasted
                    // work given the next assignment to `self.storage`
                    // is about to drop `m` entirely. `reset` itself
                    // short-circuits on `if !self.short_hash.is_empty()`,
                    // so handing it an empty `Vec` skips the fill loop.
                    // Mirrors the pre-drain pattern in the HashChain
                    // arm below (and serves the same peak-memory
                    // purpose: release the table-allocation footprint
                    // before constructing the replacement variant).
                    m.short_hash = Vec::new();
                    m.long_hash = Vec::new();
                    m.reset();
                }
                MatcherStorage::Row(m) => {
                    m.row_heads = Vec::new();
                    m.row_positions = Vec::new();
                    m.row_tags = Vec::new();
                    m.reset();
                }
                MatcherStorage::HashChain(m) => {
                    // Release oversized tables when switching away from
                    // HashChain so Best's larger allocations don't persist.
                    // hash3_table must be released alongside the other
                    // two: BtUltra2's `1 << HC3_HASH_LOG` entries would
                    // otherwise stay pinned across the backend switch,
                    // even though no future caller of this backend will
                    // touch them.
                    m.table.hash_table = Vec::new();
                    m.table.chain_table = Vec::new();
                    m.table.hash3_table = Vec::new();
                    let vec_pool = &mut self.vec_pool;
                    m.reset(|mut data| {
                        data.resize(data.capacity(), 0);
                        vec_pool.push(data);
                    });
                }
            }
            // Swap in a fresh variant for the new backend. The previous
            // `storage` is dropped here.
            self.storage = match next_backend {
                super::strategy::BackendTag::Simple => {
                    // Per-level Fast cParams from resolve_level_params:
                    // Level(1) gets (hash_log=14, mls=7); Level(-7..=-1)
                    // get donor row-0 (hash_log=13, mls=7); Fastest /
                    // Uncompressed keep (hash_log=14, mls=6). See
                    // resolve_level_params for rationale.
                    let fast = params.fast.expect("Fast level row carries a FastConfig");
                    MatcherStorage::Simple(FastKernelMatcher::with_params(
                        params.window_log,
                        fast.hash_log,
                        fast.mls,
                        fast.step_size,
                    ))
                }
                super::strategy::BackendTag::Dfast => {
                    MatcherStorage::Dfast(DfastMatchGenerator::new(max_window_size))
                }
                super::strategy::BackendTag::Row => {
                    MatcherStorage::Row(RowMatchGenerator::new(max_window_size))
                }
                super::strategy::BackendTag::HashChain => {
                    MatcherStorage::HashChain(HcMatchGenerator::new(max_window_size))
                }
            };
        }

        // Single source of truth: `LevelParams::strategy_tag` is the
        // authoritative mapping from `CompressionLevel` to strategy.
        // `storage.backend()` derives the parse family from the variant,
        // so there is no separate runtime tag that could drift against
        // `LEVEL_TABLE`.
        self.strategy_tag = params.strategy_tag;
        self.search = params.search;
        self.parse = params.parse();
        self.slice_size = self.base_slice_size.min(max_window_size);
        self.reported_window_size = max_window_size;
        let strategy_tag = self.strategy_tag;
        // Source-proportional table window for the backends whose hash-table
        // widths are recomputed here (Dfast / Row). Like the HC / Fast caps
        // in `adjust_params_for_source_size`, this sizes the internal tables
        // from the RAW source log (not the wire `window_log` floor) so a
        // small frame zeroes a small table; it never exceeds the real window.
        let table_window_size = match hint {
            Some(h) => {
                let raw_log = source_size_ceil_log(h);
                // Clamp the shift below the pointer width before `1usize <<`:
                // an oversized hint (>= 2^63 + 1, and on 32-bit usize any hint
                // >= 2^32) drives `raw_log` to 64 / >= 32, and the shift would
                // overflow (panic in debug, wrap to 0 in release) before the
                // `.min(max_window_size)` cap below could bound it. The min cap
                // still provides the real semantic window bound.
                let shift = raw_log.max(MIN_WINDOW_LOG).min(usize::BITS as u8 - 1);
                (1usize << shift).min(max_window_size)
            }
            None => max_window_size,
        };
        // The hint-dependent hash-table width the active backend applies, for
        // the primed-snapshot key. Dfast/Row compute it from `table_window_size`
        // below; HC/Fast leave it `0` because their widths live in `params`
        // (`hc.{hash,chain}_log` / `fast_hash_log`) — already part of the key.
        let mut resolved_table_bits: usize = 0;
        match &mut self.storage {
            MatcherStorage::Simple(m) => {
                // Per-level Fast cParams threaded from
                // resolve_level_params (see Simple-backend swap
                // arm above for the (level → params) mapping).
                let fast = params.fast.expect("Fast level row carries a FastConfig");
                // Same attach/copy split the dict-prime dispatch applies
                // below (`prime_with_dictionary`): only attach-mode dict
                // frames may keep the main table across the reset via an
                // epoch advance — copy-mode and no-dict frames must memset
                // it back to bias 0 for the raw-slice kernels.
                // `Some(0)` is "no dictionary" (the dict-sizing path above
                // filters it the same way): an empty dict primes nothing, so
                // an epoch-advance reset would preserve stale attach state
                // instead of clearing it.
                let dict_attach_epoch = matches!(dict_hint, Some(size) if size > 0)
                    && self
                        .reset_size_log
                        .is_none_or(|log| log <= FAST_ATTACH_DICT_CUTOFF_LOG);
                // Copy-mode dictionary frame whose primed snapshot matches
                // this exact resolved shape: `restore_primed_dictionary`
                // (called right after this reset; the caller gates the
                // restore on the same size bucket and the restore re-checks
                // the same key) will `clone_from` the snapshot over this
                // matcher, replacing the table contents and bias wholesale —
                // the reset's full-table memset would be thrown away. The
                // key components mirror `reset_shape` below: Simple leaves
                // `resolved_table_bits` 0, never carries an LDM override,
                // and `fast_attach` is false in copy mode by construction.
                let table_overwritten_by_restore = matches!(dict_hint, Some(size) if size > 0)
                    && !dict_attach_epoch
                    && self.primed.as_ref().is_some_and(|(_, _, captured)| {
                        *captured
                            == PrimedKey {
                                level,
                                params,
                                table_bits: 0,
                                fast_attach: false,
                                ldm: None,
                            }
                    });
                m.reset(
                    params.window_log,
                    fast.hash_log,
                    fast.mls,
                    fast.step_size,
                    dict_attach_epoch,
                    table_overwritten_by_restore,
                );
            }
            MatcherStorage::Dfast(dfast) => {
                dfast.max_window_size = max_window_size;
                let dcfg = params
                    .dfast
                    .expect("Dfast level row must carry a DfastConfig");
                // Donor `cParams.hashLog`/`chainLog`, capped by the
                // source-size window when hinted so tiny inputs don't
                // over-allocate.
                let long_bits = if hinted {
                    dfast_hash_bits_for_window(table_window_size).min(dcfg.long_hash_log as usize)
                } else {
                    dcfg.long_hash_log as usize
                };
                let short_bits = if hinted {
                    dfast_hash_bits_for_window(table_window_size).min(dcfg.short_hash_log as usize)
                } else {
                    dcfg.short_hash_log as usize
                };
                resolved_table_bits = long_bits;
                dfast.set_hash_bits(long_bits, short_bits);
                // Dfast holds no per-block input Vecs (history owns the
                // bytes and `add_data` returns each Vec eagerly), so
                // `reset` takes no `reuse_space` callback.
                dfast.reset();
            }
            MatcherStorage::Row(row) => {
                row.max_window_size = max_window_size;
                row.lazy_depth = params.lazy_depth;
                let mut row_cfg = params.row.expect("Row level row carries a RowConfig");
                if hinted {
                    // Clamp the configured hash width by the hinted window
                    // (donor `ZSTD_adjustCParams` caps hashLog by windowLog) —
                    // `min`, not replace, so an explicit `hash_log` param
                    // override (`row_cfg.hash_bits`) survives the hinted path
                    // instead of being overwritten by the window value.
                    //
                    // Clamp BEFORE `configure` so the backend sees ONE width
                    // per frame. Configuring with the unclamped level width
                    // and then re-clamping made `row_hash_log` oscillate on
                    // every hinted frame, and each width change clears the
                    // row tables — `ensure_tables` then re-filled all three
                    // every frame in a reused compressor.
                    row_cfg.hash_bits = row_cfg
                        .hash_bits
                        .min(row_hash_bits_for_window(table_window_size));
                }
                row.configure(row_cfg);
                // Key the primed snapshot on the width the backend ACTUALLY
                // applied (`set_hash_bits` clamps the request): recording the
                // request — or the 0 default on the unhinted path — keys
                // identical table geometries apart and forces needless
                // dictionary re-primes.
                resolved_table_bits = row.hash_bits();
                row.reset();
            }
            MatcherStorage::HashChain(hc) => {
                hc.table.max_window_size = max_window_size;
                hc.hc.lazy_depth = params.lazy_depth;
                let mut hc_cfg = params.hc.expect("HashChain level row carries an HcConfig");
                // Cap the hash / chain table logs by the hinted window so a small
                // input doesn't allocate the full level's tables (the donor
                // `ZSTD_adjustCParams_internal` clamp: `hashLog <= windowLog + 1`,
                // and `cycleLog <= windowLog` — `cycleLog == chainLog` for the HC
                // finder, `chainLog - 1` for the BT pair table, so `chainLog <=
                // windowLog` (+1 for BT)). Ratio-neutral: a hinted window of
                // `2^wlog` bytes holds at most `2^wlog` positions, so the slots
                // beyond that are never populated — capping only sheds unused
                // allocation. Was the source of L10-lazy peak-alloc ~2.15x the
                // donor on a 1 MiB input. Only applied when hinted; an
                // unknown-size stream keeps the full level tables.
                if hinted {
                    let wlog = hc_hash_bits_for_window(table_window_size);
                    let uses_bt = matches!(
                        strategy_tag,
                        super::strategy::StrategyTag::Btlazy2
                            | super::strategy::StrategyTag::BtOpt
                            | super::strategy::StrategyTag::BtUltra
                            | super::strategy::StrategyTag::BtUltra2
                    );
                    hc_cfg.hash_log = hc_cfg.hash_log.min(wlog + 1);
                    hc_cfg.chain_log = hc_cfg.chain_log.min(if uses_bt { wlog + 1 } else { wlog });
                }
                hc.configure(hc_cfg, strategy_tag, params.window_log);
                let vec_pool = &mut self.vec_pool;
                hc.reset(|mut data| {
                    data.resize(data.capacity(), 0);
                    vec_pool.push(data);
                });
                // When the source size is known, pre-size the history mirror to
                // the expected total (dictionary + payload) so per-block growth
                // does not overshoot via Vec capacity doubling (donor sizes its
                // window buffer exactly). Dominates peak once the match-finder
                // tables are dictionary-tier-small. Unhinted streams skip this
                // and keep doubling growth.
                if let Some(src) = hint {
                    let expected = (src as usize).saturating_add(dict_hint.unwrap_or(0));
                    hc.table.reserve_history(expected);
                }
            }
        }
        // LDM wiring (#27): attach (or clear) the long-distance-match
        // producer on the optimal (BT) backend. LDM is the only
        // back-reference path that crosses the regular window, so it
        // only has a home on the `BtMatcher`; non-BT strategies drop the
        // producer. Built AFTER `hc.reset()` because `BtMatcher::reset`
        // clears an existing producer's table but does not null the
        // slot — installing here gives the new frame a fresh producer.
        #[cfg(feature = "hash")]
        if let MatcherStorage::HashChain(hc) = &mut self.storage {
            let producer = self
                .param_overrides
                .as_ref()
                .and_then(|ov| ov.ldm)
                .map(|ldm_ov| {
                    let strategy_ord = ldm_strategy_ordinal(params.strategy_tag, params.lazy_depth);
                    // Seed the caller-pinned knobs, then run the donor
                    // derivation over the seed so the remaining (zero)
                    // fields are filled with cross-field consistency
                    // (e.g. `hash_rate_log = window_log - hash_log`).
                    // Clobbering after `adjust_for` would break that and
                    // hand the producer an inconsistent set.
                    let seed = super::ldm::params::LdmParams {
                        window_log: params.window_log as u32,
                        hash_log: ldm_ov.hash_log.unwrap_or(0),
                        hash_rate_log: ldm_ov.hash_rate_log.unwrap_or(0),
                        min_match_length: ldm_ov.min_match.unwrap_or(0),
                        bucket_size_log: ldm_ov.bucket_size_log.unwrap_or(0),
                    };
                    super::ldm::LdmProducer::new(seed.derive(strategy_ord))
                });
            hc.set_ldm_producer(producer);
        }
        // Record the resolved matcher shape for the primed-snapshot key. Captured
        // here (post-resolution, after the test-only param override) so the key
        // reflects exactly the geometry the restored `storage` must match. The
        // Fast attach-vs-copy mode is part of the shape ONLY for the Simple
        // backend (it decides whether a separate dict table is built); the other
        // backends prime the dictionary the same way regardless, so including
        // the bit there would over-key identical resolved shapes. When it
        // applies it matches the decision `prime_with_dictionary` makes from the
        // same `reset_size_log`.
        let fast_attach = matches!(next_backend, super::strategy::BackendTag::Simple)
            && self
                .reset_size_log
                .is_none_or(|log| log <= FAST_ATTACH_DICT_CUTOFF_LOG);
        // The LDM override is part of the snapshot identity ONLY on the
        // optimal (BinaryTree) path: that is the only backend whose cloned
        // `storage` carries a `BtMatcher::ldm_producer`. On Fast / Dfast /
        // Row and lazy-HashChain resets the producer slot does not exist,
        // so folding the override there would over-key the snapshot and
        // force needless re-primes when LDM is toggled. Gated like
        // `fast_attach` (a key bit only participates where it changes the
        // cloned matcher shape).
        let active_ldm = if matches!(params.search, super::strategy::SearchMethod::BinaryTree) {
            self.param_overrides.and_then(|ov| ov.ldm)
        } else {
            None
        };
        self.reset_shape = Some((params, resolved_table_bits, fast_attach, active_ldm));
    }

    fn prime_with_dictionary(&mut self, dict_content: &[u8], offset_hist: [u32; 3]) {
        match self.active_backend() {
            super::strategy::BackendTag::Simple => {
                // Routes through prime_offset_history so BOTH
                // offset_hist (wire encoder) and rep[0..2] (kernel)
                // are updated atomically. Without this, the two
                // tracks drift after dict priming — kernel emits
                // repcode matches against stale FAST_INITIAL_REP
                // while the wire encoder uses the primed history,
                // producing divergent wire encoding (Copilot review
                // #15 on #216).
                self.simple_mut().prime_offset_history(offset_hist);
            }
            super::strategy::BackendTag::Dfast => {
                self.dfast_matcher_mut().offset_hist = offset_hist
            }
            super::strategy::BackendTag::Row => self.row_matcher_mut().offset_hist = offset_hist,
            super::strategy::BackendTag::HashChain => {
                let matcher = self.hc_matcher_mut();
                matcher.table.offset_hist = offset_hist;
                matcher.table.mark_dictionary_primed();
            }
        }

        if dict_content.is_empty() {
            return;
        }

        // Dictionary bytes should stay addressable until produced frame output
        // itself exceeds the live window size. We bump `max_window_size`
        // by the dictionary length so the eviction band keeps the
        // primed bytes in `history`.
        //
        // Cap: `with_params`/`reset` enforce `window_log <= 30` so the
        // eviction band `2 * max_window_size` stays below `u32::MAX`
        // with headroom for one MAX_BLOCK_SIZE pending block — the
        // kernel asserts `data.len() <= u32::MAX`. A large enough
        // dictionary could otherwise push `max_window_size` past
        // that ceiling via the `saturating_add` below and silently
        // re-introduce the same overflow the `window_log` cap was
        // designed to prevent. Clamp the post-priming size so the
        // doubled-band-plus-block invariant survives.
        const MAX_PRIMED_WINDOW_SIZE: usize =
            (u32::MAX as usize - crate::common::MAX_BLOCK_SIZE as usize) / 2;

        // `requested_dict_budget` is what the caller asked for;
        // `base_max_window_size` snapshots the pre-priming cap so we
        // can compute how much window the cap actually GRANTED below.
        // The cap may clip the requested growth, in which case the
        // bookkeeping (`dictionary_retained_budget` retire path) must
        // track only the granted portion — otherwise
        // `retire_dictionary_budget()` would later reclaim more than
        // was actually added and shrink the matcher below its real
        // base window (and `cap = 2 * max_window_size` would shrink
        // with it, risking under-allocation on subsequent commits).
        // The `granted_retained_budget` calculation further below is
        // the load-bearing piece — see its block-level comment for
        // the post-clip / post-uncommitted-tail math.
        let requested_dict_budget = dict_content.len();
        let base_max_window_size = match self.active_backend() {
            super::strategy::BackendTag::Simple => self.simple_mut().max_window_size,
            super::strategy::BackendTag::Dfast => self.dfast_matcher_mut().max_window_size,
            super::strategy::BackendTag::Row => self.row_matcher_mut().max_window_size,
            super::strategy::BackendTag::HashChain => self.hc_matcher_mut().table.max_window_size,
        };
        match self.active_backend() {
            super::strategy::BackendTag::Simple => {
                let matcher = self.simple_mut();
                matcher.max_window_size = matcher
                    .max_window_size
                    .saturating_add(requested_dict_budget)
                    .min(MAX_PRIMED_WINDOW_SIZE);
            }
            super::strategy::BackendTag::Dfast => {
                let matcher = self.dfast_matcher_mut();
                matcher.max_window_size = matcher
                    .max_window_size
                    .saturating_add(requested_dict_budget)
                    .min(MAX_PRIMED_WINDOW_SIZE);
            }
            super::strategy::BackendTag::Row => {
                let matcher = self.row_matcher_mut();
                matcher.max_window_size = matcher
                    .max_window_size
                    .saturating_add(requested_dict_budget)
                    .min(MAX_PRIMED_WINDOW_SIZE);
            }
            super::strategy::BackendTag::HashChain => {
                let matcher = self.hc_matcher_mut();
                matcher.table.max_window_size = matcher
                    .table
                    .max_window_size
                    .saturating_add(requested_dict_budget)
                    .min(MAX_PRIMED_WINDOW_SIZE);
            }
        }

        let mut start = 0usize;
        let mut committed_dict_budget = 0usize;
        // insert_position needs 4 bytes of lookahead for hashing;
        // backfill_boundary_positions re-visits tail positions once the
        // next slice extends history, but cannot hash <4 byte fragments.
        let min_primed_tail = match self.active_backend() {
            super::strategy::BackendTag::Simple => MIN_MATCH_LEN,
            super::strategy::BackendTag::Dfast
            | super::strategy::BackendTag::Row
            | super::strategy::BackendTag::HashChain => 4,
        };
        while start < dict_content.len() {
            let end = (start + self.slice_size).min(dict_content.len());
            if end - start < min_primed_tail {
                break;
            }
            let mut space = self.get_next_space();
            space.clear();
            space.extend_from_slice(&dict_content[start..end]);
            self.commit_space(space);
            self.skip_matching_for_dictionary_priming();
            committed_dict_budget += end - start;
            start = end;
        }

        // Derive `granted_retained_budget` directly from the two real
        // bounds — bytes actually committed and bytes the cap allows
        // — instead of doing a cap-clip pass followed by an
        // uncommitted-tail subtract. Previous shape double-discounted
        // when the cap clipped: clip lost `(requested - allowed)`,
        // then tail-subtract lost ANOTHER `(requested - committed)`,
        // leaving `max_window_size` shy of the dictionary that was
        // actually retained (e.g. cap=900, committed=998, uncommitted=2
        // landed at granted=898 instead of the correct 900).
        let capped_retained_budget = MAX_PRIMED_WINDOW_SIZE.saturating_sub(base_max_window_size);
        let granted_retained_budget = committed_dict_budget.min(capped_retained_budget);
        let final_max_window_size = base_max_window_size.saturating_add(granted_retained_budget);
        match self.active_backend() {
            super::strategy::BackendTag::Simple => {
                self.simple_mut().max_window_size = final_max_window_size;
            }
            super::strategy::BackendTag::Dfast => {
                self.dfast_matcher_mut().max_window_size = final_max_window_size;
            }
            super::strategy::BackendTag::Row => {
                self.row_matcher_mut().max_window_size = final_max_window_size;
            }
            super::strategy::BackendTag::HashChain => {
                self.hc_matcher_mut().table.max_window_size = final_max_window_size;
            }
        }
        if granted_retained_budget > 0 {
            self.dictionary_retained_budget = self
                .dictionary_retained_budget
                .saturating_add(granted_retained_budget);
        }
        if self.active_backend() == super::strategy::BackendTag::HashChain {
            let table = &mut self.hc_matcher_mut().table;
            table.set_dictionary_limit_from_primed_bytes(committed_dict_budget);
            // Build the dictMatchState chain for BT/optimal levels so the
            // collect dual-probes the dictionary with its own compare budget
            // (the dict bytes were just committed to the front of history).
            if table.uses_bt {
                table.prime_dms_bt(committed_dict_budget);
            }
        }
        // CDict-equivalent: now that every dict chunk is indexed, mark the
        // Fast-backend dict table primed so the next frame's re-prime reuses
        // it (skips the re-hash) while still re-committing the dict bytes to
        // history. No-op when the attach path built no table (copy mode or a
        // sub-8-byte dict) — `mark_dict_primed` self-guards on table presence.
        match self.active_backend() {
            super::strategy::BackendTag::Simple => self.simple_mut().mark_dict_primed(),
            super::strategy::BackendTag::Dfast => self.dfast_matcher_mut().mark_dict_primed(),
            super::strategy::BackendTag::Row => self.row_matcher_mut().mark_dict_primed(),
            _ => {}
        }
    }

    fn restore_primed_dictionary(&mut self, level: super::CompressionLevel) -> bool {
        // Only the (storage, dictionary_retained_budget) pair is what
        // `prime_with_dictionary` writes; restoring them reproduces the
        // post-prime state exactly. Gated on the FULL resolved key (level + the
        // resolved `LevelParams` + the active backend's table width), not just
        // the level: `reset` resolves the hint into a window/table geometry, so a
        // same-level snapshot taken at a hint that resolved to a different shape
        // carries a `storage.max_window_size` / table dimensions that no longer
        // match this reset. Restoring it would let the encoder search past the
        // frame header's window (an undecodable match), so on a key mismatch we
        // refuse and the caller re-primes.
        let Some((params, table_bits, fast_attach, ldm)) = self.reset_shape else {
            return false;
        };
        let key = PrimedKey {
            level,
            params,
            table_bits,
            fast_attach,
            ldm,
        };
        let Some((snapshot, budget, captured_key)) = &self.primed else {
            return false;
        };
        if *captured_key != key {
            return false;
        }
        let budget = *budget;
        match (&mut self.storage, snapshot) {
            // Same-variant Fast restore: copy the snapshot into the retained
            // live storage. `clone_from` reuses the history / hash-table /
            // dict-table buffers, so this is the donor CDict table-copy
            // regime's cost (pure copies) instead of a full per-frame
            // allocation + copy + drop cycle.
            (MatcherStorage::Simple(live), MatcherStorage::Simple(snap)) => {
                live.clone_from(snap);
            }
            (live, snapshot_storage) => {
                let mut storage = snapshot_storage.clone();
                // A binary-tree snapshot is stored WITHOUT its live hash /
                // chain / hash3 tables (they hold no dictionary entries — the
                // dict lives in `dms` + history; see
                // `capture_primed_dictionary`). Re-allocate them zeroed to the
                // snapshot's geometry, exactly reproducing the post-prime
                // state (all `HC_EMPTY`). This is a full storage replace, so
                // no stale live-table entry from a prior frame can survive —
                // `ensure_tables` only allocates when the length mismatches,
                // so a full (HC / non-BT) snapshot whose tables are already
                // present is left untouched.
                if let MatcherStorage::HashChain(hc) = &mut storage {
                    hc.table.ensure_tables();
                }
                // The snapshot does not retain the LDM producer (it holds no
                // dict state; see `capture_primed_dictionary`). Carry over the
                // frame's freshly-reset producer — built this frame by `reset`
                // with the same params the snapshot key pins, and empty (no
                // input processed yet), so it is equivalent to the producer
                // the snapshot was captured with.
                #[cfg(feature = "hash")]
                {
                    let fresh_ldm = if let MatcherStorage::HashChain(hc) = live {
                        hc.take_ldm_producer()
                    } else {
                        None
                    };
                    if let MatcherStorage::HashChain(hc) = &mut storage {
                        hc.set_ldm_producer(fresh_ldm);
                    }
                }
                *live = storage;
            }
        }
        self.dictionary_retained_budget = budget;
        true
    }

    fn capture_primed_dictionary(&mut self, level: super::CompressionLevel) {
        // No resolved shape means `reset` has not run for this frame — nothing
        // valid to key a snapshot on, so skip the capture.
        let Some((params, table_bits, fast_attach, ldm)) = self.reset_shape else {
            return;
        };
        let key = PrimedKey {
            level,
            params,
            table_bits,
            fast_attach,
            ldm,
        };
        // Donor CDict-equivalent retained state. On the binary-tree backend the
        // dictionary is decoupled into `dms` (the donor `dictMatchState`); the
        // live hash / chain / hash3 tables carry NO dict entries at capture
        // (`skip_matching_dict_bt` keeps the dict out of the live tree), so they
        // are pure zeros. Storing them in the snapshot wastes the full table
        // footprint (a second window-tier table set resident for the whole
        // compress). Instead, move the live tables OUT of the working storage,
        // clone only the dict-state (history + `dms` + window/offset/dict-limit),
        // then move the live tables back — the snapshot keeps just what donor's
        // CDict keeps, and `restore_primed_dictionary` re-allocates the zeroed
        // live tables. HC / lazy levels keep the dict IN the live chain (no
        // `dms`), so their snapshot must retain the full tables: full clone.
        let bt_decoupled = matches!(
            &self.storage,
            MatcherStorage::HashChain(hc) if hc.table.uses_bt
        );
        if bt_decoupled {
            let MatcherStorage::HashChain(hc) = &mut self.storage else {
                unreachable!("bt_decoupled implies HashChain storage");
            };
            let hash_table = core::mem::take(&mut hc.table.hash_table);
            let chain_table = core::mem::take(&mut hc.table.chain_table);
            let hash3_table = core::mem::take(&mut hc.table.hash3_table);
            // The LDM producer carries no dictionary state (LDM is not
            // dict-primed; its hash table is empty at capture), so it is not
            // retained either — `restore` reinstates the frame's freshly-reset
            // producer. Take it out so the clone does not duplicate its table.
            #[cfg(feature = "hash")]
            let ldm_producer = hc.take_ldm_producer();
            // Clone the dict-state-only storage (live tables now empty Vecs,
            // LDM producer detached).
            let snapshot = self.storage.clone();
            // Move the live tables (and LDM producer) back into the working storage.
            let MatcherStorage::HashChain(hc) = &mut self.storage else {
                unreachable!("storage variant is stable across the take/put");
            };
            hc.table.hash_table = hash_table;
            hc.table.chain_table = chain_table;
            hc.table.hash3_table = hash3_table;
            #[cfg(feature = "hash")]
            hc.set_ldm_producer(ldm_producer);
            self.primed = Some((snapshot, self.dictionary_retained_budget, key));
        } else {
            self.primed = Some((self.storage.clone(), self.dictionary_retained_budget, key));
        }
    }

    fn invalidate_primed_dictionary(&mut self) {
        self.primed = None;
        // Drop the Fast-backend CDict-equivalent table cache too: it is keyed
        // to the dictionary being removed / replaced. Left in place, the next
        // same-params `reset` would retain it and the kernel would probe a
        // dict region whose bytes are no longer re-committed to history.
        match self.active_backend() {
            super::strategy::BackendTag::Simple => self.simple_mut().invalidate_dict_cache(),
            super::strategy::BackendTag::Dfast => self.dfast_matcher_mut().invalidate_dict_cache(),
            // Row keeps its attach index across frames (like Simple/Dfast),
            // so a dictionary swap must drop its cached dict rows too;
            // otherwise the next small/unknown-size frame reuses stale
            // attach state through `prime_dict_attach_current_block`.
            super::strategy::BackendTag::Row => self.row_matcher_mut().invalidate_dict_cache(),
            // The BT dms tree is keyed to the dict bytes; `prime_dms_bt`
            // skips the rebuild while its shape matches, so a swapped
            // dictionary of the same length would otherwise keep serving the
            // OLD dictionary's tree.
            super::strategy::BackendTag::HashChain => {
                self.hc_matcher_mut().table.dms.invalidate();
            }
        }
    }

    fn seed_dictionary_entropy(
        &mut self,
        huff: Option<&crate::huff0::huff0_encoder::HuffmanTable>,
        ll: Option<&crate::fse::fse_encoder::FSETable>,
        ml: Option<&crate::fse::fse_encoder::FSETable>,
        of: Option<&crate::fse::fse_encoder::FSETable>,
    ) {
        if self.active_backend() == super::strategy::BackendTag::HashChain {
            self.hc_matcher_mut()
                .seed_dictionary_entropy(huff, ll, ml, of);
        }
    }

    fn window_size(&self) -> u64 {
        self.reported_window_size as u64
    }

    fn get_next_space(&mut self) -> Vec<u8> {
        if let Some(mut space) = self.vec_pool.pop() {
            if space.len() > self.slice_size {
                space.truncate(self.slice_size);
            }
            if space.len() < self.slice_size {
                space.resize(self.slice_size, 0);
            }
            return space;
        }
        alloc::vec![0; self.slice_size]
    }

    fn get_last_space(&mut self) -> &[u8] {
        match &self.storage {
            MatcherStorage::Simple(m) => m.last_committed_space(),
            MatcherStorage::Dfast(m) => m.get_last_space(),
            MatcherStorage::Row(m) => m.get_last_space(),
            MatcherStorage::HashChain(m) => m.table.get_last_space(),
        }
    }

    fn commit_space(&mut self, space: Vec<u8>) {
        let mut evicted_bytes = 0usize;
        // Split borrows manually so the `add_data` closures can write
        // into `vec_pool` while the backend itself holds an exclusive
        // borrow via `storage`. (Suffix-store recycling went away
        // with the legacy `MatchGenerator`; the FastKernelMatcher
        // arm below has no pool interaction.)
        let vec_pool = &mut self.vec_pool;
        match &mut self.storage {
            MatcherStorage::Simple(m) => {
                // FastKernelMatcher owns its history as a single
                // flat Vec<u8> and the hash table as a Vec<u32> —
                // neither recycles into the driver-side pools. The
                // eager pre-commit eviction inside
                // `FastKernelMatcher::accept_data` drops bytes when
                // accepting this block would push history past 2×
                // max_window_size; that delta is what feeds
                // `evicted_bytes` here via the `pre / post`
                // history-length comparison.
                let pre = m.history_len_for_eviction_accounting();
                m.accept_data(space);
                let post = m.history_len_for_eviction_accounting();
                // `accept_data` performs eager pre-commit window
                // eviction (so this `pre - post` delta correctly
                // feeds the dictionary-budget retire flow). See
                // `FastKernelMatcher::accept_data` for the
                // commit-time-visibility rationale (closes #216
                // CodeRabbit review #5 / Copilot review #1: without
                // eager eviction, the delta was always 0 and the
                // dict budget never retired, leaving max_window_size
                // inflated post-dict-prime → matcher could emit
                // offsets exceeding the frame header's window).
                evicted_bytes += pre.saturating_sub(post);
            }
            MatcherStorage::Dfast(m) => {
                // Dfast's `add_data` callback receives the INPUT
                // `Vec<u8>` for pool recycling (Dfast stores its
                // bytes in the contiguous `history` buffer, not in
                // per-block Vecs — there is no per-block buffer to
                // pop off and hand back). Counting `data.len()` as
                // evicted bytes would conflate "new bytes ingested"
                // with "old bytes evicted from window"; the two
                // happen to coincide when the previous window was
                // saturated and the new input fills it 1:1, but
                // diverge when the eviction pop-loop drops blocks
                // of a different size than the incoming input. The
                // `dictionary_retained_budget` retire decision
                // downstream then gets driven by inflated eviction
                // counts and shrinks `max_window_size` prematurely.
                //
                // Derive the real eviction delta from `window_size`
                // before/after the call. The pop loop inside
                // `add_data` decrements `window_size` by each
                // evicted block length and then the final
                // `extend_from_slice + push_back` adds `space_len`,
                // so `evicted = pre + space_len - post`.
                let pre = m.window_size;
                let space_len = space.len();
                m.add_data(space, |data| {
                    // Same per-block recycle as the HashChain arm: push
                    // the spent input buffer back as-is rather than
                    // zero-filling to capacity. `add_data` mirrors the
                    // bytes into `history` and calls this every block, so
                    // capacity-wide zeroing would be hot-path waste;
                    // `get_next_space` zeroes at most `slice_size` bytes
                    // when it later reuses the buffer.
                    vec_pool.push(data);
                });
                evicted_bytes += pre.saturating_add(space_len).saturating_sub(m.window_size);
            }
            MatcherStorage::Row(m) => {
                // RowMatchGenerator::add_data recycles the *input* buffer
                // through this callback every commit (its bytes are mirrored
                // into `history`), not the evicted chunks. Derive the eviction
                // delta from `window_size` before/after — `evicted = pre +
                // space_len - post` — exactly like the Simple / HashChain arms.
                // Counting the callback argument as evicted would charge the
                // whole committed block as evicted and prematurely retire
                // dictionary budget on a window that evicts nothing.
                let pre = m.window_size;
                let space_len = space.len();
                m.add_data(space, |data| {
                    // Recycle the spent buffer as-is; `add_data` runs this for
                    // every committed block, so zero-filling to capacity here
                    // would be hot-path waste (`get_next_space` zeroes at most
                    // `slice_size` on reuse).
                    vec_pool.push(data);
                });
                evicted_bytes += pre.saturating_add(space_len).saturating_sub(m.window_size);
            }
            MatcherStorage::HashChain(m) => {
                // MatchTable::add_data now recycles the *incoming* buffer
                // through `reuse_space` (its bytes are copied into the
                // contiguous `history` mirror), so the callback no longer
                // reports evicted chunks. Derive the eviction delta from
                // `window_size` before/after, exactly like the Simple arm:
                // `evicted = pre + space_len - post`.
                let pre = m.table.window_size;
                let space_len = space.len();
                m.table.add_data(space, |data| {
                    // Recycle the spent input buffer to the pool as-is.
                    // `add_data` runs this callback for every committed
                    // block (the bytes are mirrored into `history`), so
                    // growing the buffer to its full capacity here would
                    // zero the whole allocation on the hot path.
                    // `get_next_space` resizes a popped buffer to
                    // `slice_size` on demand, touching at most
                    // `slice_size` bytes — never the larger capacity the
                    // pool retains.
                    vec_pool.push(data);
                });
                evicted_bytes += pre
                    .saturating_add(space_len)
                    .saturating_sub(m.table.window_size);
            }
        }
        // Gate the second backend trim pass on actual budget
        // reclamation. Without it, every slice commit on the
        // no-dictionary / no-eviction path (the common case) would
        // run a backend `match` ladder + `trim_to_window` early-out
        // for no reason — `trim_after_budget_retire` only does
        // meaningful work when `retire_dictionary_budget` shrank
        // `max_window_size` enough to make the backend's
        // `window_size > max_window_size` invariant trigger
        // eviction.
        if self.retire_dictionary_budget(evicted_bytes) {
            self.trim_after_budget_retire();
        }
    }

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        use super::strategy::{self, StrategyTag};
        // Borrowed one-shot Fast path: if the frame driver staged a
        // block range via `set_borrowed_block`, scan it in place against
        // the borrowed window instead of the owned committed block. Only
        // the Simple backend is instrumented (the gate guarantees it),
        // and the stage is consumed so the next block re-stages.
        if let Some((block_start, block_end)) = self.borrowed_pending.take() {
            match self.active_backend() {
                super::strategy::BackendTag::Simple => self.simple_mut().start_matching_borrowed(
                    block_start,
                    block_end,
                    &mut handle_sequence,
                ),
                super::strategy::BackendTag::Dfast => self
                    .dfast_matcher_mut()
                    .start_matching_borrowed(block_start, block_end, &mut handle_sequence),
                other => unreachable!("borrowed scan only for Simple/Dfast, got {other:?}"),
            }
            return;
        }
        // Decoupled parse×search dispatch (fires once per block). The
        // search axis (`self.search`) picks the candidate-finding backend;
        // the parse axis (greedy vs lazy depth) is carried by the
        // backend's runtime `lazy_depth`, set per level at `reset()`.
        // The two are independent, so any parse can run on any search
        // backend. The `BinaryTree` arm still selects the opt `Strategy`
        // ZST off `strategy_tag` so `compress_block::<S>` keeps its
        // const-folded optimal-parser monomorphisation.
        use super::strategy::SearchMethod;
        match self.search {
            SearchMethod::Fast => {
                self.simple_mut().start_matching(&mut handle_sequence);
                self.recycle_simple_space();
            }
            SearchMethod::DoubleFast => {
                self.dfast_matcher_mut()
                    .start_matching(&mut handle_sequence);
            }
            SearchMethod::RowHash => {
                // Greedy parse (depth 0) = donor-greedy entry (default
                // `ip + 1` start, greedy repcode commit); lazy / lazy2 use
                // the `pick_lazy_match` lookahead entry (reads `lazy_depth`).
                // Both bare entries dispatch on `row_log` internally into the
                // const-`ROW_LOG` hot loop (donor per-rowLog variant table).
                let greedy = self.parse == super::strategy::ParseMode::Greedy;
                let row = self.row_matcher_mut();
                if greedy {
                    row.start_matching_greedy(&mut handle_sequence);
                } else {
                    row.start_matching(&mut handle_sequence);
                }
            }
            SearchMethod::HashChain => {
                // Greedy/lazy/lazy2 all flow through the lazy parser; it
                // reads `hc.lazy_depth` (0 = greedy commit).
                self.hc_matcher_mut()
                    .start_matching_lazy(&mut handle_sequence);
            }
            SearchMethod::BinaryTree => match self.strategy_tag {
                StrategyTag::Btlazy2 => self
                    .hc_matcher_mut()
                    .start_matching_btlazy2(&mut handle_sequence),
                StrategyTag::BtOpt => self.compress_block::<strategy::BtOpt>(&mut handle_sequence),
                StrategyTag::BtUltra => {
                    self.compress_block::<strategy::BtUltra>(&mut handle_sequence)
                }
                StrategyTag::BtUltra2 => {
                    self.compress_block::<strategy::BtUltra2>(&mut handle_sequence)
                }
                _ => unreachable!(
                    "SearchMethod::BinaryTree requires a BT strategy tag (Btlazy2/BtOpt/BtUltra/BtUltra2)"
                ),
            },
        }
    }

    fn skip_matching(&mut self) {
        self.skip_matching_with_hint(None);
    }

    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        // Borrowed one-shot Fast path: a staged block range routes to the
        // borrowed skip (records the range for `get_last_space`, primes
        // hashes on the dict-priming hint) with no owned-history append
        // and nothing to recycle. Stage is consumed.
        if let Some((block_start, block_end)) = self.borrowed_pending.take() {
            match self.active_backend() {
                super::strategy::BackendTag::Simple => self.simple_mut().skip_matching_borrowed(
                    block_start,
                    block_end,
                    incompressible_hint,
                ),
                super::strategy::BackendTag::Dfast => self
                    .dfast_matcher_mut()
                    .skip_matching_borrowed(block_start, block_end, incompressible_hint),
                other => unreachable!("borrowed skip only for Simple/Dfast, got {other:?}"),
            }
            return;
        }
        match self.active_backend() {
            super::strategy::BackendTag::Simple => {
                self.simple_mut()
                    .skip_matching_with_hint(incompressible_hint);
                self.recycle_simple_space();
            }
            super::strategy::BackendTag::Dfast => {
                self.dfast_matcher_mut().skip_matching(incompressible_hint)
            }
            super::strategy::BackendTag::Row => self
                .row_matcher_mut()
                .skip_matching_with_hint(incompressible_hint),
            super::strategy::BackendTag::HashChain => {
                self.hc_matcher_mut().skip_matching(incompressible_hint)
            }
        }
    }
}

impl MatchGeneratorDriver {
    /// Monomorphised optimal-parser entry point. Only the `BinaryTree`
    /// search arm of [`Matcher::start_matching`] routes here, selecting
    /// the concrete opt `S: Strategy` (BtOpt / BtUltra / BtUltra2) off
    /// `strategy_tag`, so the optimiser keeps the cost-model predicates
    /// (`S::USE_BT` / `S::USE_HASH3` / `S::ACCURATE_PRICE` /
    /// `S::TWO_PASS_SEED`) const-folded per strategy. The non-opt search
    /// backends (Fast / DoubleFast / RowHash / HashChain) are dispatched
    /// directly off the search axis and never reach this method, so all
    /// strategies arriving here are HashChain-backed.
    fn compress_block<S: super::strategy::Strategy>(
        &mut self,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        debug_assert_eq!(S::BACKEND, super::strategy::BackendTag::HashChain);
        debug_assert!(
            S::USE_BT,
            "compress_block only handles the optimal (BT) path"
        );
        self.hc_matcher_mut()
            .start_matching_strategy::<S>(handle_sequence);
    }
}

/// Stage D: backend storage discriminator.
///
/// HC (lazy / lazy2) modes carry no extra per-frame state beyond the
/// shared `MatchTable` and `HcMatcher` runtime knobs, so the
/// [`HcBackend::Hc`] variant is zero-sized — no BT scratch is
/// allocated. BT-flavoured modes (`btopt` / `btultra` / `btultra2`)
/// hold the full [`super::bt::BtMatcher`] inside the
/// [`HcBackend::Bt`] variant (cost model, optimal-parser scratch
/// arenas, LDM candidate buffer).
///
/// The discriminator lives next to `parse_mode` so `configure()` can
/// promote between the two on a level change without touching the
/// `MatchTable` storage.
#[derive(Clone)]
pub(crate) enum HcBackend {
    /// Lazy / lazy2 modes — no per-frame backend state.
    Hc,
    /// BT-driven modes — owns the optimal parser's per-frame scratch.
    /// Boxed so the enum stays pointer-sized: HC-only matchers pay
    /// just the `Box`-niche, not the 4 KiB `BtMatcher` payload.
    Bt(alloc::boxed::Box<super::bt::BtMatcher>),
}

impl HcBackend {
    /// Heap bytes held by the backend. `Hc` is zero-sized; `Bt` boxes a
    /// `BtMatcher`, so count the boxed payload plus its own scratch heap.
    fn heap_size(&self) -> usize {
        match self {
            Self::Hc => 0,
            Self::Bt(bt) => core::mem::size_of::<super::bt::BtMatcher>() + bt.heap_size(),
        }
    }

    /// Mutable accessor on the BT matcher; panics if the active
    /// backend is `Hc`. The HC-or-Bt branches in orchestrator code use
    /// `let HcBackend::Bt(bt) = &self.backend` directly for readonly
    /// access — this helper exists so macro bodies that already drive
    /// a mutable BT update through the optimal parser can write
    /// `$self.backend.bt_mut().X` without an outer `match` ladder.
    #[inline(always)]
    pub(crate) fn bt_mut(&mut self) -> &mut super::bt::BtMatcher {
        match self {
            Self::Bt(bt) => bt,
            Self::Hc => unreachable!("BT-only accessor called in HC mode"),
        }
    }
}

#[derive(Clone)]
struct HcMatchGenerator {
    /// Shared match-finder storage (window, history, hash / chain /
    /// hash3 tables, dictionary-priming flags). Used identically by HC
    /// and BT modes; backend-specific table interpretation lives in the
    /// matcher methods on this struct.
    table: super::match_table::storage::MatchTable,
    /// HC runtime knobs (lazy_depth, search_depth, target_len). Always
    /// present — BT modes still consult `hc.search_depth` for repcode
    /// probing and chain candidate enumeration.
    hc: super::hc::HcMatcher,
    /// Backend discriminator. [`HcBackend::Hc`] is zero-sized for the
    /// lazy / lazy2 path so HC-only generators don't carry the BT
    /// optimal-parser scratch buffers. [`HcBackend::Bt`] holds the
    /// `BtMatcher` when an optimal mode is configured.
    backend: HcBackend,
    /// Compile-time strategy tag mirrored from
    /// [`MatchGeneratorDriver::strategy_tag`] during `configure()`.
    /// The driver hot path never reads this — it dispatches to
    /// `compress_block::<S>` from its own tag — but the
    /// `#[cfg(test)] start_matching` helper consumes it so artificial
    /// test setups still pick the correct concrete `S` for the
    /// const-generic optimal parser (BtOpt vs BtUltra vs BtUltra2).
    /// Without this field the test path would have to collapse
    /// `BtOpt` and `BtUltra` onto the same monomorphisation since
    /// `table.uses_bt` / `table.is_btultra2` alone can't tell them
    /// apart.
    strategy_tag: super::strategy::StrategyTag,
}

// Plain-data types relocated to [`crate::encoding::opt::types`] and
// [`crate::encoding::opt::ldm`] by #111 Phase 1. The use statements at
// the top of this file bring them back into scope so the existing
// methods on `HcMatchGenerator` compile unchanged.

/// `bt_insert_step_no_rebase` body parameterized over the per-CPU
/// `count_match_from_indices` symbol. Each kernel-specific wrapper invokes
/// the macro with its own `fastpath::<kernel>::count_match_from_indices`
/// path so the call resolves inside the wrapper's `#[target_feature]`
/// umbrella and inlines instead of paying the function-call ABI per BT walk
/// iteration. Used only by `HcMatchGenerator` BT walk wrappers below.
///
/// Crate-private: the macro body references private `encoding::*`
/// modules via `$crate::...`, so it is unusable downstream and is
/// re-exported only inside this crate via `pub(crate) use` below.
macro_rules! bt_insert_step_no_rebase_body {
    ($table:expr, $search_depth:expr, $abs_pos:ident, $current_abs_end:ident, $target_abs:ident, $cmf:path) => {{
        let idx = $abs_pos - $table.history_abs_start;
        let concat = &$table.history[$table.history_start..];
        if idx + 8 > concat.len() {
            return 1;
        }
        debug_assert!(
            $abs_pos <= $current_abs_end,
            "BT walker called past current block end"
        );
        let tail_limit = $current_abs_end - $abs_pos;
        let hash = $crate::encoding::match_table::storage::MatchTable::hash_position_at(
            concat,
            idx,
            $table.hash_log,
            $table.search_mls,
        );
        let Some(relative_pos) = $table.relative_position($abs_pos) else {
            return 1;
        };
        let stored = relative_pos + 1;
        let bt_mask = $table.bt_mask();
        // `abs_pos < bt_mask` legitimately happens for the first BT walk of
        // a fresh frame (bt_low effectively "no floor"). Saturating keeps
        // the floor at 0 so the `candidate_abs <= bt_low` check never
        // triggers early; raw subtraction would underflow into a huge
        // sentinel that ALWAYS triggers.
        let bt_low = $abs_pos.saturating_sub(bt_mask);
        let window_low = $table.window_low_abs_for_target($target_abs);
        // `abs_pos + 9` is safe in raw form: `MatchTable::add_data` caps
        // total input at `usize::MAX - STREAM_ABS_HEADROOM` (where
        // `STREAM_ABS_HEADROOM = HC_OPT_NUM + 16`), so every
        // frame-lifetime absolute cursor passed to the BT walker stays
        // below `usize::MAX - 9` regardless of stream length or
        // pointer width. The guard is hoisted to the data-ingest
        // boundary so this per-position site pays zero arithmetic
        // overhead in the hot loop.
        let mut match_end_abs = $abs_pos + 9;
        let mut best_len = 8usize;
        let mut compares_left = $search_depth;
        let mut common_length_smaller = 0usize;
        let mut common_length_larger = 0usize;
        let pair_idx = $table.bt_pair_index_for_abs($abs_pos);
        let mut smaller_slot = pair_idx;
        let mut larger_slot = pair_idx + 1;
        let mut match_stored = $table.hash_table[hash];
        $table.hash_table[hash] = stored;

        while compares_left > 0 {
            let Some(candidate_abs) =
                $crate::encoding::match_table::storage::MatchTable::stored_abs_position_fast(
                    match_stored,
                    $table.position_base,
                    $table.index_shift,
                )
            else {
                break;
            };
            if candidate_abs < window_low || candidate_abs >= $abs_pos {
                break;
            }
            compares_left -= 1;

            let next_pair_idx = $table.bt_pair_index_for_abs(candidate_abs);
            let next_smaller = $table.chain_table[next_pair_idx];
            let next_larger = $table.chain_table[next_pair_idx + 1];
            let seed_len = common_length_smaller.min(common_length_larger);
            let candidate_idx = candidate_abs - $table.history_abs_start;
            // SAFETY: BT walk invariant — `candidate_idx + tail_limit ≤
            // concat.len()` since the candidate is within
            // `[history_abs_start, abs_pos)` and `tail_limit ≤
            // current_abs_end - abs_pos`.
            let match_len = unsafe { $cmf(concat, idx, candidate_idx, tail_limit, seed_len) };

            if match_len > best_len {
                best_len = match_len;
                // `candidate_abs + match_len <= current_abs_end` by BT walk
                // invariant — `match_len <= tail_limit = current_abs_end -
                // abs_pos` and `candidate_abs < abs_pos`.
                let candidate_end = candidate_abs + match_len;
                if candidate_end > match_end_abs {
                    match_end_abs = candidate_end;
                }
            }

            if match_len >= tail_limit {
                break;
            }

            let candidate_next = candidate_idx + match_len;
            let current_next = idx + match_len;
            if concat[candidate_next] < concat[current_next] {
                $table.chain_table[smaller_slot] = match_stored;
                common_length_smaller = match_len;
                if candidate_abs <= bt_low {
                    smaller_slot = usize::MAX;
                    break;
                }
                smaller_slot = next_pair_idx + 1;
                match_stored = next_larger;
            } else {
                $table.chain_table[larger_slot] = match_stored;
                common_length_larger = match_len;
                if candidate_abs <= bt_low {
                    larger_slot = usize::MAX;
                    break;
                }
                larger_slot = next_pair_idx;
                match_stored = next_smaller;
            }
        }

        if smaller_slot != usize::MAX {
            $table.chain_table[smaller_slot] = $crate::encoding::match_table::storage::HC_EMPTY;
        }
        if larger_slot != usize::MAX {
            $table.chain_table[larger_slot] = $crate::encoding::match_table::storage::HC_EMPTY;
        }

        let speed_positions = if best_len > 384 {
            (best_len - 384).min(192)
        } else {
            0
        };
        // `match_end_abs` is initialized to `abs_pos + 9` and is only
        // reassigned inside the `candidate_end > match_end_abs` branch
        // above. So even though an individual `candidate_end =
        // candidate_abs + match_len` can land below `abs_pos` (the
        // candidate sits earlier in history and the match runs short),
        // the variable itself never drops below its initial value.
        // That gives `match_end_abs ≥ abs_pos + 9 > abs_pos + 8` as a
        // loop-wide invariant, so the raw subtraction below cannot
        // underflow.
        speed_positions.max(match_end_abs - ($abs_pos + 8))
    }};
}
pub(crate) use bt_insert_step_no_rebase_body;

/// `build_optimal_plan_impl` body parameterized over the per-CPU
/// `collect_optimal_candidates_initialized_<kernel>` method name. Caller
/// passes its `&mut self`, the seven DP entry-point arguments, and the
/// kernel-specific collect method. Each per-kernel wrapper invokes this
/// macro inside its own `#[target_feature]` umbrella so the per-position
/// `$collect` call inlines and the entire DP loop runs as one straight-line
/// hot path without an ABI barrier between the DP and the match-gathering
/// pipeline.
///
/// Body is ~730 lines but mechanically identical across kernels — the macro
/// keeps a single source of truth. The two const generics
/// (`ACCURATE_PRICE`, `FAVOR_SMALL_OFFSETS`) come from the wrapper's
/// generic parameter list and are referenced as bare identifiers; macro
/// hygiene resolves them at the expansion site.
/// Donor `offBase` for the btlazy2 lazy gain heuristic: a match whose offset
/// equals one of the three active repeat offsets prices as the cheap repcode
/// code (1/2/3); any other offset prices as `offset + 3`. So an equal-length
/// repeat-offset match always out-gains an explicit-offset one
/// (`zstd_lazy.c` `ZSTD_storeSeq` offBase convention).
#[inline]
fn btlazy2_offbase(offset: usize, reps: [u32; 3], ll0: bool) -> u32 {
    let o = offset as u32;
    // Donor repcode mapping shifts by `ll0` (zero-literal position): the cheap
    // codes become rep1 / rep2 / (rep0 - 1) instead of rep0 / rep1 / rep2,
    // because at ll0 an offset equal to rep0 is the special rep0-1 case, not
    // repcode 1. Scoring offsets against the wrong code at ll0 over-rewards a
    // rep0-distance match that does not actually encode as the cheapest code.
    if ll0 {
        if o == reps[1] {
            1
        } else if o == reps[2] {
            2
        } else if reps[0] > 1 && o == reps[0] - 1 {
            3
        } else {
            // Offsets are < window (<= 2^27), so `+ 3` never overflows u32.
            o + 3
        }
    } else if o == reps[0] {
        1
    } else if o == reps[1] {
        2
    } else if o == reps[2] {
        3
    } else {
        // Offsets are < window (<= 2^27), so `+ 3` never overflows u32.
        o + 3
    }
}

/// Donor lazy match gain (`matchLength * 4 - ZSTD_highbit32(offBase)`): the
/// selection metric that lets a shorter repeat-offset match beat a longer
/// explicit-offset one. `offBase >= 1`, so `highbit` is well-defined.
#[inline]
fn btlazy2_gain(match_len: usize, offset: usize, reps: [u32; 3], ll0: bool) -> i64 {
    let offbase = btlazy2_offbase(offset, reps, ll0);
    (match_len as i64) * 4 - (31 - offbase.leading_zeros()) as i64
}

/// Per-kernel body of the `btlazy2` (levels 13-15) greedy/lazy parse over
/// the binary-tree match finder. Mirrors `build_optimal_plan_impl_body!`'s
/// kernel-dispatch discipline: the wrapper carries the `#[target_feature]`
/// umbrella and passes its tier-specific `collect_optimal_candidates_initialized_<kernel>`
/// as `$collect`, so the per-position BT collect (and its inlined cpl)
/// stays under one umbrella — the runtime `select_kernel()` dispatch happens
/// ONCE per block in the bare `start_matching_btlazy2`, never per position.
macro_rules! start_matching_btlazy2_body {
    ($self:ident, $handle_sequence:ident, $collect:ident, $cmf:path $(,)?) => {{
        $self.table.ensure_tables();
        let current_len = *$self.table.chunk_lens.back().unwrap();
        if current_len == 0 {
            return;
        }
        let current_ptr = $self.table.get_last_space().as_ptr();
        // Mutates tables but never reallocates `history`, so this tail slice
        // stays valid for the routine's duration (same as the other parsers).
        let current: &[u8] = unsafe { core::slice::from_raw_parts(current_ptr, current_len) };
        // Full contiguous history (dict + prior blocks + current block) as a
        // raw slice, for the explicit repcode probe: a rep offset can point
        // before the current block, which `current` can't reach. Same
        // no-realloc validity contract as `current`; holds no borrow, so it
        // coexists with the `&mut self` collector calls below.
        let history_abs_start = $self.table.history_abs_start;
        let concat_full: &[u8] = unsafe {
            let base = $self.table.history.as_ptr().add($self.table.history_start);
            core::slice::from_raw_parts(base, $self.table.history.len() - $self.table.history_start)
        };
        let current_abs_start =
            $self.table.history_abs_start + $self.table.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        $self
            .table
            .apply_limited_update_after_long_match(current_abs_start);
        $self
            .table
            .backfill_boundary_positions(current_abs_start, current_abs_end);

        let profile = HcOptimalCostProfile::const_for_strategy::<super::strategy::Btlazy2>();
        let mut candidates = core::mem::take(&mut $self.backend.bt_mut().opt_candidates_scratch);

        let depth = $self.hc.lazy_depth as usize;
        let mut pos = 0usize;
        let mut literals_start = 0usize;

        // Collect + select the highest-GAIN match at a position (donor
        // `ZSTD_searchMax` plus the explicit offset_1 repcode check): scan the
        // length-sorted BT/dms ladder by gain, then probe rep0 directly since
        // the ladder's strictly-increasing-length filter drops short cheap
        // reps. Expands to `(match_len, offset)`; `match_len == 0` = no match.
        macro_rules! bt_select {
            ($p:expr) => {{
                let sel_pos: usize = $p;
                // `ll0` (donor): zero literals pending before this position, so
                // the repcode set is shifted (see `btlazy2_offbase`).
                let ll0 = sel_pos == literals_start;
                let sel_abs = current_abs_start + sel_pos;
                candidates.clear();
                let query = HcCandidateQuery {
                    reps: $self.table.offset_hist,
                    lit_len: sel_pos - literals_start,
                    // No LDM seed: L13-15 run at windowLog 22, below donor's
                    // LDM auto-enable threshold (windowLog >= 27).
                    ldm_candidate: None,
                };
                // SAFETY: called inside the wrapper's `#[target_feature]`
                // umbrella (the scalar wrapper's `$collect` is a safe fn).
                unsafe {
                    $self.$collect::<super::strategy::Btlazy2, true>(
                        sel_abs,
                        current_abs_end,
                        profile,
                        query,
                        &mut candidates,
                    );
                }
                let reps = $self.table.offset_hist;
                let mut sel_ml = 0usize;
                let mut sel_off = 0usize;
                let mut sel_gain = i64::MIN;
                for c in candidates.iter() {
                    let ml = c.match_len.min(current_len - sel_pos);
                    if ml < HC_OPT_MIN_MATCH_LEN {
                        continue;
                    }
                    let g = btlazy2_gain(ml, c.offset, reps, ll0);
                    if g > sel_gain {
                        sel_gain = g;
                        sel_ml = ml;
                        sel_off = c.offset;
                    }
                }
                let sel_idx = sel_abs - history_abs_start;
                // Donor probes `rep[0 + ll0]` directly (the length-sorted ladder
                // drops short cheap reps): rep0 normally, rep1 at a zero-literal
                // position where rep0 is not the cheapest code.
                let probe_rep = if ll0 {
                    reps[1] as usize
                } else {
                    reps[0] as usize
                };
                if probe_rep != 0 && sel_idx >= probe_rep {
                    let tail = current_len - sel_pos;
                    // SAFETY: `sel_idx - probe_rep < sel_idx`, `sel_idx + tail <=
                    // concat_full.len()`; same overshoot slack the collector
                    // relies on for this block.
                    let rep_ml =
                        unsafe { $cmf(concat_full, sel_idx, sel_idx - probe_rep, tail, 0) };
                    if rep_ml >= HC_OPT_MIN_MATCH_LEN
                        && btlazy2_gain(rep_ml, probe_rep, reps, ll0) > sel_gain
                    {
                        sel_ml = rep_ml;
                        sel_off = probe_rep;
                    }
                }
                (sel_ml, sel_off)
            }};
        }

        while pos + HC_OPT_MIN_MATCH_LEN <= current_len {
            let (mut best_ml, mut best_off) = bt_select!(pos);
            if best_ml < HC_OPT_MIN_MATCH_LEN {
                pos += 1;
                continue;
            }
            // Lazy lookahead (donor depth 1/2): advance one byte and accept the
            // later match only if it out-gains the current one by the donor
            // margin (deferring costs an extra literal — `+4` at depth 1, `+7`
            // at depth 2). `start` tracks where the chosen match begins.
            let mut start = pos;
            let mut d = 0usize;
            while d < depth && start + 1 + HC_OPT_MIN_MATCH_LEN <= current_len {
                let look = start + 1;
                let (ml2, off2) = bt_select!(look);
                if ml2 < HC_OPT_MIN_MATCH_LEN {
                    break;
                }
                let reps = $self.table.offset_hist;
                let margin = if d == 0 { 4 } else { 7 };
                // `best` sits at `start` (ll0 iff no literals precede it); the
                // lookahead match at `start + 1` always has a pending literal.
                let gain1 = btlazy2_gain(best_ml, best_off, reps, start == literals_start) + margin;
                let gain2 = btlazy2_gain(ml2, off2, reps, false);
                if gain2 > gain1 {
                    best_ml = ml2;
                    best_off = off2;
                    start = look;
                    d += 1;
                } else {
                    break;
                }
            }
            // Commit the chosen match at `start`; [literals_start, start) is
            // emitted as literals. `best_ml` was bounded to `current_len -
            // start` at selection, so `start + best_ml <= current_len`.
            let lit_len = start - literals_start;
            let literals = &current[literals_start..start];
            $handle_sequence(Sequence::Triple {
                literals,
                offset: best_off,
                match_len: best_ml,
            });
            let _ = encode_offset_with_history(
                best_off as u32,
                lit_len as u32,
                &mut $self.table.offset_hist,
            );
            pos = start + best_ml;
            literals_start = pos;
        }

        if literals_start < current_len {
            $handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
        $self.backend.bt_mut().opt_candidates_scratch = candidates;
    }};
}

macro_rules! build_optimal_plan_impl_body {
    (
        $self:expr,
        $strategy_ty:ty,
        $current:ident,
        $current_abs_start:ident,
        $current_len:ident,
        $initial_state:ident,
        $stats:ident,
        $out:ident,
        $collect:ident $(,)?
    ) => {{
        let current_abs_end = $current_abs_start + $current_len;
        let min_match_len = HC_OPT_MIN_MATCH_LEN;
        // `HC_OPT_NUM > 0` by const definition, so `HC_OPT_NUM - 1` is safe.
        let frontier_limit = $current_len.min(HC_OPT_NUM - 1);
        let initial_reps = $initial_state.reps;
        let initial_litlen = $initial_state.litlen;
        let ldm_block_offset = $initial_state.block_offset;
        let mut profile = $initial_state.profile;
        profile.sufficient_match_len = $self.hc.sufficient_match_len_for_pass(profile);
        // Const-fold from the strategy's associated `OPT_LEVEL`
        // (donor `optLevel`): BtOpt = 0, BtUltra / BtUltra2 = 2.
        // The two flags below are the only places the inner DP loop
        // used to consult `parse_mode`; lifting them into const
        // expressions drops one indirect read + one branch on every
        // candidate insertion and every traceback step.
        // `let` (not `const`) — nested `const` items inside a
        // generic fn cannot project through the outer fn's type
        // parameter, but a `let` binding from a const expression
        // does get folded by the optimiser per monomorphisation,
        // which is what we actually want here.
        debug_assert!(
            <$strategy_ty as super::strategy::Strategy>::USE_BT,
            "build_optimal_plan_impl_body called on non-BT strategy"
        );
        let abort_on_worse_match: bool =
            <$strategy_ty as super::strategy::Strategy>::OPT_LEVEL == 0;
        let opt_level: bool = <$strategy_ty as super::strategy::Strategy>::OPT_LEVEL >= 2;
        let mut nodes = core::mem::take(&mut $self.backend.bt_mut().opt_nodes_scratch);
        // `frontier_limit + 2 <= HC_OPT_NUM + 1` — bounded by const.
        let frontier_buffer_size = frontier_limit + 2;
        if nodes.len() < frontier_buffer_size {
            nodes.resize(frontier_buffer_size, HcOptimalNode::default());
        }
        let mut candidates = core::mem::take(&mut $self.backend.bt_mut().opt_candidates_scratch);
        candidates.clear();
        if candidates.capacity() < MAX_HC_SEARCH_DEPTH {
            candidates.reserve_exact(MAX_HC_SEARCH_DEPTH - candidates.capacity());
        }
        let mut store = core::mem::take(&mut $self.backend.bt_mut().opt_store_scratch);
        store.clear();
        let mut ll_prices = core::mem::take(&mut $self.backend.bt_mut().opt_ll_price_scratch);
        let mut ll_price_generations =
            core::mem::take(&mut $self.backend.bt_mut().opt_ll_price_generation);
        if ll_prices.len() <= frontier_limit {
            ll_prices.resize(frontier_limit + 1, 0);
            ll_price_generations.resize(frontier_limit + 1, 0);
        }
        $self.backend.bt_mut().opt_ll_price_stamp = $self
            .backend
            .bt_mut()
            .opt_ll_price_stamp
            .wrapping_add(1)
            .max(1);
        let ll_price_stamp = $self.backend.bt_mut().opt_ll_price_stamp;
        $self.backend.bt_mut().opt_lit_price_stamp = $self
            .backend
            .bt_mut()
            .opt_lit_price_stamp
            .wrapping_add(1)
            .max(1);
        let lit_price_stamp = $self.backend.bt_mut().opt_lit_price_stamp;
        let mut ml_prices = core::mem::take(&mut $self.backend.bt_mut().opt_ml_price_scratch);
        let mut ml_price_generations =
            core::mem::take(&mut $self.backend.bt_mut().opt_ml_price_generation);
        if ml_prices.len() <= frontier_limit {
            ml_prices.resize(frontier_limit + 1, 0);
            ml_price_generations.resize(frontier_limit + 1, 0);
        }
        $self.backend.bt_mut().opt_ml_price_stamp = $self
            .backend
            .bt_mut()
            .opt_ml_price_stamp
            .wrapping_add(1)
            .max(1);
        let ml_price_stamp = $self.backend.bt_mut().opt_ml_price_stamp;
        nodes[0] = HcOptimalNode {
            price: BtMatcher::cached_lit_length_price(
                profile,
                $stats,
                initial_litlen,
                &mut ll_prices,
                &mut ll_price_generations,
                ll_price_stamp,
            ),
            litlen: initial_litlen as u32,
            reps: initial_reps,
            ..HcOptimalNode::default()
        };
        let sufficient_len = profile.sufficient_match_len;
        let ll0_price = BtMatcher::cached_lit_length_price(
            profile,
            $stats,
            0,
            &mut ll_prices,
            &mut ll_price_generations,
            ll_price_stamp,
        );
        let ll1_price = BtMatcher::cached_lit_length_price(
            profile,
            $stats,
            1,
            &mut ll_prices,
            &mut ll_price_generations,
            ll_price_stamp,
        );
        let mut pos = 1usize;
        let mut last_pos = 0usize;
        let mut forced_end: Option<usize> = None;
        let mut forced_end_state: Option<HcOptimalNode> = None;
        let mut seed_forced_shortest_path = false;
        let mut opt_ldm = HcOptLdmState {
            seq_store: HcRawSeqStore {
                pos: 0,
                pos_in_sequence: 0,
                size: $self.backend.bt_mut().ldm_sequences.len(),
            },
            ..HcOptLdmState::default()
        };
        let has_ldm = !$self.backend.bt_mut().ldm_sequences.is_empty();
        if has_ldm {
            // `ldm_sequences` are emitted in BLOCK-relative coordinates,
            // but this optimal-parser pass runs over a SEGMENT of the
            // block starting at block-offset `$block_offset` and uses
            // segment-relative positions throughout. Fast-forward the raw
            // seq-store cursor past the bytes covered by earlier segments
            // so the (segment-relative) LDM windows below land at the
            // correct positions. Idempotent: `ldm_skip_raw_seq_store_bytes`
            // recomputes from `pos = 0`, so re-running it per segment is
            // safe. Without this, every segment after the first re-applied
            // the block's leading LDM windows at the wrong offset, emitting
            // matches that copy the wrong bytes (undecodable frame).
            if ldm_block_offset > 0 {
                $self
                    .backend
                    .bt_mut()
                    .ldm_skip_raw_seq_store_bytes(&mut opt_ldm.seq_store, ldm_block_offset);
            }
            $self
                .backend
                .bt_mut()
                .ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 0, $current_len);
        }

        // Donor-like seed at rPos=0: initialize frontier with matches starting
        // at current position before entering the generic forward DP loop.
        if $current_len >= min_match_len {
            let seed_ldm = if has_ldm {
                $self.backend.bt_mut().ldm_process_match_candidate(
                    &mut opt_ldm,
                    0,
                    $current_len,
                    min_match_len,
                )
            } else {
                None
            };
            candidates.clear();
            // SAFETY: wrapper is in the same target_feature umbrella as the
            // `$collect` kernel variant; the runtime kernel detector already
            // gated entry into the wrapper.
            unsafe {
                $self.$collect::<$strategy_ty, true>(
                    $current_abs_start,
                    current_abs_end,
                    profile,
                    HcCandidateQuery {
                        reps: initial_reps,
                        lit_len: initial_litlen,
                        ldm_candidate: seed_ldm,
                    },
                    &mut candidates,
                )
            };
            if !candidates.is_empty() {
                // `min_match_len >= HC_FORMAT_MINMATCH (3)` by invariant.
                last_pos = (min_match_len - 1).min(frontier_limit);
                for p in 1..min_match_len.min(nodes.len()) {
                    BtMatcher::reset_opt_node(&mut nodes[p]);
                    // `initial_litlen` is the litlen carried from prior
                    // optimal-plan segments — its real bound is the
                    // current block length (the frame compressor caps
                    // block scan at `HC_BLOCKSIZE_MAX`), not the segment
                    // `current_len`. `p < min_match_len` (small constant),
                    // so the sum stays well within `u32::MAX`. Use
                    // `checked_add` FIRST so the `usize` addition itself
                    // cannot overflow on i686 (where `usize` is 32-bit
                    // and a wrapping `+` would slip past `try_from`).
                    let seed_litlen = initial_litlen
                        .checked_add(p)
                        .and_then(|s| u32::try_from(s).ok())
                        .expect("optimal parser seed litlen out of u32 range");
                    nodes[p].litlen = seed_litlen;
                }
            }

            if let Some(candidate) = candidates.last() {
                let longest_len = candidate.match_len.min($current_len);
                if longest_len > sufficient_len {
                    let off_base = BtMatcher::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        initial_litlen,
                        initial_reps,
                    );
                    let off_price = profile
                        .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                    let ml_price = BtMatcher::cached_match_length_price(
                        profile,
                        $stats,
                        longest_len,
                        &mut ml_prices,
                        &mut ml_price_generations,
                        ml_price_stamp,
                    );
                    let seq_cost = BtMatcher::add_prices(
                        ll0_price,
                        profile.match_price_from_parts(off_price, ml_price, $stats),
                    );
                    let forced_price = BtMatcher::add_prices(nodes[0].price, seq_cost);
                    let forced_state = HcOptimalNode {
                        price: forced_price,
                        off: candidate.offset as u32,
                        mlen: longest_len as u32,
                        litlen: 0,
                        reps: initial_reps,
                    };
                    if longest_len < nodes.len() && forced_price < nodes[longest_len].price {
                        nodes[longest_len] = forced_state;
                    }
                    forced_end = Some(longest_len);
                    forced_end_state = Some(forced_state);
                    seed_forced_shortest_path = true;
                }
            }
            if !seed_forced_shortest_path {
                let mut prev_max_len = min_match_len - 1;
                for candidate in candidates.iter() {
                    let max_match_len = candidate.match_len.min(frontier_limit);
                    if max_match_len < min_match_len {
                        continue;
                    }
                    let start_len = (prev_max_len + 1).max(min_match_len);
                    if start_len > max_match_len {
                        prev_max_len = prev_max_len.max(max_match_len);
                        continue;
                    }
                    if max_match_len > last_pos {
                        BtMatcher::reset_opt_nodes(&mut nodes, last_pos + 1, max_match_len);
                    }
                    let off_base = BtMatcher::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        initial_litlen,
                        initial_reps,
                    );
                    let off_price = profile
                        .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                    debug_assert!(max_match_len < nodes.len());
                    let nodes0_price = nodes[0].price;
                    for match_len in (start_len..=max_match_len).rev() {
                        let ml_price = BtMatcher::cached_match_length_price(
                            profile,
                            $stats,
                            match_len,
                            &mut ml_prices,
                            &mut ml_price_generations,
                            ml_price_stamp,
                        );
                        let seq_cost = BtMatcher::add_prices(
                            ll0_price,
                            profile.match_price_from_parts(off_price, ml_price, $stats),
                        );
                        let next_cost = BtMatcher::add_prices(nodes0_price, seq_cost);
                        let node_price = unsafe { nodes.get_unchecked(match_len).price };
                        if match_len > last_pos || next_cost < node_price {
                            let slot = unsafe { nodes.get_unchecked_mut(match_len) };
                            *slot = HcOptimalNode {
                                price: next_cost,
                                off: candidate.offset as u32,
                                mlen: match_len as u32,
                                litlen: 0,
                                reps: initial_reps,
                            };
                            if match_len > last_pos {
                                last_pos = match_len;
                            }
                        } else if abort_on_worse_match {
                            break;
                        }
                    }
                    prev_max_len = prev_max_len.max(max_match_len);
                }
                if last_pos + 1 < nodes.len() {
                    nodes[last_pos + 1].price = u32::MAX;
                }
            }
        }
        while !seed_forced_shortest_path && pos <= last_pos && pos <= frontier_limit {
            debug_assert!(pos + 1 < nodes.len());
            let prev_node = unsafe { *nodes.get_unchecked(pos - 1) };
            if prev_node.price != u32::MAX {
                let lit_len = prev_node.litlen as usize + 1;
                let lit_price = {
                    let bt = $self.backend.bt_mut();
                    BtMatcher::cached_literal_price(
                        profile,
                        $stats,
                        $current[pos - 1],
                        &mut bt.opt_lit_price_scratch,
                        &mut bt.opt_lit_price_generation,
                        lit_price_stamp,
                    )
                };
                let ll_delta = BtMatcher::cached_lit_length_delta_price(
                    profile,
                    $stats,
                    lit_len,
                    &mut ll_prices,
                    &mut ll_price_generations,
                    ll_price_stamp,
                );
                let lit_cost = BtMatcher::add_price_delta(prev_node.price, lit_price, ll_delta);
                let node_pos_price = unsafe { nodes.get_unchecked(pos).price };
                if lit_cost <= node_pos_price {
                    let prev_match = unsafe { *nodes.get_unchecked(pos) };
                    let slot = unsafe { nodes.get_unchecked_mut(pos) };
                    *slot = prev_node;
                    slot.litlen = lit_len as u32;
                    slot.price = lit_cost;
                    #[allow(clippy::collapsible_if)]
                    if opt_level
                        && prev_match.mlen > 0
                        && prev_match.litlen == 0
                        && pos < $current_len
                    {
                        if ll1_price < ll0_price {
                            let next_lit_price = {
                                let bt = $self.backend.bt_mut();
                                BtMatcher::cached_literal_price(
                                    profile,
                                    $stats,
                                    $current[pos],
                                    &mut bt.opt_lit_price_scratch,
                                    &mut bt.opt_lit_price_generation,
                                    lit_price_stamp,
                                )
                            };
                            let with1literal = BtMatcher::add_price_delta(
                                prev_match.price,
                                next_lit_price,
                                ll1_price as i32 - ll0_price as i32,
                            );
                            let ll_delta_next = BtMatcher::cached_lit_length_delta_price(
                                profile,
                                $stats,
                                lit_len + 1,
                                &mut ll_prices,
                                &mut ll_price_generations,
                                ll_price_stamp,
                            );
                            let with_more_literals =
                                BtMatcher::add_price_delta(lit_cost, next_lit_price, ll_delta_next);
                            let next = pos + 1;
                            let next_price = unsafe { nodes.get_unchecked(next).price };
                            if with1literal < with_more_literals && with1literal < next_price {
                                // Donor parity (zstd_opt.c:1232): `cur >= prevMatch.mlen`.
                                debug_assert!(pos >= prev_match.mlen as usize);
                                let prev_pos = pos - prev_match.mlen as usize;
                                {
                                    let prev_state = unsafe { *nodes.get_unchecked(prev_pos) };
                                    let (_, reps_after_match) = BtMatcher::encode_offset_with_reps(
                                        prev_match.off,
                                        prev_state.litlen as usize,
                                        prev_state.reps,
                                    );
                                    let slot = unsafe { nodes.get_unchecked_mut(next) };
                                    *slot = prev_match;
                                    slot.reps = reps_after_match;
                                    slot.litlen = 1;
                                    slot.price = with1literal;
                                    if next > last_pos {
                                        last_pos = next;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let mut base_node = unsafe { *nodes.get_unchecked(pos) };
            if base_node.price == u32::MAX {
                pos += 1;
                continue;
            }
            if base_node.mlen > 0 && base_node.litlen == 0 {
                // Donor parity (zstd_opt.c:1255): `cur >= opt[cur].mlen`.
                debug_assert!(pos >= base_node.mlen as usize);
                let prev_pos = pos - base_node.mlen as usize;
                {
                    let prev_state = unsafe { *nodes.get_unchecked(prev_pos) };
                    let (_, reps_after_match) = BtMatcher::encode_offset_with_reps(
                        base_node.off,
                        prev_state.litlen as usize,
                        prev_state.reps,
                    );
                    base_node.reps = reps_after_match;
                    unsafe { nodes.get_unchecked_mut(pos).reps = reps_after_match };
                }
            }
            let base_cost = base_node.price;

            if pos + 8 > $current_len {
                pos += 1;
                continue;
            }

            if pos == last_pos {
                break;
            }

            let next_price = unsafe { nodes.get_unchecked(pos + 1).price };
            if abort_on_worse_match
                && next_price <= base_cost.saturating_add(HC_BITCOST_MULTIPLIER / 2)
            {
                pos += 1;
                continue;
            }

            let abs_pos = $current_abs_start + pos;
            let ldm_candidate = if has_ldm {
                $self.backend.bt_mut().ldm_process_match_candidate(
                    &mut opt_ldm,
                    pos,
                    $current_len - pos,
                    min_match_len,
                )
            } else {
                None
            };
            candidates.clear();
            // SAFETY: same umbrella as `$collect`.
            unsafe {
                $self.$collect::<$strategy_ty, true>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    HcCandidateQuery {
                        reps: base_node.reps,
                        lit_len: base_node.litlen as usize,
                        ldm_candidate,
                    },
                    &mut candidates,
                )
            };
            if let Some(candidate) = candidates.last() {
                let longest_len = candidate.match_len.min($current_len - pos);
                if longest_len > sufficient_len
                    || pos + longest_len >= HC_OPT_NUM
                    || pos + longest_len >= $current_len
                {
                    let lit_len = base_node.litlen as usize;
                    let off_base = BtMatcher::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        lit_len,
                        base_node.reps,
                    );
                    let off_price = profile
                        .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                    let ml_price = BtMatcher::cached_match_length_price(
                        profile,
                        $stats,
                        longest_len,
                        &mut ml_prices,
                        &mut ml_price_generations,
                        ml_price_stamp,
                    );
                    let seq_cost = BtMatcher::add_prices(
                        ll0_price,
                        profile.match_price_from_parts(off_price, ml_price, $stats),
                    );
                    let forced_price = BtMatcher::add_prices(base_cost, seq_cost);
                    let end_pos = (pos + longest_len).min($current_len);
                    forced_end = Some(end_pos);
                    forced_end_state = Some(HcOptimalNode {
                        price: forced_price,
                        off: candidate.offset as u32,
                        mlen: longest_len as u32,
                        litlen: 0,
                        reps: base_node.reps,
                    });
                    break;
                }
            }
            let mut prev_max_len = min_match_len - 1;
            for candidate in candidates.iter() {
                // Outer loop guards `pos <= frontier_limit` (see the
                // `while ... pos <= frontier_limit` condition); the
                // subtraction below is therefore safe.
                debug_assert!(pos <= frontier_limit);
                let max_match_len = candidate
                    .match_len
                    .min($current_len - pos)
                    .min(frontier_limit - pos);
                let min_len = min_match_len;
                if max_match_len < min_len {
                    continue;
                }
                let start_len = (prev_max_len + 1).max(min_len);
                if start_len > max_match_len {
                    prev_max_len = prev_max_len.max(max_match_len);
                    continue;
                }
                let max_next = pos + max_match_len;
                if max_next > last_pos {
                    BtMatcher::reset_opt_nodes(&mut nodes, last_pos + 1, max_next);
                }
                let lit_len = base_node.litlen as usize;
                let off_base = BtMatcher::encode_offset_base_with_reps(
                    candidate.offset as u32,
                    lit_len,
                    base_node.reps,
                );
                let off_price = profile
                    .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                debug_assert!(pos + max_match_len < nodes.len());
                for match_len in (start_len..=max_match_len).rev() {
                    let next = pos + match_len;
                    let ml_price = BtMatcher::cached_match_length_price(
                        profile,
                        $stats,
                        match_len,
                        &mut ml_prices,
                        &mut ml_price_generations,
                        ml_price_stamp,
                    );
                    let seq_cost = BtMatcher::add_prices(
                        ll0_price,
                        profile.match_price_from_parts(off_price, ml_price, $stats),
                    );
                    let next_cost = BtMatcher::add_prices(base_cost, seq_cost);
                    let node_next_price = unsafe { nodes.get_unchecked(next).price };
                    let improved = next > last_pos || next_cost < node_next_price;
                    if improved {
                        let slot = unsafe { nodes.get_unchecked_mut(next) };
                        *slot = HcOptimalNode {
                            price: next_cost,
                            off: candidate.offset as u32,
                            mlen: match_len as u32,
                            litlen: 0,
                            reps: base_node.reps,
                        };
                        if next > last_pos {
                            last_pos = next;
                        }
                    } else if abort_on_worse_match {
                        break;
                    }
                }
                prev_max_len = prev_max_len.max(max_match_len);
            }

            if last_pos + 1 < nodes.len() {
                unsafe {
                    nodes.get_unchecked_mut(last_pos + 1).price = u32::MAX;
                }
            }
            pos += 1;
        }

        if last_pos == 0 {
            if $current_len == 0 {
                let price = nodes[0].price;
                return $self.backend.bt_mut().finish_optimal_plan(
                    HcOptimalPlanBuffers {
                        nodes,
                        candidates,
                        store,
                        ll_prices,
                        ll_price_generations,
                        ml_prices,
                        ml_price_generations,
                    },
                    (price, initial_reps, initial_litlen, 0),
                );
            }
            let lit_price = {
                let bt = $self.backend.bt_mut();
                BtMatcher::cached_literal_price(
                    profile,
                    $stats,
                    $current[0],
                    &mut bt.opt_lit_price_scratch,
                    &mut bt.opt_lit_price_generation,
                    lit_price_stamp,
                )
            };
            // `initial_litlen` is carried across optimal-plan segments;
            // its real bound is the current block length, not
            // `current_len`. On i686 (32-bit `usize`) `+ 1` could
            // theoretically wrap if the invariant ever broke. Catch
            // that explicitly via `checked_add` rather than letting a
            // wrapping sum slip into the price lookup.
            let next_litlen = initial_litlen
                .checked_add(1)
                .expect("optimal parser next litlen out of usize range");
            let ll_delta = BtMatcher::cached_lit_length_delta_price(
                profile,
                $stats,
                next_litlen,
                &mut ll_prices,
                &mut ll_price_generations,
                ll_price_stamp,
            );
            let price = BtMatcher::add_price_delta(nodes[0].price, lit_price, ll_delta);
            return $self.backend.bt_mut().finish_optimal_plan(
                HcOptimalPlanBuffers {
                    nodes,
                    candidates,
                    store,
                    ll_prices,
                    ll_price_generations,
                    ml_prices,
                    ml_price_generations,
                },
                (price, initial_reps, next_litlen, 1),
            );
        }

        let target_pos = forced_end.unwrap_or(last_pos.min(frontier_limit));
        let last_stretch = if let Some(forced_state) = forced_end_state {
            forced_state
        } else {
            nodes[target_pos]
        };
        if last_stretch.price == u32::MAX {
            return $self.backend.bt_mut().finish_optimal_plan(
                HcOptimalPlanBuffers {
                    nodes,
                    candidates,
                    store,
                    ll_prices,
                    ll_price_generations,
                    ml_prices,
                    ml_price_generations,
                },
                (u32::MAX, initial_reps, initial_litlen, $current_len),
            );
        }

        if last_stretch.mlen == 0 {
            return $self.backend.bt_mut().finish_optimal_plan(
                HcOptimalPlanBuffers {
                    nodes,
                    candidates,
                    store,
                    ll_prices,
                    ll_price_generations,
                    ml_prices,
                    ml_price_generations,
                },
                (
                    last_stretch.price,
                    last_stretch.reps,
                    last_stretch.litlen as usize,
                    target_pos.min($current_len),
                ),
            );
        }

        let mut cur = target_pos.saturating_sub(last_stretch.mlen as usize);
        let end_reps = if last_stretch.litlen == 0 {
            let prev_state = nodes[cur];
            let (_, reps_after_match) = BtMatcher::encode_offset_with_reps(
                last_stretch.off,
                prev_state.litlen as usize,
                prev_state.reps,
            );
            reps_after_match
        } else {
            let tail_literals = last_stretch.litlen as usize;
            if cur < tail_literals {
                return $self.backend.bt_mut().finish_optimal_plan(
                    HcOptimalPlanBuffers {
                        nodes,
                        candidates,
                        store,
                        ll_prices,
                        ll_price_generations,
                        ml_prices,
                        ml_price_generations,
                    },
                    (
                        last_stretch.price,
                        last_stretch.reps,
                        tail_literals,
                        target_pos.min($current_len),
                    ),
                );
            }
            cur -= tail_literals;
            last_stretch.reps
        };
        let store_end = cur + 2;
        if store.len() <= store_end {
            store.resize(store_end + 1, HcOptimalNode::default());
        }
        let mut store_start;
        let mut stretch_pos = cur;

        if last_stretch.litlen > 0 {
            store[store_end] = HcOptimalNode {
                litlen: last_stretch.litlen,
                mlen: 0,
                ..HcOptimalNode::default()
            };
            store_start = store_end.saturating_sub(1);
            store[store_start] = last_stretch;
        }
        store[store_end] = last_stretch;
        store_start = store_end;

        loop {
            let next_stretch = nodes[stretch_pos];
            store[store_start].litlen = next_stretch.litlen;
            if next_stretch.mlen == 0 {
                break;
            }
            if store_start == 0 {
                break;
            }
            store_start -= 1;
            store[store_start] = next_stretch;
            // Parser invariant: every emitted stretch is bounded by the
            // current block, so `litlen + mlen <= current_len <=
            // HC_BLOCKSIZE_MAX (128 KiB)`. The `as usize` widening + raw
            // `+` is safe on 32-bit targets — two u32 values do NOT
            // automatically fit in `usize` on i686, the block bound is
            // what makes this addition safe.
            let litlen = next_stretch.litlen as usize;
            let mlen = next_stretch.mlen as usize;
            debug_assert!(litlen + mlen <= $current_len);
            let step = litlen + mlen;
            if step == 0 || stretch_pos < step {
                break;
            }
            stretch_pos -= step;
        }

        let mut tail_literals = initial_litlen;
        let mut store_pos = store_start;
        while store_pos <= store_end {
            let stretch = store[store_pos];
            let llen = stretch.litlen as usize;
            let mlen = stretch.mlen as usize;
            if mlen == 0 {
                tail_literals = llen;
                store_pos += 1;
                continue;
            }
            $out.push(HcOptimalSequence {
                offset: stretch.off,
                match_len: mlen as u32,
                lit_len: llen as u32,
            });
            tail_literals = 0;
            store_pos += 1;
        }
        let result = (
            last_stretch.price,
            end_reps,
            if last_stretch.litlen > 0 {
                last_stretch.litlen as usize
            } else {
                tail_literals
            },
            target_pos.min($current_len),
        );
        $self.backend.bt_mut().finish_optimal_plan(
            HcOptimalPlanBuffers {
                nodes,
                candidates,
                store,
                ll_prices,
                ll_price_generations,
                ml_prices,
                ml_price_generations,
            },
            result,
        )
    }};
}

/// `collect_optimal_candidates_initialized` body parameterized over the per-CPU
/// kernel: the `$cpl` path is the kernel's `common_prefix_len_ptr` (used in
/// the HC chain walk fallback), and the four method-name substitutions
/// (`$bt_update`, `$bt_insert`, `$for_each_rep`, `$hash3`) route to the
/// kernel-specific wrappers of the inner helpers. With every helper under
/// the same `target_feature` umbrella, the entire per-position pipeline
/// (BT-tree fill + rep probing + hash3 probing + BT match collection /
/// HC chain walk) inlines without ABI barriers on the level22 hot path.
macro_rules! collect_optimal_candidates_initialized_body {
    (
        $self:expr,
        $strategy_ty:ty,
        $abs_pos:ident,
        $current_abs_end:ident,
        $profile:ident,
        $query:ident,
        $out:ident,
        $bt_matchfinder:ident,
        $bt_update:ident,
        $bt_insert:ident,
        $for_each_rep:ident,
        $hash3:ident,
        $cpl:path $(,)?
    ) => {{
        // Per-strategy compile-time const: only BtUltra2 drives the
        // hash3 short-match table. All other monomorphisations drop
        // the entire hash3 lookup block at codegen time. The relaxed
        // implication enforces only the direction we depend on:
        // if the strategy declares hash3, the table must be live.
        // The reverse (`hash3_log != 0` without `USE_HASH3`) is OK —
        // a future caller may pre-allocate hash3 storage without
        // wiring the BtUltra2 path through.
        let use_hash3: bool = <$strategy_ty as super::strategy::Strategy>::USE_HASH3;
        debug_assert!(!$self.table.hash_table.is_empty());
        debug_assert!($self.table.hash3_log == 0 || !$self.table.hash3_table.is_empty());
        debug_assert!(
            !use_hash3 || $self.table.hash3_log != 0,
            "Strategy::USE_HASH3 = true but runtime hash3_log is 0 — call configure() first",
        );
        debug_assert!(!$self.table.chain_table.is_empty());
        let min_match_len = HC_OPT_MIN_MATCH_LEN;
        let reps = $query.reps;
        let lit_len = $query.lit_len;
        let ldm_candidate = $query.ldm_candidate;
        $out.clear();
        if $abs_pos < $self.table.skip_insert_until_abs {
            if let Some(ldm) = ldm_candidate {
                let mut best_len_for_skip = 0usize;
                let _ = super::bt::BtMatcher::push_candidate_ladder(
                    $out,
                    &mut best_len_for_skip,
                    ldm,
                    min_match_len,
                );
            }
            return;
        }
        if $bt_matchfinder {
            // SAFETY: caller is in the same target_feature umbrella as
            // `$bt_update`; the runtime kernel detector already gated entry.
            unsafe { $self.table.$bt_update($abs_pos, $current_abs_end) };
        }
        let current_idx = $abs_pos - $self.table.history_abs_start;
        if current_idx + 4 > $self.table.live_history().len() {
            if let Some(ldm) = ldm_candidate {
                let mut best_len_for_skip = 0usize;
                let _ = super::bt::BtMatcher::push_candidate_ladder(
                    $out,
                    &mut best_len_for_skip,
                    ldm,
                    min_match_len,
                );
            }
            return;
        }
        let mut best_len_for_skip = 0usize;
        let mut skip_further_match_search = false;
        let mut rep_len_candidate_found = false;
        // SAFETY: same umbrella; closure capture is monomorphized per call.
        unsafe {
            $self.hc.$for_each_rep(
                &$self.table,
                $abs_pos,
                lit_len,
                reps,
                $current_abs_end,
                min_match_len,
                |rep| {
                    if rep.match_len >= min_match_len {
                        rep_len_candidate_found = true;
                    }
                    let _ = super::bt::BtMatcher::push_candidate_ladder(
                        $out,
                        &mut best_len_for_skip,
                        rep,
                        min_match_len,
                    );
                    if rep.match_len > $profile.sufficient_match_len {
                        skip_further_match_search = true;
                    }
                    // `for_each_repcode_candidate_with_reps` caps
                    // `rep.match_len` at the per-call `tail_limit =
                    // current_abs_end - abs_pos`, so `abs_pos +
                    // rep.match_len <= current_abs_end`. The raw sum
                    // therefore stays in `usize` on every supported
                    // target.
                    if $abs_pos + rep.match_len >= $current_abs_end {
                        skip_further_match_search = true;
                    }
                },
            )
        };
        // Hash3 lookup runs only when the strategy enables it. The
        // `use_hash3` binding above is a per-monomorphisation const,
        // so non-BtUltra2 instances drop this entire block.
        if use_hash3 && !skip_further_match_search && best_len_for_skip < min_match_len {
            $self.table.update_hash3_until($abs_pos);
            // SAFETY: same umbrella for hash3_candidate.
            if let Some(h3) = unsafe {
                $self
                    .table
                    .$hash3($abs_pos, $current_abs_end, min_match_len)
            } {
                let _ = super::bt::BtMatcher::push_candidate_ladder(
                    $out,
                    &mut best_len_for_skip,
                    h3,
                    min_match_len,
                );
                if !rep_len_candidate_found
                    && (h3.match_len > $profile.sufficient_match_len
                        || $abs_pos + h3.match_len >= $current_abs_end)
                {
                    $self.table.skip_insert_until_abs = $abs_pos + 1;
                    skip_further_match_search = true;
                }
            }
        }
        if !skip_further_match_search && $bt_matchfinder {
            // SAFETY: same umbrella for bt_insert_and_collect_matches.
            unsafe {
                $self.table.$bt_insert(
                    $abs_pos,
                    $current_abs_end,
                    $profile,
                    min_match_len,
                    &mut best_len_for_skip,
                    $out,
                )
            };
        } else if !skip_further_match_search {
            $self.table.insert_position($abs_pos);
            let max_chain_depth = $profile.max_chain_depth.min($self.hc.search_depth);
            let concat = &$self.table.history[$self.table.history_start..];
            // Raw `+ 9` is safe here — see `bt_insert_step_no_rebase_body!`
            // for the full discussion of the upstream `STREAM_ABS_HEADROOM`
            // cap in `MatchTable::add_data`.
            let mut match_end_abs = $abs_pos + 9;
            if max_chain_depth > 0 {
                for (visited, candidate_abs) in $self
                    .hc
                    .chain_candidates(&$self.table, $abs_pos)
                    .into_iter()
                    .enumerate()
                {
                    if visited >= max_chain_depth {
                        break;
                    }
                    if candidate_abs == usize::MAX {
                        break;
                    }
                    if candidate_abs < $self.table.history_abs_start || candidate_abs >= $abs_pos {
                        continue;
                    }
                    let candidate_idx = candidate_abs - $self.table.history_abs_start;
                    debug_assert!(
                        $abs_pos <= $current_abs_end,
                        "HC chain walker called past current block end"
                    );
                    let tail_limit = $current_abs_end - $abs_pos;
                    let base = concat.as_ptr();
                    // SAFETY: history-relative indices; `tail_limit` bounds
                    // the scan within `concat`. `$cpl` is the kernel-specific
                    // common_prefix_len_ptr — call inlines because the
                    // surrounding wrapper carries the same target_feature.
                    let match_len =
                        unsafe { $cpl(base.add(candidate_idx), base.add(current_idx), tail_limit) };
                    if match_len < min_match_len {
                        continue;
                    }
                    let offset = $abs_pos - candidate_abs;
                    if super::bt::BtMatcher::push_candidate_ladder(
                        $out,
                        &mut best_len_for_skip,
                        MatchCandidate {
                            start: $abs_pos,
                            offset,
                            match_len,
                        },
                        min_match_len,
                    ) {
                        let candidate_end = candidate_abs + match_len;
                        if candidate_end > match_end_abs {
                            match_end_abs = candidate_end;
                        }
                    }
                    if match_len > HC_OPT_NUM || $abs_pos + match_len >= $current_abs_end {
                        break;
                    }
                }
            }
            // `match_end_abs` initialized to `abs_pos + 9`; monotonic
            // updates only ever extend it, so `match_end_abs - 8 >= 1`.
            $self.table.skip_insert_until_abs =
                $self.table.skip_insert_until_abs.max(match_end_abs - 8);
        }
        if let Some(ldm) = ldm_candidate {
            let _ = super::bt::BtMatcher::push_candidate_ladder(
                $out,
                &mut best_len_for_skip,
                ldm,
                min_match_len,
            );
        }
    }};
}

/// `hash3_candidate` body parameterized over the per-CPU
/// `common_prefix_len_ptr` symbol. The hash3 probe checks one candidate per
/// position when invoked, so the per-call ABI savings compound across the
/// segment. Crate-private (see `bt_insert_step_no_rebase_body!`).
macro_rules! hash3_candidate_body {
    (
        $table:expr,
        $abs_pos:ident,
        $current_abs_end:ident,
        $min_match_len:ident,
        $cpl:path $(,)?
    ) => {{
        if $table.hash3_log == 0 {
            return None;
        }
        let idx = $abs_pos.checked_sub($table.history_abs_start)?;
        let concat = $table.live_history();
        if idx + 4 > concat.len() {
            return None;
        }
        let hash3 = $crate::encoding::match_table::storage::MatchTable::hash_position_at(
            concat,
            idx,
            $table.hash3_log,
            3,
        );
        let entry = $table
            .hash3_table
            .get(hash3)
            .copied()
            .unwrap_or($crate::encoding::match_table::storage::HC_EMPTY);
        let candidate_abs =
            $crate::encoding::match_table::storage::MatchTable::stored_abs_position_fast(
                entry,
                $table.position_base,
                $table.index_shift,
            )?;
        if candidate_abs < $table.history_abs_start || candidate_abs >= $abs_pos {
            return None;
        }
        let offset = $abs_pos - candidate_abs;
        if offset >= $crate::encoding::bt::HC3_MAX_OFFSET {
            return None;
        }
        let candidate_idx = candidate_abs - $table.history_abs_start;
        let tail_limit = $current_abs_end.saturating_sub($abs_pos);
        let base = concat.as_ptr();
        // SAFETY: candidate/idx are within history range; tail_limit
        // bounds the scan within `concat`.
        let match_len = unsafe { $cpl(base.add(candidate_idx), base.add(idx), tail_limit) };
        (match_len >= $min_match_len).then_some($crate::encoding::opt::types::MatchCandidate {
            start: $abs_pos,
            offset,
            match_len,
        })
    }};
}
pub(crate) use hash3_candidate_body;

/// `for_each_repcode_candidate_with_reps` body parameterized over the per-CPU
/// `common_prefix_len_ptr` symbol so the per-rep prefix probe inlines under
/// the wrapper's `target_feature` umbrella instead of crossing the ABI
/// boundary through the dispatcher. Three rep probes per encoded position →
/// thousands per segment, so the per-call barrier was non-trivial.
///
/// The callback `f` runs in the wrapper's umbrella context too, so closures
/// that capture mutable state still work (FnMut). Crate-private
/// (see `bt_insert_step_no_rebase_body!`).
macro_rules! for_each_repcode_candidate_body {
    (
        $table:expr,
        $abs_pos:ident,
        $lit_len:ident,
        $reps:ident,
        $current_abs_end:ident,
        $min_match_len:ident,
        $f:ident,
        $cpl:path $(,)?
    ) => {{
        let rep_offsets: [Option<usize>; 3] = if $lit_len == 0 {
            [
                Some($reps[1] as usize),
                Some($reps[2] as usize),
                ($reps[0] > 1).then_some(($reps[0] - 1) as usize),
            ]
        } else {
            [
                Some($reps[0] as usize),
                Some($reps[1] as usize),
                Some($reps[2] as usize),
            ]
        };
        let concat = $table.live_history();
        let current_idx = $abs_pos - $table.history_abs_start;
        if current_idx + 4 > concat.len() {
            return;
        }
        let tail_limit = $current_abs_end.saturating_sub($abs_pos);
        let base = concat.as_ptr();
        let concat_len = concat.len();
        for rep in rep_offsets.into_iter().flatten() {
            if rep == 0 || rep > $abs_pos {
                continue;
            }
            let candidate_pos = $abs_pos - rep;
            if candidate_pos < $table.history_abs_start {
                continue;
            }
            let candidate_idx = candidate_pos - $table.history_abs_start;
            // Donor `ZSTD_readMINMATCH` gate (zstd_opt.c:657-674): a
            // 4-byte (3-byte when min_match_len == 3) equality probe
            // before the full prefix scan. Equivalent filtering — a
            // mismatch here means `match_len < min_match_len`, which
            // the post-scan check rejects anyway — but it skips the
            // prefix-kernel call for the common no-match case (rep
            // offsets rarely hit on low-redundancy input).
            //
            // SAFETY: `current_idx + 4 <= concat_len` (early return
            // above) and `candidate_idx < current_idx` (rep >= 1), so
            // both 4-byte reads stay inside `concat`.
            let gate_matches = unsafe {
                let cand = base.add(candidate_idx).cast::<u32>().read_unaligned();
                let cur = base.add(current_idx).cast::<u32>().read_unaligned();
                if $min_match_len == 3 {
                    // Compare the low-address 3 bytes regardless of
                    // endianness: byte-shift on LE, mask via to_le.
                    (cand.to_le() & 0x00FF_FFFF) == (cur.to_le() & 0x00FF_FFFF)
                } else {
                    cand == cur
                }
            };
            if !gate_matches {
                continue;
            }
            // SAFETY: `candidate_idx ≤ current_idx < concat_len` (since
            // candidate_pos ≤ abs_pos and we early-returned on
            // `current_idx + 4 > concat_len`). `max` clamps to the shorter
            // remaining run so neither pointer overruns `concat`.
            let max = (concat_len - candidate_idx)
                .min(concat_len - current_idx)
                .min(tail_limit);
            let match_len = unsafe { $cpl(base.add(candidate_idx), base.add(current_idx), max) };
            if match_len < $min_match_len {
                continue;
            }
            $f(MatchCandidate {
                start: $abs_pos,
                offset: rep,
                match_len,
            });
        }
    }};
}
pub(crate) use for_each_repcode_candidate_body;

/// `bt_insert_and_collect_matches` body parameterized over the per-CPU
/// `count_match_from_indices` symbol. Same shape as
/// [`bt_insert_step_no_rebase_body`] — picks up the matching kernel through
/// `$cmf` so the per-iteration vector probe inlines under the wrapper's
/// `target_feature` umbrella. Returns nothing (matches the original method).
/// Crate-private (see `bt_insert_step_no_rebase_body!`).
macro_rules! bt_insert_and_collect_matches_body {
    (
        $table:expr,
        $search_depth:expr,
        $abs_pos:ident,
        $current_abs_end:ident,
        $profile:ident,
        $min_match_len:ident,
        $best_len_for_skip:ident,
        $out:ident,
        $cmf:path $(,)?
    ) => {{
        let idx = $abs_pos - $table.history_abs_start;
        let concat = &$table.history[$table.history_start..];
        if idx + 8 > concat.len() {
            return;
        }
        debug_assert!(
            $abs_pos <= $current_abs_end,
            "BT collect called past current block end"
        );
        let tail_limit = $current_abs_end - $abs_pos;
        let hash = $crate::encoding::match_table::storage::MatchTable::hash_position_at(
            concat,
            idx,
            $table.hash_log,
            $table.search_mls,
        );
        let Some(relative_pos) = $table.relative_position($abs_pos) else {
            return;
        };
        let stored = relative_pos + 1;
        let bt_mask = $table.bt_mask();
        // See `bt_insert_step_no_rebase_body!`: saturating is needed for the
        // first BT walk of a fresh frame where `abs_pos < bt_mask`.
        let bt_low = $abs_pos.saturating_sub(bt_mask);
        let window_low = $table.window_low_abs_for_target($abs_pos);
        // Raw `+ 9` is safe here — see `bt_insert_step_no_rebase_body!`
        // for the full discussion of the upstream `STREAM_ABS_HEADROOM`
        // cap in `MatchTable::add_data`.
        let mut match_end_abs = $abs_pos + 9;
        let mut compares_left = $profile.max_chain_depth.min($search_depth);
        let mut common_length_smaller = 0usize;
        let mut common_length_larger = 0usize;
        let pair_idx = $table.bt_pair_index_for_abs($abs_pos);
        let mut smaller_slot = pair_idx;
        let mut larger_slot = pair_idx + 1;
        let mut match_stored = $table.hash_table[hash];
        $table.hash_table[hash] = stored;
        // Donor semantics: `bestLength` starts at `lengthToBeat - 1`; rep/hash3
        // probing may raise it; BT then only reports strictly longer matches.
        // `min_match_len >= HC_FORMAT_MINMATCH (3)` by configure invariant,
        // so `min_match_len - 1 >= 2` cannot underflow.
        debug_assert!(
            $min_match_len >= $crate::encoding::cost_model::HC_FORMAT_MINMATCH,
            "min_match_len must be at least HC_FORMAT_MINMATCH"
        );
        let mut best_len = (*$best_len_for_skip).max($min_match_len - 1);

        while compares_left > 0 {
            let Some(candidate_abs) =
                $crate::encoding::match_table::storage::MatchTable::stored_abs_position_fast(
                    match_stored,
                    $table.position_base,
                    $table.index_shift,
                )
            else {
                break;
            };
            if candidate_abs < window_low || candidate_abs >= $abs_pos {
                break;
            }
            compares_left -= 1;

            let next_pair_idx = $table.bt_pair_index_for_abs(candidate_abs);
            let next_smaller = $table.chain_table[next_pair_idx];
            let next_larger = $table.chain_table[next_pair_idx + 1];
            let seed_len = common_length_smaller.min(common_length_larger);
            let candidate_idx = candidate_abs - $table.history_abs_start;
            // SAFETY: BT walk invariant — `candidate_idx + tail_limit ≤
            // concat.len()`.
            let match_len = unsafe { $cmf(concat, idx, candidate_idx, tail_limit, seed_len) };

            if match_len > best_len {
                let offset = $abs_pos - candidate_abs;
                let accepted = $crate::encoding::bt::BtMatcher::push_candidate_ladder(
                    $out,
                    $best_len_for_skip,
                    $crate::encoding::opt::types::MatchCandidate {
                        start: $abs_pos,
                        offset,
                        match_len,
                    },
                    $min_match_len,
                );
                if accepted {
                    best_len = match_len;
                    // BT walker invariants: `candidate_abs < abs_pos`
                    // and `match_len <= tail_limit = current_abs_end -
                    // abs_pos`. So `candidate_abs + match_len <
                    // abs_pos + tail_limit = current_abs_end`, which
                    // fits in `usize` on every supported target (32-bit
                    // i686 included) — the addition stays within the
                    // current block.
                    let candidate_end = candidate_abs + match_len;
                    if candidate_end > match_end_abs {
                        match_end_abs = candidate_end;
                    }
                    if match_len >= tail_limit
                        || match_len > $crate::encoding::cost_model::HC_OPT_NUM
                    {
                        break;
                    }
                }
            }

            if match_len >= tail_limit {
                break;
            }

            let candidate_next = candidate_idx + match_len;
            let current_next = idx + match_len;
            if concat[candidate_next] < concat[current_next] {
                $table.chain_table[smaller_slot] = match_stored;
                common_length_smaller = match_len;
                if candidate_abs <= bt_low {
                    smaller_slot = usize::MAX;
                    break;
                }
                smaller_slot = next_pair_idx + 1;
                match_stored = next_larger;
            } else {
                $table.chain_table[larger_slot] = match_stored;
                common_length_larger = match_len;
                if candidate_abs <= bt_low {
                    larger_slot = usize::MAX;
                    break;
                }
                larger_slot = next_pair_idx;
                match_stored = next_smaller;
            }
        }

        if smaller_slot != usize::MAX {
            $table.chain_table[smaller_slot] = $crate::encoding::match_table::storage::HC_EMPTY;
        }
        if larger_slot != usize::MAX {
            $table.chain_table[larger_slot] = $crate::encoding::match_table::storage::HC_EMPTY;
        }

        // Dict dual-probe (donor `ZSTD_dictMatchState`, zstd_opt.c:777-813):
        // after the live tree, descend the immutable dictionary BINARY TREE
        // (built in `prime_dms_bt`) with its OWN compare budget and push any
        // dict match longer than the live best into the ladder. The DUBT
        // descent reaches the longest dict match efficiently (a hash-chain
        // surfaced only the few same-bucket candidates and left most of the
        // dict savings unrealised at btlazy2 / btopt). Dict positions are
        // dictionary-relative concat indices in `[0, region)`, pinned at the
        // front of history, so a dict candidate at `dict_idx` sits at offset
        // `idx - dict_idx` (no donor `dmsIndexDelta`). The optimal parser
        // prices these (its DP lookahead values the repcode chain a dict match
        // seeds); the greedy/lazy parser commits the longest.
        if let Some(dms) = $table.dms.table() {
            let region = $table.dms.region_len();
            let dh = $crate::encoding::match_table::storage::MatchTable::hash_position_at(
                concat,
                idx,
                dms.hash_log,
                dms.mls,
            );
            let mut dcur = dms.hash_table[dh];
            // DUBT seed lengths: bytes already known common on each side, so
            // `$cmf` resumes from there (donor commonLengthSmaller/Larger).
            let mut common_smaller = 0usize;
            let mut common_larger = 0usize;
            let mut dms_compares = $profile.max_chain_depth.min($search_depth);
            while dms_compares > 0 && dcur != $crate::encoding::match_table::storage::HC_EMPTY {
                let dict_idx = (dcur - 1) as usize;
                // The dict tree holds only dict positions (`< region <= idx`).
                if dict_idx >= region || dict_idx >= idx {
                    break;
                }
                dms_compares -= 1;
                let pair = 2 * dict_idx;
                let seed = common_smaller.min(common_larger);
                // SAFETY: `dict_idx < idx` and `idx + tail_limit <=
                // concat.len()` (checked at entry); same umbrella as the live
                // walk's `$cmf`. `seed <= prior match_len <= tail_limit`.
                let match_len = unsafe { $cmf(concat, idx, dict_idx, tail_limit, seed) };
                if match_len > best_len {
                    let offset = idx - dict_idx;
                    let accepted = $crate::encoding::bt::BtMatcher::push_candidate_ladder(
                        $out,
                        $best_len_for_skip,
                        $crate::encoding::opt::types::MatchCandidate {
                            start: $abs_pos,
                            offset,
                            match_len,
                        },
                        $min_match_len,
                    );
                    if accepted {
                        best_len = match_len;
                        let candidate_end = $abs_pos + match_len;
                        if candidate_end > match_end_abs {
                            match_end_abs = candidate_end;
                        }
                        if match_len > $crate::encoding::cost_model::HC_OPT_NUM {
                            break;
                        }
                    }
                }
                // Match reached the block tail: can't order the pair (donor
                // `ip+matchLength == iLimit`), and indexing `concat[idx +
                // match_len]` below would step past the searchable region.
                if match_len >= tail_limit {
                    break;
                }
                // Descend the DUBT (donor zstd_opt.c:806-811): dict candidate
                // smaller than input → its larger child is closer to `idx`.
                if concat[dict_idx + match_len] < concat[idx + match_len] {
                    common_smaller = match_len;
                    dcur = dms.chain_table[pair + 1];
                } else {
                    common_larger = match_len;
                    dcur = dms.chain_table[pair];
                }
            }
        }

        // `match_end_abs >= abs_pos + 9 >= 9` (initialized and monotonic),
        // so `match_end_abs - 8 >= 1` cannot underflow.
        $table.skip_insert_until_abs = match_end_abs - 8;
    }};
}
pub(crate) use bt_insert_and_collect_matches_body;

impl HcMatchGenerator {
    /// Heap bytes this generator owns: the shared match table plus the BT
    /// backend's optimal-parser / LDM scratch (the HC knobs are inline).
    fn heap_size(&self) -> usize {
        self.table.heap_size() + self.backend.heap_size()
    }

    fn should_run_btultra2_seed_pass<S: super::strategy::Strategy>(
        &self,
        current_len: usize,
    ) -> bool {
        // The in-block two-pass dynamic-stats seed (`initStats_ultra`)
        // is btultra2-only. `TWO_PASS_SEED` is `false` for every other
        // strategy — including btultra, which now shares the hash3
        // short-match probe but stays single-pass — so the seed call and
        // its body drop at codegen time for all non-btultra2 kernels.
        if !S::TWO_PASS_SEED {
            return false;
        }
        let HcBackend::Bt(bt) = &self.backend else {
            return false;
        };
        bt.opt_state.lit_length_sum == 0
            && bt.opt_state.dictionary_seed.is_none()
            && !self.table.dictionary_primed_for_frame
            && bt.ldm_sequences.is_empty()
            && self.table.window_size == current_len
            && self.table.history_abs_start == 0
            && self.table.chunk_lens.len() == 1
            && current_len > HC_PREDEF_THRESHOLD
    }

    fn new(max_window_size: usize) -> Self {
        Self {
            table: super::match_table::storage::MatchTable::new(max_window_size),
            hc: super::hc::HcMatcher::new(2, HC_SEARCH_DEPTH, HC_TARGET_LEN),
            // Default to the zero-sized HC backend; `configure()` swaps
            // in a `BtMatcher` only when an optimal strategy lands.
            backend: HcBackend::Hc,
            // Lazy is the per-construct default — every production
            // caller calls `configure()` before the first encode and
            // overwrites this. Tests that drive `HcMatchGenerator`
            // without calling `configure()` end up in the
            // `start_matching_lazy` arm of the test dispatcher, which
            // matches the previous default behaviour.
            strategy_tag: super::strategy::StrategyTag::Lazy,
        }
    }

    fn configure(&mut self, config: HcConfig, tag: super::strategy::StrategyTag, window_log: u8) {
        use super::strategy::StrategyTag;
        // Mirror the driver-resolved strategy tag so the
        // `#[cfg(test)] start_matching` dispatcher can route
        // BtOpt / BtUltra / BtUltra2 to distinct monomorphisations.
        self.strategy_tag = tag;
        let is_btultra2 = tag == StrategyTag::BtUltra2;
        let uses_bt = matches!(
            tag,
            StrategyTag::Btlazy2
                | StrategyTag::BtOpt
                | StrategyTag::BtUltra
                | StrategyTag::BtUltra2
        );
        // btultra and btultra2 both run the mls=3 hash3 short-match probe
        // (clevels.h minMatch 3). The `is_btultra2` flag below stays
        // exclusive to btultra2 because it tweaks the BT rebase boundary,
        // not match finding.
        let wants_hash3 = matches!(tag, StrategyTag::BtUltra | StrategyTag::BtUltra2);
        let next_hash3_log = if wants_hash3 {
            HC3_HASH_LOG.min(window_log as usize)
        } else {
            0
        };
        let resize = self.table.hash_log != config.hash_log
            || self.table.chain_log != config.chain_log
            || self.table.hash3_log != next_hash3_log;
        self.table.hash_log = config.hash_log;
        self.table.chain_log = config.chain_log;
        self.table.hash3_log = next_hash3_log;
        self.hc.search_depth = if uses_bt {
            config.search_depth
        } else {
            config.search_depth.min(MAX_HC_SEARCH_DEPTH)
        };
        self.hc.target_len = config.target_len;
        // Mirror strategy-derived flags + HC search depth onto MatchTable
        // so the BT walker and rebase machinery can read them directly
        // without dispatching back through HcMatchGenerator.
        self.table.search_depth = self.hc.search_depth;
        self.table.is_btultra2 = is_btultra2;
        self.table.uses_bt = uses_bt;
        // BT finder hash width, donor `mls = BOUNDED(4, cParams.minMatch, 6)`,
        // carried explicitly in the level config so a `target_length` override
        // cannot silently flip the finder between 5- and 4-byte hashing. Only
        // the BT body reads it; HC/lazy levels leave it at 4. clevels.h
        // (srcSize > 256 KiB tier): btlazy2 L13-15 + btopt L16 are minMatch=5,
        // btopt L17 is minMatch=4, btultra/btultra2 are minMatch=3 (4-byte main
        // hash + the hash3 short-match probe).
        self.table.search_mls = config.search_mls;
        // Stage D: promote the backend discriminator. HC modes drop the
        // BT scratch buffers entirely; switching back into a BT mode
        // allocates a fresh `BtMatcher` on demand.
        match (&self.backend, self.table.uses_bt) {
            (HcBackend::Hc, true) => {
                self.backend = HcBackend::Bt(alloc::boxed::Box::new(super::bt::BtMatcher::new()));
            }
            (HcBackend::Bt(_), false) => {
                self.backend = HcBackend::Hc;
            }
            _ => {}
        }
        if resize && !self.table.hash_table.is_empty() {
            // Force reallocation on next ensure_tables() call.
            self.table.hash_table.clear();
            self.table.hash3_table.clear();
            self.table.chain_table.clear();
        }
    }

    fn seed_dictionary_entropy(
        &mut self,
        huff: Option<&crate::huff0::huff0_encoder::HuffmanTable>,
        ll: Option<&crate::fse::fse_encoder::FSETable>,
        ml: Option<&crate::fse::fse_encoder::FSETable>,
        of: Option<&crate::fse::fse_encoder::FSETable>,
    ) {
        if let HcBackend::Bt(bt) = &mut self.backend {
            bt.opt_state.seed_dictionary_entropy(huff, ll, ml, of);
        }
    }

    /// Install (or clear) the long-distance-match producer (#27). Only
    /// the BT backend owns an `ldm_producer` slot; on the HC (lazy)
    /// backend the producer is dropped because there is no optimal-parser
    /// candidate buffer to seed. Call after [`Self::reset`].
    #[cfg(feature = "hash")]
    fn set_ldm_producer(&mut self, producer: Option<super::ldm::LdmProducer>) {
        if let HcBackend::Bt(bt) = &mut self.backend {
            bt.ldm_producer = producer;
        }
    }

    /// Move the LDM producer out of the BT backend, leaving `None`. Used by the
    /// dictionary snapshot path: the producer carries no dictionary state (LDM
    /// is not dict-primed; its hash table is empty at capture), so it is not
    /// retained in the snapshot — the working frame's freshly-reset producer is
    /// reinstated on restore instead.
    #[cfg(feature = "hash")]
    fn take_ldm_producer(&mut self) -> Option<super::ldm::LdmProducer> {
        if let HcBackend::Bt(bt) = &mut self.backend {
            bt.ldm_producer.take()
        } else {
            None
        }
    }

    fn reset(&mut self, reuse_space: impl FnMut(Vec<u8>)) {
        self.table.reset(reuse_space);
        if let HcBackend::Bt(bt) = &mut self.backend {
            bt.reset();
        }
    }

    /// Backfill positions from the tail of the previous slice that couldn't be
    /// hashed at the time (insert_position needs 4 bytes of lookahead).
    fn skip_matching(&mut self, incompressible_hint: Option<bool>) {
        self.table.skip_matching(incompressible_hint);
    }

    /// Runtime-dispatched entry kept only for in-crate tests. Production
    /// callers reach the inner loops through
    /// [`Self::start_matching_strategy`] / [`MatchGeneratorDriver::compress_block`]
    /// which pick the lazy / optimal arm from `S::USE_BT` at
    /// monomorphisation time.
    #[cfg(test)]
    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        use super::strategy::{self, StrategyTag};
        // Dispatch on the mirrored `strategy_tag` so each test runs
        // under the same monomorphisation production would pick.
        // `BtOpt` / `BtUltra` / `BtUltra2` remain distinct here even
        // though `table.uses_bt` / `is_btultra2` alone can't separate
        // BtOpt from BtUltra.
        match self.strategy_tag {
            StrategyTag::Fast | StrategyTag::Dfast | StrategyTag::Greedy | StrategyTag::Lazy => {
                self.start_matching_lazy(&mut handle_sequence)
            }
            StrategyTag::Btlazy2 => self.start_matching_btlazy2(&mut handle_sequence),
            StrategyTag::BtOpt => {
                self.start_matching_optimal::<strategy::BtOpt>(&mut handle_sequence)
            }
            StrategyTag::BtUltra => {
                self.start_matching_optimal::<strategy::BtUltra>(&mut handle_sequence)
            }
            StrategyTag::BtUltra2 => {
                self.start_matching_optimal::<strategy::BtUltra2>(&mut handle_sequence)
            }
        }
    }

    /// Strategy-aware entry point used by
    /// [`MatchGeneratorDriver::compress_block`]. Branches on
    /// `S::USE_BT` — a compile-time `const` — so each
    /// monomorphisation keeps exactly one arm: `Lazy` /
    /// `Fast` / `Dfast` / `Greedy` see only `start_matching_lazy`,
    /// `BtOpt` / `BtUltra` / `BtUltra2` see only
    /// `start_matching_optimal`. The inherent test-only
    /// [`HcMatchGenerator::start_matching`] reaches the same arms by
    /// runtime-matching on `self.strategy_tag` (the parse-mode field
    /// has been removed); production never invokes that path.
    pub(crate) fn start_matching_strategy<S: super::strategy::Strategy>(
        &mut self,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        debug_assert_eq!(
            self.table.uses_bt,
            S::USE_BT,
            "Strategy::USE_BT disagrees with runtime table.uses_bt at HC dispatch"
        );
        if S::USE_BT {
            self.start_matching_optimal::<S>(handle_sequence)
        } else {
            self.start_matching_lazy(handle_sequence)
        }
    }

    pub(crate) fn start_matching_lazy(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        self.table.ensure_tables();

        let current_len = *self.table.chunk_lens.back().unwrap();
        if current_len == 0 {
            return;
        }
        // The current block is the tail of `history`. Hoist it as a raw
        // slice (like `start_matching_optimal`): the routine mutates the
        // hash/chain tables + `offset_hist` but never reallocates
        // `history`, so the slice stays valid and we avoid re-borrowing
        // `self.table` (which would conflict with the `offset_hist` write).
        let current_ptr = self.table.get_last_space().as_ptr();
        let current: &[u8] = unsafe { core::slice::from_raw_parts(current_ptr, current_len) };

        let current_abs_start = self.table.history_abs_start + self.table.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        self.table
            .backfill_boundary_positions(current_abs_start, current_abs_end);

        let mut pos = 0usize;
        let mut literals_start = 0usize;
        while pos + HC_MIN_MATCH_LEN <= current_len {
            let abs_pos = current_abs_start + pos;
            let lit_len = pos - literals_start;

            let best = self.hc.find_best_match(&self.table, abs_pos, lit_len);
            if let Some(candidate) = self.hc.pick_lazy_match(&self.table, abs_pos, lit_len, best) {
                self.table
                    .insert_match_span(abs_pos, candidate.start + candidate.match_len);
                let start = candidate.start - current_abs_start;
                let literals = &current[literals_start..start];
                handle_sequence(Sequence::Triple {
                    literals,
                    offset: candidate.offset,
                    match_len: candidate.match_len,
                });
                let _ = encode_offset_with_history(
                    candidate.offset as u32,
                    literals.len() as u32,
                    &mut self.table.offset_hist,
                );
                pos = start + candidate.match_len;
                literals_start = pos;
            } else {
                self.table.insert_position(abs_pos);
                pos += 1;
            }
        }

        // Insert remaining hashable positions in the tail (the matching loop
        // stops at HC_MIN_MATCH_LEN but insert_position only needs 4 bytes).
        while pos + 4 <= current_len {
            self.table.insert_position(current_abs_start + pos);
            pos += 1;
        }

        if literals_start < current_len {
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    /// Donor `ZSTD_btlazy2` (levels 13-15): binary-tree match finder with a
    /// greedy/lazy parse. Bare dispatcher — resolves the runtime tier ONCE
    /// per block via `select_kernel()` and calls the matching
    /// `start_matching_btlazy2_<kernel>` wrapper, so the per-position BT
    /// collect runs under a single `#[target_feature]` umbrella (mirrors
    /// `build_optimal_plan_impl`). See `start_matching_btlazy2_body!` for the
    /// shared loop.
    fn start_matching_btlazy2(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.start_matching_btlazy2_neon(&mut handle_sequence)
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.start_matching_btlazy2_avx2_bmi2(&mut handle_sequence)
                },
                FastpathKernel::Sse42 => unsafe {
                    self.start_matching_btlazy2_sse42(&mut handle_sequence)
                },
                FastpathKernel::Scalar => self.start_matching_btlazy2_scalar(&mut handle_sequence),
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.start_matching_btlazy2_scalar(&mut handle_sequence)
        }
    }

    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    unsafe fn start_matching_btlazy2_neon(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_btlazy2_body!(
            self,
            handle_sequence,
            collect_optimal_candidates_initialized_neon,
            crate::encoding::fastpath::neon::count_match_from_indices
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    unsafe fn start_matching_btlazy2_sse42(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_btlazy2_body!(
            self,
            handle_sequence,
            collect_optimal_candidates_initialized_sse42,
            crate::encoding::fastpath::sse42::count_match_from_indices
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    unsafe fn start_matching_btlazy2_avx2_bmi2(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_btlazy2_body!(
            self,
            handle_sequence,
            collect_optimal_candidates_initialized_avx2_bmi2,
            crate::encoding::fastpath::avx2_bmi2::count_match_from_indices
        )
    }

    // Scalar wrapper: no `#[target_feature]`; `$collect` (the scalar collect)
    // is a safe fn, so the body macro's `unsafe` block is inert here. Same cfg
    // as `collect_optimal_candidates_initialized_scalar` (absent on
    // aarch64-little, where NEON is the baseline tier).
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    #[allow(unused_unsafe)]
    fn start_matching_btlazy2_scalar(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_btlazy2_body!(
            self,
            handle_sequence,
            collect_optimal_candidates_initialized_scalar,
            crate::encoding::fastpath::scalar::count_match_from_indices
        )
    }

    fn start_matching_optimal<S: super::strategy::Strategy>(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        self.table.ensure_tables();
        let current_len = *self.table.chunk_lens.back().unwrap();
        if current_len == 0 {
            return;
        }
        let current_ptr = self.table.get_last_space().as_ptr();
        // `start_matching_optimal()` mutates tables/state but never mutates or
        // reallocates `self.table.history`, so this tail slice remains valid for
        // the duration of the routine and avoids cloning the full block.
        let current = unsafe { core::slice::from_raw_parts(current_ptr, current_len) };

        let current_abs_start = self.table.history_abs_start + self.table.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        self.table
            .apply_limited_update_after_long_match(current_abs_start);
        let hash3_start_cursor = self
            .table
            .skip_insert_until_abs
            .max(self.table.history_abs_start);
        self.table
            .backfill_boundary_positions(current_abs_start, current_abs_end);
        self.table.next_to_update3 = hash3_start_cursor;
        // Borrow split: `prepare_ldm_candidates` needs immutable
        // access to the live history (the post-`history_start`
        // slice of `self.table.history`) while it mutates the LDM
        // bucket table owned by `self.backend.bt_mut()`. Both live
        // in disjoint fields of `Self`, so we capture the slice +
        // its base before reaching for `bt_mut()`.
        //
        // The producer operates in absolute stream coordinates
        // throughout; `live_history[0]` corresponds to absolute
        // `history_abs_start` (donor `base + dictLimit`), and the
        // abs→slice translation happens inside the producer at
        // each `live_history[..]` access. Passing the full
        // `history` Vec would index into the dead prefix (the
        // bytes already retired past `history_start`).
        let live_history = self.table.live_history();
        let history_abs_start = self.table.history_abs_start;
        self.backend.bt_mut().prepare_ldm_candidates(
            live_history,
            history_abs_start,
            current_abs_start,
            current_len,
        );

        if self.should_run_btultra2_seed_pass::<S>(current_len) {
            self.run_btultra2_seed_pass(current, current_abs_start, current_len);
        }

        // Const-generic profile selection: every field is folded from
        // S's associated consts (MAX_CHAIN_DEPTH /
        // SUFFICIENT_MATCH_LEN / ACCURATE_PRICE / FAVOR_SMALL_OFFSETS),
        // so the optimiser produces the literal at codegen time
        // without a runtime match.
        let profile = HcOptimalCostProfile::const_for_strategy::<S>();
        let mut opt_state =
            core::mem::replace(&mut self.backend.bt_mut().opt_state, HcOptState::new());
        opt_state.rescale_freqs(current, profile);
        let mut best_plan = core::mem::take(&mut self.backend.bt_mut().opt_segment_plan_scratch);
        best_plan.clear();
        let mut plan_reps = self.table.offset_hist;
        let (mut cursor, mut plan_litlen) = self
            .table
            .donor_opt_start_cursor_and_litlen(current_abs_start);
        let mut plan_literals_cursor = 0usize;
        let match_loop_limit = current_len.saturating_sub(8);
        while cursor < match_loop_limit {
            let remaining_len = current_len - cursor;
            let segment_abs_start = current_abs_start + cursor;
            let segment_start = best_plan.len();
            let (_, end_reps, end_litlen, consumed_len) = self.build_optimal_plan::<S>(
                &current[cursor..],
                segment_abs_start,
                remaining_len,
                HcOptimalPlanState {
                    block_offset: cursor,
                    reps: plan_reps,
                    litlen: plan_litlen,
                    profile,
                },
                &opt_state,
                &mut best_plan,
            );
            BtMatcher::update_plan_stats_segment(
                current,
                current_len,
                &best_plan[segment_start..],
                &mut plan_literals_cursor,
                &mut plan_reps,
                &mut opt_state,
                profile.accurate,
            );
            plan_reps = end_reps;
            plan_litlen = end_litlen;
            cursor += consumed_len;
        }

        self.table
            .emit_optimal_plan(current_len, &best_plan, &mut handle_sequence);
        best_plan.clear();
        self.backend.bt_mut().opt_segment_plan_scratch = best_plan;
        self.backend.bt_mut().opt_state = opt_state;
    }

    fn run_btultra2_seed_pass(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
    ) {
        // The seed pass is BtUltra2-exclusive by name (the only
        // caller is `should_run_btultra2_seed_pass`), so pin `S` to
        // `BtUltra2` for both the cost-profile lookup and the
        // `build_optimal_plan::<S>` call below.
        type S = super::strategy::BtUltra2;
        let seed_profile = HcOptimalCostProfile::const_for_strategy::<S>();
        let mut opt_state =
            core::mem::replace(&mut self.backend.bt_mut().opt_state, HcOptState::new());
        opt_state.rescale_freqs(current, seed_profile);
        let mut seed_reps = self.table.offset_hist;
        let (mut cursor, mut seed_litlen) = self
            .table
            .donor_opt_start_cursor_and_litlen(current_abs_start);
        let mut seed_literals_cursor = 0usize;
        let mut seed_plan = core::mem::take(&mut self.backend.bt_mut().opt_seed_plan_scratch);
        seed_plan.clear();
        let match_loop_limit = current_len.saturating_sub(8);
        while cursor < match_loop_limit {
            let remaining_len = current_len - cursor;
            let segment_abs_start = current_abs_start + cursor;
            let segment_start = seed_plan.len();
            let (_, end_reps, end_litlen, consumed_len) = self.build_optimal_plan::<S>(
                &current[cursor..],
                segment_abs_start,
                remaining_len,
                HcOptimalPlanState {
                    block_offset: cursor,
                    reps: seed_reps,
                    litlen: seed_litlen,
                    profile: seed_profile,
                },
                &opt_state,
                &mut seed_plan,
            );
            BtMatcher::update_plan_stats_segment(
                current,
                current_len,
                &seed_plan[segment_start..],
                &mut seed_literals_cursor,
                &mut seed_reps,
                &mut opt_state,
                seed_profile.accurate,
            );
            seed_plan.truncate(segment_start);
            seed_reps = end_reps;
            seed_litlen = end_litlen;
            cursor += consumed_len;
        }
        seed_plan.clear();
        self.backend.bt_mut().opt_seed_plan_scratch = seed_plan;
        self.backend.bt_mut().opt_state = opt_state;

        // Donor initStats_ultra keeps the collected entropy statistics but
        // invalidates the first-pass matchfinder history before the real pass.
        self.table.position_base = self.table.history_abs_start;
        self.table.index_shift = current_len;
        self.table.next_to_update3 = current_abs_start;
        self.table.skip_insert_until_abs = current_abs_start;
        // Donor `ZSTD_initStats_ultra()` invalidates the first scan by moving
        // `window.base` back by `srcSize`, making the real pass start at
        // `curr == srcSize` instead of 0. Position 0 is therefore a valid
        // table entry in the second pass even though raw C tables reserve
        // value 0 as empty during an unshifted first pass.
        self.table.allow_zero_relative_position = true;
    }

    fn build_optimal_plan<S: super::strategy::Strategy>(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        debug_assert!(S::USE_BT, "build_optimal_plan called on non-BT strategy");
        debug_assert_eq!(initial_state.profile.accurate, S::ACCURATE_PRICE);
        debug_assert_eq!(
            initial_state.profile.favor_small_offsets,
            S::FAVOR_SMALL_OFFSETS
        );
        // `S::ACCURATE_PRICE` / `S::FAVOR_SMALL_OFFSETS` cannot appear
        // as const-generic arguments yet (`generic_const_exprs` is
        // still unstable), so dispatch over a 4-arm match — but on the
        // strategy's ASSOCIATED CONSTS, not the runtime profile (the
        // `debug_assert_eq`s above pin the runtime profile to those
        // consts). A const scrutinee folds the three dead arms at
        // monomorphisation; matching the runtime profile instead kept
        // all four `#[inline(always)]` DP bodies (~16 KB each) alive in
        // EVERY `S` instantiation — ~360 KB of the wasm payload.
        match (S::ACCURATE_PRICE, S::FAVOR_SMALL_OFFSETS) {
            (true, false) => self.build_optimal_plan_impl::<S, true, false>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            ),
            (true, true) => self.build_optimal_plan_impl::<S, true, true>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            ),
            (false, false) => self.build_optimal_plan_impl::<S, false, false>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            ),
            (false, true) => self.build_optimal_plan_impl::<S, false, true>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            ),
        }
    }

    /// Cross-platform DP entry. Picks the kernel-specific variant so the
    /// entire optimal-parser DP body (per-position match gathering, price
    /// updates, traceback) runs inside a single `target_feature` umbrella
    /// alongside the per-position `collect_optimal_candidates_initialized_
    /// <kernel>`. This eliminates the final ABI barrier on the hot per-
    /// position match-collection call — the level22 critical path is now
    /// one straight-line inline chain from DP body down through BT walk
    /// and match-length probes.
    #[inline(always)]
    fn build_optimal_plan_impl<
        S: super::strategy::Strategy,
        const ACCURATE_PRICE: bool,
        const FAVOR_SMALL_OFFSETS: bool,
    >(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.build_optimal_plan_impl_neon::<S, ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            )
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.build_optimal_plan_impl_avx2_bmi2::<S, ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                        current,
                        current_abs_start,
                        current_len,
                        initial_state,
                        stats,
                        out,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.build_optimal_plan_impl_sse42::<S, ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                        current,
                        current_abs_start,
                        current_len,
                        initial_state,
                        stats,
                        out,
                    )
                },
                FastpathKernel::Scalar => self
                    .build_optimal_plan_impl_scalar::<S, ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                        current,
                        current_abs_start,
                        current_len,
                        initial_state,
                        stats,
                        out,
                    ),
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.build_optimal_plan_impl_scalar::<S, ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            )
        }
    }

    /// NEON-umbrella DP body. Inlines
    /// `collect_optimal_candidates_initialized_neon` (and its entire
    /// per-position pipeline) directly into the DP loop.
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    unsafe fn build_optimal_plan_impl_neon<
        S: super::strategy::Strategy,
        const ACCURATE_PRICE: bool,
        const FAVOR_SMALL_OFFSETS: bool,
    >(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        build_optimal_plan_impl_body!(
            self,
            S,
            current,
            current_abs_start,
            current_len,
            initial_state,
            stats,
            out,
            collect_optimal_candidates_initialized_neon,
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    unsafe fn build_optimal_plan_impl_sse42<
        S: super::strategy::Strategy,
        const ACCURATE_PRICE: bool,
        const FAVOR_SMALL_OFFSETS: bool,
    >(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        build_optimal_plan_impl_body!(
            self,
            S,
            current,
            current_abs_start,
            current_len,
            initial_state,
            stats,
            out,
            collect_optimal_candidates_initialized_sse42,
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    unsafe fn build_optimal_plan_impl_avx2_bmi2<
        S: super::strategy::Strategy,
        const ACCURATE_PRICE: bool,
        const FAVOR_SMALL_OFFSETS: bool,
    >(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        build_optimal_plan_impl_body!(
            self,
            S,
            current,
            current_abs_start,
            current_len,
            initial_state,
            stats,
            out,
            collect_optimal_candidates_initialized_avx2_bmi2,
        )
    }

    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    // Body macros wrap callees in `unsafe { }` for the NEON/AVX/SSE
    // variants where callees are `unsafe fn`. The scalar wrappers route
    // through safe fns, so those blocks are redundant on this path.
    #[allow(unused_unsafe)]
    fn build_optimal_plan_impl_scalar<
        S: super::strategy::Strategy,
        const ACCURATE_PRICE: bool,
        const FAVOR_SMALL_OFFSETS: bool,
    >(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        build_optimal_plan_impl_body!(
            self,
            S,
            current,
            current_abs_start,
            current_len,
            initial_state,
            stats,
            out,
            collect_optimal_candidates_initialized_scalar,
        )
    }

    #[cfg(test)]
    fn collect_optimal_candidates(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        use super::strategy::{self, StrategyTag};
        self.table.ensure_tables();
        // Dispatch purely from `self.strategy_tag` (set by
        // `configure()`). Tests must configure the matcher the same
        // way production does — wiring up `table.hash3_log` directly
        // without setting a matching `strategy_tag` is no longer
        // allowed.
        match self.strategy_tag {
            StrategyTag::BtUltra2 => self
                .collect_optimal_candidates_initialized::<strategy::BtUltra2, true>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    query,
                    out,
                ),
            StrategyTag::BtUltra => self
                .collect_optimal_candidates_initialized::<strategy::BtUltra, true>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    query,
                    out,
                ),
            StrategyTag::Btlazy2 => self
                .collect_optimal_candidates_initialized::<strategy::Btlazy2, true>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    query,
                    out,
                ),
            StrategyTag::BtOpt => self
                .collect_optimal_candidates_initialized::<strategy::BtOpt, true>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    query,
                    out,
                ),
            StrategyTag::Fast | StrategyTag::Dfast | StrategyTag::Greedy | StrategyTag::Lazy => {
                self.collect_optimal_candidates_initialized::<strategy::Lazy, false>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    query,
                    out,
                )
            }
        }
    }

    /// Cross-platform entry. Picks the kernel-specific variant so the per-
    /// position pipeline (BT-tree fill, rep probing, hash3 probing, BT
    /// collect / HC chain walk) runs inside a single `target_feature`
    /// umbrella — all inner SIMD probes inline without ABI barriers.
    ///
    /// The on-encode hot path bypasses this dispatcher: `build_optimal_plan_impl_<kernel>`
    /// calls the matching `_<kernel>` variant directly. This entry is kept
    /// for the cfg(test)-only `collect_optimal_candidates` shim and any
    /// future caller that isn't already inside a kernel umbrella.
    #[allow(dead_code)]
    #[inline(always)]
    fn collect_optimal_candidates_initialized<
        S: super::strategy::Strategy,
        const USE_BT_MATCHFINDER: bool,
    >(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.collect_optimal_candidates_initialized_neon::<S, USE_BT_MATCHFINDER>(
                abs_pos,
                current_abs_end,
                profile,
                query,
                out,
            )
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.collect_optimal_candidates_initialized_avx2_bmi2::<S, USE_BT_MATCHFINDER>(
                        abs_pos,
                        current_abs_end,
                        profile,
                        query,
                        out,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.collect_optimal_candidates_initialized_sse42::<S, USE_BT_MATCHFINDER>(
                        abs_pos,
                        current_abs_end,
                        profile,
                        query,
                        out,
                    )
                },
                FastpathKernel::Scalar => self
                    .collect_optimal_candidates_initialized_scalar::<S, USE_BT_MATCHFINDER>(
                        abs_pos,
                        current_abs_end,
                        profile,
                        query,
                        out,
                    ),
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.collect_optimal_candidates_initialized_scalar::<S, USE_BT_MATCHFINDER>(
                abs_pos,
                current_abs_end,
                profile,
                query,
                out,
            )
        }
    }

    /// NEON-umbrella variant. Every inner helper (`bt_update_tree_until_neon`,
    /// `for_each_repcode_candidate_with_reps_neon`, `hash3_candidate_neon`,
    /// `bt_insert_and_collect_matches_neon`, `fastpath::neon::
    /// common_prefix_len_ptr`) shares the NEON umbrella so the per-position
    /// pipeline executes as a single straight-line inline sequence.
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    unsafe fn collect_optimal_candidates_initialized_neon<
        S: super::strategy::Strategy,
        const USE_BT_MATCHFINDER: bool,
    >(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
            S,
            abs_pos,
            current_abs_end,
            profile,
            query,
            out,
            USE_BT_MATCHFINDER,
            bt_update_tree_until_neon,
            bt_insert_and_collect_matches_neon,
            for_each_repcode_candidate_with_reps_neon,
            hash3_candidate_neon,
            crate::encoding::fastpath::neon::common_prefix_len_ptr,
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    unsafe fn collect_optimal_candidates_initialized_sse42<
        S: super::strategy::Strategy,
        const USE_BT_MATCHFINDER: bool,
    >(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
            S,
            abs_pos,
            current_abs_end,
            profile,
            query,
            out,
            USE_BT_MATCHFINDER,
            bt_update_tree_until_sse42,
            bt_insert_and_collect_matches_sse42,
            for_each_repcode_candidate_with_reps_sse42,
            hash3_candidate_sse42,
            crate::encoding::fastpath::sse42::common_prefix_len_ptr,
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    unsafe fn collect_optimal_candidates_initialized_avx2_bmi2<
        S: super::strategy::Strategy,
        const USE_BT_MATCHFINDER: bool,
    >(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
            S,
            abs_pos,
            current_abs_end,
            profile,
            query,
            out,
            USE_BT_MATCHFINDER,
            bt_update_tree_until_avx2_bmi2,
            bt_insert_and_collect_matches_avx2_bmi2,
            for_each_repcode_candidate_with_reps_avx2_bmi2,
            hash3_candidate_avx2_bmi2,
            crate::encoding::fastpath::avx2_bmi2::common_prefix_len_ptr,
        )
    }

    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    // Macro emits `unsafe { }` wrappers for NEON/AVX/SSE variants; scalar
    // callees are safe so the blocks are redundant here only.
    #[allow(unused_unsafe)]
    fn collect_optimal_candidates_initialized_scalar<
        S: super::strategy::Strategy,
        const USE_BT_MATCHFINDER: bool,
    >(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
            S,
            abs_pos,
            current_abs_end,
            profile,
            query,
            out,
            USE_BT_MATCHFINDER,
            bt_update_tree_until_scalar,
            bt_insert_and_collect_matches_scalar,
            for_each_repcode_candidate_with_reps_scalar,
            hash3_candidate_scalar,
            crate::encoding::fastpath::scalar::common_prefix_len_ptr,
        )
    }
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn matches() {
    let mut matcher = MatchGenerator::new(1000);
    let mut original_data = Vec::new();
    let mut reconstructed = Vec::new();

    let replay_sequence = |seq: Sequence<'_>, reconstructed: &mut Vec<u8>| match seq {
        Sequence::Literals { literals } => {
            assert!(!literals.is_empty());
            reconstructed.extend_from_slice(literals);
        }
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            assert!(offset > 0);
            assert!(match_len >= MIN_MATCH_LEN);
            reconstructed.extend_from_slice(literals);
            assert!(offset <= reconstructed.len());
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    };

    matcher.add_data(
        alloc::vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));

    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![
            1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0,
        ],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[
        1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0,
    ]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![7, 8, 9, 10, 11],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[7, 8, 9, 10, 11]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 3, 5, 7, 9],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    matcher.skip_matching();
    original_data.extend_from_slice(&[1, 3, 5, 7, 9]);
    reconstructed.extend_from_slice(&[1, 3, 5, 7, 9]);
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 3, 5, 7, 9],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[1, 3, 5, 7, 9]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    assert_eq!(reconstructed, original_data);
}

#[test]
fn dfast_matches_roundtrip_multi_block_pattern() {
    let pattern = [9, 21, 44, 184, 19, 96, 171, 109, 141, 251];
    let first_block: Vec<u8> = pattern.iter().copied().cycle().take(128 * 1024).collect();
    let second_block: Vec<u8> = pattern.iter().copied().cycle().take(128 * 1024).collect();

    let mut matcher = DfastMatchGenerator::new(1 << 22);
    let replay_sequence = |decoded: &mut Vec<u8>, seq: Sequence<'_>| match seq {
        Sequence::Literals { literals } => decoded.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            decoded.extend_from_slice(literals);
            let start = decoded.len() - offset;
            for i in 0..match_len {
                let byte = decoded[start + i];
                decoded.push(byte);
            }
        }
    };

    matcher.add_data(first_block.clone(), |_| {});
    let mut history = Vec::new();
    matcher.start_matching(|seq| replay_sequence(&mut history, seq));
    assert_eq!(history, first_block);

    matcher.add_data(second_block.clone(), |_| {});
    let prefix_len = history.len();
    matcher.start_matching(|seq| replay_sequence(&mut history, seq));

    assert_eq!(&history[prefix_len..], second_block.as_slice());
}

/// Regression for the `DFAST_MIN_MATCH_LEN: 6 -> 5` drop. The fixture
/// is built so the longest available match is EXACTLY 5 bytes — a
/// matcher that still effectively requires a 6-byte floor would emit
/// only literals here and the assertion would catch the silent
/// 5-byte miss.
///
/// Fixture layout (34 B):
///   bytes 0..5    `"ABCDE"`  — match source
///   bytes 5..28   `'!'` × 23 — filler that does NOT start with 'A'
///   bytes 28..33  `"ABCDE"`  — match site (repeats the prefix)
///   byte  33      `'F'`      — terminator: differs from byte 5 (`'!'`),
///                              so the forward extension at the match
///                              site stops at exactly length 5.
///
/// A 5-byte match at offset 28 must be emitted; a 6-byte+ match at the
/// same offset must NOT.
#[test]
fn dfast_accepts_exact_five_byte_match() {
    // Layout the input so that:
    //   byte  0      = 'Z'            (lead byte — keeps the match SOURCE off
    //                                  position 0, which the greedy loop never
    //                                  inserts: like the donor it starts the
    //                                  cursor at ip+1 and hashes only visited
    //                                  positions)
    //   bytes 1..6   = "ABCDE"        (the match source — position 1 IS visited)
    //   bytes 6..29  = 23 filler bytes that do NOT start with 'A'
    //   bytes 29..34 = "ABCDE"        (the 5-byte match site)
    //   byte  34     = 'F'            (differs from byte 6 = '!')
    // The longest available copy at position 29 is exactly 5 bytes:
    // the byte at position 34 ('F') differs from the byte at position 6
    // ('!'), so the forward extension stops at length 5.
    let mut data = Vec::new();
    data.push(b'Z'); // 0
    data.extend_from_slice(b"ABCDE"); // 1..6
    data.extend_from_slice(b"!!!!!!!!!!!!!!!!!!!!!!!"); // 6..29 (23 bytes)
    data.extend_from_slice(b"ABCDE"); // 29..34
    data.push(b'F'); // 34: forces forward extension to stop at length 5
    // Trailing filler so the match site (29) sits at least HASH_READ_SIZE (8)
    // bytes before the block end. The greedy double-fast — like the donor —
    // stops probing at `ilimit = iend - HASH_READ_SIZE`, so a match in the
    // final 8 bytes is never searched (donor parity, not a regression).
    data.extend_from_slice(b"GHIJKLMNOPQRSTUVWXYZ"); // 35..55
    assert_eq!(data.len(), 55);

    let mut matcher = DfastMatchGenerator::new(1 << 22);
    matcher.add_data(data.clone(), |_| {});

    let mut saw_five_byte_match = false;
    let mut saw_longer_match = false;
    matcher.start_matching(|seq| {
        if let Sequence::Triple {
            offset, match_len, ..
        } = seq
        {
            if offset == 28 && match_len == 5 {
                saw_five_byte_match = true;
            } else if offset == 28 && match_len > 5 {
                saw_longer_match = true;
            }
        }
    });

    assert!(
        saw_five_byte_match,
        "dfast must accept the exact-5-byte match — a 6-byte floor would skip it"
    );
    assert!(
        !saw_longer_match,
        "fixture pinned to length 5 — byte 33 ('F') must terminate the extension"
    );
}

#[test]
fn driver_switches_backends_and_initializes_dfast_via_reset() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Default);
    assert_eq!(driver.active_backend(), super::strategy::BackendTag::Dfast);
    assert_eq!(driver.window_size(), (1u64 << 21));

    let mut first = driver.get_next_space();
    first[..12].copy_from_slice(b"abcabcabcabc");
    first.truncate(12);
    driver.commit_space(first);
    assert_eq!(driver.get_last_space(), b"abcabcabcabc");
    driver.skip_matching_with_hint(None);

    let mut second = driver.get_next_space();
    second[..12].copy_from_slice(b"abcabcabcabc");
    second.truncate(12);
    driver.commit_space(second);

    let mut reconstructed = b"abcabcabcabc".to_vec();
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            reconstructed.extend_from_slice(literals);
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    });
    assert_eq!(reconstructed, b"abcabcabcabcabcabcabcabc");

    driver.reset(CompressionLevel::Fastest);
    assert_eq!(driver.window_size(), (1u64 << 19));
}

#[test]
fn driver_level5_selects_row_backend() {
    let mut driver = MatchGeneratorDriver::new(32, 2);
    driver.reset(CompressionLevel::Level(5));
    assert_eq!(driver.active_backend(), super::strategy::BackendTag::Row);
    // Greedy-specific routing assertion: `MatchGeneratorDriver::start_matching`
    // dispatches the Row backend into `start_matching_greedy` iff
    // `self.parse == ParseMode::Greedy`, so assert that actual selector —
    // round-trip alone passes on the lazy parser too. `row_matcher().lazy_depth`
    // is a secondary corroboration of the same routing decision (a mirror of
    // the parse mode); checking `parse` directly catches a regression even if
    // the two ever drift apart.
    assert_eq!(
        driver.parse,
        super::strategy::ParseMode::Greedy,
        "L5 must route to start_matching_greedy (parse == Greedy)",
    );
    assert_eq!(
        driver.row_matcher().lazy_depth,
        0,
        "row matcher lazy_depth must mirror the greedy parse mode",
    );
}

/// Level 4 maps to `StrategyTag::Dfast` (the greedy double-fast, donor
/// `ZSTD_dfast` — "greedy" is the parse discipline, not the Row/Greedy
/// strategy at Level 5). Round-trip alone doesn't pin match quality (a lazy
/// parser would also reconstruct the input correctly), so this test guards the
/// parse output itself: a small repeating pattern must produce at least one
/// `Sequence::Triple`, so a future regression that emits literals-only (e.g. a
/// `min_match` or rep-probe guard regression) is caught.
#[test]
fn driver_level4_greedy_round_trip_single_slice() {
    let mut driver = MatchGeneratorDriver::new(64, 2);
    driver.reset(CompressionLevel::Level(4));
    let input = b"abcdefgh_abcdefgh_abcdefgh_abcdefgh";
    let mut space = driver.get_next_space();
    space[..input.len()].copy_from_slice(input);
    space.truncate(input.len());
    driver.commit_space(space);

    let mut reconstructed: Vec<u8> = Vec::new();
    let mut saw_triple = false;
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            saw_triple = true;
            reconstructed.extend_from_slice(literals);
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    });
    assert_eq!(
        reconstructed,
        input.to_vec(),
        "L4 greedy parse failed to reconstruct repeating-pattern input",
    );
    assert!(
        saw_triple,
        "L4 greedy parse on a repeating pattern must emit at least one match (Triple)",
    );
}

#[test]
fn driver_level4_greedy_round_trip_cross_slice() {
    // Verifies that the greedy parse carries repcode / hash-table state
    // across slice boundaries: the second slice repeats the first byte
    // for byte, so the parse must pick up matches reaching back into
    // the previous slice's history.
    let mut driver = MatchGeneratorDriver::new(32, 4);
    driver.reset(CompressionLevel::Level(4));
    let chunk = b"the quick brown fox jumps over!!";
    assert_eq!(chunk.len(), 32);

    let mut first = driver.get_next_space();
    first[..chunk.len()].copy_from_slice(chunk);
    first.truncate(chunk.len());
    driver.commit_space(first);

    let mut first_recon: Vec<u8> = Vec::new();
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => first_recon.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            first_recon.extend_from_slice(literals);
            let start = first_recon.len() - offset;
            for i in 0..match_len {
                let byte = first_recon[start + i];
                first_recon.push(byte);
            }
        }
    });
    assert_eq!(
        first_recon,
        chunk.to_vec(),
        "first slice failed to round-trip"
    );

    let mut second = driver.get_next_space();
    second[..chunk.len()].copy_from_slice(chunk);
    second.truncate(chunk.len());
    driver.commit_space(second);

    let mut full = first_recon.clone();
    let mut saw_cross_slice_match = false;
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => full.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            // A match whose offset reaches >= the current slice's literal
            // run plus the second slice's index means we matched into the
            // first slice — exactly the cross-slice behavior under test.
            if offset >= chunk.len() {
                saw_cross_slice_match = true;
            }
            full.extend_from_slice(literals);
            let start = full.len() - offset;
            for i in 0..match_len {
                let byte = full[start + i];
                full.push(byte);
            }
        }
    });
    let mut expected = chunk.to_vec();
    expected.extend_from_slice(chunk);
    assert_eq!(
        full, expected,
        "cross-slice L4 greedy parse failed to reconstruct"
    );
    assert!(
        saw_cross_slice_match,
        "L4 greedy parse must match across slice boundaries (history is shared)",
    );
}

/// Helper: round-trip `data` through the L4 greedy parse and assert
/// the reconstructed bytes match. Returns `(triple_count, max_offset)`
/// so callers can probe parse shape (matches emitted, max-offset).
#[cfg(test)]
impl MatchGeneratorDriver {
    /// Test-only: stage a parse×search recipe override applied on the
    /// next `reset()`. Routes a level through a non-default (parse,
    /// search) pair so the decoupling can be exercised end-to-end.
    pub(crate) fn set_config_override(
        &mut self,
        search: super::strategy::SearchMethod,
        parse: super::strategy::ParseMode,
    ) {
        self.config_override = Some((search, parse));
    }

    /// Test-only: reset `level` routed onto the lazy HashChain pairing.
    /// The lazy band runs on the Row backend in production, so HC-specific
    /// behaviour (live-chain dict prime, eviction budget accounting, seed
    /// pass gates) is exercised through this override-backed reset.
    pub(crate) fn reset_on_hc_lazy(&mut self, level: CompressionLevel) {
        self.set_config_override(
            super::strategy::SearchMethod::HashChain,
            super::strategy::ParseMode::Lazy2,
        );
        self.reset(level);
    }
}

/// Drive a full compress parse for `data` at `level` (optionally with a
/// parse×search override) and reconstruct the bytes from the emitted
/// sequences. The returned buffer must equal `data` for a correct parse.
#[cfg(test)]
fn drive_roundtrip_with_override(
    level: CompressionLevel,
    over: Option<(super::strategy::SearchMethod, super::strategy::ParseMode)>,
    data: &[u8],
) -> Vec<u8> {
    let mut driver = MatchGeneratorDriver::new(1 << 17, 8);
    if let Some((s, p)) = over {
        driver.set_config_override(s, p);
    }
    driver.reset(level);

    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    let mut offset_in_data = 0usize;
    while offset_in_data < data.len() {
        let mut space = driver.get_next_space();
        let take = (data.len() - offset_in_data).min(space.len());
        space[..take].copy_from_slice(&data[offset_in_data..offset_in_data + take]);
        space.truncate(take);
        driver.commit_space(space);
        offset_in_data += take;

        driver.start_matching(|seq| match seq {
            Sequence::Literals { literals } => out.extend_from_slice(literals),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => {
                out.extend_from_slice(literals);
                let start = out.len() - offset;
                for i in 0..match_len {
                    let byte = out[start + i];
                    out.push(byte);
                }
            }
        });
    }
    out
}

/// Phase 1 capability proof: parse and search are decoupled, so a level
/// can run any parse mode on any non-opt search backend. Greedy-on-
/// HashChain and Lazy2-on-RowHash are pairings the legacy `strategy_tag`
/// could not express; both must reconstruct the input exactly.
#[test]
fn parse_search_matrix_decoupled_roundtrips() {
    use super::strategy::{ParseMode, SearchMethod};
    // Mixed repetitive + literal payload that exercises matches and reps.
    let mut data = Vec::new();
    for i in 0..4000u32 {
        data.extend_from_slice(b"the quick brown fox ");
        data.extend_from_slice(&i.to_le_bytes());
    }

    // Greedy parse on the HashChain search backend (legacy: Greedy was
    // welded to RowHash).
    let got = drive_roundtrip_with_override(
        CompressionLevel::Level(5),
        Some((SearchMethod::HashChain, ParseMode::Greedy)),
        &data,
    );
    assert_eq!(got, data, "greedy-on-hashchain diverged");

    // Lazy2 parse on the RowHash search backend (legacy: Lazy was welded
    // to HashChain).
    let got = drive_roundtrip_with_override(
        CompressionLevel::Level(8),
        Some((SearchMethod::RowHash, ParseMode::Lazy2)),
        &data,
    );
    assert_eq!(got, data, "lazy2-on-rowhash diverged");

    // Lazy on RowHash too (depth 1).
    let got = drive_roundtrip_with_override(
        CompressionLevel::Level(6),
        Some((SearchMethod::RowHash, ParseMode::Lazy)),
        &data,
    );
    assert_eq!(got, data, "lazy-on-rowhash diverged");
}

/// The row `mls` knob (C-like `minMatch`) is respected: every accepted
/// match (regular row + repcode, on the lazy parse) is at least `mls`
/// bytes, and the stream still round-trips for the whole 4..=7 range. The
/// default (5) reproduces the historical `ROW_MIN_MATCH_LEN` behaviour.
#[test]
fn row_mls_knob_gates_matches_and_roundtrips() {
    let data: Vec<u8> = (0..4000u32)
        .flat_map(|i| {
            let mut v = b"abcdefgh".to_vec();
            v.extend_from_slice(&i.to_le_bytes());
            v
        })
        .collect();

    for mls in [4usize, 5, 6, 7] {
        let mut matcher = RowMatchGenerator::new(1 << 22);
        let mut cfg = ROW_CONFIG;
        cfg.mls = mls;
        matcher.configure(cfg);
        matcher.add_data(data.clone(), |_| {});

        let mut out: Vec<u8> = Vec::with_capacity(data.len());
        let mut shortest_match = usize::MAX;
        matcher.start_matching(|seq| match seq {
            Sequence::Literals { literals } => out.extend_from_slice(literals),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => {
                out.extend_from_slice(literals);
                shortest_match = shortest_match.min(match_len);
                let start = out.len() - offset;
                for i in 0..match_len {
                    let byte = out[start + i];
                    out.push(byte);
                }
            }
        });

        assert_eq!(out, data, "mls={mls} round-trip diverged");
        if shortest_match != usize::MAX {
            assert!(
                shortest_match >= mls,
                "mls={mls}: emitted a {shortest_match}-byte match below the floor",
            );
        }
    }
}

/// `LevelParams::parse()` derives the parse mode from the `search` axis, not
/// the strategy tag, so the decoupling holds even for a `Bt*`-tagged level
/// overridden to a non-BT search backend. Pre-fix the method matched on
/// `strategy_tag` and returned `Optimal` for any `Bt*` tag regardless of
/// `search`/`lazy_depth`.
#[test]
fn parse_mode_follows_search_axis_not_strategy_tag() {
    use super::strategy::{ParseMode, SearchMethod};
    // LEVEL_TABLE[15] is level 16: BtOpt tag, BinaryTree search.
    let mut p = LEVEL_TABLE[15];
    assert_eq!(p.parse(), ParseMode::Optimal, "BinaryTree search → Optimal");
    // Override the Bt-tagged level's search to a non-BT backend: parse must
    // follow the search axis (derive from lazy_depth), not stay Optimal.
    p.search = SearchMethod::RowHash;
    p.lazy_depth = 0;
    assert_eq!(p.parse(), ParseMode::Greedy, "RowHash + depth 0 → Greedy");
    p.lazy_depth = 2;
    assert_eq!(p.parse(), ParseMode::Lazy2, "RowHash + depth 2 → Lazy2");
}

/// The test-only `config_override` is consumed by the first `reset()` (one
/// shot), so a reused driver does not silently keep the synthetic pairing
/// armed across later resets. Pre-fix `reset()` copied the override and left
/// it set.
#[test]
fn config_override_is_consumed_by_reset() {
    use super::strategy::{ParseMode, SearchMethod};
    let mut driver = MatchGeneratorDriver::new(1 << 17, 8);
    driver.set_config_override(SearchMethod::RowHash, ParseMode::Lazy2);
    assert!(driver.config_override.is_some());
    driver.reset(CompressionLevel::Level(5));
    assert!(
        driver.config_override.is_none(),
        "override must be consumed after one reset",
    );
}

// Level 4 maps to the greedy Dfast (double-fast) backend — "greedy" here is the
// parse discipline (no lazy lookahead, donor `ZSTD_dfast`), NOT the Row/Greedy
// strategy (which is Level 5). This roundtrip is intentional Dfast L4 coverage;
// the Row backend is exercised by the `Level(5)` fixtures elsewhere in this file.
#[cfg(test)]
fn l4_greedy_round_trip(slice_size: usize, max_slices: usize, data: &[u8]) -> (usize, usize) {
    let mut driver = MatchGeneratorDriver::new(slice_size, max_slices);
    driver.reset(CompressionLevel::Level(4));

    let mut reconstructed: Vec<u8> = Vec::with_capacity(data.len());
    let mut triple_count = 0usize;
    let mut max_offset = 0usize;

    // `start_matching` consumes the current pending slice; multi-slice
    // payloads require commit + drive per slice so earlier slices'
    // bytes actually round-trip out before they're displaced from the
    // window.
    let mut offset_in_data = 0usize;
    while offset_in_data < data.len() {
        let mut space = driver.get_next_space();
        let space_cap = space.len();
        let take = (data.len() - offset_in_data).min(space_cap);
        space[..take].copy_from_slice(&data[offset_in_data..offset_in_data + take]);
        space.truncate(take);
        driver.commit_space(space);
        offset_in_data += take;

        driver.start_matching(|seq| match seq {
            Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => {
                triple_count += 1;
                if offset > max_offset {
                    max_offset = offset;
                }
                reconstructed.extend_from_slice(literals);
                let start = reconstructed.len() - offset;
                for i in 0..match_len {
                    let byte = reconstructed[start + i];
                    reconstructed.push(byte);
                }
            }
        });
    }

    // Empty payload still needs one commit/drive round so the empty-
    // input path of `start_matching_greedy` (the `current_len == 0`
    // early-return guard) gets exercised.
    if data.is_empty() {
        let mut space = driver.get_next_space();
        space.truncate(0);
        driver.commit_space(space);
        driver.start_matching(|seq| match seq {
            Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
            Sequence::Triple { .. } => panic!("empty input must not emit any matches"),
        });
    }

    assert_eq!(reconstructed, data, "L4 greedy round-trip diverged");
    (triple_count, max_offset)
}

/// CodeRabbit-flagged tail rep-only case: the previous outer-loop
/// guard `pos + ROW_MIN_MATCH_LEN <= current_len` (6) meant the last
/// 5-byte position was unreachable. The rep probe at `abs_pos + 1`
/// only needs 4 bytes of lookahead beyond the probe point, so the
/// guard was relaxed to `pos + GREEDY_MIN_LOOKAHEAD <= current_len`
/// (5). This test drives the slices separately and asserts a match
/// is emitted **from the second slice's parse pass**, so a future
/// regression that re-tightens the guard or breaks the cross-slice
/// repcode lookup fails the test instead of being masked by
/// first-slice matches.
#[test]
fn driver_level5_greedy_tail_rep_only_reachable() {
    // Period-4 first slice locks rep1 = 4 into `offset_hist` by the
    // time the parse reaches the slice tail. Second slice is exactly
    // 5 bytes ( = `GREEDY_MIN_LOOKAHEAD`) so the outer loop runs
    // **once** at `pos = 0`; the regular `row_candidate` requires 6
    // bytes from `abs_pos`, which is past the live history, so the
    // only viable hit is the `abs_pos + 1` rep probe. `second[0..]`
    // is shaped so the rep probe at `abs_pos + 1` finds a 4-byte
    // match at offset 4 (`second[1..5] == first[13..16] ++ second[0]
    // == "BCDA"`), and `extend_backwards_shared` then absorbs
    // `second[0]` into the match (extending one byte back into the
    // implicit anchor, no further because anchor itself is the
    // current `abs_pos`).
    let first: &[u8] = b"ABCDABCDABCDABCD"; // 16 bytes — strict period 4
    let second: &[u8] = b"ABCDA"; // 5 bytes — exact GREEDY_MIN_LOOKAHEAD
    let mut driver = MatchGeneratorDriver::new(16, 2);
    driver.reset(CompressionLevel::Level(5));

    let mut first_space = driver.get_next_space();
    first_space[..first.len()].copy_from_slice(first);
    first_space.truncate(first.len());
    driver.commit_space(first_space);
    driver.start_matching(|_| {});

    let mut second_space = driver.get_next_space();
    second_space[..second.len()].copy_from_slice(second);
    second_space.truncate(second.len());
    driver.commit_space(second_space);

    let mut second_slice_triples = 0usize;
    driver.start_matching(|seq| {
        if matches!(seq, Sequence::Triple { .. }) {
            second_slice_triples += 1;
        }
    });

    assert!(
        second_slice_triples >= 1,
        "tail rep-only position must produce a match in the second slice \
         (got {second_slice_triples} triples)",
    );
}

#[test]
fn driver_level4_greedy_empty_input_emits_nothing() {
    // Empty input: no slices committed → no sequences emitted, no
    // panic. Exercises the `current_len == 0` early-return guard at
    // the top of `start_matching_greedy`.
    let mut driver = MatchGeneratorDriver::new(64, 2);
    driver.reset(CompressionLevel::Level(4));
    // Commit an empty space so the matcher has SOMETHING to start
    // matching on (otherwise `start_matching` panics on the
    // `window.back()` unwrap — that's a separate path covered by
    // existing reset tests).
    let mut space = driver.get_next_space();
    space.truncate(0);
    driver.commit_space(space);
    let mut emitted_anything = false;
    driver.start_matching(|_| emitted_anything = true);
    assert!(!emitted_anything, "empty slice must not emit any sequences",);
}

#[test]
fn driver_level4_greedy_sub_min_lookahead_input() {
    // Input shorter than `GREEDY_MIN_LOOKAHEAD = 5` — the outer loop
    // never executes a body iteration; the tail literal path must
    // still emit the input bytes as a single `Sequence::Literals`.
    let data: &[u8] = b"abcd"; // 4 bytes
    let (triples, _) = l4_greedy_round_trip(64, 2, data);
    assert_eq!(
        triples, 0,
        "sub-min-lookahead input must not emit any matches (got {triples})",
    );
}

#[test]
fn driver_level4_greedy_incompressible_input() {
    // Pseudo-random bytes with no exploitable structure — every
    // position is a "miss" in both the rep probe and the row
    // candidate. Exercises the miss branch + `SKIP_STRENGTH = 10`
    // skip-step grow (irrelevant at this size, but the path runs).
    let mut data = alloc::vec::Vec::with_capacity(256);
    let mut x: u32 = 0xDEAD_BEEF;
    for _ in 0..256 {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        data.push((x >> 16) as u8);
    }
    let (_triples, _) = l4_greedy_round_trip(64, 8, &data);
    // No structural assertion — the test passes if round-trip is
    // bit-exact and no panic / debug_assert fires.
}

#[test]
fn driver_level4_greedy_long_literal_run_skip_step_growth() {
    // 2 KiB of unstructured bytes drives the literal-run length past
    // the `SKIP_STRENGTH = 10` threshold (~1 KiB), so the miss branch
    // + per-miss step-grow path in `start_matching_greedy` is
    // exercised. This test is a stress smoke — it only asserts
    // bit-exact round-trip + no panic / `debug_assert!` fires; it
    // does NOT pin the `SKIP_STRENGTH` constant or the per-iteration
    // step count (round-trip would still pass on `SKIP_STRENGTH = 6`
    // or `= 14` since both produce valid sequences). Pinning the
    // exact step growth would require returning step / iteration
    // metadata from the parse, which is invasive plumbing for a
    // constant that hasn't been re-tuned in months. The value of
    // this test is catching panics or correctness regressions on
    // long incompressible runs, which is what its existing
    // round-trip assertion checks.
    let mut data = alloc::vec::Vec::with_capacity(2048);
    let mut x: u32 = 0xC0FF_EE00;
    for _ in 0..2048 {
        x = x.wrapping_mul(0x9E37_79B9).wrapping_add(0xCAFEBABE);
        data.push((x >> 24) as u8);
    }
    let (_triples, _) = l4_greedy_round_trip(512, 8, &data);
}

#[test]
fn driver_level4_greedy_all_zeros_heavy_rep1() {
    // All zeros: every position after the first byte has `byte[pos]
    // == byte[pos - 1]`, so the rep1 probe at `abs_pos + 1` hits
    // immediately and the parse collapses to a single long match.
    // Exercises the `cheap rep at +1, full-match length` path.
    let data: Vec<u8> = alloc::vec![0u8; 128];
    let (triples, max_offset) = l4_greedy_round_trip(64, 8, &data);
    assert!(
        triples >= 1,
        "all-zeros input must produce at least one rep1 match",
    );
    // The dominant match should reference rep1 (offset 1), since
    // every byte at pos matches pos-1. A larger offset would
    // indicate the rep1 probe was bypassed.
    assert_eq!(
        max_offset, 1,
        "all-zeros L4 greedy parse should commit at offset 1 (got {max_offset})",
    );
}

/// Periodic-pattern payload covers the steady-state rep-cascade path
/// of the greedy parse — the main-loop rep probe at `abs_pos + 1`
/// fires every iteration once the period is locked into
/// `offset_hist[0]`, and the parse emits a long chain of triples at
/// the same offset.
#[test]
fn driver_level4_greedy_periodic_pattern_rep_cascade() {
    let unit: &[u8] = b"alpha_beta_gamma";
    assert_eq!(unit.len(), 16);
    let mut data: Vec<u8> = Vec::with_capacity(unit.len() * 32);
    for _ in 0..32 {
        data.extend_from_slice(unit);
    }
    let (triples, max_offset) = l4_greedy_round_trip(64, 16, &data);
    assert!(
        triples >= 1,
        "periodic 16-byte payload must emit matches (got {triples})",
    );
    assert!(
        max_offset >= 16,
        "periodic 16-byte payload must produce at least one offset >= 16 \
         (got max_offset = {max_offset})",
    );
}

#[test]
fn driver_reset_keeps_strategy_tag_in_sync_with_active_backend() {
    use super::strategy::StrategyTag;

    fn check(level: CompressionLevel, expected: StrategyTag) {
        let mut driver = MatchGeneratorDriver::new(32, 2);
        driver.reset(level);
        assert_eq!(
            driver.strategy_tag, expected,
            "strategy_tag wrong for {level:?}"
        );
        assert_eq!(
            driver.strategy_tag.backend(),
            driver.active_backend(),
            "strategy_tag backend disagrees with active_backend for {level:?}"
        );
    }

    check(CompressionLevel::Level(1), StrategyTag::Fast);
    check(CompressionLevel::Level(2), StrategyTag::Fast);
    check(CompressionLevel::Level(3), StrategyTag::Dfast);
    check(CompressionLevel::Level(4), StrategyTag::Dfast);
    check(CompressionLevel::Level(5), StrategyTag::Greedy);
    check(CompressionLevel::Level(7), StrategyTag::Lazy);
    check(CompressionLevel::Level(12), StrategyTag::Lazy);
    check(CompressionLevel::Level(13), StrategyTag::Btlazy2);
    check(CompressionLevel::Level(14), StrategyTag::Btlazy2);
    check(CompressionLevel::Level(15), StrategyTag::Btlazy2);
    check(CompressionLevel::Level(16), StrategyTag::BtOpt);
    check(CompressionLevel::Level(18), StrategyTag::BtUltra);
    check(CompressionLevel::Level(22), StrategyTag::BtUltra2);
    check(CompressionLevel::Fastest, StrategyTag::Fast);
    check(CompressionLevel::Default, StrategyTag::Dfast);
    check(CompressionLevel::Better, StrategyTag::Lazy);
    check(CompressionLevel::Best, StrategyTag::Lazy);
}

#[test]
fn level_16_17_map_to_btopt_strategy() {
    use super::strategy::{BackendTag, StrategyTag};
    let p16 = resolve_level_params(CompressionLevel::Level(16), None);
    let p17 = resolve_level_params(CompressionLevel::Level(17), None);
    assert_eq!(p16.backend(), BackendTag::HashChain);
    assert_eq!(p17.backend(), BackendTag::HashChain);
    assert_eq!(StrategyTag::for_level(16), StrategyTag::BtOpt);
    assert_eq!(StrategyTag::for_level(17), StrategyTag::BtOpt);
}

#[test]
fn level_18_maps_to_btultra_level_19_to_btultra2_strategy() {
    use super::strategy::{BackendTag, StrategyTag};
    // Donor `clevels.h` (srcSize > 256 KiB tier): level 18 = `ZSTD_btultra`,
    // level 19 = `ZSTD_btultra2`. Level 19 was previously mapped to plain
    // btultra, which under-searched (searchLog 6 vs 7) and lost ~3.7% ratio
    // on the repo corpus.
    let p18 = resolve_level_params(CompressionLevel::Level(18), None);
    let p19 = resolve_level_params(CompressionLevel::Level(19), None);
    assert_eq!(p18.backend(), BackendTag::HashChain);
    assert_eq!(p19.backend(), BackendTag::HashChain);
    assert_eq!(StrategyTag::for_level(18), StrategyTag::BtUltra);
    assert_eq!(StrategyTag::for_level(19), StrategyTag::BtUltra2);
}

#[test]
fn level_20_22_map_to_btultra2_strategy() {
    use super::strategy::{BackendTag, StrategyTag};
    for level in 20..=22 {
        let params = resolve_level_params(CompressionLevel::Level(level), None);
        assert_eq!(params.backend(), BackendTag::HashChain);
        assert_eq!(StrategyTag::for_level(level as u8), StrategyTag::BtUltra2);
    }
}

#[test]
fn level22_uses_donor_target_length_and_large_input_tables() {
    let params = resolve_level_params(CompressionLevel::Level(22), None);
    assert_eq!(params.window_log, 27);
    let hc = params.hc.unwrap();
    assert_eq!(hc.hash_log, 25);
    assert_eq!(hc.chain_log, 27);
    assert_eq!(hc.search_depth, 1 << 9);
    assert_eq!(hc.target_len, 999);
}

#[test]
fn bt_levels_16_to_21_pin_clevels_params() {
    // Pins the BT-level (window_log, hash_log, chain_log, search_depth,
    // target_len) tuples so the clevels.h alignment cannot silently drift.
    // Levels 16-20 mirror upstream `clevels.h` (srcSize > 256 KiB tier,
    // search_depth = 1 << searchLog); level 21 intentionally keeps a deeper
    // search_depth (512 vs upstream's 128) — it beats C on ratio there and
    // the deeper walk is a deliberate ratio-positive divergence.
    let expected = [
        // (level, window_log, hash_log, chain_log, search_depth, target_len)
        (16u8, 22u8, 22usize, 22usize, 32usize, 48usize),
        (17, 23, 22, 23, 32, 64),
        (18, 23, 22, 23, 64, 64),
        (19, 23, 22, 24, 128, 256),
        (20, 25, 23, 25, 128, 256),
        (21, 26, 24, 24, 512, 256),
    ];
    for (level, wlog, hlog, clog, sd, tl) in expected {
        let p = resolve_level_params(CompressionLevel::Level(level as i32), None);
        assert_eq!(p.window_log, wlog, "level {level} window_log");
        let hc = p.hc.unwrap();
        assert_eq!(hc.hash_log, hlog, "level {level} hash_log");
        assert_eq!(hc.chain_log, clog, "level {level} chain_log");
        assert_eq!(hc.search_depth, sd, "level {level} search_depth");
        assert_eq!(hc.target_len, tl, "level {level} target_len");
    }
}

#[test]
fn level22_source_size_hint_uses_donor_btultra2_tiers() {
    let p16k = resolve_level_params(CompressionLevel::Level(22), Some(16 * 1024));
    assert_eq!(p16k.window_log, 14);
    let hc16k = p16k.hc.unwrap();
    assert_eq!(hc16k.hash_log, 15);
    assert_eq!(hc16k.chain_log, 15);
    assert_eq!(hc16k.search_depth, 1 << 10);
    assert_eq!(hc16k.target_len, 999);

    let p128k = resolve_level_params(CompressionLevel::Level(22), Some(128 * 1024));
    assert_eq!(p128k.window_log, 17);
    let hc128k = p128k.hc.unwrap();
    assert_eq!(hc128k.hash_log, 17);
    assert_eq!(hc128k.chain_log, 18);
    assert_eq!(hc128k.search_depth, 1 << 11);
    assert_eq!(hc128k.target_len, 999);

    let p256k = resolve_level_params(CompressionLevel::Level(22), Some(256 * 1024));
    assert_eq!(p256k.window_log, 18);
    let hc256k = p256k.hc.unwrap();
    assert_eq!(hc256k.hash_log, 19);
    assert_eq!(hc256k.chain_log, 19);
    assert_eq!(hc256k.search_depth, 1 << 13);
    assert_eq!(hc256k.target_len, 999);
}

#[test]
fn level22_small_source_size_hint_matches_donor_cparams() {
    use zstd::zstd_safe::zstd_sys;

    let source_size = 15_027u64;
    let donor = unsafe { zstd_sys::ZSTD_getCParams(22, source_size, 0) };
    let params = resolve_level_params(CompressionLevel::Level(22), Some(source_size));

    let hc = params.hc.unwrap();
    assert_eq!(params.window_log as u32, donor.windowLog);
    assert_eq!(hc.chain_log as u32, donor.chainLog);
    assert_eq!(hc.hash_log as u32, donor.hashLog);
    assert_eq!(hc.search_depth as u32, 1u32 << donor.searchLog);
    assert_eq!(HC_OPT_MIN_MATCH_LEN as u32, donor.minMatch);
    assert_eq!(hc.target_len as u32, donor.targetLength);
}

#[test]
fn level22_small_source_uses_window_bounded_hash3_log() {
    let mut hc = HcMatchGenerator::new(1 << 14);
    hc.configure(
        BTULTRA2_HC_CONFIG_L22_16K,
        super::strategy::StrategyTag::BtUltra2,
        14,
    );
    assert_eq!(hc.table.hash3_log, 14);

    hc.configure(
        BTULTRA2_HC_CONFIG_L22,
        super::strategy::StrategyTag::BtUltra2,
        27,
    );
    assert_eq!(hc.table.hash3_log, HC3_HASH_LOG);
}

#[test]
fn btultra2_seed_pass_initializes_opt_state() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG,
        super::strategy::StrategyTag::BtUltra2,
        26,
    );
    let data: Vec<u8> = (0..32 * 1024).map(|i| (i % 251) as u8).collect();
    hc.table.add_data(data, |_| {});
    hc.start_matching(|_| {});
    assert!(
        hc.backend.bt_mut().opt_state.lit_length_sum > 0,
        "btultra2 first block should seed non-zero sequence statistics"
    );
    assert!(
        hc.backend.bt_mut().opt_state.off_code_sum > 0,
        "btultra2 first block should seed offset-code statistics"
    );
}

#[test]
fn btultra2_profile_disables_small_offset_handicap() {
    // Pre-Phase-3 this test duplicated the profile build with
    // `pass2=false` and `pass2=true` since `for_mode` differentiated
    // them. With `const_for_strategy::<BtUltra2>()` there is only one
    // profile — the donor `opt2` pricing — so a single binding
    // captures the invariant the test is asserting.
    let profile = HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra2>();
    assert!(
        !profile.favor_small_offsets,
        "btultra2 should match donor opt2 offset pricing"
    );
    assert!(
        profile.accurate,
        "btultra2 should use donor opt2 accurate pricing"
    );
}

#[test]
fn btultra_profile_keeps_donor_search_depth_budget() {
    let p = HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra>();
    assert_eq!(
        p.max_chain_depth, 64,
        "btultra chain-depth budget must match clevels.h level 18 searchLog 6 (1 << 6 = 64)"
    );
}

#[test]
fn btopt_profile_keeps_donor_search_depth_budget() {
    let p = HcOptimalCostProfile::const_for_strategy::<super::strategy::BtOpt>();
    assert_eq!(
        p.max_chain_depth, 32,
        "btopt should not cap chain depth below donor btopt search budget"
    );
}

#[test]
fn sufficient_match_len_is_clamped_by_target_len() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG,
        super::strategy::StrategyTag::BtUltra2,
        26,
    );
    hc.hc.target_len = 13;
    let profile = HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra2>();
    assert_eq!(hc.hc.sufficient_match_len_for_pass(profile), 13);
}

#[test]
fn opt_modes_use_target_len_as_sufficient_len() {
    use super::strategy;
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.hc.target_len = 57;
    let profiles = [
        HcOptimalCostProfile::const_for_strategy::<strategy::BtOpt>(),
        HcOptimalCostProfile::const_for_strategy::<strategy::BtUltra>(),
        HcOptimalCostProfile::const_for_strategy::<strategy::BtUltra2>(),
    ];
    for profile in profiles {
        assert_eq!(hc.hc.sufficient_match_len_for_pass(profile), 57);
    }
}

#[test]
fn sufficient_match_len_is_capped_by_opt_num() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.hc.target_len = usize::MAX / 2;
    let profile = HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra2>();
    assert_eq!(hc.hc.sufficient_match_len_for_pass(profile), HC_OPT_NUM - 1);
}

#[test]
#[allow(clippy::borrow_deref_ref)]
fn dictionary_entropy_seed_initializes_opt_state_from_tables() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG,
        super::strategy::StrategyTag::BtUltra2,
        26,
    );

    let huff = crate::huff0::huff0_encoder::HuffmanTable::build_from_data(
        b"aaabbbbccccddddeeeeefffffgggg",
    );
    let ll = crate::fse::fse_encoder::default_ll_table();
    let ml = crate::fse::fse_encoder::default_ml_table();
    let of = crate::fse::fse_encoder::default_of_table();
    hc.seed_dictionary_entropy(Some(&huff), Some(&*ll), Some(&*ml), Some(&*of));

    hc.backend.bt_mut().opt_state.rescale_freqs(
        b"abcd",
        HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra2>(),
    );

    let base_ll_freqs: [u32; HC_MAX_LL + 1] = [
        4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1,
    ];

    assert_ne!(
        hc.backend.bt_mut().opt_state.lit_length_freq,
        base_ll_freqs,
        "dictionary entropy should override fallback LL bootstrap frequencies"
    );
    assert!(
        hc.backend
            .bt_mut()
            .opt_state
            .match_length_freq
            .iter()
            .any(|&v| v != 1),
        "dictionary entropy should seed non-uniform ML frequencies"
    );
    assert_ne!(
        hc.backend.bt_mut().opt_state.off_code_freq[0],
        6,
        "dictionary entropy should override fallback OF bootstrap frequencies"
    );
}

#[test]
#[allow(clippy::borrow_deref_ref)]
fn dictionary_fse_seed_applies_without_huffman_seed() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG,
        super::strategy::StrategyTag::BtUltra2,
        26,
    );

    let ll = crate::fse::fse_encoder::default_ll_table();
    let ml = crate::fse::fse_encoder::default_ml_table();
    let of = crate::fse::fse_encoder::default_of_table();
    hc.seed_dictionary_entropy(None, Some(&*ll), Some(&*ml), Some(&*of));
    hc.backend.bt_mut().opt_state.rescale_freqs(
        b"abcd",
        HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra2>(),
    );

    let base_ll_freqs: [u32; HC_MAX_LL + 1] = [
        4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1,
    ];
    assert_ne!(
        hc.backend.bt_mut().opt_state.lit_length_freq,
        base_ll_freqs,
        "FSE seed should still override LL bootstrap frequencies without huffman seed"
    );
    assert!(
        hc.backend
            .bt_mut()
            .opt_state
            .match_length_freq
            .iter()
            .any(|&v| v != 1),
        "FSE seed should still seed non-uniform ML frequencies"
    );
    assert_ne!(
        hc.backend.bt_mut().opt_state.off_code_freq[0],
        6,
        "FSE seed should still override OF bootstrap frequencies without huffman seed"
    );
}

#[test]
#[allow(clippy::borrow_deref_ref)]
fn dictionary_seed_overrides_predef_price_mode_on_tiny_input() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG,
        super::strategy::StrategyTag::BtUltra2,
        26,
    );

    let ll = crate::fse::fse_encoder::default_ll_table();
    let ml = crate::fse::fse_encoder::default_ml_table();
    let of = crate::fse::fse_encoder::default_of_table();
    hc.seed_dictionary_entropy(None, Some(&*ll), Some(&*ml), Some(&*of));
    hc.backend.bt_mut().opt_state.rescale_freqs(
        b"abc",
        HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra2>(),
    );
    assert!(
        matches!(
            hc.backend.bt_mut().opt_state.price_type,
            HcOptPriceType::Dynamic
        ),
        "dictionary-seeded first block should stay in dynamic mode even for tiny src"
    );
}

#[test]
fn lit_length_price_blocksize_max_costs_one_extra_bit() {
    let profile_predef = HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra2>();
    let mut stats_predef = HcOptState::new();
    stats_predef.price_type = HcOptPriceType::Predefined;
    let predef_max = profile_predef.lit_length_price(&stats_predef, HC_BLOCKSIZE_MAX);
    let predef_prev =
        profile_predef.lit_length_price(&stats_predef, HC_BLOCKSIZE_MAX.saturating_sub(1));
    assert_eq!(
        predef_max,
        predef_prev + HC_BITCOST_MULTIPLIER,
        "predefined litLength pricing at BLOCKSIZE_MAX must add exactly one bit"
    );

    let profile_dyn = HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra2>();
    let mut stats_dyn = HcOptState::new();
    stats_dyn.price_type = HcOptPriceType::Dynamic;
    stats_dyn.lit_length_freq.fill(1);
    stats_dyn.lit_length_sum = (HC_MAX_LL + 1) as u32;
    stats_dyn.match_length_freq.fill(1);
    stats_dyn.match_length_sum = (HC_MAX_ML + 1) as u32;
    stats_dyn.off_code_freq.fill(1);
    stats_dyn.off_code_sum = (HC_MAX_OFF + 1) as u32;
    stats_dyn.lit_freq.fill(1);
    stats_dyn.lit_sum = (HC_MAX_LIT + 1) as u32;
    stats_dyn.set_base_prices(true);
    let dyn_max = profile_dyn.lit_length_price(&stats_dyn, HC_BLOCKSIZE_MAX);
    let dyn_prev = profile_dyn.lit_length_price(&stats_dyn, HC_BLOCKSIZE_MAX.saturating_sub(1));
    assert_eq!(
        dyn_max,
        dyn_prev + HC_BITCOST_MULTIPLIER,
        "dynamic litLength pricing at BLOCKSIZE_MAX must add exactly one bit"
    );
}

#[test]
#[allow(clippy::borrow_deref_ref)]
fn btultra2_seed_pass_disabled_when_dictionary_entropy_seed_present() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG,
        super::strategy::StrategyTag::BtUltra2,
        26,
    );
    let ll = crate::fse::fse_encoder::default_ll_table();
    let ml = crate::fse::fse_encoder::default_ml_table();
    let of = crate::fse::fse_encoder::default_of_table();
    hc.seed_dictionary_entropy(None, Some(&*ll), Some(&*ml), Some(&*of));
    assert!(
        !hc.should_run_btultra2_seed_pass::<super::strategy::BtUltra2>(HC_PREDEF_THRESHOLD + 1),
        "dictionary-seeded first block should skip btultra2 warmup pass"
    );
}

#[test]
fn btultra2_seed_pass_disabled_when_prefix_history_exists() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG,
        super::strategy::StrategyTag::BtUltra2,
        26,
    );
    hc.table.history_abs_start = 17;
    hc.table.push_test_chunk(b"abcdefghijklmnop".to_vec());
    assert!(
        !hc.should_run_btultra2_seed_pass::<super::strategy::BtUltra2>(HC_PREDEF_THRESHOLD + 9),
        "btultra2 warmup must be first-block only (no prefix history)"
    );
}

#[test]
fn btultra2_seed_pass_disabled_for_tiny_block() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG,
        super::strategy::StrategyTag::BtUltra2,
        26,
    );
    assert!(
        !hc.should_run_btultra2_seed_pass::<super::strategy::BtUltra2>(HC_PREDEF_THRESHOLD),
        "btultra2 warmup should not run at or below predefined threshold"
    );
}

#[test]
fn btultra2_seed_pass_disabled_after_stats_initialized() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG,
        super::strategy::StrategyTag::BtUltra2,
        26,
    );
    hc.backend.bt_mut().opt_state.lit_length_sum = 1;
    assert!(
        !hc.should_run_btultra2_seed_pass::<super::strategy::BtUltra2>(HC_PREDEF_THRESHOLD + 32),
        "btultra2 warmup should run only for first block before stats are initialized"
    );
}

#[test]
fn btultra2_seed_pass_disabled_when_not_at_frame_start() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG,
        super::strategy::StrategyTag::BtUltra2,
        26,
    );
    // Simulate non-first block state: current block has no prefix in deque,
    // but total produced window already includes prior output.
    hc.table.window_size = HC_PREDEF_THRESHOLD + 64;
    // window_size set manually above to simulate prior output; record the
    // current block as one live chunk (seed-pass check reads lengths, not bytes).
    hc.table.chunk_lens.push_back(HC_PREDEF_THRESHOLD + 32);
    assert!(
        !hc.should_run_btultra2_seed_pass::<super::strategy::BtUltra2>(HC_PREDEF_THRESHOLD + 32),
        "btultra2 warmup must not run after frame start"
    );
}

#[test]
fn btultra2_seed_pass_disabled_when_ldm_sequences_exist() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG,
        super::strategy::StrategyTag::BtUltra2,
        26,
    );
    hc.table.window_size = HC_PREDEF_THRESHOLD + 64;
    hc.table.chunk_lens.push_back(HC_PREDEF_THRESHOLD + 64);
    hc.backend.bt_mut().ldm_sequences.push(HcRawSeq {
        lit_length: 8,
        offset: 16,
        match_length: 32,
    });
    assert!(
        !hc.should_run_btultra2_seed_pass::<super::strategy::BtUltra2>(HC_PREDEF_THRESHOLD + 32),
        "btultra2 warmup must not run when LDM already produced sequences"
    );
}

#[test]
fn literal_price_uses_eight_bits_when_literals_uncompressed() {
    let profile = HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra2>();
    let mut stats = HcOptState::new();
    stats.set_literals_compressed_for_tests(false);
    stats.price_type = HcOptPriceType::Predefined;
    assert_eq!(
        profile.literal_price(&stats, b'a'),
        8 * HC_BITCOST_MULTIPLIER,
        "uncompressed literals should cost 8 bits regardless of price mode"
    );
}

#[test]
fn update_stats_skips_literal_frequencies_when_uncompressed() {
    let mut stats = HcOptState::new();
    stats.set_literals_compressed_for_tests(false);
    stats.update_stats(3, b"abc", 4, 8);
    assert_eq!(
        stats.lit_sum, 0,
        "literal sum must remain unchanged when literal compression is disabled"
    );
    assert_eq!(
        stats.lit_freq.iter().copied().sum::<u32>(),
        0,
        "literal frequencies must not be updated when literal compression is disabled"
    );
    assert_eq!(
        stats.lit_length_sum, 1,
        "literal-length stats still update for sequence modeling"
    );
    assert_eq!(
        stats.match_length_sum, 1,
        "match-length stats still update for sequence modeling"
    );
    assert_eq!(
        stats.off_code_sum, 1,
        "offset-code stats still update for sequence modeling"
    );
}

#[test]
#[allow(clippy::borrow_deref_ref)]
fn dictionary_huffman_seed_ignored_when_literals_uncompressed() {
    let mut stats = HcOptState::new();
    stats.set_literals_compressed_for_tests(false);
    let huff = crate::huff0::huff0_encoder::HuffmanTable::build_from_data(
        b"aaaaabbbbcccddeeff00112233445566778899",
    );
    let ll = crate::fse::fse_encoder::default_ll_table();
    let ml = crate::fse::fse_encoder::default_ml_table();
    let of = crate::fse::fse_encoder::default_of_table();
    stats.seed_dictionary_entropy(Some(&huff), Some(&*ll), Some(&*ml), Some(&*of));
    stats.rescale_freqs(
        b"abcd",
        HcOptimalCostProfile::const_for_strategy::<super::strategy::BtUltra2>(),
    );
    assert_eq!(
        stats.lit_sum, 0,
        "literal sum must stay zero when literals are uncompressed"
    );
    assert_eq!(
        stats.lit_freq.iter().copied().sum::<u32>(),
        0,
        "literal frequencies must ignore dictionary huffman seed when uncompressed"
    );
}

#[test]
fn hc_repcode_candidates_respect_litlen_dependent_rep_order() {
    let mut hc = HcMatchGenerator::new(64);
    hc.table.history = b"xxxxxxABCDEFABCDEF".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;

    let abs_pos = 12usize; // points at second "ABCDEF"
    let current_abs_end = hc.table.history.len();
    let reps = [6u32, 3u32, 9u32];

    let mut lit_pos_candidates = Vec::new();
    hc.hc.for_each_repcode_candidate_with_reps(
        &hc.table,
        abs_pos,
        1,
        reps,
        current_abs_end,
        HC_OPT_MIN_MATCH_LEN,
        |c| {
            lit_pos_candidates.push(c.offset);
        },
    );
    assert!(
        lit_pos_candidates.contains(&6),
        "when lit_len>0, rep0 should be considered and match"
    );

    let mut ll0_candidates = Vec::new();
    hc.hc.for_each_repcode_candidate_with_reps(
        &hc.table,
        abs_pos,
        0,
        reps,
        current_abs_end,
        HC_OPT_MIN_MATCH_LEN,
        |c| {
            ll0_candidates.push(c.offset);
        },
    );
    assert!(
        !ll0_candidates.contains(&6),
        "when lit_len==0, rep0 is not directly eligible (ll0 semantics)"
    );
}

#[test]
fn hc_collect_optimal_candidates_keeps_reps_when_chain_depth_zero() {
    let mut hc = HcMatchGenerator::new(64);
    hc.hc.search_depth = 0;
    hc.table.history = b"xyzxyzxyzxyz".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;

    let abs_pos = 6usize;
    let current_abs_end = hc.table.history.len();
    let profile = HcOptimalCostProfile {
        max_chain_depth: 0,
        sufficient_match_len: usize::MAX / 2,
        accurate: false,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();
    hc.collect_optimal_candidates(
        abs_pos,
        current_abs_end,
        profile,
        HcCandidateQuery {
            reps: [3, 6, 9],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );
    assert!(
        !out.is_empty(),
        "rep candidates should remain available even when chain depth is zero"
    );
    assert!(
        out.iter().any(|c| c.offset == 3),
        "rep0 candidate should be retained"
    );
}

#[test]
fn hc_collect_optimal_candidates_rep_tail_match_skips_chain_probe() {
    let mut hc = HcMatchGenerator::new(64);
    hc.table.history = b"aaaaaaaaaa".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;
    hc.table.position_base = 0;
    hc.hc.search_depth = 32;
    let abs_pos = 6usize;
    hc.table.ensure_tables();
    hc.table.insert_positions(0, abs_pos);

    let profile = HcOptimalCostProfile {
        max_chain_depth: 32,
        sufficient_match_len: usize::MAX / 2,
        accurate: true,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();
    hc.collect_optimal_candidates(
        abs_pos,
        hc.table.history.len(),
        profile,
        HcCandidateQuery {
            reps: [1, 4, 8],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );

    assert!(
        out.iter()
            .all(|candidate| matches!(candidate.offset, 1 | 4)),
        "terminal rep match should return before chain probing adds non-rep offsets"
    );
}

#[test]
fn hc_collect_optimal_candidates_long_chain_match_advances_skip_window() {
    let mut hc = HcMatchGenerator::new(128);
    hc.table.history = b"abcabcabcabcabcabcabcabc".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;
    hc.table.position_base = 0;
    hc.hc.search_depth = 32;
    let abs_pos = 9usize;
    hc.table.ensure_tables();
    hc.table.insert_positions(0, abs_pos);
    hc.table.skip_insert_until_abs = 0;

    let profile = HcOptimalCostProfile {
        max_chain_depth: 32,
        sufficient_match_len: usize::MAX / 2,
        accurate: true,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();
    hc.collect_optimal_candidates(
        abs_pos,
        hc.table.history.len(),
        profile,
        HcCandidateQuery {
            reps: [1, 4, 8],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );

    assert!(
        hc.table.skip_insert_until_abs > abs_pos,
        "long chain match should advance skip window to avoid redundant immediate insertions"
    );
}

#[test]
fn hc_collect_optimal_candidates_chain_fast_skip_uses_match_end_minus_8() {
    let mut hc = HcMatchGenerator::new(128);
    hc.table.history = b"abcabcabcabcabcabcabcabc".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;
    hc.table.position_base = 0;
    hc.hc.search_depth = 32;
    let abs_pos = 9usize;
    hc.table.ensure_tables();
    hc.table.insert_positions(0, abs_pos);
    hc.table.skip_insert_until_abs = 0;

    let profile = HcOptimalCostProfile {
        max_chain_depth: 32,
        sufficient_match_len: 10,
        accurate: true,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();
    hc.collect_optimal_candidates(
        abs_pos,
        hc.table.history.len(),
        profile,
        HcCandidateQuery {
            reps: [1, 4, 8],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );

    let best_match_end = out
        .iter()
        .map(|candidate| candidate.start.saturating_add(candidate.match_len))
        .max()
        .expect("expected at least one candidate");
    assert!(
        hc.table.skip_insert_until_abs > abs_pos,
        "chain fast-skip must advance past current position"
    );
    assert!(
        hc.table.skip_insert_until_abs <= best_match_end.saturating_sub(8),
        "chain fast-skip must not exceed donor-style matchEndIdx - 8 bound"
    );
}

#[test]
fn hc_collect_optimal_candidates_advances_skip_window_on_plain_bt_path() {
    let mut hc = HcMatchGenerator::new(256);
    hc.table.history = b"abcdefghijklmnop".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;
    hc.table.position_base = 0;
    hc.hc.search_depth = 0;
    hc.table.ensure_tables();

    let abs_pos = 8usize;
    hc.table.skip_insert_until_abs = 0;

    let profile = HcOptimalCostProfile {
        max_chain_depth: 0,
        sufficient_match_len: usize::MAX / 2,
        accurate: true,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();
    hc.collect_optimal_candidates(
        abs_pos,
        hc.table.history.len(),
        profile,
        HcCandidateQuery {
            reps: [1, 4, 8],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );

    assert_eq!(
        hc.table.skip_insert_until_abs,
        abs_pos.saturating_add(1),
        "plain BT path should advance skip window by 1 via donor matchEndIdx baseline"
    );
}

// Removed: the three `hc_collect_optimal_candidates_*_hash3_*` /
// `hc_hash3_tail_match_*` tests forced `search_depth = 0` together
// with `hash3_log != 0`, an HC-chain-walker-only fixture state that
// production never reaches (hash3 is BtUltra2-only and BtUltra2 always
// runs `search_depth = 512`). They depended on the `has_hash3 =>
// BtUltra2` escape hatch in the test dispatcher; with that hatch gone
// (CR review on PR #123) and the dispatcher routing purely from
// `self.strategy_tag`, there is no production-shaped configuration
// that reproduces what those tests asserted. The corresponding hash3
// invariants are exercised end-to-end by the existing level22 roundtrip
// + donor-parity ratio gate.

#[test]
fn hc_ldm_candidates_are_merged_into_optimal_candidates() {
    let mut hc = HcMatchGenerator::new(512);
    hc.table.history = (0..256).map(|i| (i % 251) as u8).collect();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;

    let abs_pos = 128usize;
    let current_abs_end = 256usize;
    let ldm = MatchCandidate {
        start: abs_pos,
        offset: 96,
        match_len: 40,
    };

    let profile = HcOptimalCostProfile {
        max_chain_depth: 0,
        sufficient_match_len: usize::MAX / 2,
        accurate: true,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();
    hc.collect_optimal_candidates(
        abs_pos,
        current_abs_end,
        profile,
        HcCandidateQuery {
            reps: [1, 4, 8],
            lit_len: 1,
            ldm_candidate: Some(ldm),
        },
        &mut out,
    );
    assert!(
        out.iter().any(
            |candidate| candidate.offset == ldm.offset && candidate.match_len == ldm.match_len
        ),
        "LDM candidate should be present in optimal candidate set"
    );
}

#[test]
fn btultra_and_btultra2_both_keep_dictionary_candidates() {
    // Routes the BtUltra2 / BtUltra fixture through the production
    // `configure()` path so derived state (`hash3_log`, `is_btultra2`,
    // `uses_bt`, `backend`) stays consistent — manually flipping the
    // strategy flags here used to leave `hash3_log` / `hash3_table` in
    // the previous mode's shape and trip the
    // `Strategy::USE_HASH3 ⇒ hash3_log != 0` debug invariant inside
    // `collect_optimal_candidates_initialized_body`.
    use super::strategy::StrategyTag;

    let test_config = HcConfig {
        hash_log: 23,
        chain_log: 22,
        search_depth: 32,
        target_len: 256,
        search_mls: 4,
    };
    let window_log = 20u8;

    let prepare_history = |hc: &mut HcMatchGenerator, abs_pos: usize| {
        hc.table.history = alloc::vec![0u8; 160];
        for i in 0..64 {
            hc.table.history[i] = b'a' + (i % 7) as u8;
        }
        for i in 64..160 {
            hc.table.history[i] = b'k' + (i % 5) as u8;
        }
        for i in 0..24 {
            hc.table.history[abs_pos + i] = hc.table.history[16 + i];
        }
        hc.table.history_start = 0;
        hc.table.history_abs_start = 0;
        hc.table.position_base = 0;
        hc.table.ensure_tables();
        hc.table.insert_positions(0, abs_pos);
        hc.table.dictionary_limit_abs = Some(64);
        hc.table.skip_insert_until_abs = 0;
    };

    let profile = HcOptimalCostProfile {
        max_chain_depth: 32,
        sufficient_match_len: usize::MAX / 2,
        accurate: true,
        favor_small_offsets: false,
    };
    let abs_pos = 96usize;
    let mut out = Vec::new();

    let mut hc = HcMatchGenerator::new(256);
    hc.configure(test_config, StrategyTag::BtUltra2, window_log);
    prepare_history(&mut hc, abs_pos);
    hc.collect_optimal_candidates(
        abs_pos,
        160,
        profile,
        HcCandidateQuery {
            reps: [1, 4, 8],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );
    assert!(
        out.iter().any(|candidate| candidate.offset >= 32),
        "btultra2 should retain dictionary candidates on donor-parity path"
    );

    let mut hc = HcMatchGenerator::new(256);
    hc.configure(test_config, StrategyTag::BtUltra, window_log);
    prepare_history(&mut hc, abs_pos);
    hc.collect_optimal_candidates(
        abs_pos,
        160,
        profile,
        HcCandidateQuery {
            reps: [1, 4, 8],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );
    assert!(
        out.iter().any(|candidate| candidate.offset >= 32),
        "btultra should retain dictionary candidates"
    );
}

#[test]
fn driver_small_source_hint_shrinks_dfast_hash_tables() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Level(3));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);
    // Donor-parity split sizes: long-hash = DFAST_HASH_BITS,
    // short-hash = DFAST_HASH_BITS - DFAST_SHORT_HASH_BITS_DELTA.
    let full_long = driver.dfast_matcher().long_hash.len();
    let full_short = driver.dfast_matcher().short_hash.len();
    assert_eq!(full_long, 1 << DFAST_HASH_BITS);
    assert_eq!(
        full_short,
        1 << (DFAST_HASH_BITS - DFAST_SHORT_HASH_BITS_DELTA)
    );

    driver.set_source_size_hint(1024);
    driver.reset(CompressionLevel::Level(3));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"xyzxyzxyzxyz");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);
    let hinted_long = driver.dfast_matcher().long_hash.len();
    let hinted_short = driver.dfast_matcher().short_hash.len();

    // The wire `window_log` stays at its floor (decoder-interop), but the
    // internal dfast tables are sized from the RAW 1 KiB source, not the
    // floored window: `table_window = 1 << ceil_log2(1024) = 1 << 10`, so
    // both tables land at the `MIN_WINDOW_LOG` floor (the long table at
    // `dfast_hash_bits_for_window(1 << 10) = 10`, the short table one
    // `DFAST_SHORT_HASH_BITS_DELTA` step below but clamped back up to
    // `MIN_WINDOW_LOG`).
    assert_eq!(driver.window_size(), 1 << MIN_HINTED_WINDOW_LOG);
    assert_eq!(hinted_long, 1 << MIN_WINDOW_LOG);
    assert_eq!(hinted_short, 1 << MIN_WINDOW_LOG);
    assert!(
        hinted_long < full_long && hinted_short < full_short,
        "tiny source hint should reduce both dfast tables"
    );
}

#[test]
fn driver_huge_source_hint_does_not_overflow_table_window_shift() {
    // Regression: the Dfast / Row table-window sizing in `reset` derives a
    // shift from `ceil_log2(hint)`. A hint >= 2^63 + 1 makes that shift 64,
    // and `1usize << 64` panics in debug / wraps to 0 in release before the
    // `.min(max_window_size)` cap can apply. A `u64::MAX` pledged source size
    // must size the table to the real window, never panic or wrap to zero.
    let mut driver = MatchGeneratorDriver::new(32, 2);
    driver.set_source_size_hint(u64::MAX);
    driver.reset(CompressionLevel::Level(3));

    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);

    assert!(
        driver.dfast_matcher().long_hash.len() >= 1 << MIN_WINDOW_LOG,
        "huge hint must size the dfast table from the real window, not wrap to zero"
    );
}

#[test]
fn driver_small_source_hint_shrinks_row_hash_tables() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Level(5));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);
    let full_rows = driver.row_matcher().row_heads.len();
    // Level 5 uses the donor row_log (clamp(searchLog=3, 4, 6) = 4), not the
    // shared ROW_LOG, so the row count is 1 << (hash_bits - ROW_L5.row_log).
    assert_eq!(full_rows, 1 << (ROW_HASH_BITS - ROW_L5.row_log));

    driver.set_source_size_hint(1024);
    driver.reset(CompressionLevel::Level(5));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"xyzxyzxyzxyz");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);
    let hinted_rows = driver.row_matcher().row_heads.len();

    // Wire `window_log` stays floored, but the row hash table is sized from
    // the RAW 1 KiB source: `table_window = 1 << 10`, so
    // `row_hash_bits_for_window(1 << 10) = 11` (donor `hashLog <=
    // windowLog + 1`) and the row count is `1 << (11 - ROW_L5.row_log)`.
    assert_eq!(driver.window_size(), 1 << MIN_HINTED_WINDOW_LOG);
    assert_eq!(
        hinted_rows,
        1 << ((MIN_WINDOW_LOG as usize) + 1 - ROW_L5.row_log)
    );
    assert!(
        hinted_rows < full_rows,
        "tiny source hint should reduce row hash table footprint"
    );
}

#[test]
fn row_matches_roundtrip_multi_block_pattern() {
    let pattern = [7, 13, 44, 184, 19, 96, 171, 109, 141, 251];
    let first_block: Vec<u8> = pattern.iter().copied().cycle().take(128 * 1024).collect();
    let second_block: Vec<u8> = pattern.iter().copied().cycle().take(128 * 1024).collect();

    let mut matcher = RowMatchGenerator::new(1 << 22);
    matcher.configure(ROW_CONFIG);
    matcher.ensure_tables();
    let replay_sequence = |decoded: &mut Vec<u8>, seq: Sequence<'_>| match seq {
        Sequence::Literals { literals } => decoded.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            decoded.extend_from_slice(literals);
            let start = decoded.len() - offset;
            for i in 0..match_len {
                let byte = decoded[start + i];
                decoded.push(byte);
            }
        }
    };

    matcher.add_data(first_block.clone(), |_| {});
    let mut history = Vec::new();
    matcher.start_matching(|seq| replay_sequence(&mut history, seq));
    assert_eq!(history, first_block);

    matcher.add_data(second_block.clone(), |_| {});
    let prefix_len = history.len();
    matcher.start_matching(|seq| replay_sequence(&mut history, seq));

    assert_eq!(&history[prefix_len..], second_block.as_slice());

    // Force a literals-only pass so the Sequence::Literals arm is exercised.
    let third_block: Vec<u8> = (0u8..=255).collect();
    matcher.add_data(third_block.clone(), |_| {});
    let third_prefix = history.len();
    matcher.start_matching(|seq| replay_sequence(&mut history, seq));
    assert_eq!(&history[third_prefix..], third_block.as_slice());
}

#[test]
fn row_short_block_emits_literals_only() {
    let mut matcher = RowMatchGenerator::new(1 << 22);
    matcher.configure(ROW_CONFIG);

    matcher.add_data(b"abcde".to_vec(), |_| {});

    let mut saw_triple = false;
    let mut reconstructed = Vec::new();
    matcher.start_matching(|seq| match seq {
        Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
        Sequence::Triple { .. } => saw_triple = true,
    });

    assert!(
        !saw_triple,
        "row backend must not emit triples for short blocks"
    );
    assert_eq!(reconstructed, b"abcde");

    // Then feed a clearly matchable block and ensure the Triple arm is reachable.
    saw_triple = false;
    matcher.add_data(b"abcdeabcde".to_vec(), |_| {});
    matcher.start_matching(|seq| {
        if let Sequence::Triple { .. } = seq {
            saw_triple = true;
        }
    });
    assert!(
        saw_triple,
        "row backend should emit triples on repeated data"
    );
}

#[test]
fn row_pick_lazy_returns_best_when_lookahead_is_out_of_bounds() {
    let mut matcher = RowMatchGenerator::new(1 << 22);
    matcher.configure(ROW_CONFIG);
    matcher.add_data(b"abcabc".to_vec(), |_| {});
    // Build the row tables before probing: the lookahead path reaches
    // `row_candidate` -> `row_heads[..]` once the accept floor is small
    // enough to pass the length gate, so the tables must be allocated
    // (production always calls this before any candidate probe).
    matcher.ensure_tables();

    let best = MatchCandidate {
        start: 0,
        offset: 1,
        match_len: ROW_MIN_MATCH_LEN,
    };
    let picked = matcher
        .pick_lazy_match(0, 0, Some(best))
        .expect("best candidate must survive");

    assert_eq!(picked.start, best.start);
    assert_eq!(picked.offset, best.offset);
    assert_eq!(picked.match_len, best.match_len);
}

#[test]
fn row_backfills_previous_block_tail_for_cross_boundary_match() {
    let mut matcher = RowMatchGenerator::new(1 << 22);
    matcher.configure(ROW_CONFIG);

    let mut first_block = alloc::vec![0xA5; 64];
    first_block.extend_from_slice(b"XYZ");
    let second_block = b"XYZXYZtail".to_vec();

    let replay_sequence = |decoded: &mut Vec<u8>, seq: Sequence<'_>| match seq {
        Sequence::Literals { literals } => decoded.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            decoded.extend_from_slice(literals);
            let start = decoded.len() - offset;
            for i in 0..match_len {
                let byte = decoded[start + i];
                decoded.push(byte);
            }
        }
    };

    matcher.add_data(first_block.clone(), |_| {});
    let mut reconstructed = Vec::new();
    matcher.start_matching(|seq| replay_sequence(&mut reconstructed, seq));
    assert_eq!(reconstructed, first_block);

    matcher.add_data(second_block.clone(), |_| {});
    let mut saw_cross_boundary = false;
    let prefix_len = reconstructed.len();
    matcher.start_matching(|seq| {
        if let Sequence::Triple {
            literals,
            offset,
            match_len,
        } = seq
            && literals.is_empty()
            && offset == 3
            && match_len >= ROW_MIN_MATCH_LEN
        {
            saw_cross_boundary = true;
        }
        replay_sequence(&mut reconstructed, seq);
    });

    assert!(
        saw_cross_boundary,
        "row matcher should reuse the 3-byte previous-block tail"
    );
    assert_eq!(&reconstructed[prefix_len..], second_block.as_slice());
}

#[test]
fn row_skip_matching_with_incompressible_hint_uses_sparse_prefix() {
    let data = deterministic_high_entropy_bytes(0xA713_9C5D_44E2_10B1, 4096);

    let mut dense = RowMatchGenerator::new(1 << 22);
    dense.configure(ROW_CONFIG);
    dense.add_data(data.clone(), |_| {});
    dense.skip_matching_with_hint(Some(false));
    let dense_slots = dense
        .row_positions
        .iter()
        .filter(|&&pos| pos != ROW_EMPTY_SLOT)
        .count();

    let mut sparse = RowMatchGenerator::new(1 << 22);
    sparse.configure(ROW_CONFIG);
    sparse.add_data(data, |_| {});
    sparse.skip_matching_with_hint(Some(true));
    let sparse_slots = sparse
        .row_positions
        .iter()
        .filter(|&&pos| pos != ROW_EMPTY_SLOT)
        .count();

    assert!(
        sparse_slots < dense_slots,
        "incompressible hint should seed fewer row slots (sparse={sparse_slots}, dense={dense_slots})"
    );
}

/// Regression for the `None` arm of `skip_matching_with_hint`: the
/// row table must NOT receive dense inserts across the skipped range.
/// Donor parity (`ZSTD_row_fillHashCache` only pre-fills the next-scan
/// cache, not the skipped block's interior) trades cross-block
/// matches into the skipped interior for the per-block O(block_size)
/// insert cost.
///
/// At input < 1 block (4096 B with default 128 KiB block boundary),
/// the only positions in the row table after the call should be those
/// produced by the `backfill_start` lookback at the block's start
/// (≤ `ROW_HASH_KEY_LEN - 1` positions when block_start <
/// ROW_HASH_KEY_LEN). For `current_abs_start == 0`, even that backfill
/// is empty — so the table stays fully empty.
#[test]
fn row_skip_matching_with_none_hint_leaves_interior_empty() {
    let data = deterministic_high_entropy_bytes(0x9B47_F2A1_8C5E_3306, 4096);

    let mut none_hint = RowMatchGenerator::new(1 << 22);
    none_hint.configure(ROW_CONFIG);
    none_hint.add_data(data.clone(), |_| {});
    none_hint.skip_matching_with_hint(None);
    let none_slots = none_hint
        .row_positions
        .iter()
        .filter(|&&pos| pos != ROW_EMPTY_SLOT)
        .count();

    // Dense (Some(false), dict-priming path) for comparison — that
    // path inserts every position in the skipped range.
    let mut dense = RowMatchGenerator::new(1 << 22);
    dense.configure(ROW_CONFIG);
    dense.add_data(data, |_| {});
    dense.skip_matching_with_hint(Some(false));
    let dense_slots = dense
        .row_positions
        .iter()
        .filter(|&&pos| pos != ROW_EMPTY_SLOT)
        .count();

    // Two assertions pin the contract:
    // 1) None hint is dramatically sparser than dense (the whole point).
    // 2) None hint at block-start==0 inserts ZERO positions (no
    //    backfill possible before position 0).
    assert_eq!(
        none_slots, 0,
        "None hint at block_start=0 must leave row table fully empty \
         (donor parity — interior NOT inserted, no pre-block backfill possible)",
    );
    assert!(
        dense_slots > 0,
        "Some(false) dict-priming path must still insert densely \
         (sanity check: control case for the `none_slots == 0` assertion)",
    );
}

#[test]
fn driver_unhinted_level2_keeps_default_dfast_hash_table_size() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Level(3));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);

    // Donor-parity split: long-hash at DFAST_HASH_BITS, short-hash one
    // bit smaller (DFAST_SHORT_HASH_BITS_DELTA = 1, matching donor
    // `chainLog = hashLog - 1` for dfast levels).
    let long_len = driver.dfast_matcher().long_hash.len();
    let short_len = driver.dfast_matcher().short_hash.len();
    assert_eq!(
        long_len,
        1 << DFAST_HASH_BITS,
        "unhinted Level(2) should keep default long-hash table size"
    );
    assert_eq!(
        short_len,
        1 << (DFAST_HASH_BITS - DFAST_SHORT_HASH_BITS_DELTA),
        "unhinted Level(2) short-hash should be one bit smaller than long-hash"
    );
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_backend_rejects_undersized_pooled_suffix_store() {
    let mut driver = MatchGeneratorDriver::new(128 * 1024, 2);
    driver.reset(CompressionLevel::Fastest);

    driver.suffix_pool.push(SuffixStore::with_capacity(1024));

    let mut space = driver.get_next_space();
    space.clear();
    space.resize(4096, 0xAB);
    driver.commit_space(space);

    let last_suffix_slots = driver
        .simple()
        .window
        .last()
        .expect("window entry must exist after commit")
        .suffixes
        .slots
        .len();
    assert!(
        last_suffix_slots >= 4096,
        "undersized pooled suffix store must not be reused for larger blocks"
    );
}

#[test]
fn source_hint_clamps_driver_slice_size_to_window() {
    let mut driver = MatchGeneratorDriver::new(128 * 1024, 2);
    driver.set_source_size_hint(1024);
    driver.reset(CompressionLevel::Default);

    let window = driver.window_size() as usize;
    assert_eq!(window, 1 << MIN_HINTED_WINDOW_LOG);
    assert_eq!(driver.slice_size, window);

    let space = driver.get_next_space();
    assert_eq!(space.len(), window);
    driver.commit_space(space);
}

#[test]
fn pooled_space_keeps_capacity_when_slice_size_shrinks() {
    let mut driver = MatchGeneratorDriver::new(128 * 1024, 2);
    driver.reset(CompressionLevel::Default);

    let large = driver.get_next_space();
    let large_capacity = large.capacity();
    assert!(large_capacity >= 128 * 1024);
    driver.commit_space(large);

    driver.set_source_size_hint(1024);
    driver.reset(CompressionLevel::Default);

    let small = driver.get_next_space();
    assert_eq!(small.len(), 1 << MIN_HINTED_WINDOW_LOG);
    assert!(
        small.capacity() >= large_capacity,
        "pooled buffer capacity should be preserved to avoid shrink/grow churn"
    );
}

#[test]
fn driver_best_to_fastest_releases_oversized_hc_tables() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    // Initialize at Best routed onto HashChain (the production `Best`
    // resolves to Row now) — allocates large HC tables (4M hash, 2M chain)
    // so the swap below exercises the HC drain path this test pins.
    driver.reset_on_hc_lazy(CompressionLevel::Best);
    assert_eq!(driver.window_size(), (1u64 << 22));

    // Feed data so tables are actually allocated via ensure_tables().
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);

    // Switch to Fastest — the [`MatcherStorage`] enum swaps to the
    // `Simple` variant and the `HashChain` variant is dropped. The
    // drain block in `Matcher::reset` reassigns
    // `m.table.hash_table` / `chain_table` / `hash3_table` to
    // `Vec::new()` BEFORE constructing the replacement variant so the
    // table backing allocations are released up front — this caps
    // peak memory during the swap to "old data buffers being drained
    // into `vec_pool` + new `MatchGenerator` skeleton" rather than
    // "old tables still resident + new variant under construction".
    // The eventual `Drop` on the old variant would release the tables
    // anyway, but only after the new variant is built, so the early
    // reassign shifts the peak. Post-switch the HC variant no longer
    // exists; the assertion that storage is now `Simple` covers the
    // invariant the old hash_table/chain_table checks were proxying.
    driver.reset(CompressionLevel::Fastest);
    assert_eq!(driver.window_size(), (1u64 << 19));
    assert_eq!(driver.active_backend(), super::strategy::BackendTag::Simple);
}

#[test]
fn driver_better_to_best_resizes_hc_tables() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    // The lazy band runs on the Row backend now, so the HC resize path is
    // exercised across two BT levels whose native `HcConfig` widths differ:
    // L13 (hash_log 22, chain_log 22) -> L15 (hash_log 23, chain_log 23).
    driver.reset(CompressionLevel::Level(13));
    assert_eq!(driver.window_size(), (1u64 << 22));

    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);

    let hc = driver.hc_matcher();
    let better_hash_len = hc.table.hash_table.len();
    let better_chain_len = hc.table.chain_table.len();

    // Switch to L15 — must resize to larger tables.
    driver.reset(CompressionLevel::Level(15));
    assert_eq!(driver.window_size(), (1u64 << 22));

    // Feed data to trigger ensure_tables with new sizes.
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"xyzxyzxyzxyz");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);

    let hc = driver.hc_matcher();
    assert!(
        hc.table.hash_table.len() > better_hash_len,
        "L15 hash_table ({}) should be larger than L13 ({})",
        hc.table.hash_table.len(),
        better_hash_len
    );
    assert!(
        hc.table.chain_table.len() > better_chain_len,
        "L15 chain_table ({}) should be larger than L13 ({})",
        hc.table.chain_table.len(),
        better_chain_len
    );
}

#[cfg(any())]
// disabled: tests legacy SuffixStore behavior incompatible with donor-shape kernel's HASH_READ_SIZE geometry
#[test]
fn prime_with_dictionary_preserves_history_for_first_full_block() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);

    driver.prime_with_dictionary(b"abcdefgh", [1, 4, 8]);

    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(b"abcdefgh");
    driver.commit_space(space);

    let mut saw_match = false;
    driver.start_matching(|seq| {
        if let Sequence::Triple {
            literals,
            offset,
            match_len,
        } = seq
            && literals.is_empty()
            && offset == 8
            && match_len >= MIN_MATCH_LEN
        {
            saw_match = true;
        }
    });

    assert!(
        saw_match,
        "first full block should still match dictionary-primed history"
    );
}

#[cfg(any())]
// disabled: tests legacy SuffixStore behavior incompatible with donor-shape kernel's HASH_READ_SIZE geometry
#[test]
fn prime_with_large_dictionary_preserves_early_history_until_first_block() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);

    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);

    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(b"abcdefgh");
    driver.commit_space(space);

    let mut saw_match = false;
    driver.start_matching(|seq| {
        if let Sequence::Triple {
            literals,
            offset,
            match_len,
        } = seq
            && literals.is_empty()
            && offset == 24
            && match_len >= MIN_MATCH_LEN
        {
            saw_match = true;
        }
    });

    assert!(
        saw_match,
        "dictionary bytes should remain addressable until frame output exceeds the live window"
    );
}

#[test]
fn prime_with_dictionary_applies_offset_history_even_when_content_is_empty() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);

    driver.prime_with_dictionary(&[], [11, 7, 3]);

    assert_eq!(driver.simple_mut().offset_hist, [11, 7, 3]);
}

#[test]
fn hc_prime_with_empty_dictionary_disables_btultra2_seed_pass() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset_on_hc_lazy(CompressionLevel::Better);

    driver.prime_with_dictionary(&[], [11, 7, 3]);

    assert_eq!(driver.hc_matcher().table.offset_hist, [11, 7, 3]);
    assert!(
        !driver
            .hc_matcher()
            .should_run_btultra2_seed_pass::<super::strategy::BtUltra2>(HC_PREDEF_THRESHOLD + 1),
        "btultra2 warmup must stay disabled after dictionary priming, even when dict content is empty"
    );
}

#[test]
fn primed_snapshot_not_restored_across_ldm_config_change() {
    // The CDict-equivalent primed snapshot clones `storage`, which on the
    // BT backend carries `BtMatcher::ldm_producer`. A snapshot captured
    // under one LDM configuration must NOT be restored into a reset that
    // resolved a different LDM configuration (else the restored producer
    // is stale). `PrimedKey` must fold the LDM override into the key so
    // such a restore is refused and the caller re-primes.
    use super::parameters::CompressionParameters;

    let dict = b"abcdefghabcdefghabcdefgh";
    let ldm_on = CompressionParameters::builder(CompressionLevel::Level(19))
        .enable_long_distance_matching(true)
        .build()
        .unwrap()
        .overrides();
    let ldm_off = CompressionParameters::builder(CompressionLevel::Level(19))
        .build()
        .unwrap()
        .overrides();

    let mut driver = MatchGeneratorDriver::new(1024, 1);

    // Capture a snapshot primed under LDM-on at level 19.
    driver.set_param_overrides(Some(ldm_on));
    driver.reset(CompressionLevel::Level(19));
    driver.prime_with_dictionary(dict, [1, 4, 8]);
    driver.capture_primed_dictionary(CompressionLevel::Level(19));

    // Same dictionary + level, but LDM now OFF: the snapshot's LDM state
    // is stale, so restore must be refused.
    driver.set_param_overrides(Some(ldm_off));
    driver.reset(CompressionLevel::Level(19));
    assert!(
        !driver.restore_primed_dictionary(CompressionLevel::Level(19)),
        "primed snapshot restored across an LDM config change (stale producer)",
    );

    // Sanity: re-priming + capturing under LDM-off, then restoring under
    // the IDENTICAL LDM-off config DOES match (the key is not over-tight).
    driver.prime_with_dictionary(dict, [1, 4, 8]);
    driver.capture_primed_dictionary(CompressionLevel::Level(19));
    driver.reset(CompressionLevel::Level(19));
    assert!(
        driver.restore_primed_dictionary(CompressionLevel::Level(19)),
        "primed snapshot not restored under identical LDM config",
    );
}

#[test]
fn hc_prime_with_dictionary_disables_btultra2_seed_pass() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset_on_hc_lazy(CompressionLevel::Better);

    driver.prime_with_dictionary(b"abcdefgh", [1, 4, 8]);

    assert!(
        !driver
            .hc_matcher()
            .should_run_btultra2_seed_pass::<super::strategy::BtUltra2>(HC_PREDEF_THRESHOLD + 1),
        "btultra2 warmup must stay disabled after dictionary priming with content"
    );
}

#[test]
fn dfast_prime_with_dictionary_preserves_history_for_first_full_block() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    // Level(4) is Dfast with the greedy double-fast loop (donor parity:
    // clevels.h L3/L4 are both `ZSTD_dfast`, which has no lazy lookahead).
    // The fast loop needs at least `HASH_READ_SIZE` (8) bytes ahead of the
    // probe cursor, so this exercises a 16-byte dict + 16-byte block (the
    // whole block matches the dict, offset = dict length = 16).
    driver.reset(CompressionLevel::Level(4));

    let payload = b"abcdefghijklmnop";
    driver.prime_with_dictionary(payload, [1, 4, 8]);

    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(payload);
    driver.commit_space(space);

    let mut saw_match = false;
    driver.start_matching(|seq| {
        if let Sequence::Triple {
            literals,
            offset,
            match_len,
        } = seq
            && literals.is_empty()
            && offset == payload.len()
            && match_len >= DFAST_MIN_MATCH_LEN
        {
            saw_match = true;
        }
    });

    assert!(
        saw_match,
        "dfast backend should match dictionary-primed history in first full block"
    );
}

#[test]
fn prime_with_dictionary_does_not_inflate_reported_window_size() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);

    let before = driver.window_size();
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    let after = driver.window_size();

    assert_eq!(
        after, before,
        "dictionary retention budget must not change reported frame window size"
    );
}

#[test]
fn primed_snapshot_not_restored_when_window_hint_differs() {
    // The copy-snapshot must be keyed on the resolved reset parameters, not
    // just the CompressionLevel. `reset()` caps window_log by the source-size
    // hint, so two same-level frames with different hints resolve to different
    // windows. Restoring a snapshot captured at the larger hint into a reset
    // for the smaller hint would advertise the smaller window in the frame
    // header while the matcher's `max_window_size` (from the restored storage)
    // still spans the larger window — the encoder could then emit a match
    // (e.g. into the dictionary) past the advertised window, producing an
    // undecodable frame. Restore must REFUSE when the resolved window differs.
    let mut driver = MatchGeneratorDriver::new(8, 1);
    let level = CompressionLevel::Best;

    // Frame A: large hint → larger resolved window. Prime + capture.
    driver.set_source_size_hint(256 * 1024);
    driver.reset(level);
    let big_window = driver.window_size();
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    driver.capture_primed_dictionary(level);

    // Frame B: smaller hint, SAME level → smaller resolved window.
    driver.set_source_size_hint(48 * 1024);
    driver.reset(level);
    let small_window = driver.window_size();
    assert!(
        small_window < big_window,
        "precondition: the two hints must resolve to different windows \
         (small={small_window}, big={big_window})"
    );

    let restored = driver.restore_primed_dictionary(level);
    assert!(
        !restored,
        "snapshot captured at window {big_window} must NOT be restored into a \
         reset advertising window {small_window} (level alone is an insufficient key)"
    );
}

#[test]
fn primed_snapshot_restored_for_hints_in_same_window_bucket() {
    // The snapshot key must normalize the source-size hint to the resolved
    // matcher geometry, not the raw hinted byte count. `reset()` derives every
    // hint-dependent parameter (window_log cap, HC/Fast/Dfast/Row table widths,
    // the Fast attach-vs-copy cutoff) from `ceil_log2(hint)`, so two distinct
    // hints that share a ceil-log bucket resolve to the *identical* matcher
    // shape. Keying on the raw bytes over-keys: it forces a full re-prime on the
    // second frame even though the cached snapshot is a perfect fit. Restore
    // must SUCCEED across same-bucket hints.
    let mut driver = MatchGeneratorDriver::new(8, 1);
    let level = CompressionLevel::Best;

    // Both hints fall in ceil_log2 bucket 19 (2^18 < n <= 2^19): 300 KiB and
    // 400 KiB resolve to the same window and table widths.
    driver.set_source_size_hint(300 * 1024);
    driver.reset(level);
    let window_a = driver.window_size();
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    driver.capture_primed_dictionary(level);

    driver.set_source_size_hint(400 * 1024);
    driver.reset(level);
    let window_b = driver.window_size();
    assert_eq!(
        window_a, window_b,
        "precondition: same-bucket hints must resolve to the same window \
         (a={window_a}, b={window_b})"
    );

    let restored = driver.restore_primed_dictionary(level);
    assert!(
        restored,
        "snapshot captured at a 300 KiB hint must be restored into a 400 KiB \
         hint that resolves to the identical matcher shape (raw bytes over-key)"
    );
}

#[test]
fn primed_snapshot_restored_across_level22_donor_tier_hints() {
    // Level 22 collapses several ceil-log buckets onto one donor source-size
    // tier: `resolve_level_params(Level(22), ..)` selects the HC config and
    // window_log by raw `<= 16 KiB / 128 KiB / 256 KiB` thresholds, so a 20 KiB
    // and a 100 KiB hint (ceil-log buckets 15 and 17) both land in the
    // `<= 128 KiB` tier and resolve to the IDENTICAL matcher (same window_log,
    // same HC hash/chain/search geometry). Keying on the raw ceil-log bucket
    // would still reject the restore here because the buckets differ; the key
    // must compare the resolved matcher shape so these share one snapshot.
    let mut driver = MatchGeneratorDriver::new(8, 1);
    let level = CompressionLevel::Level(22);

    driver.set_source_size_hint(20 * 1024);
    driver.reset(level);
    let window_a = driver.window_size();
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    driver.capture_primed_dictionary(level);

    driver.set_source_size_hint(100 * 1024);
    driver.reset(level);
    let window_b = driver.window_size();
    assert_eq!(
        window_a, window_b,
        "precondition: both hints must land in the same Level 22 donor tier \
         (a={window_a}, b={window_b})"
    );

    let restored = driver.restore_primed_dictionary(level);
    assert!(
        restored,
        "Level 22 snapshot captured at a 20 KiB hint must be restored into a \
         100 KiB hint that resolves to the same donor tier (different ceil-log \
         buckets, identical matcher shape)"
    );
}

#[test]
fn primed_snapshot_not_restored_across_fast_attach_copy_boundary() {
    // The Fast attach-vs-copy cutoff (8 KiB) falls INSIDE a single resolved
    // matcher shape: a 8192-byte and a 8193-byte hint both clamp Level 1 to
    // window_log 14 and the same Fast table widths, so `LevelParams` +
    // `table_bits` are identical, yet 8192 attaches (separate dict table) while
    // 8193 copies (dict primed into the live table). The snapshot key must
    // therefore carry the attach/copy mode itself; without it the two resets
    // would share a key and a copy-mode snapshot could be restored into an
    // attach-mode reset (a different `storage` shape). Restore must REFUSE
    // across the boundary.
    let mut driver = MatchGeneratorDriver::new(8, 1);
    let level = CompressionLevel::Level(1);

    // Copy side (hint > 8 KiB): prime + capture.
    driver.set_source_size_hint(8193);
    driver.reset(level);
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    driver.capture_primed_dictionary(level);

    // Attach side (hint <= 8 KiB), same resolved window/table shape.
    driver.set_source_size_hint(8192);
    driver.reset(level);
    let restored = driver.restore_primed_dictionary(level);
    assert!(
        !restored,
        "a copy-mode snapshot (8193 B hint) must NOT be restored into an \
         attach-mode reset (8192 B hint) that resolves to the same params but a \
         different dict-table shape"
    );
}

#[test]
fn primed_snapshot_fast_attach_does_not_over_key_non_simple_backends() {
    // `fast_attach` is a Simple/Fast-backend concept (the 8 KiB attach-vs-copy
    // table split). On the HashChain/Dfast/Row backends the dictionary is
    // always primed the same way, so the bit must NOT enter their snapshot key
    // — otherwise an unhinted capture (which would record `fast_attach = true`)
    // and a hinted reset that resolves to the IDENTICAL `LevelParams` would key
    // differently and force a needless re-prime. `Best` is a Row-backend lazy
    // level; this also pins the Row arm recording its RESOLVED hash width on
    // the unhinted path (a 0 default there keyed unhinted-vs-hinted apart).
    let mut driver = MatchGeneratorDriver::new(8, 1);
    let level = CompressionLevel::Best;

    // Capture with no hint.
    driver.reset(level);
    let window_a = driver.window_size();
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    driver.capture_primed_dictionary(level);

    // Reset with a hint large enough to resolve to the same window/params as
    // the unhinted level (>= 2^window_log, so the source-size cap is a no-op).
    driver.set_source_size_hint(64 * 1024 * 1024);
    driver.reset(level);
    let window_b = driver.window_size();
    assert_eq!(
        window_a, window_b,
        "precondition: the large hint must resolve to the same window as the \
         unhinted level (a={window_a}, b={window_b})"
    );

    let restored = driver.restore_primed_dictionary(level);
    assert!(
        restored,
        "a HashChain snapshot must restore across an unhinted vs large-hinted \
         reset that resolves to the identical matcher — `fast_attach` is a Fast \
         backend concept and must not over-key non-Simple shapes"
    );
}

#[cfg(any())] // disabled: tested SuffixStore-per-block tail-handling specific to legacy MatchGenerator
#[test]
fn prime_with_dictionary_does_not_reuse_tiny_suffix_store() {
    let mut driver = MatchGeneratorDriver::new(8, 2);
    driver.reset(CompressionLevel::Fastest);

    // This dictionary leaves a 1-byte tail chunk (capacity=1 suffix table),
    // which should never be committed to the matcher window.
    driver.prime_with_dictionary(b"abcdefghi", [1, 4, 8]);

    assert!(
        driver
            .simple()
            .window
            .iter()
            .all(|entry| entry.data.len() >= MIN_MATCH_LEN),
        "dictionary priming must not commit tails shorter than MIN_MATCH_LEN"
    );
}

#[test]
fn prime_with_dictionary_counts_only_committed_tail_budget() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);

    let before = driver.simple_mut().max_window_size;
    // One full slice plus a 1-byte tail that cannot be committed.
    driver.prime_with_dictionary(b"abcdefghi", [1, 4, 8]);

    assert_eq!(
        driver.simple_mut().max_window_size,
        before + 8,
        "retention budget must account only for dictionary bytes actually committed to history"
    );
}

#[test]
fn dfast_prime_with_dictionary_counts_four_byte_tail_budget() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(3));

    let before = driver.dfast_matcher().max_window_size;
    // One full slice plus a 4-byte tail. Dfast can still use this tail through
    // short-hash overlap into the next block, so it should stay retained.
    driver.prime_with_dictionary(b"abcdefghijkl", [1, 4, 8]);

    assert_eq!(
        driver.dfast_matcher().max_window_size,
        before + 12,
        "dfast retention budget should include 4-byte dictionary tails"
    );
}

#[test]
fn row_prime_with_dictionary_preserves_history_for_first_full_block() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    // Level(5) is the greedy Row backend (LEVEL_TABLE row 5: Greedy / RowHash).
    // Level(4) now routes to Dfast, so this test must use Level(5) to actually
    // exercise `RowMatchGenerator`'s dictionary priming. The 16-byte dict +
    // 16-byte block lets the whole block match the primed dict (offset = dict
    // length = 16).
    driver.reset(CompressionLevel::Level(5));

    let payload = b"abcdefghijklmnop";
    driver.prime_with_dictionary(payload, [1, 4, 8]);

    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(payload);
    driver.commit_space(space);

    let mut saw_match = false;
    driver.start_matching(|seq| {
        if let Sequence::Triple {
            literals,
            offset,
            match_len,
        } = seq
            && literals.is_empty()
            && offset == payload.len()
            && match_len >= ROW_MIN_MATCH_LEN
        {
            saw_match = true;
        }
    });

    assert!(
        saw_match,
        "row backend should match dictionary-primed history in first full block"
    );
}

#[test]
fn row_prime_with_dictionary_subtracts_uncommitted_tail_budget() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(5));

    let base_window = driver.row_matcher().max_window_size;
    // Slice size is 8. The trailing byte cannot be committed (<4 tail),
    // so it must be subtracted from retained budget.
    driver.prime_with_dictionary(b"abcdefghi", [1, 4, 8]);

    assert_eq!(
        driver.row_matcher().max_window_size,
        base_window + 8,
        "row retained window must exclude uncommitted 1-byte tail"
    );
}

#[test]
fn prime_with_dictionary_budget_shrinks_after_row_eviction() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(5));
    // Keep live window tiny so dictionary-primed slices are evicted quickly.
    driver.row_matcher_mut().max_window_size = 8;
    driver.reported_window_size = 8;

    let base_window = driver.row_matcher().max_window_size;
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    assert_eq!(driver.row_matcher().max_window_size, base_window + 24);

    for block in [b"AAAAAAAA", b"BBBBBBBB"] {
        let mut space = driver.get_next_space();
        space.clear();
        space.extend_from_slice(block);
        driver.commit_space(space);
        driver.skip_matching_with_hint(None);
    }

    assert_eq!(
        driver.dictionary_retained_budget, 0,
        "dictionary budget should be fully retired once primed dict slices are evicted"
    );
    assert_eq!(
        driver.row_matcher().max_window_size,
        base_window,
        "retired dictionary budget must not remain reusable for live history"
    );
}

/// Row → Simple transition drops the Row variant and the
/// post-switch active backend is exactly Simple. The window-emptied
/// check from the pre-enum era (`driver.row_matcher().window.is_empty()`)
/// is intentionally gone — the `Row` variant no longer exists after
/// the swap, so there is nothing to inspect by accessor; the "window
/// cleared" invariant is replaced by "variant dropped", and a
/// subsequent `row_matcher()` call would panic by design. The
/// pool-recycling side of the row backend is covered by
/// [`driver_row_commit_recycles_block_buffer_into_pool`].
#[test]
fn row_get_last_space_then_reset_to_fastest_drops_row_variant() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(5));
    assert_eq!(driver.active_backend(), super::strategy::BackendTag::Row);

    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(b"row-data");
    driver.commit_space(space);

    assert_eq!(driver.get_last_space(), b"row-data");

    driver.reset(CompressionLevel::Fastest);
    assert_eq!(driver.active_backend(), super::strategy::BackendTag::Simple);
}

/// Committing a Row block must return the input buffer to `vec_pool`
/// immediately (the bytes are mirrored into the contiguous `history`,
/// so there is no reason to retain a second copy in the window). This
/// guards the chunk-length window: the previous `VecDeque<Vec<u8>>`
/// window retained a full `block_capacity` buffer per committed block,
/// which on a heavily pre-split frame ballooned peak memory to many
/// times the live byte count. With the buffer recycled at commit time
/// the pool grows by exactly one Vec per committed block.
#[test]
fn driver_row_commit_recycles_block_buffer_into_pool() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(5));
    assert_eq!(driver.active_backend(), super::strategy::BackendTag::Row);

    let before_pool = driver.vec_pool.len();
    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(b"row-data-to-recycle");
    driver.commit_space(space);

    // `>` not `>=`: a fresh driver starts with `before_pool == 0`, so the
    // weaker bound passes even if the commit failed to recycle. Strict
    // growth proves the buffer was returned to the pool at commit time
    // rather than retained in the window (the pre-`chunk_lens` bug).
    assert!(
        driver.vec_pool.len() > before_pool,
        "row commit must recycle the committed block buffer into vec_pool \
         (before_pool = {before_pool}, after = {})",
        driver.vec_pool.len()
    );
    // The bytes still resolve through the contiguous history mirror.
    assert_eq!(driver.get_last_space(), b"row-data-to-recycle");
}

#[test]
fn adjust_params_for_zero_source_size_uses_min_hinted_window_floor() {
    let mut params = resolve_level_params(CompressionLevel::Level(4), None);
    params.window_log = 22;
    let adjusted = adjust_params_for_source_size(params, 0);
    assert_eq!(adjusted.window_log, MIN_HINTED_WINDOW_LOG);
}

#[test]
fn common_prefix_len_matches_scalar_reference_across_offsets() {
    fn scalar_reference(a: &[u8], b: &[u8]) -> usize {
        a.iter()
            .zip(b.iter())
            .take_while(|(lhs, rhs)| lhs == rhs)
            .count()
    }

    for total_len in [
        0usize, 1, 5, 15, 16, 17, 31, 32, 33, 64, 65, 127, 191, 257, 320,
    ] {
        let base: Vec<u8> = (0..total_len)
            .map(|i| ((i * 13 + 7) & 0xFF) as u8)
            .collect();

        for start in [0usize, 1, 3] {
            if start > total_len {
                continue;
            }
            let a = &base[start..];
            let b = a.to_vec();
            assert_eq!(
                common_prefix_len(a, &b),
                scalar_reference(a, &b),
                "equal slices total_len={total_len} start={start}"
            );

            let len = a.len();
            for mismatch in [0usize, 1, 7, 15, 16, 31, 32, 47, 63, 95, 127, 128, 129, 191] {
                if mismatch >= len {
                    continue;
                }
                let mut altered = b.clone();
                altered[mismatch] ^= 0x5A;
                assert_eq!(
                    common_prefix_len(a, &altered),
                    scalar_reference(a, &altered),
                    "total_len={total_len} start={start} mismatch={mismatch}"
                );
            }

            if len > 0 {
                let mismatch = len - 1;
                let mut altered = b.clone();
                altered[mismatch] ^= 0xA5;
                assert_eq!(
                    common_prefix_len(a, &altered),
                    scalar_reference(a, &altered),
                    "tail mismatch total_len={total_len} start={start} mismatch={mismatch}"
                );
            }
        }
    }

    let long = alloc::vec![0xAB; 320];
    let shorter = alloc::vec![0xAB; 137];
    assert_eq!(
        common_prefix_len(&long, &shorter),
        scalar_reference(&long, &shorter)
    );
}

#[test]
fn row_pick_lazy_returns_none_when_next_is_better() {
    let mut matcher = RowMatchGenerator::new(1 << 22);
    matcher.configure(ROW_CONFIG);
    matcher.add_data(alloc::vec![b'a'; 64], |_| {});
    matcher.ensure_tables();

    let abs_pos = matcher.history_abs_start + 16;
    let best = MatchCandidate {
        start: abs_pos,
        offset: 8,
        match_len: ROW_MIN_MATCH_LEN,
    };
    assert!(
        matcher.pick_lazy_match(abs_pos, 0, Some(best)).is_none(),
        "lazy picker should defer when next position is clearly better"
    );
}

#[test]
fn row_pick_lazy_depth2_returns_none_when_next2_significantly_better() {
    let mut matcher = RowMatchGenerator::new(1 << 22);
    matcher.configure(ROW_CONFIG);
    matcher.lazy_depth = 2;
    matcher.search_depth = 0;
    matcher.offset_hist = [6, 9, 1];

    let mut data = alloc::vec![b'x'; 40];
    data[11..30].copy_from_slice(b"EFABCABCAEFABCAEFAB");
    matcher.add_data(data, |_| {});
    matcher.ensure_tables();

    let abs_pos = matcher.history_abs_start + 20;
    let best = matcher
        .best_match(abs_pos, 0)
        .expect("expected baseline repcode match");
    assert_eq!(best.offset, 9);
    // Baseline match length is fixed by the fixture data (the offset-9
    // rep run is 6 bytes long), independent of the accept threshold.
    assert_eq!(best.match_len, 6);

    if let Some(next) = matcher.best_match(abs_pos + 1, 1) {
        assert!(next.match_len <= best.match_len);
    }

    let next2 = matcher
        .best_match(abs_pos + 2, 2)
        .expect("expected +2 candidate");
    assert!(
        next2.match_len > best.match_len + 1,
        "+2 candidate must be significantly better for depth-2 lazy skip"
    );
    assert!(
        matcher.pick_lazy_match(abs_pos, 0, Some(best)).is_none(),
        "lazy picker should defer when +2 candidate is significantly better"
    );
}

#[test]
fn row_pick_lazy_depth2_keeps_best_when_next2_is_only_one_byte_better() {
    let mut matcher = RowMatchGenerator::new(1 << 22);
    matcher.configure(ROW_CONFIG);
    matcher.lazy_depth = 2;
    matcher.search_depth = 0;
    matcher.offset_hist = [6, 9, 1];

    let mut data = alloc::vec![b'x'; 40];
    data[11..30].copy_from_slice(b"EFABCABCAEFABCAEFAZ");
    matcher.add_data(data, |_| {});
    matcher.ensure_tables();

    let abs_pos = matcher.history_abs_start + 20;
    let best = matcher
        .best_match(abs_pos, 0)
        .expect("expected baseline repcode match");
    assert_eq!(best.offset, 9);
    // Baseline match length is fixed by the fixture data (the offset-9
    // rep run is 6 bytes long), independent of the accept threshold.
    assert_eq!(best.match_len, 6);

    let next2 = matcher
        .best_match(abs_pos + 2, 2)
        .expect("expected +2 candidate");
    assert_eq!(next2.match_len, best.match_len + 1);
    let chosen = matcher
        .pick_lazy_match(abs_pos, 0, Some(best))
        .expect("lazy picker should keep current best");
    assert_eq!(chosen.start, best.start);
    assert_eq!(chosen.offset, best.offset);
    assert_eq!(chosen.match_len, best.match_len);
}

/// Verifies row/tag extraction uses the shared hash mix bit-splitting contract.
#[test]
fn row_hash_and_row_extracts_high_bits() {
    let mut matcher = RowMatchGenerator::new(1 << 22);
    matcher.configure(ROW_CONFIG);
    matcher.add_data(
        alloc::vec![
            0xAA, 0xBB, 0xCC, 0x11, 0x10, 0x20, 0x30, 0x40, 0xAA, 0xBB, 0xCC, 0x22, 0x50, 0x60,
            0x70, 0x80,
        ],
        |_| {},
    );
    matcher.ensure_tables();

    let pos = matcher.history_abs_start + 8;
    let (row, tag) = matcher
        .hash_and_row(pos)
        .expect("row hash should be available");

    let idx = pos - matcher.history_abs_start;
    let concat = matcher.live_history();
    let value = u32::from_le_bytes(concat[idx..idx + ROW_HASH_KEY_LEN].try_into().unwrap()) as u64;
    let hash = crate::encoding::fastpath::hash_mix_u64_with_kernel(matcher.hash_kernel, value);
    let total_bits = matcher.row_hash_log + ROW_TAG_BITS;
    let combined = hash >> (u64::BITS as usize - total_bits);
    let expected_row =
        ((combined >> ROW_TAG_BITS) as usize) & ((1usize << matcher.row_hash_log) - 1);
    let expected_tag = combined as u8;

    assert_eq!(row, expected_row);
    assert_eq!(tag, expected_tag);
}

#[test]
fn row_repcode_skips_candidate_before_history_start() {
    let mut matcher = RowMatchGenerator::new(1 << 22);
    matcher.configure(ROW_CONFIG);
    matcher.history = alloc::vec![b'a'; 20];
    matcher.history_start = 0;
    matcher.history_abs_start = 10;
    matcher.offset_hist = [3, 0, 0];

    assert!(matcher.repcode_candidate(12, 1).is_none());
}

#[test]
fn row_repcode_returns_none_when_position_too_close_to_history_end() {
    let mut matcher = RowMatchGenerator::new(1 << 22);
    matcher.configure(ROW_CONFIG);
    matcher.history = b"abcde".to_vec();
    matcher.history_start = 0;
    matcher.history_abs_start = 0;
    matcher.offset_hist = [1, 0, 0];

    assert!(matcher.repcode_candidate(4, 1).is_none());
}

#[cfg(all(feature = "std", target_arch = "x86_64"))]
#[test]
fn hash_mix_sse42_path_is_available_and_matches_accelerated_impl_when_supported() {
    use crate::encoding::fastpath::{self, FastpathKernel};
    if !is_x86_feature_detected!("sse4.2") {
        return;
    }
    let v = 0x0123_4567_89AB_CDEFu64;
    // SAFETY: feature check above guarantees SSE4.2 is available.
    let accelerated = unsafe { fastpath::sse42::hash_mix_u64(v) };
    // Dispatcher must resolve to SSE4.2 (or better) and produce the same mix.
    let dispatched = fastpath::dispatch_hash_mix_u64(v);
    let kernel = fastpath::select_kernel();
    if kernel == FastpathKernel::Sse42 {
        assert_eq!(dispatched, accelerated);
    } else {
        // AVX2 kernel uses the same CRC32 instruction under the hood.
        assert_eq!(dispatched, accelerated, "AVX2/SSE4.2 share CRC32 mix");
    }
}

#[cfg(all(feature = "std", target_arch = "aarch64", target_endian = "little"))]
#[test]
fn hash_mix_crc_path_is_available_and_matches_accelerated_impl_when_supported() {
    use crate::encoding::fastpath;
    if !is_aarch64_feature_detected!("crc") {
        return;
    }
    let v = 0x0123_4567_89AB_CDEFu64;
    // SAFETY: feature check above guarantees CRC32 is available.
    let accelerated = unsafe { fastpath::neon::hash_mix_u64(v) };
    let dispatched = fastpath::dispatch_hash_mix_u64(v);
    assert_eq!(dispatched, accelerated);
}

#[test]
fn hc_hash3_position_matches_donor_formula() {
    let bytes = [b'a', b'b', b'c', b'd'];
    let read32 = u32::from_le_bytes(bytes);
    let expected = (((read32 << 8).wrapping_mul(HC_PRIME3BYTES)) >> (32 - HC3_HASH_LOG)) as usize;
    assert_eq!(
        super::match_table::storage::MatchTable::hash3_position(&bytes, HC3_HASH_LOG),
        expected
    );
}

#[test]
fn hc_hash_position_matches_donor_hash4_formula() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(HC_CONFIG, super::strategy::StrategyTag::Lazy, 22);
    let bytes = [b'a', b'b', b'c', b'd'];
    let read32 = u32::from_le_bytes(bytes);
    let expected = ((read32.wrapping_mul(HC_PRIME4BYTES)) >> (32 - hc.table.hash_log)) as usize;
    assert_eq!(hc.table.hash_position(&bytes), expected);
}

#[test]
fn btultra2_main_hash_uses_donor_hash4_formula() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(
        BTULTRA2_HC_CONFIG_L22,
        super::strategy::StrategyTag::BtUltra2,
        27,
    );
    let bytes = [b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h'];
    let read32 = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    let expected = ((read32.wrapping_mul(HC_PRIME4BYTES)) >> (32 - hc.table.hash_log)) as usize;
    let actual = super::match_table::storage::MatchTable::hash_position_with_mls(
        &bytes,
        hc.table.hash_log,
        super::bt::BtMatcher::HASH_MLS,
    );
    assert_eq!(actual, expected);
}

#[test]
fn row_candidate_returns_none_when_abs_pos_near_end_of_history() {
    let mut matcher = RowMatchGenerator::new(1 << 22);
    matcher.configure(ROW_CONFIG);
    // One byte short of the accept floor: from abs_pos 0 there are fewer
    // than `ROW_MIN_MATCH_LEN` bytes left, so the length gate in
    // `row_candidate` must short-circuit to `None` before touching the
    // (here unbuilt) row tables.
    matcher.history = alloc::vec![b'a'; ROW_MIN_MATCH_LEN - 1];
    matcher.history_start = 0;
    matcher.history_abs_start = 0;

    assert!(matcher.row_candidate(0, 0).is_none());
}

#[test]
fn hc_chain_candidates_returns_sentinels_for_short_suffix() {
    let mut hc = HcMatchGenerator::new(32);
    hc.table.history = b"abc".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;
    hc.table.ensure_tables();

    let candidates = hc.hc.chain_candidates(&hc.table, 0);
    assert!(candidates.iter().all(|&pos| pos == usize::MAX));
}

#[test]
fn hc_reset_advances_floor_past_prior_frame_entries() {
    use super::match_table::storage::MatchTable;
    let mut hc = HcMatchGenerator::new(32);
    hc.table.add_data(b"abcdeabcde".to_vec(), |_| {});
    hc.table.ensure_tables();
    // Populate real hash / chain entries for the first frame's positions.
    hc.table.insert_positions(0, 6);
    let prev_end = hc.table.history_abs_end();
    assert_eq!(prev_end, 10);
    assert!(hc.table.hash_table.iter().any(|&v| v != HC_EMPTY));

    hc.reset(|_| {});

    // Behavioural contract: the previous frame's entries are no longer
    // matchable. `reset` advances the floor past every prior position
    // instead of zeroing the tables, so each populated slot now decodes
    // to an absolute position strictly below `history_abs_start` and is
    // rejected by the `window_low` guard before any byte is read.
    assert_eq!(hc.table.history_abs_start, prev_end);
    for &slot in hc.table.hash_table.iter() {
        if let Some(candidate_abs) =
            MatchTable::stored_abs_position_fast(slot, hc.table.position_base, hc.table.index_shift)
        {
            assert!(
                candidate_abs < hc.table.history_abs_start,
                "a prior-frame entry must resolve below the advanced floor"
            );
        }
    }
}

#[test]
fn hc_reset_full_zeroes_when_floor_would_cross_ceiling() {
    use super::match_table::storage::REBASE_RESET_FLOOR_CEILING;
    let mut hc = HcMatchGenerator::new(32);
    hc.table.add_data(b"abcdeabcde".to_vec(), |_| {});
    hc.table.ensure_tables();
    hc.table.hash_table.fill(123);
    hc.table.chain_table.fill(456);
    // Push the would-be floor (`history_abs_end`) past the ceiling so
    // `reset` takes the bounded fallback: rewind to the origin and zero
    // the tables, keeping the absolute cursor from climbing toward
    // `usize::MAX` on 32-bit targets.
    hc.table.history_abs_start = REBASE_RESET_FLOOR_CEILING;

    hc.reset(|_| {});

    assert_eq!(hc.table.history_abs_start, 0);
    assert_eq!(hc.table.position_base, 0);
    assert!(hc.table.hash_table.iter().all(|&v| v == HC_EMPTY));
    assert!(hc.table.chain_table.iter().all(|&v| v == HC_EMPTY));
}

#[test]
fn hc_start_matching_returns_early_for_empty_current_block() {
    let mut hc = HcMatchGenerator::new(32);
    hc.table.add_data(Vec::new(), |_| {});
    let mut called = false;
    hc.start_matching(|_| called = true);
    assert!(!called, "empty current block should not emit sequences");
}

#[cfg(test)]
fn deterministic_high_entropy_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = seed;
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push((state >> 40) as u8);
    }
    out
}

#[cfg(test)]
fn level22_donor_block_ranges(data: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut cursor = 0usize;
    let mut savings = 0i64;
    while cursor < data.len() {
        let remaining = data.len() - cursor;
        let candidate_len = remaining.min(HC_BLOCKSIZE_MAX);
        let block_len = crate::encoding::frame_compressor::optimal_block_size(
            CompressionLevel::Level(22),
            &data[cursor..cursor + candidate_len],
            remaining,
            HC_BLOCKSIZE_MAX,
            savings,
        )
        .min(candidate_len)
        .max(1);
        ranges.push((cursor, block_len));
        cursor += block_len;
        // The exact donor gate uses compressed-size savings. For this corpus
        // parity harness, after the first full block has compressed, savings is
        // sufficient to authorize the same pre-block splitter path.
        if cursor >= HC_BLOCKSIZE_MAX {
            savings = 3;
        }
    }
    ranges
}

#[cfg(test)]
fn merge_block_delimiters_like_donor(
    sequences: Vec<(usize, usize, usize)>,
) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::with_capacity(sequences.len());
    let mut pending_lits = 0usize;
    for (lit_len, offset, match_len) in sequences {
        if offset == 0 && match_len == 0 {
            pending_lits = pending_lits.saturating_add(lit_len);
            continue;
        }
        out.push((lit_len.saturating_add(pending_lits), offset, match_len));
        pending_lits = 0;
    }
    if pending_lits > 0 {
        out.push((pending_lits, 0, 0));
    }
    out
}

#[cfg(test)]
fn collect_level22_sequences(data: &[u8]) -> Vec<(usize, usize, usize)> {
    merge_block_delimiters_like_donor(collect_level22_sequences_with_delimiters(data))
        .into_iter()
        .filter(|(_, offset, match_len)| *offset != 0 || *match_len != 0)
        .collect()
}

#[cfg(test)]
fn collect_level22_sequences_with_delimiters(data: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut driver = MatchGeneratorDriver::new(HC_BLOCKSIZE_MAX, 1);
    driver.set_source_size_hint(data.len() as u64);
    driver.reset(CompressionLevel::Level(22));

    let mut sequences = Vec::new();
    for (chunk_start, chunk_len) in level22_donor_block_ranges(data) {
        let chunk = &data[chunk_start..chunk_start + chunk_len];
        let mut space = driver.get_next_space();
        space[..chunk.len()].copy_from_slice(chunk);
        space.truncate(chunk.len());
        driver.commit_space(space);
        driver.start_matching(|seq| {
            let entry = match seq {
                Sequence::Literals { literals } => (literals.len(), 0usize, 0usize),
                Sequence::Triple {
                    literals,
                    offset,
                    match_len,
                } => (literals.len(), offset, match_len),
            };
            sequences.push(entry);
        });
    }
    sequences
}

#[cfg(test)]
fn donor_level22_sequences(data: &[u8]) -> Vec<(usize, usize, usize)> {
    merge_block_delimiters_like_donor(donor_level22_sequences_with_delimiters(data))
        .into_iter()
        .filter(|(_, offset, match_len)| *offset != 0 || *match_len != 0)
        .collect()
}

#[cfg(test)]
fn donor_level22_sequences_with_delimiters(data: &[u8]) -> Vec<(usize, usize, usize)> {
    use zstd::zstd_safe;
    use zstd::zstd_safe::zstd_sys;

    fn assert_zstd_ok(code: usize, context: &str) {
        assert_eq!(
            unsafe { zstd_sys::ZSTD_isError(code) },
            0,
            "{context} failed: {}",
            zstd_safe::get_error_name(code)
        );
    }

    unsafe {
        let cctx = zstd_sys::ZSTD_createCCtx();
        assert!(!cctx.is_null(), "ZSTD_createCCtx returned null");

        assert_zstd_ok(
            zstd_sys::ZSTD_CCtx_setParameter(
                cctx,
                zstd_sys::ZSTD_cParameter::ZSTD_c_compressionLevel,
                22,
            ),
            "ZSTD_c_compressionLevel",
        );

        let seq_capacity = zstd_safe::sequence_bound(data.len());
        let mut seqs = alloc::vec![
            zstd_sys::ZSTD_Sequence {
                offset: 0,
                litLength: 0,
                matchLength: 0,
                rep: 0,
            };
            seq_capacity
        ];

        let seq_count = zstd_sys::ZSTD_generateSequences(
            cctx,
            seqs.as_mut_ptr(),
            seqs.len(),
            data.as_ptr().cast(),
            data.len(),
        );
        assert_zstd_ok(seq_count, "ZSTD_generateSequences");
        let rc = zstd_sys::ZSTD_freeCCtx(cctx);
        assert_eq!(rc, 0, "ZSTD_freeCCtx failed");

        seqs.truncate(seq_count);
        seqs.into_iter()
            .map(|seq| {
                (
                    seq.litLength as usize,
                    seq.offset as usize,
                    seq.matchLength as usize,
                )
            })
            .collect()
    }
}

#[test]
fn level22_sequences_match_donor_on_corpus_proxy() {
    let data = include_bytes!("../../decodecorpus_files/z000033");
    assert_level22_sequences_match_donor(data);
}

#[test]
fn level22_sequences_match_donor_on_small_corpus_proxy() {
    let data = include_bytes!("../../decodecorpus_files/z000030");
    assert_level22_sequences_match_donor(data);
}

#[cfg(test)]
fn assert_level22_sequences_match_donor(data: &[u8]) {
    let rust = collect_level22_sequences(data);
    let donor = donor_level22_sequences(data);

    if rust != donor {
        let first_diff = rust
            .iter()
            .zip(donor.iter())
            .position(|(lhs, rhs)| lhs != rhs)
            .unwrap_or_else(|| rust.len().min(donor.len()));
        let rust_pos = rust
            .iter()
            .take(first_diff)
            .fold(0usize, |acc, seq| acc + seq.0 + seq.2);
        let donor_pos = donor
            .iter()
            .take(first_diff)
            .fold(0usize, |acc, seq| acc + seq.0 + seq.2);
        let start = first_diff.saturating_sub(4);
        let rust_window = &rust[start..rust.len().min(first_diff + 4)];
        let donor_window = &donor[start..donor.len().min(first_diff + 4)];
        let mut reps = [1u32, 4, 8];
        for (lit_len, offset, _) in rust.iter().take(first_diff) {
            let _ = encode_offset_with_history(*offset as u32, *lit_len as u32, &mut reps);
        }
        panic!(
            "level22 sequence path diverged at idx {}: rust={:?} donor={:?} (rust_len={} donor_len={} rust_pos={} donor_pos={} reps_before={:?} rust_window={:?} donor_window={:?} block_ranges={:?})",
            first_diff,
            rust.get(first_diff),
            donor.get(first_diff),
            rust.len(),
            donor.len(),
            rust_pos,
            donor_pos,
            reps,
            rust_window,
            donor_window,
            level22_donor_block_ranges(data)
                .into_iter()
                .filter(|(start, len)| *start <= rust_pos && rust_pos < start + len)
                .collect::<Vec<_>>(),
        );
    }
}

#[test]
fn hc_sparse_skip_matching_preserves_tail_cross_block_match() {
    let mut matcher = HcMatchGenerator::new(1 << 22);
    let tail = b"Qz9kLm2Rp";
    let mut first = deterministic_high_entropy_bytes(0xD1B5_4A32_9C77_0E19, 4096);
    let tail_start = first.len() - tail.len();
    first[tail_start..].copy_from_slice(tail);
    matcher.table.add_data(first.clone(), |_| {});
    matcher.skip_matching(Some(true));

    let mut second = tail.to_vec();
    second.extend_from_slice(b"after-tail-literals");
    matcher.table.add_data(second, |_| {});

    let mut first_sequence = None;
    matcher.start_matching(|seq| {
        if first_sequence.is_some() {
            return;
        }
        first_sequence = Some(match seq {
            Sequence::Literals { literals } => (literals.len(), 0usize, 0usize),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => (literals.len(), offset, match_len),
        });
    });

    let (literals_len, offset, match_len) =
        first_sequence.expect("expected at least one sequence after sparse skip");
    assert_eq!(
        literals_len, 0,
        "first sequence should start at block boundary"
    );
    assert_eq!(
        offset,
        tail.len(),
        "first match should reference previous tail"
    );
    assert!(
        match_len >= tail.len(),
        "tail-aligned cross-block match must be preserved"
    );
}

#[test]
fn btultra2_sparse_skip_matching_preserves_tail_cross_block_match() {
    let mut matcher = HcMatchGenerator::new(1 << 20);
    matcher.configure(
        BTULTRA2_HC_CONFIG_L22,
        super::strategy::StrategyTag::BtUltra2,
        20,
    );
    let tail = b"Bt9kLm2Rp";
    let mut first = deterministic_high_entropy_bytes(0xA9C3_7F21_D4E8_510B, 4096);
    let tail_start = first.len() - tail.len();
    first[tail_start..].copy_from_slice(tail);
    matcher.table.add_data(first, |_| {});
    matcher.skip_matching(Some(true));

    let mut second = tail.to_vec();
    second.extend_from_slice(b"after-tail-literals");
    matcher.table.add_data(second, |_| {});

    let mut first_sequence = None;
    matcher.start_matching(|seq| {
        if first_sequence.is_some() {
            return;
        }
        first_sequence = Some(match seq {
            Sequence::Literals { literals } => (literals.len(), 0usize, 0usize),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => (literals.len(), offset, match_len),
        });
    });

    let (literals_len, offset, match_len) =
        first_sequence.expect("expected at least one sequence after sparse BT skip");
    assert_eq!(
        literals_len, 0,
        "BT sparse skip should preserve an immediate boundary match"
    );
    assert_eq!(
        offset,
        tail.len(),
        "first BT match should reference previous tail"
    );
    assert!(
        match_len >= tail.len(),
        "BT sparse skip must seed the dense tail for cross-block matching"
    );
}

#[test]
fn hc_sparse_skip_matching_does_not_reinsert_sparse_tail_positions() {
    let mut matcher = HcMatchGenerator::new(1 << 22);
    let first = deterministic_high_entropy_bytes(0xC2B2_AE3D_27D4_EB4F, 4096);
    matcher.table.add_data(first.clone(), |_| {});
    matcher.skip_matching(Some(true));

    let current_len = first.len();
    let current_abs_start =
        matcher.table.history_abs_start + matcher.table.window_size - current_len;
    let current_abs_end = current_abs_start + current_len;
    let dense_tail = HC_MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
    let tail_start = current_abs_end
        .saturating_sub(dense_tail)
        .max(matcher.table.history_abs_start)
        .max(current_abs_start);

    let overlap_pos = (tail_start..current_abs_end)
        .find(|&pos| (pos - current_abs_start).is_multiple_of(INCOMPRESSIBLE_SKIP_STEP))
        .expect("fixture should contain at least one sparse-grid overlap in dense tail");

    let rel = matcher
        .table
        .relative_position(overlap_pos)
        .expect("overlap position should be representable as relative position");
    let chain_idx = rel as usize & ((1 << matcher.table.chain_log) - 1);
    assert_ne!(
        matcher.table.chain_table[chain_idx],
        rel + 1,
        "sparse-grid tail positions must not be reinserted (self-loop chain entry)"
    );
}

#[test]
fn hc_compact_history_drains_when_threshold_crossed() {
    let mut hc = HcMatchGenerator::new(8);
    hc.table.history = b"abcdefghijklmnopqrstuvwxyz".to_vec();
    hc.table.history_start = 16;
    hc.table.compact_history();
    assert_eq!(hc.table.history_start, 0);
    assert_eq!(hc.table.history, b"qrstuvwxyz");
}

#[test]
fn hc_insert_position_no_rebase_returns_when_relative_pos_unavailable() {
    let mut hc = HcMatchGenerator::new(32);
    hc.table.history = b"abcdefghijklmnop".to_vec();
    hc.table.history_abs_start = 0;
    hc.table.position_base = 1;
    hc.table.ensure_tables();
    let before_hash = hc.table.hash_table.clone();
    let before_chain = hc.table.chain_table.clone();

    hc.table.insert_position_no_rebase(0);

    assert_eq!(hc.table.hash_table, before_hash);
    assert_eq!(hc.table.chain_table, before_chain);
}

#[test]
fn hc_insert_positions_advances_next_to_update3_for_contiguous_range() {
    let mut hc = HcMatchGenerator::new(64);
    hc.table.history = b"abcdefghijklmnopqrstuvwxyz".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;
    hc.table.position_base = 0;
    hc.table.ensure_tables();
    hc.table.next_to_update3 = 0;

    hc.table.insert_positions(0, 9);

    assert_eq!(
        hc.table.next_to_update3, 9,
        "contiguous insert_positions should advance hash3 update cursor"
    );
}

#[test]
fn hc_insert_positions_with_step_keeps_next_to_update3_cursor_for_sparse_ranges() {
    let mut hc = HcMatchGenerator::new(64);
    hc.table.history = b"abcdefghijklmnopqrstuvwxyz".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;
    hc.table.position_base = 0;
    hc.table.ensure_tables();
    hc.table.next_to_update3 = 0;

    hc.table.insert_positions_with_step(0, 16, 4);

    assert_eq!(
        hc.table.next_to_update3, 0,
        "sparse insert_positions_with_step must not mark skipped positions as hash3-updated"
    );
}

#[cfg(any())]
// disabled: tests legacy SuffixStore behavior incompatible with donor-shape kernel's HASH_READ_SIZE geometry
#[test]
fn prime_with_dictionary_budget_shrinks_after_simple_eviction() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);
    // Use a small live window so dictionary-primed slices are evicted
    // quickly and budget retirement can be asserted deterministically.
    driver.simple_mut().max_window_size = 8;
    driver.reported_window_size = 8;

    let base_window = driver.simple_mut().max_window_size;
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    assert_eq!(driver.simple_mut().max_window_size, base_window + 24);

    for block in [b"AAAAAAAA", b"BBBBBBBB"] {
        let mut space = driver.get_next_space();
        space.clear();
        space.extend_from_slice(block);
        driver.commit_space(space);
        driver.skip_matching_with_hint(None);
    }

    assert_eq!(
        driver.dictionary_retained_budget, 0,
        "dictionary budget should be fully retired once primed dict slices are evicted"
    );
    assert_eq!(
        driver.simple_mut().max_window_size,
        base_window,
        "retired dictionary budget must not remain reusable for live history"
    );
}

#[test]
fn prime_with_dictionary_budget_shrinks_after_dfast_eviction() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(3));
    // Use a small live window in this regression so dictionary-primed slices are
    // evicted quickly and budget retirement can be asserted deterministically.
    driver.dfast_matcher_mut().max_window_size = 8;
    driver.reported_window_size = 8;

    let base_window = driver.dfast_matcher().max_window_size;
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    assert_eq!(driver.dfast_matcher().max_window_size, base_window + 24);

    for block in [b"AAAAAAAA", b"BBBBBBBB"] {
        let mut space = driver.get_next_space();
        space.clear();
        space.extend_from_slice(block);
        driver.commit_space(space);
        driver.skip_matching_with_hint(None);
    }

    assert_eq!(
        driver.dictionary_retained_budget, 0,
        "dictionary budget should be fully retired once primed dict slices are evicted"
    );
    assert_eq!(
        driver.dfast_matcher().max_window_size,
        base_window,
        "retired dictionary budget must not remain reusable for live history"
    );
}

#[test]
fn hc_prime_with_dictionary_preserves_history_for_first_full_block() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    // Route onto HashChain explicitly — `Better` resolves to the Row
    // backend in production, and this test pins HC dict-prime behaviour.
    driver.reset_on_hc_lazy(CompressionLevel::Better);

    driver.prime_with_dictionary(b"abcdefgh", [1, 4, 8]);

    let mut space = driver.get_next_space();
    space.clear();
    // Repeat the dictionary content so the HC matcher can find it.
    // HC_MIN_MATCH_LEN is 5, so an 8-byte match is well above threshold.
    space.extend_from_slice(b"abcdefgh");
    driver.commit_space(space);

    let mut saw_match = false;
    driver.start_matching(|seq| {
        if let Sequence::Triple {
            literals,
            offset,
            match_len,
        } = seq
            && literals.is_empty()
            && offset == 8
            && match_len >= HC_MIN_MATCH_LEN
        {
            saw_match = true;
        }
    });

    assert!(
        saw_match,
        "hash-chain backend should match dictionary-primed history in first full block"
    );
}

#[test]
fn prime_with_dictionary_budget_shrinks_after_hc_eviction() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset_on_hc_lazy(CompressionLevel::Better);
    // Use a small live window so dictionary-primed slices are evicted quickly.
    driver.hc_matcher_mut().table.max_window_size = 8;
    driver.reported_window_size = 8;

    let base_window = driver.hc_matcher().table.max_window_size;
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    assert_eq!(driver.hc_matcher().table.max_window_size, base_window + 24);

    for block in [b"AAAAAAAA", b"BBBBBBBB"] {
        let mut space = driver.get_next_space();
        space.clear();
        space.extend_from_slice(block);
        driver.commit_space(space);
        driver.skip_matching_with_hint(None);
    }

    assert_eq!(
        driver.dictionary_retained_budget, 0,
        "dictionary budget should be fully retired once primed dict slices are evicted"
    );
    assert_eq!(
        driver.hc_matcher().table.max_window_size,
        base_window,
        "retired dictionary budget must not remain reusable for live history"
    );
}

#[test]
fn hc_commit_without_eviction_retires_no_dictionary_budget() {
    // Regression: after the window<->history dedup, MatchTable::add_data
    // invokes its reuse_space callback for the *input* buffer (recycle),
    // not for evicted chunks. The HC arm of commit_space must therefore
    // derive eviction bytes from the window_size delta — counting the
    // callback argument as evicted would charge the whole committed block
    // as "evicted" and prematurely retire dictionary budget even when the
    // window is nowhere near full.
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset_on_hc_lazy(CompressionLevel::Better);
    // A large live window so a small committed block evicts nothing.
    driver.hc_matcher_mut().table.max_window_size = 1 << 20;
    driver.reported_window_size = 1 << 20;
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    let budget_after_prime = driver.dictionary_retained_budget;
    assert!(
        budget_after_prime > 0,
        "priming must retain a non-zero dictionary budget"
    );

    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(b"AAAAAAAA");
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);

    assert_eq!(
        driver.dictionary_retained_budget, budget_after_prime,
        "a commit that evicts nothing must retire no dictionary budget"
    );
}

#[test]
fn row_commit_without_eviction_retires_no_dictionary_budget() {
    // Regression for the Row arm of commit_space after the window ->
    // chunk_lens migration: RowMatchGenerator::add_data now invokes its
    // reuse_space callback for the *input* buffer (per-commit recycle),
    // not for evicted chunks. The Row arm must derive eviction bytes from
    // the window_size delta like the Dfast / HashChain arms — counting the
    // callback argument as evicted charges the whole committed block as
    // "evicted" and prematurely retires dictionary budget even when the
    // window is nowhere near full.
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(5));
    assert!(matches!(driver.storage, MatcherStorage::Row(_)));
    // A large live window so a small committed block evicts nothing.
    driver.row_matcher_mut().max_window_size = 1 << 20;
    driver.reported_window_size = 1 << 20;
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    let budget_after_prime = driver.dictionary_retained_budget;
    assert!(
        budget_after_prime > 0,
        "priming must retain a non-zero dictionary budget"
    );

    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(b"AAAAAAAA");
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);

    assert_eq!(
        driver.dictionary_retained_budget, budget_after_prime,
        "a Row commit that evicts nothing must retire no dictionary budget"
    );
}

#[test]
fn hc_rebases_positions_after_u32_boundary() {
    let mut matcher = HcMatchGenerator::new(64);
    matcher.table.add_data(b"abcdeabcdeabcde".to_vec(), |_| {});
    matcher.table.ensure_tables();
    matcher.table.position_base = 0;
    let history_abs_start: usize = match (u64::from(u32::MAX) + 64).try_into() {
        Ok(value) => value,
        Err(_) => return,
    };
    // Simulate a long-running stream where absolute history positions crossed
    // the u32 range. Before #51 this disabled HC inserts entirely.
    matcher.table.history_abs_start = history_abs_start;
    matcher.skip_matching(None);
    assert_eq!(
        matcher.table.position_base, matcher.table.history_abs_start,
        "rebase should anchor to the oldest live absolute position"
    );

    assert!(
        matcher
            .table
            .hash_table
            .iter()
            .any(|entry| *entry != HC_EMPTY),
        "HC hash table should still be populated after crossing u32 boundary"
    );

    // Verify rebasing preserves candidate lookup, not just table population.
    let abs_pos = matcher.table.history_abs_start + 10;
    let candidates = matcher.hc.chain_candidates(&matcher.table, abs_pos);
    assert!(
        candidates.iter().any(|candidate| *candidate != usize::MAX),
        "chain_candidates should return valid matches after rebase"
    );
}

// 64-bit only: the >4 GiB absolute cursor this test fabricates cannot exist on
// a 32-bit target (usize == u32 can't address that much), and setting
// `history_abs_start` near `u32::MAX` there overflows `usize` in the
// `check_stream_abs_headroom` guard before the rebase path is reached. Mirrors
// the `try_into()` early-return guard on `hc_rebases_positions_after_u32_boundary`.
#[cfg(target_pointer_width = "64")]
#[test]
fn row_rebases_positions_after_u32_boundary() {
    // Row stores absolute match positions as u32. On a long stream the
    // cumulative absolute cursor crosses the u32 range even while the live
    // window stays bounded; `add_data` must rebase the coordinate origin
    // down to the oldest live byte instead of asserting. Before the rebase
    // landed this panicked on the `< u32::MAX` assertion, dropping valid
    // long Row-backed frames.
    let mut m = RowMatchGenerator::new(64);
    m.add_data(b"abcdeabcdeabcde".to_vec(), |_| {});

    // Simulate ~4 GiB of stream behind a bounded window: the live bytes now
    // sit just under the u32 absolute ceiling.
    let near_ceiling = (u32::MAX as usize) - 16;
    m.history_abs_start = near_ceiling;

    // The next commit would push a u32 position past the ceiling; add_data
    // must rebase the origin rather than panic.
    m.add_data(b"fghij".to_vec(), |_| {});

    assert!(
        m.history_abs_start < near_ceiling,
        "add_data must rebase the absolute origin down when the cursor nears \
         u32::MAX (got {})",
        m.history_abs_start
    );
    assert!(
        (m.history_abs_start + m.window_size) < u32::MAX as usize,
        "after rebase the live window must fit below the u32 position ceiling"
    );
}

#[test]
fn hc_rebase_rebuilds_only_inserted_prefix() {
    let mut matcher = HcMatchGenerator::new(64);
    matcher.table.add_data(b"abcdeabcdeabcde".to_vec(), |_| {});
    matcher.table.ensure_tables();
    matcher.table.position_base = 0;
    let history_abs_start: usize = match (u64::from(u32::MAX) + 64).try_into() {
        Ok(value) => value,
        Err(_) => return,
    };
    matcher.table.history_abs_start = history_abs_start;
    let abs_pos = matcher.table.history_abs_start + 6;

    let mut expected = HcMatchGenerator::new(64);
    expected.table.add_data(b"abcdeabcdeabcde".to_vec(), |_| {});
    expected.table.ensure_tables();
    expected.table.history_abs_start = history_abs_start;
    expected.table.position_base = expected.table.history_abs_start;
    expected.table.hash_table.fill(HC_EMPTY);
    expected.table.chain_table.fill(HC_EMPTY);
    for pos in expected.table.history_abs_start..abs_pos {
        expected.table.insert_position_no_rebase(pos);
    }

    matcher.table.maybe_rebase_positions(abs_pos);

    assert_eq!(
        matcher.table.position_base, matcher.table.history_abs_start,
        "rebase should still anchor to the oldest live absolute position"
    );
    assert_eq!(
        matcher.table.hash_table, expected.table.hash_table,
        "rebase must rebuild only positions already inserted before abs_pos"
    );
    assert_eq!(
        matcher.table.chain_table, expected.table.chain_table,
        "future positions must not be pre-seeded into HC chains during rebase"
    );
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn suffix_store_with_single_slot_does_not_panic_on_keying() {
    let mut suffixes = SuffixStore::with_capacity(1);
    suffixes.insert(b"abcde", 0);
    assert!(suffixes.contains_key(b"abcde"));
    assert_eq!(suffixes.get(b"abcde"), Some(0));
}

#[cfg(any())]
// disabled: hash_fill_step is a legacy MatchGenerator field; FastKernelMatcher walks stride=1 today
#[test]
fn fastest_reset_uses_interleaved_hash_fill_step() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Uncompressed);
    assert_eq!(driver.simple().hash_fill_step, 1);

    driver.reset(CompressionLevel::Fastest);
    assert_eq!(driver.simple().hash_fill_step, FAST_HASH_FILL_STEP);

    // Better uses the HashChain backend with lazy2; verify that the backend switch
    // happened and the lazy_depth is configured correctly.
    driver.reset(CompressionLevel::Better);
    assert_eq!(
        driver.active_backend(),
        super::strategy::BackendTag::HashChain
    );
    assert_eq!(driver.window_size(), (1u64 << 23));
    assert_eq!(driver.hc_matcher().hc.lazy_depth, 2);
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_matcher_updates_offset_history_after_emitting_match() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdeabcdeabcde".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );

    assert!(matcher.next_sequence(|seq| {
        assert_eq!(
            seq,
            Sequence::Triple {
                literals: b"abcde",
                offset: 5,
                match_len: 10,
            }
        );
    }));
    assert_eq!(matcher.offset_hist, [5, 1, 4]);
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_matcher_zero_literal_repcode_checks_rep1_before_hash_lookup() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdefghijabcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );

    matcher.suffix_idx = 10;
    matcher.last_idx_in_sequence = 10;
    matcher.offset_hist = [99, 10, 4];

    let candidate = matcher.repcode_candidate(&matcher.window.last().unwrap().data[10..], 0);
    assert_eq!(candidate, Some((10, 10)));
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_matcher_repcode_can_target_previous_window_entry() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.skip_matching();
    matcher.add_data(
        b"abcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );

    matcher.offset_hist = [99, 10, 4];

    let candidate = matcher.repcode_candidate(&matcher.window.last().unwrap().data, 0);
    assert_eq!(candidate, Some((10, 10)));
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_matcher_zero_literal_repcode_checks_rep2() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdefghijabcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.suffix_idx = 10;
    matcher.last_idx_in_sequence = 10;
    // rep1=4 does not match at idx 10, rep2=10 does.
    matcher.offset_hist = [99, 4, 10];

    let candidate = matcher.repcode_candidate(&matcher.window.last().unwrap().data[10..], 0);
    assert_eq!(candidate, Some((10, 10)));
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_matcher_zero_literal_repcode_checks_rep0_minus1() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdefghijabcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.suffix_idx = 10;
    matcher.last_idx_in_sequence = 10;
    // rep1=4 and rep2=99 do not match; rep0-1 == 10 does.
    matcher.offset_hist = [11, 4, 99];

    let candidate = matcher.repcode_candidate(&matcher.window.last().unwrap().data[10..], 0);
    assert_eq!(candidate, Some((10, 10)));
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_matcher_repcode_rejects_offsets_beyond_searchable_prefix() {
    let mut matcher = MatchGenerator::new(64);
    matcher.add_data(
        b"abcdefghij".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.skip_matching();
    matcher.add_data(
        b"klmnopqrst".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.suffix_idx = 3;

    let candidate = matcher.offset_match_len(14, &matcher.window.last().unwrap().data[3..]);
    assert_eq!(candidate, None);
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_matcher_skip_matching_seeds_every_position_even_with_fast_step() {
    let mut matcher = MatchGenerator::new(64);
    matcher.hash_fill_step = FAST_HASH_FILL_STEP;
    matcher.add_data(
        b"abcdefghijklmnop".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.skip_matching();
    matcher.add_data(b"bcdef".to_vec(), SuffixStore::with_capacity(64), |_, _| {});

    assert!(matcher.next_sequence(|seq| {
        assert_eq!(
            seq,
            Sequence::Triple {
                literals: b"",
                offset: 15,
                match_len: 5,
            }
        );
    }));
    assert!(!matcher.next_sequence(|_| {}));
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_matcher_skip_matching_with_incompressible_hint_uses_sparse_prefix() {
    let mut matcher = MatchGenerator::new(128);
    let first = b"abcdefghijklmnopqrstuvwxyz012345".to_vec();
    let sparse_probe = first[3..3 + MIN_MATCH_LEN].to_vec();
    let tail_start = first.len() - MIN_MATCH_LEN;
    let tail_probe = first[tail_start..tail_start + MIN_MATCH_LEN].to_vec();
    matcher.add_data(first, SuffixStore::with_capacity(256), |_, _| {});

    matcher.skip_matching_with_hint(Some(true));

    // Observable behavior check: sparse-prefix probe should not immediately match.
    matcher.add_data(sparse_probe, SuffixStore::with_capacity(256), |_, _| {});
    let mut sparse_first_is_literals = None;
    assert!(matcher.next_sequence(|seq| {
        if sparse_first_is_literals.is_none() {
            sparse_first_is_literals = Some(matches!(seq, Sequence::Literals { .. }));
        }
    }));
    assert!(
        sparse_first_is_literals.unwrap_or(false),
        "sparse-start probe should not produce an immediate match"
    );

    // Dense tail remains indexed for cross-block boundary matching.
    let mut matcher = MatchGenerator::new(128);
    matcher.add_data(
        b"abcdefghijklmnopqrstuvwxyz012345".to_vec(),
        SuffixStore::with_capacity(256),
        |_, _| {},
    );
    matcher.skip_matching_with_hint(Some(true));
    matcher.add_data(tail_probe, SuffixStore::with_capacity(256), |_, _| {});
    let mut tail_first_is_immediate_match = None;
    assert!(matcher.next_sequence(|seq| {
        if tail_first_is_immediate_match.is_none() {
            tail_first_is_immediate_match =
                Some(matches!(seq, Sequence::Triple { literals, .. } if literals.is_empty()));
        }
    }));
    assert!(
        tail_first_is_immediate_match.unwrap_or(false),
        "dense tail probe should match immediately at block start"
    );
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_matcher_add_suffixes_till_backfills_last_searchable_anchor() {
    let mut matcher = MatchGenerator::new(64);
    matcher.hash_fill_step = FAST_HASH_FILL_STEP;
    matcher.add_data(
        b"01234abcde".to_vec(),
        SuffixStore::with_capacity(64),
        |_, _| {},
    );
    matcher.add_suffixes_till(10, FAST_HASH_FILL_STEP);

    let last = matcher.window.last().unwrap();
    let tail = &last.data[5..10];
    assert_eq!(last.suffixes.get(tail), Some(5));
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_matcher_add_suffixes_till_skips_when_idx_below_min_match_len() {
    let mut matcher = MatchGenerator::new(128);
    matcher.hash_fill_step = FAST_HASH_FILL_STEP;
    matcher.add_data(
        b"abcdefghijklmnopqrstuvwxyz".to_vec(),
        SuffixStore::with_capacity(1 << 16),
        |_, _| {},
    );

    matcher.add_suffixes_till(MIN_MATCH_LEN - 1, FAST_HASH_FILL_STEP);

    let last = matcher.window.last().unwrap();
    let first_key = &last.data[..MIN_MATCH_LEN];
    assert_eq!(last.suffixes.get(first_key), None);
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn simple_matcher_add_suffixes_till_fast_step_registers_interleaved_positions() {
    let mut matcher = MatchGenerator::new(128);
    matcher.hash_fill_step = FAST_HASH_FILL_STEP;
    matcher.add_data(
        b"abcdefghijklmnopqrstuvwxyz".to_vec(),
        SuffixStore::with_capacity(1 << 16),
        |_, _| {},
    );

    matcher.add_suffixes_till(17, FAST_HASH_FILL_STEP);

    let last = matcher.window.last().unwrap();
    for pos in [0usize, 3, 6, 9, 12] {
        let key = &last.data[pos..pos + MIN_MATCH_LEN];
        assert_eq!(
            last.suffixes.get(key),
            Some(pos),
            "expected interleaved suffix registration at pos {pos}"
        );
    }
}

#[test]
fn dfast_skip_matching_handles_window_eviction() {
    let mut matcher = DfastMatchGenerator::new(16);

    matcher.add_data(alloc::vec![1, 2, 3, 4, 5, 6], |_| {});
    matcher.skip_matching(None);
    matcher.add_data(alloc::vec![7, 8, 9, 10, 11, 12], |_| {});
    matcher.skip_matching(None);
    matcher.add_data(alloc::vec![7, 8, 9, 10, 11, 12], |_| {});

    let mut reconstructed = alloc::vec![7, 8, 9, 10, 11, 12];
    matcher.start_matching(|seq| match seq {
        Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            reconstructed.extend_from_slice(literals);
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    });

    assert_eq!(reconstructed, [7, 8, 9, 10, 11, 12, 7, 8, 9, 10, 11, 12]);
}

#[test]
fn dfast_add_data_callback_reports_evicted_len_not_capacity() {
    let mut matcher = DfastMatchGenerator::new(8);

    let mut first = Vec::with_capacity(64);
    first.extend_from_slice(b"abcdefgh");
    matcher.add_data(first, |_| {});

    let mut second = Vec::with_capacity(64);
    second.extend_from_slice(b"ijklmnop");

    let mut observed_evicted_len = None;
    matcher.add_data(second, |data| {
        observed_evicted_len = Some(data.len());
    });

    assert_eq!(
        observed_evicted_len,
        Some(8),
        "eviction callback must report evicted byte length, not backing capacity"
    );
}

/// Regression for the `commit_space` Dfast-branch eviction accounting bug
/// (CodeRabbit Critical on PR #146). Old code counted the INPUT buffer
/// length as `evicted_bytes` because Dfast's `add_data` callback receives
/// the input `Vec<u8>` for pool recycling (Dfast stores bytes in `history`,
/// not per-block Vecs). On the saturated-window 1:1 path the two coincide
/// so the previous test fixture passed by accident; this test forces the
/// divergent case where evicted != input by sequencing block lengths
/// `[4, 4, 5]` against `max_window_size = 10`:
///
///   * after 1st commit: `window_blocks = [4]`, `window_size = 4`
///   * after 2nd commit: `window_blocks = [4, 4]`, `window_size = 8`
///   * 3rd commit (5 bytes): `8 + 5 > 10` → pop one 4-byte block (evict=4),
///     then push 5 (window_size=9). Bug counts `5`, fix counts `4`.
///
/// The fix derives eviction from `window_size` delta + input length:
/// `evicted = pre + space_len - post`. Verified via the
/// `dictionary_retained_budget` observable: starting budget 100, after
/// the third commit (4 bytes actually evicted) the budget must read 96,
/// not 95.
/// Driver-path regression for the `commit_space` Dfast eviction accounting
/// bug. Exercises `MatchGeneratorDriver::commit_space` directly (not just
/// `DfastMatchGenerator::add_data`) so the assertion catches a future
/// regression that swaps the Dfast branch in `commit_space` back to
/// `evicted_bytes += data.len()` — the older draft of this regression
/// hand-recomputed the formula on the matcher and would pass either way.
///
/// Fixture: `max_window_size = 10`, commit sequence `[4, 4, 5]`. The
/// divergent case where the popped block (4 bytes) and the new input
/// (5 bytes) have different sizes:
///
///   * after commit `"abcd"` (4 B): window_blocks=[4], ws=4
///   * after commit `"efgh"` (4 B): window_blocks=[4,4], ws=8
///   * commit `"ijklm"` (5 B): 8+5>10 → pop front [4] (evict=4),
///     push 5 → window_blocks=[4,5], ws=9
///
/// `commit_space` then calls `retire_dictionary_budget(evicted)`. With
/// the fix `evicted=4`; with the bug it would be `evicted=5`. The
/// downstream `trim_after_budget_retire` cascade (which fires whenever
/// `retire_dictionary_budget` returns true) drives the budget further
/// down by trimming the now-oversize window; the final
/// `dictionary_retained_budget` differs between the two paths because
/// the cascade starting state differs (max_window_size after first
/// retire is `10 - evicted`).
///
/// Tracing the fix path end-to-end with starting budget = 100:
///   1st commit: evicted=0, no retire.
///   2nd commit: evicted=0, no retire.
///   3rd commit: evicted=4. retire(4) → budget=96, max_window=6.
///     trim_after_budget_retire:
///       iter1: ws=9 > max=6, pop [4] → ws=5, evicted=4.
///              retire(4) → budget=92, max_window=2.
///       iter2: ws=5 > max=2, pop [5] → ws=0, evicted=5.
///              retire(5) → budget=87, max_window=0.
///       iter3: ws=0, no trim, retire(0) → false, exit.
///   Final budget = 87. Final max_window_size = 0.
///
/// In the buggy path the 3rd commit would compute `evicted=5`, retire
/// would reclaim 5 instead of 4, shrinking max_window_size to 5
/// instead of 6 — and then the cascade arithmetic produces a
/// different final budget (and on the 2nd commit the cascade would
/// already have shrunk max_window_size to 0, causing the 3rd commit
/// to panic on `data.len() <= max_window_size`). Either way the
/// regression surfaces as a test failure.
#[test]
fn dfast_commit_space_eviction_uses_window_size_delta() {
    use crate::encoding::CompressionLevel;

    let mut driver = MatchGeneratorDriver::new(10, 1);
    driver.reset(CompressionLevel::Level(3));
    assert!(matches!(driver.storage, MatcherStorage::Dfast(_)));

    // Override the level-derived window with a tiny one so the
    // 4 + 4 + 5 = 13 commit sequence below actually crosses the
    // boundary. A 16 KiB+ default window would never evict on this
    // little data and the bug would stay invisible.
    driver.dfast_matcher_mut().max_window_size = 10;
    driver.dictionary_retained_budget = 100;

    let mut space1 = Vec::with_capacity(64);
    space1.extend_from_slice(b"abcd");
    driver.commit_space(space1);
    assert_eq!(
        driver.dictionary_retained_budget, 100,
        "1st commit fills window 0 → 4, no eviction, no retire"
    );

    let mut space2 = Vec::with_capacity(64);
    space2.extend_from_slice(b"efgh");
    driver.commit_space(space2);
    assert_eq!(
        driver.dictionary_retained_budget, 100,
        "2nd commit fills window 4 → 8, no eviction, no retire"
    );

    let mut space3 = Vec::with_capacity(64);
    space3.extend_from_slice(b"ijklm");
    driver.commit_space(space3);
    assert_eq!(
        driver.dictionary_retained_budget, 87,
        "3rd commit + trim_after_budget_retire cascade. With the fix \
         (evicted=4 from window_size delta) the cascade reclaims 100 \
         → 96 → 92 → 87. With the bug (evicted=5 from data.len()) the \
         3rd commit would panic on `data.len() <= max_window_size` \
         after the 2nd commit's cascade had already shrunk \
         max_window_size to 0."
    );
    assert_eq!(
        driver.dfast_matcher_mut().max_window_size,
        0,
        "cascade drains max_window_size to 0 once budget reclaim \
         exceeds the initial window size"
    );
}

#[test]
fn dfast_trim_to_window_evicts_oldest_block_by_length() {
    // After the history-only storage refactor (#111 Phase 7c step 3),
    // Dfast no longer retains input `Vec<u8>`s — the `history`
    // contiguous buffer is the sole byte store, and `add_data`
    // returns the input Vec to the caller's pool eagerly. So
    // `trim_to_window` doesn't have anything to hand back to the
    // closure (no Vec exists to give). The eviction is observable
    // instead through `window_size` shrinking by the per-block
    // length recorded in `window_blocks`.
    let mut matcher = DfastMatchGenerator::new(16);

    let mut first = Vec::with_capacity(64);
    first.extend_from_slice(b"abcdefgh");
    matcher.add_data(first, |_| {});

    let mut second = Vec::with_capacity(64);
    second.extend_from_slice(b"ijklmnop");
    matcher.add_data(second, |_| {});

    assert_eq!(matcher.window_size, 16);
    assert_eq!(matcher.window_blocks.len(), 2);

    matcher.max_window_size = 8;

    matcher.trim_to_window();

    // No callback signature to assert on: the Dfast variant of
    // `trim_to_window` takes none. That signature shape (vs HC/Row
    // which accept `impl FnMut(Vec<u8>)`) is the property locking in
    // the contract — there is no closure to invoke or skip, so no
    // future change can "start invoking the callback" without a
    // compile-time signature break that the dispatcher and this test
    // would force the author to address.
    assert_eq!(
        matcher.window_size, 8,
        "exactly one 8-byte block must remain"
    );
    assert_eq!(matcher.window_blocks.len(), 1);
    assert_eq!(matcher.history_abs_start, 8);
}

#[test]
fn dfast_inserts_tail_positions_for_next_block_matching() {
    let mut matcher = DfastMatchGenerator::new(1 << 22);

    matcher.add_data(b"012345bcdea".to_vec(), |_| {});
    let mut history = Vec::new();
    matcher.start_matching(|seq| match seq {
        Sequence::Literals { literals } => history.extend_from_slice(literals),
        Sequence::Triple { .. } => unreachable!("first block should not match history"),
    });
    assert_eq!(history, b"012345bcdea");

    matcher.add_data(b"bcdeabcdeab".to_vec(), |_| {});
    let mut saw_first_sequence = false;
    matcher.start_matching(|seq| {
        assert!(!saw_first_sequence, "expected a single cross-block match");
        saw_first_sequence = true;
        match seq {
            Sequence::Literals { .. } => {
                panic!("expected tail-anchored cross-block match before any literals")
            }
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => {
                assert_eq!(literals, b"");
                assert_eq!(offset, 5);
                assert_eq!(match_len, 11);
                let start = history.len() - offset;
                for i in 0..match_len {
                    let byte = history[start + i];
                    history.push(byte);
                }
            }
        }
    });

    assert!(
        saw_first_sequence,
        "expected tail-anchored cross-block match"
    );
    assert_eq!(history, b"012345bcdeabcdeabcdeab");
}

/// Regression for #49 — locks down `MatchTable::backfill_boundary_positions`
/// for the [`HcMatchGenerator`] lazy path. `backfill_boundary_positions`
/// seeds ONLY the last `< 4` bytes of the previous slice (positions in
/// `[current_abs_start - 3, current_abs_start)`) — the bytes that
/// `insert_position` could not hash at the time because hashing needs
/// 4 bytes of lookahead. The existing 8 MiB window roundtrip test
/// exercises cross-slice behaviour end-to-end, but does not isolate
/// the backfill of those final 1-3 unhashable bytes.
///
/// Fixture is built so the cross-block match's candidate position
/// MUST lie in `[block_1_end - 3, block_1_end)`:
///
/// - Block 1 = `b"PQRSTBCD"` (8 bytes). Block 1's `start_matching`
///   hashes positions 0..=4 (each has 4 bytes of forward context);
///   positions 5/6/7 are the unhashable tail.
/// - Block 2 = `b"BCDBCDBCDB"` (10 bytes). At absolute position 8
///   (block 2 start) the 4-byte window is `b"BCDB"`. The ONLY place
///   `b"BCDB"` was inserted in the hash + chain tables is position 5
///   — via `backfill_boundary_positions` on the next-slice entry
///   (the 4-byte window at position 5 is `data[5..9] = b"BCD" +
///   block_2[0] = b"BCDB"`).
///
/// If `backfill_boundary_positions` regresses, position 5 is never
/// hashed, position 8's lookup misses, and the lazy parser falls
/// through to a leading literals run — `offset == 3, match_len >= 4`
/// would no longer hold.
#[test]
fn hashchain_inserts_tail_positions_for_next_block_matching() {
    let mut matcher = HcMatchGenerator::new(1 << 22);
    matcher.configure(HC_CONFIG, super::strategy::StrategyTag::Lazy, 22);

    matcher.table.add_data(b"PQRSTBCD".to_vec(), |_| {});
    let mut history = alloc::vec::Vec::new();
    matcher.start_matching(|seq| match seq {
        Sequence::Literals { literals } => history.extend_from_slice(literals),
        Sequence::Triple { .. } => unreachable!("first block has no internal repeats"),
    });
    assert_eq!(history, b"PQRSTBCD");

    matcher.table.add_data(b"BCDBCDBCDB".to_vec(), |_| {});
    let mut first_sequence_offset: Option<usize> = None;
    let mut first_sequence_match_len: Option<usize> = None;
    matcher.start_matching(|seq| {
        if first_sequence_offset.is_some() {
            return;
        }
        match seq {
            Sequence::Literals { .. } => {
                panic!(
                    "expected tail-anchored cross-block match before any literals — \
                     backfill_boundary_positions did not seed positions 5/6/7"
                )
            }
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => {
                assert_eq!(literals, b"", "no leading literals on the boundary match");
                first_sequence_offset = Some(offset);
                first_sequence_match_len = Some(match_len);
            }
        }
    });

    let offset = first_sequence_offset.expect(
        "expected tail-anchored cross-block match emitted from backfill_boundary_positions",
    );
    assert!(
        (1..=3).contains(&offset),
        "boundary match offset {offset} must point into the unhashable tail \
         (positions 5/6/7 of an 8-byte block 1) so the test specifically \
         locks down backfill_boundary_positions",
    );
    assert_eq!(
        offset, 3,
        "candidate position must land at 5 (= block_1_len - 3) so the 4-byte \
         window `data[5..9] = b\"BCDB\"` matches block 2's first hash lookup",
    );
    let match_len = first_sequence_match_len.unwrap();
    assert!(
        match_len >= HC_MIN_MATCH_LEN,
        "match_len {match_len} must clear the HC min-match floor",
    );
}

#[test]
fn dfast_dense_skip_matching_backfills_previous_tail_for_next_block() {
    let mut matcher = DfastMatchGenerator::new(1 << 22);
    let tail = b"Qz9kLm2Rp";
    let mut first = b"0123456789abcdef".to_vec();
    first.extend_from_slice(tail);
    matcher.add_data(first.clone(), |_| {});
    matcher.skip_matching(Some(false));

    let mut second = tail.to_vec();
    second.extend_from_slice(b"after-tail-literals");
    matcher.add_data(second, |_| {});

    let mut first_sequence = None;
    matcher.start_matching(|seq| {
        if first_sequence.is_some() {
            return;
        }
        first_sequence = Some(match seq {
            Sequence::Literals { literals } => (literals.len(), 0usize, 0usize),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => (literals.len(), offset, match_len),
        });
    });

    let (lit_len, offset, match_len) = first_sequence.expect("expected at least one sequence");
    assert_eq!(
        lit_len, 0,
        "expected immediate cross-block match at block start"
    );
    assert_eq!(
        offset,
        tail.len(),
        "expected dense skip to preserve cross-boundary tail match"
    );
    assert!(
        match_len >= DFAST_MIN_MATCH_LEN,
        "match length should satisfy dfast minimum match length"
    );
}

#[test]
fn dfast_sparse_skip_matching_preserves_tail_cross_block_match() {
    let mut matcher = DfastMatchGenerator::new(1 << 22);
    let tail = b"Qz9kLm2Rp";
    let mut first = deterministic_high_entropy_bytes(0x9E37_79B9_7F4A_7C15, 4096);
    let tail_start = first.len() - tail.len();
    first[tail_start..].copy_from_slice(tail);
    matcher.add_data(first.clone(), |_| {});

    matcher.skip_matching(Some(true));

    let mut second = tail.to_vec();
    second.extend_from_slice(b"after-tail-literals");
    matcher.add_data(second, |_| {});

    let mut first_sequence = None;
    matcher.start_matching(|seq| {
        if first_sequence.is_some() {
            return;
        }
        first_sequence = Some(match seq {
            Sequence::Literals { literals } => (literals.len(), 0usize, 0usize),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => (literals.len(), offset, match_len),
        });
    });

    let (lit_len, offset, match_len) = first_sequence.expect("expected at least one sequence");
    assert_eq!(
        lit_len, 0,
        "expected immediate cross-block match at block start"
    );
    assert_eq!(
        offset,
        tail.len(),
        "expected match against densely seeded tail"
    );
    assert!(
        match_len >= DFAST_MIN_MATCH_LEN,
        "match length should satisfy dfast minimum match length"
    );
}

#[test]
fn dfast_skip_matching_dense_backfills_newly_hashable_long_tail_positions() {
    let mut matcher = DfastMatchGenerator::new(1 << 22);
    let first = deterministic_high_entropy_bytes(0x7A64_0315_D4E1_91C3, 4096);
    let first_len = first.len();
    matcher.add_data(first, |_| {});
    matcher.skip_matching_dense();

    // Appending one byte makes exactly the previous block's last 7 starts
    // newly eligible for 8-byte long-hash insertion.
    matcher.add_data(alloc::vec![0xAB], |_| {});
    matcher.skip_matching_dense();

    let target_abs_pos = first_len - 7;
    let target_rel = target_abs_pos - matcher.history_abs_start;
    let live = matcher.live_history();
    assert!(
        target_rel + 8 <= live.len(),
        "fixture must make the boundary start long-hashable"
    );
    let long_hash = matcher.long_hash_index(&live[target_rel..]);
    let target_slot = matcher.pack_slot(target_abs_pos);
    // Single-slot tables (donor parity): the bucket holds at most one
    // u32; the assertion below is a direct equality (no `.contains`).
    assert_ne!(
        target_slot, DFAST_EMPTY_SLOT,
        "pack_slot must never return the empty-slot sentinel for a real position"
    );
    assert_eq!(
        matcher.long_hash[long_hash], target_slot,
        "dense skip must seed long-hash entry for newly hashable boundary start"
    );
}

#[test]
fn dfast_seed_remaining_hashable_starts_seeds_last_short_hash_positions() {
    let mut matcher = DfastMatchGenerator::new(1 << 20);
    let block = deterministic_high_entropy_bytes(0x13F0_9A6D_55CE_7B21, 64);
    matcher.add_data(block, |_| {});
    matcher.ensure_hash_tables();

    let current_len = matcher.window_blocks.back().copied().unwrap_or(0);
    let current_abs_start = matcher.history_abs_start + matcher.window_size - current_len;
    let seed_start = current_len - DFAST_MIN_MATCH_LEN;
    matcher.seed_remaining_hashable_starts(current_abs_start, current_len, seed_start);

    let target_abs_pos = current_abs_start + current_len - 5;
    let target_rel = target_abs_pos - matcher.history_abs_start;
    let live = matcher.live_history();
    assert!(
        target_rel + 5 <= live.len(),
        "fixture must leave the last short-hash start valid"
    );
    let short_hash = matcher.short_hash_index(&live[target_rel..]);
    let target_slot = matcher.pack_slot(target_abs_pos);
    assert_ne!(
        target_slot, DFAST_EMPTY_SLOT,
        "pack_slot must never return the empty-slot sentinel for a real position"
    );
    assert_eq!(
        matcher.short_hash[short_hash], target_slot,
        "tail seeding must include the last 5-byte-hashable start"
    );
}

#[test]
fn dfast_seed_remaining_hashable_starts_handles_pos_at_block_end() {
    let mut matcher = DfastMatchGenerator::new(1 << 20);
    let block = deterministic_high_entropy_bytes(0x7BB2_DA91_441E_C0EF, 64);
    matcher.add_data(block, |_| {});
    matcher.ensure_hash_tables();

    let current_len = matcher.window_blocks.back().copied().unwrap_or(0);
    let current_abs_start = matcher.history_abs_start + matcher.window_size - current_len;
    matcher.seed_remaining_hashable_starts(current_abs_start, current_len, current_len);

    let target_abs_pos = current_abs_start + current_len - 5;
    let target_rel = target_abs_pos - matcher.history_abs_start;
    let live = matcher.live_history();
    assert!(
        target_rel + 5 <= live.len(),
        "fixture must leave the last short-hash start valid"
    );
    let short_hash = matcher.short_hash_index(&live[target_rel..]);
    let target_slot = matcher.pack_slot(target_abs_pos);
    assert_ne!(
        target_slot, DFAST_EMPTY_SLOT,
        "pack_slot must never return the empty-slot sentinel for a real position"
    );
    assert_eq!(
        matcher.short_hash[short_hash], target_slot,
        "tail seeding must still include the last 5-byte-hashable start when pos is at block end"
    );
}

/// `ensure_room_for` must trigger `reduce()` when the requested
/// absolute position would push a relative offset past
/// `u32::MAX - DFAST_REBASE_GUARD_BAND`. After the rebase, the
/// pre-existing entry at a much-smaller absolute position falls
/// below `reducer` and gets cleared to `DFAST_EMPTY_SLOT`; a fresh
/// insert at the boundary position must `pack_slot` to a valid
/// non-sentinel value that `unpack_slot` resolves back to the same
/// absolute position. Mirrors `LdmHashTable::ensure_room_for_*`
/// from PR #139.
///
/// Runs on every target — `trigger_abs = u32::MAX -
/// DFAST_REBASE_GUARD_BAND + 1 = 0xC0000000`, which fits in `usize`
/// on i686 (`usize::MAX = u32::MAX`) without overflow, so the
/// packed-slot boundary path + u32 ↔ usize round-trip is exercised
/// on every pointer width we ship.
#[test]
fn dfast_ensure_room_for_rebases_above_guard_band() {
    let mut dfast = DfastMatchGenerator::new(1 << 22);
    dfast.set_hash_bits(10, 10);
    dfast.ensure_hash_tables();

    // Seed an early insert near the current base in BOTH tables.
    // `ensure_room_for` / `reduce` is a shared contract for both
    // `short_hash` and `long_hash`; without seeding both, a
    // regression that only cleared short_hash would still pass.
    // Direct `pack_slot` + bucket write keeps the test focused on
    // the rebase mechanics and avoids dragging in the full
    // `insert_position` flow with its history/window setup.
    let early_abs = 1024usize;
    let early_packed = dfast.pack_slot(early_abs);
    assert_ne!(early_packed, DFAST_EMPTY_SLOT);
    dfast.short_hash[0] = early_packed;
    dfast.long_hash[0] = early_packed;

    // Pick a trigger position that forces the first rebase. With
    // `position_base = 0`, the smallest `abs_pos` that fails the
    // `rel <= max_rel` test is `u32::MAX - DFAST_REBASE_GUARD_BAND
    // + 1`. After one `reduce(DFAST_REBASE_GUARD_BAND)` the base
    // advances by `DFAST_REBASE_GUARD_BAND`.
    let trigger_abs = (u32::MAX as usize) - (DFAST_REBASE_GUARD_BAND as usize) + 1;
    assert_eq!(dfast.position_base, 0);
    dfast.ensure_room_for(trigger_abs);
    assert_eq!(
        dfast.position_base, DFAST_REBASE_GUARD_BAND as usize,
        "rebase must advance position_base by DFAST_REBASE_GUARD_BAND"
    );

    // The early entry at abs=1024 had packed slot 1025; the rebase
    // subtracts `DFAST_REBASE_GUARD_BAND` (= 2^30) from every slot.
    // 1025 <= 2^30 so the slot drops to the empty sentinel —
    // donor parity for `ZSTD_window_reduce`'s clamp-at-zero rule.
    // Verify BOTH tables — `reduce()` walks them in sequence.
    assert_eq!(
        dfast.short_hash[0], DFAST_EMPTY_SLOT,
        "pre-rebase short-hash entries below the reducer must become empty"
    );
    assert_eq!(
        dfast.long_hash[0], DFAST_EMPTY_SLOT,
        "pre-rebase long-hash entries below the reducer must become empty"
    );

    // A fresh insert past the rebase boundary must round-trip:
    // pack to a non-sentinel value, then unpack back to the same
    // absolute position via `position_base + slot - 1`.
    let post_packed = dfast.pack_slot(trigger_abs);
    assert_ne!(post_packed, DFAST_EMPTY_SLOT);
    let unpacked = dfast.position_base + (post_packed as usize) - 1;
    assert_eq!(
        unpacked, trigger_abs,
        "post-rebase pack/unpack must round-trip the absolute position"
    );
}

#[test]
fn dfast_sparse_skip_matching_backfills_previous_tail_for_consecutive_sparse_blocks() {
    let mut matcher = DfastMatchGenerator::new(1 << 22);
    let boundary_prefix = [0xFA, 0xFB, 0xFC];
    let boundary_suffix = [0xFD, 0xEE, 0xAD, 0xBE, 0xEF, 0x11, 0x22, 0x33];

    let mut first = deterministic_high_entropy_bytes(0xA5A5_5A5A_C3C3_3C3C, 4096);
    let first_tail_start = first.len() - boundary_prefix.len();
    first[first_tail_start..].copy_from_slice(&boundary_prefix);
    matcher.add_data(first, |_| {});
    matcher.skip_matching(Some(true));

    let mut second = deterministic_high_entropy_bytes(0xA5A5_5A5A_C3C3_3C3C, 4096);
    second[..boundary_suffix.len()].copy_from_slice(&boundary_suffix);
    matcher.add_data(second.clone(), |_| {});
    matcher.skip_matching(Some(true));

    let mut third = boundary_prefix.to_vec();
    third.extend_from_slice(&boundary_suffix);
    third.extend_from_slice(b"-trailing-literals");
    matcher.add_data(third, |_| {});

    let mut first_sequence = None;
    matcher.start_matching(|seq| {
        if first_sequence.is_some() {
            return;
        }
        first_sequence = Some(match seq {
            Sequence::Literals { literals } => (literals.len(), 0usize, 0usize),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => (literals.len(), offset, match_len),
        });
    });

    let (lit_len, offset, match_len) = first_sequence.expect("expected at least one sequence");
    assert_eq!(
        lit_len, 0,
        "expected immediate match from the prior sparse-skip boundary"
    );
    assert_eq!(
        offset,
        second.len() + boundary_prefix.len(),
        "expected match against backfilled first→second boundary start"
    );
    assert!(
        match_len >= DFAST_MIN_MATCH_LEN,
        "match length should satisfy dfast minimum match length"
    );
}

#[test]
fn fastest_hint_iteration_23_sequences_reconstruct_source() {
    fn generate_data(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed;
        let mut data = Vec::with_capacity(len);
        for _ in 0..len {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            data.push((state >> 33) as u8);
        }
        data
    }

    let i = 23u64;
    let len = (i * 89 % 16384) as usize;
    let mut data = generate_data(i, len);
    // Append a repeated slice so the fixture deterministically exercises
    // the match path (Sequence::Triple) instead of only literals.
    let repeat = data[128..256].to_vec();
    data.extend_from_slice(&repeat);
    data.extend_from_slice(&repeat);

    let mut driver = MatchGeneratorDriver::new(1024 * 128, 1);
    driver.set_source_size_hint(data.len() as u64);
    driver.reset(CompressionLevel::Fastest);
    let mut space = driver.get_next_space();
    space[..data.len()].copy_from_slice(&data);
    space.truncate(data.len());
    driver.commit_space(space);

    let mut rebuilt = Vec::with_capacity(data.len());
    let mut saw_triple = false;
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => rebuilt.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            saw_triple = true;
            rebuilt.extend_from_slice(literals);
            assert!(offset > 0, "offset must be non-zero");
            assert!(
                offset <= rebuilt.len(),
                "offset must reference already-produced bytes: offset={} produced={}",
                offset,
                rebuilt.len()
            );
            let start = rebuilt.len() - offset;
            for idx in 0..match_len {
                let b = rebuilt[start + idx];
                rebuilt.push(b);
            }
        }
    });

    // Whether THIS specific iteration produces a Triple depends on
    // the matcher's step-skip schedule (donor-shape kernel walks ip0
    // with kSearchStrength-driven stride growth) — the legacy
    // SuffixStore-based matcher iterated every position and always
    // hit short repeats, but the donor-shape kernel may skip over
    // them when the step has grown large by the time it reaches the
    // repeat region. The substance of this test is the
    // reconstruction assertion below; `saw_triple` was a legacy
    // tuning preference, not a correctness invariant.
    let _ = saw_triple;
    assert_eq!(rebuilt, data);
}

#[test]
fn fast_levels_dispatch_per_level_hash_log_and_mls() {
    // Level 1 — donor `{ 19, 13, 14, 1, 7, 0, ZSTD_fast }` row:
    // window_log=19, hash_log=14, mls=7.
    let f1 = resolve_level_params(CompressionLevel::Level(1), None)
        .fast
        .unwrap();
    assert_eq!(f1.hash_log, 14);
    assert_eq!(f1.mls, 7);
    assert_eq!(f1.step_size, 2);

    // Negative levels — donor row-0 ("base for negative"):
    // hash_log=13, mls=7. The 32 KiB table is L1d-resident (every
    // probe an L1 hit, vs an L2 access for a 64 KiB hash_log=14
    // table), and minMatch=7 drops short-distance 6-byte matches —
    // donor parity on both ratio and throughput.
    // step_size follows donor's formula: targetLength = -level,
    // step_size = (-level) + 1, giving 2..8 for L-1..L-7.
    for n in -7..=-1 {
        let f = resolve_level_params(CompressionLevel::Level(n), None)
            .fast
            .unwrap();
        assert_eq!(f.hash_log, 13, "Level({n}) fast_hash_log");
        assert_eq!(f.mls, 7, "Level({n}) fast_mls");
        let expected_step = ((-n) as usize) + 1;
        assert_eq!(f.step_size, expected_step, "Level({n}) fast_step_size");
    }

    // Fastest + Uncompressed keep hash_log=14 / mls=6 (their own
    // tuning; not part of the negative-level donor ladder).
    let pf = resolve_level_params(CompressionLevel::Fastest, None);
    let ff = pf.fast.unwrap();
    assert_eq!(
        (pf.window_log, ff.hash_log, ff.mls, ff.step_size),
        (19, 14, 6, 2),
    );
    // Uncompressed keeps window_log=17 (no history references, smaller
    // decoder reservation); fast cParams same as negative-base row.
    let pu = resolve_level_params(CompressionLevel::Uncompressed, None);
    let fu = pu.fast.unwrap();
    assert_eq!(
        (pu.window_log, fu.hash_log, fu.mls, fu.step_size),
        (17, 14, 6, 2),
    );
}

/// Exercise the actual driver wiring: for every Fast level, reset a
/// `MatchGeneratorDriver` and assert the inner `FastKernelMatcher`
/// observed the same `(hash_log, mls, step_size)` tuple that
/// `resolve_level_params` reports. Catches plumbing bugs — argument
/// reordering, stale step_size carried from a prior frame,
/// stuck-on-default values — that the parameter-only test above
/// would miss.
#[test]
fn fast_levels_driver_wiring_threads_cparams_into_inner_matcher() {
    let mut driver = MatchGeneratorDriver::new(64 * 1024, 1);

    let fast_levels = [
        CompressionLevel::Level(1),
        CompressionLevel::Fastest,
        CompressionLevel::Uncompressed,
        CompressionLevel::Level(-1),
        CompressionLevel::Level(-2),
        CompressionLevel::Level(-3),
        CompressionLevel::Level(-4),
        CompressionLevel::Level(-5),
        CompressionLevel::Level(-6),
        CompressionLevel::Level(-7),
    ];

    for &level in &fast_levels {
        let p = resolve_level_params(level, None);
        // Sanity: every level in the table above must resolve to a
        // Fast-strategy row — otherwise this test isn't testing what
        // it claims to test.
        assert_eq!(
            p.strategy_tag,
            super::strategy::StrategyTag::Fast,
            "{level:?} must resolve to Fast strategy",
        );

        // Bounce through a non-Fast strategy first so the next
        // reset actually goes through the backend-switch path
        // (`MatchGeneratorDriver::new` / `simple_mut` recreate the
        // Fast variant via `FastKernelMatcher::with_params`). Without
        // this hop the loop would only ever stay in `BackendTag::Simple`
        // and exercise `FastKernelMatcher::reset` — leaving the
        // `with_params` wiring untested on the production path.
        // `Default` resolves to Dfast strategy (a non-Fast row),
        // which is enough to force the swap.
        crate::encoding::Matcher::reset(&mut driver, CompressionLevel::Default);

        // Drive the production reset path (same code paths exercised
        // by FrameCompressor / StreamingEncoder).
        crate::encoding::Matcher::reset(&mut driver, level);

        let f = p.fast.unwrap();
        let m = driver.simple_mut();
        assert_eq!(
            m.hash_log(),
            f.hash_log,
            "{level:?}: inner matcher hash_log mismatch — argument swap?",
        );
        assert_eq!(
            m.mls(),
            f.mls,
            "{level:?}: inner matcher mls mismatch — argument swap?",
        );
        assert_eq!(
            m.step_size(),
            f.step_size,
            "{level:?}: inner matcher step_size mismatch — stale value carried from prior reset?",
        );
    }
}

/// Pins `hc.target_len` to the reference `cParams.targetLength` from
/// `clevels.h` table[0] (default — `srcSize > 256 KB`) across levels
/// 5-15. The reference's lazy outer loop treats `targetLength` as
/// `sufficient_len` — the "nice match" threshold that breaks the chain
/// walk as soon as a candidate reaches that length.
///
/// Levels 13-15 run btlazy2 in the reference and the hash-chain Lazy
/// parser here, but the reference `targetLength` (32) is the same nice-match
/// threshold for both finders, so we mirror it directly.
///
/// Test queries the reference via `ZSTD_getCParams(level, 0, 0)` so any
/// future table tweak upstream is reflected automatically.
#[test]
fn lazy_band_target_len_matches_donor_default_table() {
    use zstd::zstd_safe::zstd_sys;

    for level in 5..=15i32 {
        // SAFETY: `ZSTD_getCParams` reads from a static table; safe to
        // call with any (level, srcSize, dictSize) combination.
        let reference = unsafe { zstd_sys::ZSTD_getCParams(level, 0, 0) };
        let params = resolve_level_params(CompressionLevel::Level(level), None);
        // L5 = greedy (Row backend → `row`); L6-15 = lazy (HashChain → `hc`).
        // Both surface the donor `targetLength` as their nice-match threshold.
        let target_len = params
            .hc
            .map(|hc| hc.target_len)
            .or_else(|| params.row.map(|row| row.target_len))
            .expect("lazy/greedy level carries hc or row config");
        assert_eq!(
            target_len as u32, reference.targetLength,
            "L{level}: target_len ({target_len}) must match reference cParams.targetLength ({})",
            reference.targetLength
        );
    }
}

/// Levels 13-15 mirror the reference btlazy2 window/hash/chain/search
/// budget from `clevels.h` table[0]: `search_depth == 1 << cParams.searchLog`
/// (16 / 32 / 64) plus `window_log` / `hash_log` / `chain_log` equal to the
/// reference `windowLog` / `hashLog` / `chainLog`. We run them on the
/// hash-chain Lazy parser rather than a binary-tree finder, so they do not
/// re-establish a strict ratio ladder above L12 on window-fitting inputs;
/// asserting the full row (not just `search_depth`) keeps the whole budget
/// aligned and guards every field against silent drift.
#[test]
fn upper_lazy_band_params_match_donor_default_table() {
    use zstd::zstd_safe::zstd_sys;

    for level in 13..=15i32 {
        // SAFETY: `ZSTD_getCParams` reads from a static table; safe to
        // call with any (level, srcSize, dictSize) combination.
        let reference = unsafe { zstd_sys::ZSTD_getCParams(level, 0, 0) };
        let params = resolve_level_params(CompressionLevel::Level(level), None);
        let hc = params.hc.unwrap();
        assert_eq!(
            hc.search_depth as u32,
            1u32 << reference.searchLog,
            "L{level}: hc.search_depth ({}) must equal 1<<cParams.searchLog ({})",
            hc.search_depth,
            1u32 << reference.searchLog
        );
        assert_eq!(
            params.window_log as u32, reference.windowLog,
            "L{level}: window_log ({}) must equal cParams.windowLog ({})",
            params.window_log, reference.windowLog
        );
        assert_eq!(
            hc.hash_log as u32, reference.hashLog,
            "L{level}: hc.hash_log ({}) must equal cParams.hashLog ({})",
            hc.hash_log, reference.hashLog
        );
        assert_eq!(
            hc.chain_log as u32, reference.chainLog,
            "L{level}: hc.chain_log ({}) must equal cParams.chainLog ({})",
            hc.chain_log, reference.chainLog
        );
    }
}
