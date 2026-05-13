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
use super::BETTER_WINDOW_LOG;
use super::CompressionLevel;
use super::Matcher;
use super::Sequence;
use super::blocks::encode_offset_with_history;
use super::cost_model::{
    HC_BITCOST_MULTIPLIER, HC_FORMAT_MINMATCH, HC_MAX_LIT, HC_OPT_NUM, HC_PREDEF_THRESHOLD,
    HcOptState, HcOptimalCostProfile,
};
#[cfg(test)]
use super::cost_model::{HC_BLOCKSIZE_MAX, HC_MAX_LL, HC_MAX_ML, HC_MAX_OFF, HcOptPriceType};
use super::dfast::DfastMatchGenerator;
#[cfg(test)]
use super::match_table::helpers::FAST_HASH_FILL_STEP;
#[cfg(test)]
use super::match_table::helpers::common_prefix_len;
use super::match_table::helpers::{INCOMPRESSIBLE_SKIP_STEP, MIN_MATCH_LEN};
#[cfg(test)]
use super::opt::ldm::HcRawSeq;
use super::opt::ldm::{HcOptLdmState, HcRawSeqStore};
use super::opt::types::{
    HcCandidateQuery, HcOptimalNode, HcOptimalPlanBuffers, HcOptimalPlanState, HcOptimalSequence,
    MatchCandidate,
};
use super::row::RowMatchGenerator;
use super::simple::{MatchGenerator, SuffixStore};
use super::strategy::HcParseMode;
#[cfg(all(
    test,
    feature = "std",
    target_arch = "aarch64",
    target_endian = "little"
))]
use std::arch::is_aarch64_feature_detected;
#[cfg(all(
    test,
    feature = "std",
    any(target_arch = "x86", target_arch = "x86_64")
))]
use std::arch::is_x86_feature_detected;

pub(crate) const DFAST_MIN_MATCH_LEN: usize = 6;
pub(crate) const DFAST_SHORT_HASH_LOOKAHEAD: usize = 4;
pub(crate) const ROW_MIN_MATCH_LEN: usize = 6;
pub(crate) const DFAST_TARGET_LEN: usize = 48;
// Keep these aligned with the issue's zstd level-3/dfast target unless ratio
// measurements show we can shrink them without regressing acceptance tests.
pub(crate) const DFAST_HASH_BITS: usize = 20;
pub(crate) const DFAST_SEARCH_DEPTH: usize = 4;
pub(crate) const DFAST_EMPTY_SLOT: usize = usize::MAX;
pub(crate) const DFAST_SKIP_SEARCH_STRENGTH: usize = 6;
pub(crate) const DFAST_SKIP_STEP_GROWTH_INTERVAL: usize = 1 << DFAST_SKIP_SEARCH_STRENGTH;
pub(crate) const DFAST_LOCAL_SKIP_TRIGGER: usize = 256;
pub(crate) const DFAST_MAX_SKIP_STEP: usize = 8;
pub(crate) const DFAST_INCOMPRESSIBLE_SKIP_STEP: usize = 16;
pub(crate) const ROW_HASH_BITS: usize = 20;
pub(crate) const ROW_LOG: usize = 5;
pub(crate) const ROW_SEARCH_DEPTH: usize = 16;
pub(crate) const ROW_TARGET_LEN: usize = 48;
pub(crate) const ROW_TAG_BITS: usize = 8;
pub(crate) const ROW_EMPTY_SLOT: usize = usize::MAX;
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
use super::match_table::storage::{HC_CHAIN_LOG, HC_EMPTY, HC_HASH_LOG, HC3_HASH_LOG};
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

// `HcParseMode` lives in `crate::encoding::strategy` so the extracted
// `cost_model::HcOptimalCostProfile::for_mode` can branch on it without
// importing back from this monolith — see the use statement at the top
// of the file.

/// Bundled tuning knobs for the hash-chain matcher. Using a typed config
/// instead of positional `usize` args eliminates parameter-order hazards.
#[derive(Copy, Clone)]
struct HcConfig {
    hash_log: usize,
    chain_log: usize,
    search_depth: usize,
    target_len: usize,
    parse_mode: HcParseMode,
}

#[derive(Copy, Clone)]
pub(crate) struct RowConfig {
    pub(crate) hash_bits: usize,
    pub(crate) row_log: usize,
    pub(crate) search_depth: usize,
    pub(crate) target_len: usize,
}

const HC_CONFIG: HcConfig = HcConfig {
    hash_log: HC_HASH_LOG,
    chain_log: HC_CHAIN_LOG,
    search_depth: HC_SEARCH_DEPTH,
    target_len: HC_TARGET_LEN,
    parse_mode: HcParseMode::Lazy2,
};

/// Best-level: deeper search, larger tables, higher target length.
const BEST_HC_CONFIG: HcConfig = HcConfig {
    hash_log: 21,
    chain_log: 20,
    search_depth: 32,
    target_len: 128,
    parse_mode: HcParseMode::Lazy2,
};

const BTOPT_HC_CONFIG: HcConfig = HcConfig {
    hash_log: 23,
    chain_log: 22,
    search_depth: 32,
    target_len: 256,
    parse_mode: HcParseMode::BtOpt,
};

const BTULTRA_HC_CONFIG: HcConfig = HcConfig {
    hash_log: 23,
    chain_log: 23,
    search_depth: 32,
    target_len: 256,
    parse_mode: HcParseMode::BtUltra,
};

const BTULTRA2_HC_CONFIG: HcConfig = HcConfig {
    hash_log: 24,
    chain_log: 24,
    search_depth: 512,
    target_len: 256,
    parse_mode: HcParseMode::BtUltra2,
};

const BTULTRA2_HC_CONFIG_L22: HcConfig = HcConfig {
    hash_log: 25,
    chain_log: 27,
    search_depth: 512,
    target_len: 999,
    parse_mode: HcParseMode::BtUltra2,
};

const BTULTRA2_HC_CONFIG_L22_256K: HcConfig = HcConfig {
    hash_log: 19,
    chain_log: 19,
    search_depth: 1 << 13,
    target_len: 999,
    parse_mode: HcParseMode::BtUltra2,
};

const BTULTRA2_HC_CONFIG_L22_128K: HcConfig = HcConfig {
    hash_log: 17,
    chain_log: 18,
    search_depth: 1 << 11,
    target_len: 999,
    parse_mode: HcParseMode::BtUltra2,
};

const BTULTRA2_HC_CONFIG_L22_16K: HcConfig = HcConfig {
    hash_log: 15,
    chain_log: 15,
    search_depth: 1 << 10,
    target_len: 999,
    parse_mode: HcParseMode::BtUltra2,
};

const ROW_CONFIG: RowConfig = RowConfig {
    hash_bits: ROW_HASH_BITS,
    row_log: ROW_LOG,
    search_depth: ROW_SEARCH_DEPTH,
    target_len: ROW_TARGET_LEN,
};

/// Resolved tuning parameters for a compression level.
#[derive(Copy, Clone)]
struct LevelParams {
    backend: MatcherBackend,
    window_log: u8,
    hash_fill_step: usize,
    lazy_depth: u8,
    hc: HcConfig,
    row: RowConfig,
}

fn dfast_hash_bits_for_window(max_window_size: usize) -> usize {
    let window_log = (usize::BITS - 1 - max_window_size.leading_zeros()) as usize;
    window_log.clamp(MIN_WINDOW_LOG as usize, DFAST_HASH_BITS)
}

fn row_hash_bits_for_window(max_window_size: usize) -> usize {
    let window_log = (usize::BITS - 1 - max_window_size.leading_zeros()) as usize;
    window_log.clamp(MIN_WINDOW_LOG as usize, ROW_HASH_BITS)
}

/// Parameter table for numeric compression levels 1–22.
///
/// Each entry maps a zstd compression level to the best-available matcher
/// backend and tuning knobs. High levels map to dedicated parse modes:
/// btopt (16-17), btultra (18-19), btultra2 (20-22).
///
/// Index 0 = level 1, index 21 = level 22.
#[rustfmt::skip]
const LEVEL_TABLE: [LevelParams; 22] = [
    // Lvl  Strategy       wlog  step  lazy  HC config                                   row config
    // ---  -------------- ----  ----  ----  ------------------------------------------  ----------
    /* 1 */ LevelParams { backend: MatcherBackend::Simple,    window_log: 17, hash_fill_step: 3, lazy_depth: 0, hc: HC_CONFIG, row: ROW_CONFIG },
    /* 2 */ LevelParams { backend: MatcherBackend::Dfast,     window_log: 19, hash_fill_step: 1, lazy_depth: 1, hc: HC_CONFIG, row: ROW_CONFIG },
    /* 3 */ LevelParams { backend: MatcherBackend::Dfast,     window_log: 22, hash_fill_step: 1, lazy_depth: 1, hc: HC_CONFIG, row: ROW_CONFIG },
    /* 4 */ LevelParams { backend: MatcherBackend::Row,       window_log: 22, hash_fill_step: 1, lazy_depth: 1, hc: HC_CONFIG, row: ROW_CONFIG },
    /* 5 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 22, hash_fill_step: 1, lazy_depth: 1, hc: HcConfig { hash_log: 18, chain_log: 17, search_depth: 4,  target_len: 32,  parse_mode: HcParseMode::Lazy2 }, row: ROW_CONFIG },
    /* 6 */ LevelParams { backend: MatcherBackend::HashChain, window_log: BETTER_WINDOW_LOG, hash_fill_step: 1, lazy_depth: 1, hc: HcConfig { hash_log: 19, chain_log: 18, search_depth: 8,  target_len: 48,  parse_mode: HcParseMode::Lazy2 }, row: ROW_CONFIG },
    /* 7 */ LevelParams { backend: MatcherBackend::HashChain, window_log: BETTER_WINDOW_LOG, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 20, chain_log: 19, search_depth: 16, target_len: 48,  parse_mode: HcParseMode::Lazy2 }, row: ROW_CONFIG },
    /* 8 */ LevelParams { backend: MatcherBackend::HashChain, window_log: BETTER_WINDOW_LOG, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 20, chain_log: 19, search_depth: 24, target_len: 64,  parse_mode: HcParseMode::Lazy2 }, row: ROW_CONFIG },
    /* 9 */ LevelParams { backend: MatcherBackend::HashChain, window_log: BETTER_WINDOW_LOG, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 21, chain_log: 20, search_depth: 24, target_len: 64,  parse_mode: HcParseMode::Lazy2 }, row: ROW_CONFIG },
    /*10 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 24, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 21, chain_log: 20, search_depth: 28, target_len: 96,  parse_mode: HcParseMode::Lazy2 }, row: ROW_CONFIG },
    /*11 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 24, hash_fill_step: 1, lazy_depth: 2, hc: BEST_HC_CONFIG, row: ROW_CONFIG },
    /*12 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 25, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 22, chain_log: 21, search_depth: 32, target_len: 128, parse_mode: HcParseMode::Lazy2 }, row: ROW_CONFIG },
    /*13 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 25, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 22, chain_log: 21, search_depth: 32, target_len: 160, parse_mode: HcParseMode::Lazy2 }, row: ROW_CONFIG },
    /*14 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 25, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 22, chain_log: 22, search_depth: 32, target_len: 192, parse_mode: HcParseMode::Lazy2 }, row: ROW_CONFIG },
    /*15 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 23, chain_log: 22, search_depth: 32, target_len: 192, parse_mode: HcParseMode::Lazy2 }, row: ROW_CONFIG },
    /*16 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: BTOPT_HC_CONFIG, row: ROW_CONFIG },
    /*17 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: BTOPT_HC_CONFIG, row: ROW_CONFIG },
    /*18 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: BTULTRA_HC_CONFIG, row: ROW_CONFIG },
    /*19 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: BTULTRA_HC_CONFIG, row: ROW_CONFIG },
    /*20 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: BTULTRA2_HC_CONFIG, row: ROW_CONFIG },
    /*21 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: BTULTRA2_HC_CONFIG, row: ROW_CONFIG },
    /*22 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 27, hash_fill_step: 1, lazy_depth: 2, hc: BTULTRA2_HC_CONFIG_L22, row: ROW_CONFIG },
];

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
    let src_log = if src_size == 0 {
        MIN_WINDOW_LOG
    } else {
        (64 - (src_size - 1).leading_zeros()) as u8 // ceil_log2
    };
    let src_log = src_log.max(MIN_WINDOW_LOG).max(MIN_HINTED_WINDOW_LOG);
    if src_log < params.window_log {
        params.window_log = src_log;
    }
    // For HC backend: also cap hash_log and chain_log so tables are
    // proportional to the source, avoiding multi-MB allocations for
    // tiny inputs.
    if params.backend == MatcherBackend::HashChain {
        if (src_log + 2) < params.hc.hash_log as u8 {
            params.hc.hash_log = (src_log + 2) as usize;
        }
        if (src_log + 1) < params.hc.chain_log as u8 {
            params.hc.chain_log = (src_log + 1) as usize;
        }
    } else if params.backend == MatcherBackend::Row {
        let max_window_size = 1usize << params.window_log;
        params.row.hash_bits = row_hash_bits_for_window(max_window_size);
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
        let src_log = if size == 0 {
            MIN_WINDOW_LOG
        } else {
            (64 - (size - 1).leading_zeros()) as u8
        };
        window_log = window_log.min(src_log.max(MIN_WINDOW_LOG));
        let adjusted_table_log = window_log as usize + 1;
        hc.hash_log = hc.hash_log.min(adjusted_table_log);
        hc.chain_log = hc.chain_log.min(adjusted_table_log);
    }
    LevelParams {
        backend: MatcherBackend::HashChain,
        window_log,
        hash_fill_step: 1,
        lazy_depth: 2,
        hc,
        row: ROW_CONFIG,
    }
}

/// Resolve a [`CompressionLevel`] to internal tuning parameters,
/// optionally adjusted for a known source size.
fn resolve_level_params(level: CompressionLevel, source_size: Option<u64>) -> LevelParams {
    if matches!(level, CompressionLevel::Level(22)) {
        return level22_btultra2_params_for_source_size(source_size);
    }
    let params = match level {
        CompressionLevel::Uncompressed => LevelParams {
            backend: MatcherBackend::Simple,
            window_log: 17,
            hash_fill_step: 1,
            lazy_depth: 0,
            hc: HC_CONFIG,
            row: ROW_CONFIG,
        },
        CompressionLevel::Fastest => LEVEL_TABLE[0],
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
                // Negative levels: ultra-fast with the Simple backend.
                // Acceleration grows with magnitude, expressed as larger
                // hash_fill_step (fewer positions indexed).
                let acceleration =
                    (n.saturating_abs() as usize).min((-CompressionLevel::MIN_LEVEL) as usize);
                let step = (acceleration + 3).min(128);
                LevelParams {
                    backend: MatcherBackend::Simple,
                    window_log: 17,
                    hash_fill_step: step,
                    lazy_depth: 0,
                    hc: HC_CONFIG,
                    row: ROW_CONFIG,
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum MatcherBackend {
    Simple,
    Dfast,
    Row,
    HashChain,
}

/// This is the default implementation of the `Matcher` trait. It allocates and reuses the buffers when possible.
pub struct MatchGeneratorDriver {
    vec_pool: Vec<Vec<u8>>,
    suffix_pool: Vec<SuffixStore>,
    match_generator: MatchGenerator,
    dfast_match_generator: Option<DfastMatchGenerator>,
    row_match_generator: Option<RowMatchGenerator>,
    hc_match_generator: Option<HcMatchGenerator>,
    active_backend: MatcherBackend,
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
}

impl MatchGeneratorDriver {
    /// `slice_size` sets the base block allocation size used for matcher input chunks.
    /// `max_slices_in_window` determines the initial window capacity at construction
    /// time. Effective window sizing is recalculated on every [`reset`](Self::reset)
    /// from the resolved compression level and optional source-size hint.
    pub(crate) fn new(slice_size: usize, max_slices_in_window: usize) -> Self {
        let max_window_size = max_slices_in_window * slice_size;
        Self {
            vec_pool: Vec::new(),
            suffix_pool: Vec::new(),
            match_generator: MatchGenerator::new(max_window_size),
            dfast_match_generator: None,
            row_match_generator: None,
            hc_match_generator: None,
            active_backend: MatcherBackend::Simple,
            slice_size,
            base_slice_size: slice_size,
            reported_window_size: max_window_size,
            dictionary_retained_budget: 0,
            source_size_hint: None,
        }
    }

    fn level_params(level: CompressionLevel, source_size: Option<u64>) -> LevelParams {
        resolve_level_params(level, source_size)
    }

    fn dfast_matcher(&self) -> &DfastMatchGenerator {
        self.dfast_match_generator
            .as_ref()
            .expect("dfast backend must be initialized by reset() before use")
    }

    fn dfast_matcher_mut(&mut self) -> &mut DfastMatchGenerator {
        self.dfast_match_generator
            .as_mut()
            .expect("dfast backend must be initialized by reset() before use")
    }

    fn row_matcher(&self) -> &RowMatchGenerator {
        self.row_match_generator
            .as_ref()
            .expect("row backend must be initialized by reset() before use")
    }

    fn row_matcher_mut(&mut self) -> &mut RowMatchGenerator {
        self.row_match_generator
            .as_mut()
            .expect("row backend must be initialized by reset() before use")
    }

    fn hc_matcher(&self) -> &HcMatchGenerator {
        self.hc_match_generator
            .as_ref()
            .expect("hash chain backend must be initialized by reset() before use")
    }

    fn hc_matcher_mut(&mut self) -> &mut HcMatchGenerator {
        self.hc_match_generator
            .as_mut()
            .expect("hash chain backend must be initialized by reset() before use")
    }

    fn retire_dictionary_budget(&mut self, evicted_bytes: usize) {
        let reclaimed = evicted_bytes.min(self.dictionary_retained_budget);
        if reclaimed == 0 {
            return;
        }
        self.dictionary_retained_budget -= reclaimed;
        match self.active_backend {
            MatcherBackend::Simple => {
                self.match_generator.max_window_size = self
                    .match_generator
                    .max_window_size
                    .saturating_sub(reclaimed);
            }
            MatcherBackend::Dfast => {
                let matcher = self.dfast_matcher_mut();
                matcher.max_window_size = matcher.max_window_size.saturating_sub(reclaimed);
            }
            MatcherBackend::Row => {
                let matcher = self.row_matcher_mut();
                matcher.max_window_size = matcher.max_window_size.saturating_sub(reclaimed);
            }
            MatcherBackend::HashChain => {
                let matcher = self.hc_matcher_mut();
                matcher.table.max_window_size =
                    matcher.table.max_window_size.saturating_sub(reclaimed);
            }
        }
    }

    fn trim_after_budget_retire(&mut self) {
        loop {
            let mut evicted_bytes = 0usize;
            match self.active_backend {
                MatcherBackend::Simple => {
                    let vec_pool = &mut self.vec_pool;
                    let suffix_pool = &mut self.suffix_pool;
                    self.match_generator.reserve(0, |mut data, mut suffixes| {
                        evicted_bytes += data.len();
                        data.resize(data.capacity(), 0);
                        vec_pool.push(data);
                        suffixes.slots.clear();
                        suffixes.slots.resize(suffixes.slots.capacity(), None);
                        suffix_pool.push(suffixes);
                    });
                }
                MatcherBackend::Dfast => {
                    let mut retired = Vec::new();
                    self.dfast_matcher_mut().trim_to_window(|data| {
                        evicted_bytes += data.len();
                        retired.push(data);
                    });
                    for mut data in retired {
                        data.resize(data.capacity(), 0);
                        self.vec_pool.push(data);
                    }
                }
                MatcherBackend::Row => {
                    let mut retired = Vec::new();
                    self.row_matcher_mut().trim_to_window(|data| {
                        evicted_bytes += data.len();
                        retired.push(data);
                    });
                    for mut data in retired {
                        data.resize(data.capacity(), 0);
                        self.vec_pool.push(data);
                    }
                }
                MatcherBackend::HashChain => {
                    let mut retired = Vec::new();
                    self.hc_matcher_mut().table.trim_to_window(|data| {
                        evicted_bytes += data.len();
                        retired.push(data);
                    });
                    for mut data in retired {
                        data.resize(data.capacity(), 0);
                        self.vec_pool.push(data);
                    }
                }
            }
            if evicted_bytes == 0 {
                break;
            }
            self.retire_dictionary_budget(evicted_bytes);
        }
    }

    fn skip_matching_for_dictionary_priming(&mut self) {
        match self.active_backend {
            MatcherBackend::Simple => self.match_generator.skip_matching_with_hint(Some(false)),
            MatcherBackend::Dfast => self.dfast_matcher_mut().skip_matching_dense(),
            MatcherBackend::Row => self.row_matcher_mut().skip_matching_with_hint(Some(false)),
            MatcherBackend::HashChain => self.hc_matcher_mut().skip_matching(Some(false)),
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

    fn reset(&mut self, level: CompressionLevel) {
        let hint = self.source_size_hint.take();
        let hinted = hint.is_some();
        let params = Self::level_params(level, hint);
        let max_window_size = 1usize << params.window_log;
        self.dictionary_retained_budget = 0;
        if self.active_backend != params.backend {
            match self.active_backend {
                MatcherBackend::Simple => {
                    let vec_pool = &mut self.vec_pool;
                    let suffix_pool = &mut self.suffix_pool;
                    self.match_generator.reset(|mut data, mut suffixes| {
                        data.resize(data.capacity(), 0);
                        vec_pool.push(data);
                        suffixes.slots.clear();
                        suffixes.slots.resize(suffixes.slots.capacity(), None);
                        suffix_pool.push(suffixes);
                    });
                }
                MatcherBackend::Dfast => {
                    if let Some(dfast) = self.dfast_match_generator.as_mut() {
                        let vec_pool = &mut self.vec_pool;
                        dfast.reset(|mut data| {
                            data.resize(data.capacity(), 0);
                            vec_pool.push(data);
                        });
                    }
                }
                MatcherBackend::Row => {
                    if let Some(row) = self.row_match_generator.as_mut() {
                        row.row_heads = Vec::new();
                        row.row_positions = Vec::new();
                        row.row_tags = Vec::new();
                        let vec_pool = &mut self.vec_pool;
                        row.reset(|mut data| {
                            data.resize(data.capacity(), 0);
                            vec_pool.push(data);
                        });
                    }
                }
                MatcherBackend::HashChain => {
                    if let Some(hc) = self.hc_match_generator.as_mut() {
                        // Release oversized tables when switching away from
                        // HashChain so Best's larger allocations don't persist.
                        // hash3_table must be released alongside the other
                        // two: BtUltra2's `1 << HC3_HASH_LOG` entries would
                        // otherwise stay pinned across the backend switch,
                        // even though no future caller of this backend will
                        // touch them.
                        hc.table.hash_table = Vec::new();
                        hc.table.chain_table = Vec::new();
                        hc.table.hash3_table = Vec::new();
                        let vec_pool = &mut self.vec_pool;
                        hc.reset(|mut data| {
                            data.resize(data.capacity(), 0);
                            vec_pool.push(data);
                        });
                    }
                }
            }
        }

        self.active_backend = params.backend;
        self.slice_size = self.base_slice_size.min(max_window_size);
        self.reported_window_size = max_window_size;
        match self.active_backend {
            MatcherBackend::Simple => {
                let vec_pool = &mut self.vec_pool;
                let suffix_pool = &mut self.suffix_pool;
                self.match_generator.max_window_size = max_window_size;
                self.match_generator.hash_fill_step = params.hash_fill_step;
                self.match_generator.reset(|mut data, mut suffixes| {
                    data.resize(data.capacity(), 0);
                    vec_pool.push(data);
                    suffixes.slots.clear();
                    suffixes.slots.resize(suffixes.slots.capacity(), None);
                    suffix_pool.push(suffixes);
                });
            }
            MatcherBackend::Dfast => {
                let dfast = self
                    .dfast_match_generator
                    .get_or_insert_with(|| DfastMatchGenerator::new(max_window_size));
                dfast.max_window_size = max_window_size;
                dfast.lazy_depth = params.lazy_depth;
                dfast.use_fast_loop = matches!(
                    level,
                    CompressionLevel::Default
                        | CompressionLevel::Level(0)
                        | CompressionLevel::Level(3)
                );
                dfast.set_hash_bits(if hinted {
                    dfast_hash_bits_for_window(max_window_size)
                } else {
                    DFAST_HASH_BITS
                });
                let vec_pool = &mut self.vec_pool;
                dfast.reset(|mut data| {
                    data.resize(data.capacity(), 0);
                    vec_pool.push(data);
                });
            }
            MatcherBackend::Row => {
                let row = self
                    .row_match_generator
                    .get_or_insert_with(|| RowMatchGenerator::new(max_window_size));
                row.max_window_size = max_window_size;
                row.lazy_depth = params.lazy_depth;
                row.configure(params.row);
                if hinted {
                    row.set_hash_bits(row_hash_bits_for_window(max_window_size));
                }
                let vec_pool = &mut self.vec_pool;
                row.reset(|mut data| {
                    data.resize(data.capacity(), 0);
                    vec_pool.push(data);
                });
            }
            MatcherBackend::HashChain => {
                let hc = self
                    .hc_match_generator
                    .get_or_insert_with(|| HcMatchGenerator::new(max_window_size));
                hc.table.max_window_size = max_window_size;
                hc.hc.lazy_depth = params.lazy_depth;
                hc.configure(params.hc, params.window_log);
                let vec_pool = &mut self.vec_pool;
                hc.reset(|mut data| {
                    data.resize(data.capacity(), 0);
                    vec_pool.push(data);
                });
            }
        }
    }

    fn prime_with_dictionary(&mut self, dict_content: &[u8], offset_hist: [u32; 3]) {
        match self.active_backend {
            MatcherBackend::Simple => self.match_generator.offset_hist = offset_hist,
            MatcherBackend::Dfast => self.dfast_matcher_mut().offset_hist = offset_hist,
            MatcherBackend::Row => self.row_matcher_mut().offset_hist = offset_hist,
            MatcherBackend::HashChain => {
                let matcher = self.hc_matcher_mut();
                matcher.table.offset_hist = offset_hist;
                matcher.table.mark_dictionary_primed();
            }
        }

        if dict_content.is_empty() {
            return;
        }

        // Dictionary bytes should stay addressable until produced frame output
        // itself exceeds the live window size.
        let retained_dict_budget = dict_content.len();
        match self.active_backend {
            MatcherBackend::Simple => {
                self.match_generator.max_window_size = self
                    .match_generator
                    .max_window_size
                    .saturating_add(retained_dict_budget);
            }
            MatcherBackend::Dfast => {
                let matcher = self.dfast_matcher_mut();
                matcher.max_window_size =
                    matcher.max_window_size.saturating_add(retained_dict_budget);
            }
            MatcherBackend::Row => {
                let matcher = self.row_matcher_mut();
                matcher.max_window_size =
                    matcher.max_window_size.saturating_add(retained_dict_budget);
            }
            MatcherBackend::HashChain => {
                let matcher = self.hc_matcher_mut();
                matcher.table.max_window_size = matcher
                    .table
                    .max_window_size
                    .saturating_add(retained_dict_budget);
            }
        }

        let mut start = 0usize;
        let mut committed_dict_budget = 0usize;
        // insert_position needs 4 bytes of lookahead for hashing;
        // backfill_boundary_positions re-visits tail positions once the
        // next slice extends history, but cannot hash <4 byte fragments.
        let min_primed_tail = match self.active_backend {
            MatcherBackend::Simple => MIN_MATCH_LEN,
            MatcherBackend::Dfast | MatcherBackend::Row | MatcherBackend::HashChain => 4,
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

        let uncommitted_tail_budget = retained_dict_budget.saturating_sub(committed_dict_budget);
        if uncommitted_tail_budget > 0 {
            match self.active_backend {
                MatcherBackend::Simple => {
                    self.match_generator.max_window_size = self
                        .match_generator
                        .max_window_size
                        .saturating_sub(uncommitted_tail_budget);
                }
                MatcherBackend::Dfast => {
                    let matcher = self.dfast_matcher_mut();
                    matcher.max_window_size = matcher
                        .max_window_size
                        .saturating_sub(uncommitted_tail_budget);
                }
                MatcherBackend::Row => {
                    let matcher = self.row_matcher_mut();
                    matcher.max_window_size = matcher
                        .max_window_size
                        .saturating_sub(uncommitted_tail_budget);
                }
                MatcherBackend::HashChain => {
                    let matcher = self.hc_matcher_mut();
                    matcher.table.max_window_size = matcher
                        .table
                        .max_window_size
                        .saturating_sub(uncommitted_tail_budget);
                }
            }
        }
        if committed_dict_budget > 0 {
            self.dictionary_retained_budget = self
                .dictionary_retained_budget
                .saturating_add(committed_dict_budget);
        }
        if self.active_backend == MatcherBackend::HashChain {
            self.hc_matcher_mut()
                .table
                .set_dictionary_limit_from_primed_bytes(committed_dict_budget);
        }
    }

    fn seed_dictionary_entropy(
        &mut self,
        huff: Option<&crate::huff0::huff0_encoder::HuffmanTable>,
        ll: Option<&crate::fse::fse_encoder::FSETable>,
        ml: Option<&crate::fse::fse_encoder::FSETable>,
        of: Option<&crate::fse::fse_encoder::FSETable>,
    ) {
        if self.active_backend == MatcherBackend::HashChain {
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
        match self.active_backend {
            MatcherBackend::Simple => self.match_generator.window.last().unwrap().data.as_slice(),
            MatcherBackend::Dfast => self.dfast_matcher().get_last_space(),
            MatcherBackend::Row => self.row_matcher().get_last_space(),
            MatcherBackend::HashChain => self.hc_matcher().table.get_last_space(),
        }
    }

    fn commit_space(&mut self, space: Vec<u8>) {
        match self.active_backend {
            MatcherBackend::Simple => {
                let vec_pool = &mut self.vec_pool;
                let mut evicted_bytes = 0usize;
                let suffixes = match self.suffix_pool.pop() {
                    Some(store) if store.slots.len() >= space.len() => store,
                    _ => SuffixStore::with_capacity(space.len()),
                };
                let suffix_pool = &mut self.suffix_pool;
                self.match_generator
                    .add_data(space, suffixes, |mut data, mut suffixes| {
                        evicted_bytes += data.len();
                        data.resize(data.capacity(), 0);
                        vec_pool.push(data);
                        suffixes.slots.clear();
                        suffixes.slots.resize(suffixes.slots.capacity(), None);
                        suffix_pool.push(suffixes);
                    });
                self.retire_dictionary_budget(evicted_bytes);
                self.trim_after_budget_retire();
            }
            MatcherBackend::Dfast => {
                let vec_pool = &mut self.vec_pool;
                let mut evicted_bytes = 0usize;
                self.dfast_match_generator
                    .as_mut()
                    .expect("dfast backend must be initialized by reset() before use")
                    .add_data(space, |mut data| {
                        evicted_bytes += data.len();
                        data.resize(data.capacity(), 0);
                        vec_pool.push(data);
                    });
                self.retire_dictionary_budget(evicted_bytes);
                self.trim_after_budget_retire();
            }
            MatcherBackend::Row => {
                let vec_pool = &mut self.vec_pool;
                let mut evicted_bytes = 0usize;
                self.row_match_generator
                    .as_mut()
                    .expect("row backend must be initialized by reset() before use")
                    .add_data(space, |mut data| {
                        evicted_bytes += data.len();
                        data.resize(data.capacity(), 0);
                        vec_pool.push(data);
                    });
                self.retire_dictionary_budget(evicted_bytes);
                self.trim_after_budget_retire();
            }
            MatcherBackend::HashChain => {
                let vec_pool = &mut self.vec_pool;
                let mut evicted_bytes = 0usize;
                self.hc_match_generator
                    .as_mut()
                    .expect("hash chain backend must be initialized by reset() before use")
                    .table
                    .add_data(space, |mut data| {
                        evicted_bytes += data.len();
                        data.resize(data.capacity(), 0);
                        vec_pool.push(data);
                    });
                self.retire_dictionary_budget(evicted_bytes);
                self.trim_after_budget_retire();
            }
        }
    }

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        match self.active_backend {
            MatcherBackend::Simple => {
                while self.match_generator.next_sequence(&mut handle_sequence) {}
            }
            MatcherBackend::Dfast => self
                .dfast_matcher_mut()
                .start_matching(&mut handle_sequence),
            MatcherBackend::Row => self.row_matcher_mut().start_matching(&mut handle_sequence),
            MatcherBackend::HashChain => self.hc_matcher_mut().start_matching(&mut handle_sequence),
        }
    }

    fn skip_matching(&mut self) {
        self.skip_matching_with_hint(None);
    }

    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        match self.active_backend {
            MatcherBackend::Simple => self
                .match_generator
                .skip_matching_with_hint(incompressible_hint),
            MatcherBackend::Dfast => self.dfast_matcher_mut().skip_matching(incompressible_hint),
            MatcherBackend::Row => self
                .row_matcher_mut()
                .skip_matching_with_hint(incompressible_hint),
            MatcherBackend::HashChain => self.hc_matcher_mut().skip_matching(incompressible_hint),
        }
    }
}

struct HcMatchGenerator {
    /// Shared match-finder storage (window, history, hash / chain /
    /// hash3 tables, dictionary-priming flags). Used identically by HC
    /// and BT modes; backend-specific table interpretation lives in the
    /// matcher methods on this struct.
    table: super::match_table::storage::MatchTable,
    /// HC (lazy / lazy2) runtime knobs. Owns lazy_depth, search_depth,
    /// target_len; method bodies still live on this struct but will
    /// move onto `impl HcMatcher` once Stage 2b threads `&mut MatchTable`
    /// through the chain-walk path.
    hc: super::hc::HcMatcher,
    parse_mode: HcParseMode,
    /// BT (btopt / btultra / btultra2) runtime state — cost model,
    /// optimal-parser scratch arenas, LDM candidate buffer. Method
    /// bodies still live on this struct; they'll move onto
    /// `impl BtMatcher` with `&mut MatchTable` threaded through in a
    /// follow-up stage.
    bt: super::bt::BtMatcher,
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
macro_rules! bt_insert_step_no_rebase_body {
    ($self:expr, $abs_pos:ident, $current_abs_end:ident, $target_abs:ident, $cmf:path) => {{
        let idx = $abs_pos - $self.table.history_abs_start;
        let concat = &$self.table.history[$self.table.history_start..];
        if idx + 8 > concat.len() {
            return 1;
        }
        let tail_limit = $current_abs_end.saturating_sub($abs_pos);
        let hash = super::match_table::storage::MatchTable::hash_position_at(
            concat,
            idx,
            $self.table.hash_log,
            super::bt::BtMatcher::HASH_MLS,
        );
        let Some(relative_pos) = $self.table.relative_position($abs_pos) else {
            return 1;
        };
        let stored = relative_pos + 1;
        let bt_mask = $self.table.bt_mask();
        let bt_low = $abs_pos.saturating_sub(bt_mask);
        let window_low = $self.table.window_low_abs_for_target($target_abs);
        let mut match_end_abs = $abs_pos.saturating_add(9);
        let mut best_len = 8usize;
        let mut compares_left = $self.hc.search_depth;
        let mut common_length_smaller = 0usize;
        let mut common_length_larger = 0usize;
        let pair_idx = $self.table.bt_pair_index_for_abs($abs_pos);
        let mut smaller_slot = pair_idx;
        let mut larger_slot = pair_idx + 1;
        let mut match_stored = $self.table.hash_table[hash];
        $self.table.hash_table[hash] = stored;

        while compares_left > 0 {
            let Some(candidate_abs) =
                super::match_table::storage::MatchTable::stored_abs_position_fast(
                    match_stored,
                    $self.table.position_base,
                    $self.table.index_shift,
                )
            else {
                break;
            };
            if candidate_abs < window_low || candidate_abs >= $abs_pos {
                break;
            }
            compares_left -= 1;

            let next_pair_idx = $self.table.bt_pair_index_for_abs(candidate_abs);
            let next_smaller = $self.table.chain_table[next_pair_idx];
            let next_larger = $self.table.chain_table[next_pair_idx + 1];
            let seed_len = common_length_smaller.min(common_length_larger);
            let candidate_idx = candidate_abs - $self.table.history_abs_start;
            // SAFETY: BT walk invariant — `candidate_idx + tail_limit ≤
            // concat.len()` since the candidate is within
            // `[history_abs_start, abs_pos)` and `tail_limit ≤
            // current_abs_end - abs_pos`.
            let match_len = unsafe { $cmf(concat, idx, candidate_idx, tail_limit, seed_len) };

            if match_len > best_len {
                best_len = match_len;
                let candidate_end = candidate_abs.saturating_add(match_len);
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
                $self.table.chain_table[smaller_slot] = match_stored;
                common_length_smaller = match_len;
                if candidate_abs <= bt_low {
                    smaller_slot = usize::MAX;
                    break;
                }
                smaller_slot = next_pair_idx + 1;
                match_stored = next_larger;
            } else {
                $self.table.chain_table[larger_slot] = match_stored;
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
            $self.table.chain_table[smaller_slot] = HC_EMPTY;
        }
        if larger_slot != usize::MAX {
            $self.table.chain_table[larger_slot] = HC_EMPTY;
        }

        let speed_positions = if best_len > 384 {
            (best_len - 384).min(192)
        } else {
            0
        };
        speed_positions.max(match_end_abs.saturating_sub($abs_pos.saturating_add(8)))
    }};
}

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
macro_rules! build_optimal_plan_impl_body {
    (
        $self:expr,
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
        let frontier_limit = $current_len.min(HC_OPT_NUM.saturating_sub(1));
        let initial_reps = $initial_state.reps;
        let initial_litlen = $initial_state.litlen;
        let mut profile = $initial_state.profile;
        profile.sufficient_match_len = $self.sufficient_match_len_for_pass(profile);
        let abort_on_worse_match = $self.parse_mode == HcParseMode::BtOpt;
        let opt_level = matches!(
            $self.parse_mode,
            HcParseMode::BtUltra | HcParseMode::BtUltra2
        );
        let mut nodes = core::mem::take(&mut $self.bt.opt_nodes_scratch);
        if nodes.len() < frontier_limit.saturating_add(2) {
            nodes.resize(frontier_limit.saturating_add(2), HcOptimalNode::default());
        }
        let mut candidates = core::mem::take(&mut $self.bt.opt_candidates_scratch);
        candidates.clear();
        if candidates.capacity() < MAX_HC_SEARCH_DEPTH {
            candidates.reserve_exact(MAX_HC_SEARCH_DEPTH - candidates.capacity());
        }
        let mut store = core::mem::take(&mut $self.bt.opt_store_scratch);
        store.clear();
        let mut ll_prices = core::mem::take(&mut $self.bt.opt_ll_price_scratch);
        let mut ll_price_generations = core::mem::take(&mut $self.bt.opt_ll_price_generation);
        if ll_prices.len() <= frontier_limit {
            ll_prices.resize(frontier_limit + 1, 0);
            ll_price_generations.resize(frontier_limit + 1, 0);
        }
        $self.bt.opt_ll_price_stamp = $self.bt.opt_ll_price_stamp.wrapping_add(1).max(1);
        let ll_price_stamp = $self.bt.opt_ll_price_stamp;
        $self.bt.opt_lit_price_stamp = $self.bt.opt_lit_price_stamp.wrapping_add(1).max(1);
        let lit_price_stamp = $self.bt.opt_lit_price_stamp;
        let mut ml_prices = core::mem::take(&mut $self.bt.opt_ml_price_scratch);
        let mut ml_price_generations = core::mem::take(&mut $self.bt.opt_ml_price_generation);
        if ml_prices.len() <= frontier_limit {
            ml_prices.resize(frontier_limit + 1, 0);
            ml_price_generations.resize(frontier_limit + 1, 0);
        }
        $self.bt.opt_ml_price_stamp = $self.bt.opt_ml_price_stamp.wrapping_add(1).max(1);
        let ml_price_stamp = $self.bt.opt_ml_price_stamp;
        nodes[0] = HcOptimalNode {
            price: HcMatchGenerator::cached_lit_length_price(
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
        let ll0_price = HcMatchGenerator::cached_lit_length_price(
            profile,
            $stats,
            0,
            &mut ll_prices,
            &mut ll_price_generations,
            ll_price_stamp,
        );
        let ll1_price = HcMatchGenerator::cached_lit_length_price(
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
                size: $self.bt.ldm_sequences.len(),
            },
            ..HcOptLdmState::default()
        };
        let has_ldm = !$self.bt.ldm_sequences.is_empty();
        if has_ldm {
            $self.ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 0, $current_len);
        }

        // Donor-like seed at rPos=0: initialize frontier with matches starting
        // at current position before entering the generic forward DP loop.
        if $current_len >= min_match_len {
            let seed_ldm = if has_ldm {
                $self.ldm_process_match_candidate(&mut opt_ldm, 0, $current_len, min_match_len)
            } else {
                None
            };
            candidates.clear();
            // SAFETY: wrapper is in the same target_feature umbrella as the
            // `$collect` kernel variant; the runtime kernel detector already
            // gated entry into the wrapper.
            unsafe {
                $self.$collect::<true>(
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
                last_pos = min_match_len.saturating_sub(1).min(frontier_limit);
                for p in 1..min_match_len.min(nodes.len()) {
                    HcMatchGenerator::reset_opt_node(&mut nodes[p]);
                    nodes[p].litlen = initial_litlen.saturating_add(p) as u32;
                }
            }

            if let Some(candidate) = candidates.last() {
                let longest_len = candidate.match_len.min($current_len);
                if longest_len > sufficient_len {
                    let off_base = HcMatchGenerator::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        initial_litlen,
                        initial_reps,
                    );
                    let off_price = profile
                        .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                    let ml_price = HcMatchGenerator::cached_match_length_price(
                        profile,
                        $stats,
                        longest_len,
                        &mut ml_prices,
                        &mut ml_price_generations,
                        ml_price_stamp,
                    );
                    let seq_cost = HcMatchGenerator::add_prices(
                        ll0_price,
                        profile.match_price_from_parts(off_price, ml_price, $stats),
                    );
                    let forced_price = HcMatchGenerator::add_prices(nodes[0].price, seq_cost);
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
                let mut prev_max_len = min_match_len.saturating_sub(1);
                for candidate in candidates.iter() {
                    let max_match_len = candidate.match_len.min(frontier_limit);
                    if max_match_len < min_match_len {
                        continue;
                    }
                    let start_len = prev_max_len.saturating_add(1).max(min_match_len);
                    if start_len > max_match_len {
                        prev_max_len = prev_max_len.max(max_match_len);
                        continue;
                    }
                    if max_match_len > last_pos {
                        HcMatchGenerator::reset_opt_nodes(&mut nodes, last_pos + 1, max_match_len);
                    }
                    let off_base = HcMatchGenerator::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        initial_litlen,
                        initial_reps,
                    );
                    let off_price = profile
                        .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                    debug_assert!(max_match_len < nodes.len());
                    let nodes0_price = nodes[0].price;
                    for match_len in (start_len..=max_match_len).rev() {
                        let ml_price = HcMatchGenerator::cached_match_length_price(
                            profile,
                            $stats,
                            match_len,
                            &mut ml_prices,
                            &mut ml_price_generations,
                            ml_price_stamp,
                        );
                        let seq_cost = HcMatchGenerator::add_prices(
                            ll0_price,
                            profile.match_price_from_parts(off_price, ml_price, $stats),
                        );
                        let next_cost = HcMatchGenerator::add_prices(nodes0_price, seq_cost);
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
                let lit_price = HcMatchGenerator::cached_literal_price(
                    profile,
                    $stats,
                    $current[pos - 1],
                    &mut $self.bt.opt_lit_price_scratch,
                    &mut $self.bt.opt_lit_price_generation,
                    lit_price_stamp,
                );
                let ll_delta = HcMatchGenerator::cached_lit_length_delta_price(
                    profile,
                    $stats,
                    lit_len,
                    &mut ll_prices,
                    &mut ll_price_generations,
                    ll_price_stamp,
                );
                let lit_cost =
                    HcMatchGenerator::add_price_delta(prev_node.price, lit_price, ll_delta);
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
                            let next_lit_price = HcMatchGenerator::cached_literal_price(
                                profile,
                                $stats,
                                $current[pos],
                                &mut $self.bt.opt_lit_price_scratch,
                                &mut $self.bt.opt_lit_price_generation,
                                lit_price_stamp,
                            );
                            let with1literal = HcMatchGenerator::add_price_delta(
                                prev_match.price,
                                next_lit_price,
                                ll1_price as i32 - ll0_price as i32,
                            );
                            let ll_delta_next = HcMatchGenerator::cached_lit_length_delta_price(
                                profile,
                                $stats,
                                lit_len.saturating_add(1),
                                &mut ll_prices,
                                &mut ll_price_generations,
                                ll_price_stamp,
                            );
                            let with_more_literals = HcMatchGenerator::add_price_delta(
                                lit_cost,
                                next_lit_price,
                                ll_delta_next,
                            );
                            let next = pos + 1;
                            let next_price = unsafe { nodes.get_unchecked(next).price };
                            if with1literal < with_more_literals && with1literal < next_price {
                                // Donor parity (zstd_opt.c:1232): `cur >= prevMatch.mlen`.
                                debug_assert!(pos >= prev_match.mlen as usize);
                                let prev_pos = pos - prev_match.mlen as usize;
                                {
                                    let prev_state = unsafe { *nodes.get_unchecked(prev_pos) };
                                    let (_, reps_after_match) =
                                        HcMatchGenerator::encode_offset_with_reps(
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
                    let (_, reps_after_match) = HcMatchGenerator::encode_offset_with_reps(
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
                $self.ldm_process_match_candidate(
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
                $self.$collect::<true>(
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
                    let off_base = HcMatchGenerator::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        lit_len,
                        base_node.reps,
                    );
                    let off_price = profile
                        .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                    let ml_price = HcMatchGenerator::cached_match_length_price(
                        profile,
                        $stats,
                        longest_len,
                        &mut ml_prices,
                        &mut ml_price_generations,
                        ml_price_stamp,
                    );
                    let seq_cost = HcMatchGenerator::add_prices(
                        ll0_price,
                        profile.match_price_from_parts(off_price, ml_price, $stats),
                    );
                    let forced_price = HcMatchGenerator::add_prices(base_cost, seq_cost);
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
            let mut prev_max_len = min_match_len.saturating_sub(1);
            for candidate in candidates.iter() {
                let max_match_len = candidate
                    .match_len
                    .min($current_len - pos)
                    .min(frontier_limit.saturating_sub(pos));
                let min_len = min_match_len;
                if max_match_len < min_len {
                    continue;
                }
                let start_len = prev_max_len.saturating_add(1).max(min_len);
                if start_len > max_match_len {
                    prev_max_len = prev_max_len.max(max_match_len);
                    continue;
                }
                let max_next = pos + max_match_len;
                if max_next > last_pos {
                    HcMatchGenerator::reset_opt_nodes(&mut nodes, last_pos + 1, max_next);
                }
                let lit_len = base_node.litlen as usize;
                let off_base = HcMatchGenerator::encode_offset_base_with_reps(
                    candidate.offset as u32,
                    lit_len,
                    base_node.reps,
                );
                let off_price = profile
                    .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                debug_assert!(pos + max_match_len < nodes.len());
                for match_len in (start_len..=max_match_len).rev() {
                    let next = pos + match_len;
                    let ml_price = HcMatchGenerator::cached_match_length_price(
                        profile,
                        $stats,
                        match_len,
                        &mut ml_prices,
                        &mut ml_price_generations,
                        ml_price_stamp,
                    );
                    let seq_cost = HcMatchGenerator::add_prices(
                        ll0_price,
                        profile.match_price_from_parts(off_price, ml_price, $stats),
                    );
                    let next_cost = HcMatchGenerator::add_prices(base_cost, seq_cost);
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
                return $self.finish_optimal_plan(
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
            let lit_price = HcMatchGenerator::cached_literal_price(
                profile,
                $stats,
                $current[0],
                &mut $self.bt.opt_lit_price_scratch,
                &mut $self.bt.opt_lit_price_generation,
                lit_price_stamp,
            );
            let next_litlen = initial_litlen.saturating_add(1);
            let ll_delta = HcMatchGenerator::cached_lit_length_delta_price(
                profile,
                $stats,
                next_litlen,
                &mut ll_prices,
                &mut ll_price_generations,
                ll_price_stamp,
            );
            let price = HcMatchGenerator::add_price_delta(nodes[0].price, lit_price, ll_delta);
            return $self.finish_optimal_plan(
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
            return $self.finish_optimal_plan(
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
            return $self.finish_optimal_plan(
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
            let (_, reps_after_match) = HcMatchGenerator::encode_offset_with_reps(
                last_stretch.off,
                prev_state.litlen as usize,
                prev_state.reps,
            );
            reps_after_match
        } else {
            let tail_literals = last_stretch.litlen as usize;
            if cur < tail_literals {
                return $self.finish_optimal_plan(
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
            let step = (next_stretch.litlen as usize).saturating_add(next_stretch.mlen as usize);
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
        $self.finish_optimal_plan(
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
        debug_assert!(!$self.table.hash_table.is_empty());
        debug_assert!($self.table.hash3_log == 0 || !$self.table.hash3_table.is_empty());
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
            unsafe { $self.$bt_update($abs_pos, $current_abs_end) };
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
                    if $abs_pos.saturating_add(rep.match_len) >= $current_abs_end {
                        skip_further_match_search = true;
                    }
                },
            )
        };
        if !skip_further_match_search && best_len_for_skip < min_match_len {
            $self.update_hash3_until($abs_pos);
            // SAFETY: same umbrella for hash3_candidate.
            if let Some(h3) = unsafe {
                $self
                    .bt
                    .$hash3(&$self.table, $abs_pos, $current_abs_end, min_match_len)
            } {
                let _ = super::bt::BtMatcher::push_candidate_ladder(
                    $out,
                    &mut best_len_for_skip,
                    h3,
                    min_match_len,
                );
                if !rep_len_candidate_found
                    && (h3.match_len > $profile.sufficient_match_len
                        || $abs_pos.saturating_add(h3.match_len) >= $current_abs_end)
                {
                    $self.table.skip_insert_until_abs = $abs_pos.saturating_add(1);
                    skip_further_match_search = true;
                }
            }
        }
        if !skip_further_match_search && $bt_matchfinder {
            // SAFETY: same umbrella for bt_insert_and_collect_matches.
            unsafe {
                $self.$bt_insert(
                    $abs_pos,
                    $current_abs_end,
                    $profile,
                    min_match_len,
                    &mut best_len_for_skip,
                    $out,
                )
            };
        } else if !skip_further_match_search {
            $self.insert_position($abs_pos);
            let max_chain_depth = $profile.max_chain_depth.min($self.hc.search_depth);
            let concat = &$self.table.history[$self.table.history_start..];
            let mut match_end_abs = $abs_pos.saturating_add(9);
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
                    let tail_limit = $current_abs_end.saturating_sub($abs_pos);
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
                        let candidate_end = candidate_abs.saturating_add(match_len);
                        if candidate_end > match_end_abs {
                            match_end_abs = candidate_end;
                        }
                    }
                    if match_len > HC_OPT_NUM
                        || $abs_pos.saturating_add(match_len) >= $current_abs_end
                    {
                        break;
                    }
                }
            }
            $self.table.skip_insert_until_abs = $self
                .table
                .skip_insert_until_abs
                .max(match_end_abs.saturating_sub(8));
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
/// segment.
#[macro_export]
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

/// `for_each_repcode_candidate_with_reps` body parameterized over the per-CPU
/// `common_prefix_len_ptr` symbol so the per-rep prefix probe inlines under
/// the wrapper's `target_feature` umbrella instead of crossing the ABI
/// boundary through the dispatcher. Three rep probes per encoded position →
/// thousands per segment, so the per-call barrier was non-trivial.
///
/// The callback `f` runs in the wrapper's umbrella context too, so closures
/// that capture mutable state still work (FnMut).
#[macro_export]
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

/// `bt_insert_and_collect_matches` body parameterized over the per-CPU
/// `count_match_from_indices` symbol. Same shape as
/// [`bt_insert_step_no_rebase_body`] — picks up the matching kernel through
/// `$cmf` so the per-iteration vector probe inlines under the wrapper's
/// `target_feature` umbrella. Returns nothing (matches the original method).
macro_rules! bt_insert_and_collect_matches_body {
    (
        $self:expr,
        $abs_pos:ident,
        $current_abs_end:ident,
        $profile:ident,
        $min_match_len:ident,
        $best_len_for_skip:ident,
        $out:ident,
        $cmf:path $(,)?
    ) => {{
        let idx = $abs_pos - $self.table.history_abs_start;
        let concat = &$self.table.history[$self.table.history_start..];
        if idx + 8 > concat.len() {
            return;
        }
        let tail_limit = $current_abs_end.saturating_sub($abs_pos);
        let hash = super::match_table::storage::MatchTable::hash_position_at(
            concat,
            idx,
            $self.table.hash_log,
            super::bt::BtMatcher::HASH_MLS,
        );
        let Some(relative_pos) = $self.table.relative_position($abs_pos) else {
            return;
        };
        let stored = relative_pos + 1;
        let bt_mask = $self.table.bt_mask();
        let bt_low = $abs_pos.saturating_sub(bt_mask);
        let window_low = $self.table.window_low_abs_for_target($abs_pos);
        let mut match_end_abs = $abs_pos.saturating_add(9);
        let mut compares_left = $profile.max_chain_depth.min($self.hc.search_depth);
        let mut common_length_smaller = 0usize;
        let mut common_length_larger = 0usize;
        let pair_idx = $self.table.bt_pair_index_for_abs($abs_pos);
        let mut smaller_slot = pair_idx;
        let mut larger_slot = pair_idx + 1;
        let mut match_stored = $self.table.hash_table[hash];
        $self.table.hash_table[hash] = stored;
        // Donor semantics: `bestLength` starts at `lengthToBeat - 1`; rep/hash3
        // probing may raise it; BT then only reports strictly longer matches.
        let mut best_len = (*$best_len_for_skip).max($min_match_len.saturating_sub(1));

        while compares_left > 0 {
            let Some(candidate_abs) =
                super::match_table::storage::MatchTable::stored_abs_position_fast(
                    match_stored,
                    $self.table.position_base,
                    $self.table.index_shift,
                )
            else {
                break;
            };
            if candidate_abs < window_low || candidate_abs >= $abs_pos {
                break;
            }
            compares_left -= 1;

            let next_pair_idx = $self.table.bt_pair_index_for_abs(candidate_abs);
            let next_smaller = $self.table.chain_table[next_pair_idx];
            let next_larger = $self.table.chain_table[next_pair_idx + 1];
            let seed_len = common_length_smaller.min(common_length_larger);
            let candidate_idx = candidate_abs - $self.table.history_abs_start;
            // SAFETY: BT walk invariant — `candidate_idx + tail_limit ≤
            // concat.len()`.
            let match_len = unsafe { $cmf(concat, idx, candidate_idx, tail_limit, seed_len) };

            if match_len > best_len {
                let offset = $abs_pos - candidate_abs;
                let accepted = super::bt::BtMatcher::push_candidate_ladder(
                    $out,
                    $best_len_for_skip,
                    MatchCandidate {
                        start: $abs_pos,
                        offset,
                        match_len,
                    },
                    $min_match_len,
                );
                if accepted {
                    best_len = match_len;
                    let candidate_end = candidate_abs.saturating_add(match_len);
                    if candidate_end > match_end_abs {
                        match_end_abs = candidate_end;
                    }
                    if match_len >= tail_limit || match_len > HC_OPT_NUM {
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
                $self.table.chain_table[smaller_slot] = match_stored;
                common_length_smaller = match_len;
                if candidate_abs <= bt_low {
                    smaller_slot = usize::MAX;
                    break;
                }
                smaller_slot = next_pair_idx + 1;
                match_stored = next_larger;
            } else {
                $self.table.chain_table[larger_slot] = match_stored;
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
            $self.table.chain_table[smaller_slot] = HC_EMPTY;
        }
        if larger_slot != usize::MAX {
            $self.table.chain_table[larger_slot] = HC_EMPTY;
        }
        $self.table.skip_insert_until_abs = match_end_abs.saturating_sub(8);
    }};
}

impl HcMatchGenerator {
    fn donor_opt_start_cursor_and_litlen(&self, current_abs_start: usize) -> (usize, usize) {
        let start_cursor = usize::from(current_abs_start == self.table.history_abs_start);
        (start_cursor, start_cursor)
    }

    fn sufficient_match_len_for_pass(&self, profile: HcOptimalCostProfile) -> usize {
        profile
            .sufficient_match_len
            .min(self.hc.target_len)
            .clamp(HC_OPT_MIN_MATCH_LEN, HC_OPT_NUM - 1)
    }

    fn should_run_btultra2_seed_pass(&self, current_len: usize) -> bool {
        self.parse_mode == HcParseMode::BtUltra2
            && self.bt.opt_state.lit_length_sum == 0
            && self.bt.opt_state.dictionary_seed.is_none()
            && !self.table.dictionary_primed_for_frame
            && self.bt.ldm_sequences.is_empty()
            && self.table.window_size == current_len
            && self.table.history_abs_start == 0
            && self.table.window.len() == 1
            && current_len > HC_PREDEF_THRESHOLD
    }

    fn encode_offset_with_reps(
        actual_offset: u32,
        lit_len: usize,
        reps: [u32; 3],
    ) -> (u32, [u32; 3]) {
        let mut next_reps = reps;
        let encoded = if lit_len > 0 {
            if actual_offset == reps[0] {
                1
            } else if actual_offset == reps[1] {
                2
            } else if actual_offset == reps[2] {
                3
            } else {
                actual_offset.saturating_add(3)
            }
        } else if actual_offset == reps[1] {
            1
        } else if actual_offset == reps[2] {
            2
        } else if reps[0] > 1 && actual_offset == reps[0] - 1 {
            3
        } else {
            actual_offset.saturating_add(3)
        };

        if lit_len > 0 {
            match encoded {
                1 => {}
                2 => {
                    next_reps[1] = next_reps[0];
                    next_reps[0] = actual_offset;
                }
                _ => {
                    next_reps[2] = next_reps[1];
                    next_reps[1] = next_reps[0];
                    next_reps[0] = actual_offset;
                }
            }
        } else {
            match encoded {
                1 => {
                    next_reps[1] = next_reps[0];
                    next_reps[0] = actual_offset;
                }
                _ => {
                    next_reps[2] = next_reps[1];
                    next_reps[1] = next_reps[0];
                    next_reps[0] = actual_offset;
                }
            }
        }

        (encoded, next_reps)
    }

    #[inline(always)]
    fn encode_offset_base_with_reps(actual_offset: u32, lit_len: usize, reps: [u32; 3]) -> u32 {
        if lit_len > 0 {
            if actual_offset == reps[0] {
                1
            } else if actual_offset == reps[1] {
                2
            } else if actual_offset == reps[2] {
                3
            } else {
                actual_offset.saturating_add(3)
            }
        } else if actual_offset == reps[1] {
            1
        } else if actual_offset == reps[2] {
            2
        } else if reps[0] > 1 && actual_offset == reps[0] - 1 {
            3
        } else {
            actual_offset.saturating_add(3)
        }
    }

    fn new(max_window_size: usize) -> Self {
        Self {
            table: super::match_table::storage::MatchTable::new(max_window_size),
            hc: super::hc::HcMatcher::new(2, HC_SEARCH_DEPTH, HC_TARGET_LEN),
            parse_mode: HcParseMode::Lazy2,
            bt: super::bt::BtMatcher::new(),
        }
    }

    fn configure(&mut self, config: HcConfig, window_log: u8) {
        let next_hash3_log = if config.parse_mode == HcParseMode::BtUltra2 {
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
        self.hc.search_depth = if matches!(
            config.parse_mode,
            HcParseMode::BtOpt | HcParseMode::BtUltra | HcParseMode::BtUltra2
        ) {
            config.search_depth
        } else {
            config.search_depth.min(MAX_HC_SEARCH_DEPTH)
        };
        self.hc.target_len = config.target_len;
        self.parse_mode = config.parse_mode;
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
        self.bt.opt_state.seed_dictionary_entropy(huff, ll, ml, of);
    }

    fn reset(&mut self, reuse_space: impl FnMut(Vec<u8>)) {
        self.table.reset(reuse_space);
        self.bt.reset();
    }

    /// Backfill positions from the tail of the previous slice that couldn't be
    /// hashed at the time (insert_position needs 4 bytes of lookahead).
    fn backfill_boundary_positions(&mut self, current_abs_start: usize, current_abs_end: usize) {
        let backfill_start = current_abs_start
            .saturating_sub(3)
            .max(self.table.history_abs_start);
        if backfill_start < current_abs_start {
            if self.uses_bt_matchfinder() {
                self.bt_update_tree_until(current_abs_start, current_abs_end);
            } else {
                self.insert_positions(backfill_start, current_abs_start);
            }
        }
    }

    fn apply_limited_update_after_long_match(&mut self, current_abs_start: usize) {
        if !self.uses_bt_matchfinder() {
            return;
        }
        let gap = current_abs_start.saturating_sub(self.table.skip_insert_until_abs);
        if gap > 384 {
            self.table.skip_insert_until_abs = current_abs_start - (gap - 384).min(192);
        }
    }

    fn skip_matching(&mut self, incompressible_hint: Option<bool>) {
        self.table.ensure_tables();
        let current_len = self.table.window.back().unwrap().len();
        let current_abs_start = self.table.history_abs_start + self.table.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        self.backfill_boundary_positions(current_abs_start, current_abs_end);
        if self.uses_bt_matchfinder() {
            if incompressible_hint == Some(true) {
                self.bt_insert_sparse_incompressible_block(current_abs_start, current_abs_end);
                return;
            }
            self.bt_update_tree_until(current_abs_end, current_abs_end);
            return;
        }
        if incompressible_hint == Some(true) {
            self.insert_positions_with_step(
                current_abs_start,
                current_abs_end,
                INCOMPRESSIBLE_SKIP_STEP,
            );
            let dense_tail = HC_MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
            let tail_start = current_abs_end
                .saturating_sub(dense_tail)
                .max(self.table.history_abs_start);
            let tail_start = tail_start.max(current_abs_start);
            for pos in tail_start..current_abs_end {
                if !(pos - current_abs_start).is_multiple_of(INCOMPRESSIBLE_SKIP_STEP) {
                    self.insert_position(pos);
                }
            }
        } else {
            self.insert_positions(current_abs_start, current_abs_end);
        }
    }

    fn bt_insert_sparse_incompressible_block(
        &mut self,
        current_abs_start: usize,
        current_abs_end: usize,
    ) {
        let mut pos = current_abs_start;
        while pos < current_abs_end {
            self.maybe_rebase_positions(pos);
            let _ = self.bt_insert_step_no_rebase(pos, current_abs_end, current_abs_end);
            self.table.insert_hash3_only_no_rebase(pos);
            let next = pos.saturating_add(INCOMPRESSIBLE_SKIP_STEP);
            if next <= pos {
                break;
            }
            pos = next;
        }

        let dense_tail = HC_MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
        let tail_start = current_abs_end
            .saturating_sub(dense_tail)
            .max(self.table.history_abs_start)
            .max(current_abs_start);
        for pos in tail_start..current_abs_end {
            if (pos - current_abs_start).is_multiple_of(INCOMPRESSIBLE_SKIP_STEP) {
                continue;
            }
            self.maybe_rebase_positions(pos);
            let _ = self.bt_insert_step_no_rebase(pos, current_abs_end, current_abs_end);
            self.table.insert_hash3_only_no_rebase(pos);
        }

        self.table.skip_insert_until_abs = self.table.skip_insert_until_abs.max(current_abs_end);
        self.table.next_to_update3 = self.table.next_to_update3.max(current_abs_end);
    }

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        match self.parse_mode {
            HcParseMode::Lazy2 => self.start_matching_lazy(&mut handle_sequence),
            HcParseMode::BtOpt | HcParseMode::BtUltra | HcParseMode::BtUltra2 => {
                self.start_matching_optimal(&mut handle_sequence)
            }
        }
    }

    fn start_matching_lazy(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        self.table.ensure_tables();

        let current_len = self.table.window.back().unwrap().len();
        if current_len == 0 {
            return;
        }

        let current_abs_start = self.table.history_abs_start + self.table.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        self.backfill_boundary_positions(current_abs_start, current_abs_end);

        let mut pos = 0usize;
        let mut literals_start = 0usize;
        while pos + HC_MIN_MATCH_LEN <= current_len {
            let abs_pos = current_abs_start + pos;
            let lit_len = pos - literals_start;

            let best = self.hc.find_best_match(&self.table, abs_pos, lit_len);
            if let Some(candidate) = self.hc.pick_lazy_match(&self.table, abs_pos, lit_len, best) {
                self.insert_positions(abs_pos, candidate.start + candidate.match_len);
                let current = self.table.window.back().unwrap().as_slice();
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
                self.insert_position(abs_pos);
                pos += 1;
            }
        }

        // Insert remaining hashable positions in the tail (the matching loop
        // stops at HC_MIN_MATCH_LEN but insert_position only needs 4 bytes).
        while pos + 4 <= current_len {
            self.insert_position(current_abs_start + pos);
            pos += 1;
        }

        if literals_start < current_len {
            let current = self.table.window.back().unwrap().as_slice();
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    fn start_matching_optimal(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        self.table.ensure_tables();
        let current_len = self.table.window.back().unwrap().len();
        if current_len == 0 {
            return;
        }
        let current_ptr = self.table.window.back().unwrap().as_ptr();
        // `start_matching_optimal()` mutates tables/state but never mutates or
        // reorders `self.table.window`, so this block slice remains valid for the
        // duration of the routine and avoids cloning the full block.
        let current = unsafe { core::slice::from_raw_parts(current_ptr, current_len) };

        let current_abs_start = self.table.history_abs_start + self.table.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        self.apply_limited_update_after_long_match(current_abs_start);
        let hash3_start_cursor = self
            .table
            .skip_insert_until_abs
            .max(self.table.history_abs_start);
        self.backfill_boundary_positions(current_abs_start, current_abs_end);
        self.table.next_to_update3 = hash3_start_cursor;
        self.prepare_ldm_candidates(current_abs_start, current_len);

        if self.should_run_btultra2_seed_pass(current_len) {
            self.run_btultra2_seed_pass(current, current_abs_start, current_len);
        }

        let profile = if self.parse_mode == HcParseMode::BtUltra2 {
            // Donor btultra2 runs one accurate opt pass after initStats_ultra.
            HcOptimalCostProfile::for_mode(self.parse_mode, true)
        } else {
            HcOptimalCostProfile::for_mode(self.parse_mode, false)
        };
        let mut opt_state = core::mem::replace(&mut self.bt.opt_state, HcOptState::new());
        opt_state.rescale_freqs(current, profile);
        let mut best_plan = core::mem::take(&mut self.bt.opt_segment_plan_scratch);
        best_plan.clear();
        let mut plan_reps = self.table.offset_hist;
        let (mut cursor, mut plan_litlen) =
            self.donor_opt_start_cursor_and_litlen(current_abs_start);
        let mut plan_literals_cursor = 0usize;
        let match_loop_limit = current_len.saturating_sub(8);
        while cursor < match_loop_limit {
            let remaining_len = current_len - cursor;
            let segment_abs_start = current_abs_start + cursor;
            let segment_start = best_plan.len();
            let (_, end_reps, end_litlen, consumed_len) = self.build_optimal_plan(
                &current[cursor..],
                segment_abs_start,
                remaining_len,
                HcOptimalPlanState {
                    reps: plan_reps,
                    litlen: plan_litlen,
                    profile,
                },
                &opt_state,
                &mut best_plan,
            );
            Self::update_plan_stats_segment(
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

        self.emit_optimal_plan(current_len, &best_plan, &mut handle_sequence);
        best_plan.clear();
        self.bt.opt_segment_plan_scratch = best_plan;
        self.bt.opt_state = opt_state;
    }

    fn run_btultra2_seed_pass(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
    ) {
        let seed_profile = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, true);
        let mut opt_state = core::mem::replace(&mut self.bt.opt_state, HcOptState::new());
        opt_state.rescale_freqs(current, seed_profile);
        let mut seed_reps = self.table.offset_hist;
        let (mut cursor, mut seed_litlen) =
            self.donor_opt_start_cursor_and_litlen(current_abs_start);
        let mut seed_literals_cursor = 0usize;
        let mut seed_plan = core::mem::take(&mut self.bt.opt_seed_plan_scratch);
        seed_plan.clear();
        let match_loop_limit = current_len.saturating_sub(8);
        while cursor < match_loop_limit {
            let remaining_len = current_len - cursor;
            let segment_abs_start = current_abs_start + cursor;
            let segment_start = seed_plan.len();
            let (_, end_reps, end_litlen, consumed_len) = self.build_optimal_plan(
                &current[cursor..],
                segment_abs_start,
                remaining_len,
                HcOptimalPlanState {
                    reps: seed_reps,
                    litlen: seed_litlen,
                    profile: seed_profile,
                },
                &opt_state,
                &mut seed_plan,
            );
            Self::update_plan_stats_segment(
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
        self.bt.opt_seed_plan_scratch = seed_plan;
        self.bt.opt_state = opt_state;

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

    fn update_plan_stats_segment(
        current: &[u8],
        current_len: usize,
        plan: &[HcOptimalSequence],
        literals_start: &mut usize,
        reps: &mut [u32; 3],
        opt_state: &mut HcOptState,
        accurate: bool,
    ) {
        if plan.is_empty() {
            return;
        }
        for item in plan {
            let lit_len = item.lit_len as usize;
            let match_len = item.match_len as usize;
            let start = literals_start.saturating_add(lit_len);
            if start < *literals_start || start + match_len > current_len {
                continue;
            }
            let literals = &current[*literals_start..start];
            let (off_base, next_reps) =
                Self::encode_offset_with_reps(item.offset, literals.len(), *reps);
            opt_state.update_stats(literals.len(), literals, off_base, match_len);
            *reps = next_reps;
            *literals_start = start + match_len;
        }
        opt_state.set_base_prices(accurate);
    }

    fn build_optimal_plan(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        let profile = initial_state.profile;
        match (profile.accurate, profile.favor_small_offsets) {
            (true, false) => self.build_optimal_plan_impl::<true, false>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            ),
            (true, true) => self.build_optimal_plan_impl::<true, true>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            ),
            (false, false) => self.build_optimal_plan_impl::<false, false>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            ),
            (false, true) => self.build_optimal_plan_impl::<false, true>(
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
    fn build_optimal_plan_impl<const ACCURATE_PRICE: bool, const FAVOR_SMALL_OFFSETS: bool>(
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
            self.build_optimal_plan_impl_neon::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
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
                    self.build_optimal_plan_impl_avx2_bmi2::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                        current,
                        current_abs_start,
                        current_len,
                        initial_state,
                        stats,
                        out,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.build_optimal_plan_impl_sse42::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                        current,
                        current_abs_start,
                        current_len,
                        initial_state,
                        stats,
                        out,
                    )
                },
                FastpathKernel::Scalar => self
                    .build_optimal_plan_impl_scalar::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
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
            self.build_optimal_plan_impl_scalar::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
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
            current,
            current_abs_start,
            current_len,
            initial_state,
            stats,
            out,
            collect_optimal_candidates_initialized_scalar,
        )
    }

    fn finish_optimal_plan(
        &mut self,
        buffers: HcOptimalPlanBuffers,
        result: (u32, [u32; 3], usize, usize),
    ) -> (u32, [u32; 3], usize, usize) {
        let HcOptimalPlanBuffers {
            nodes,
            mut candidates,
            store,
            ll_prices,
            ll_price_generations,
            ml_prices,
            ml_price_generations,
        } = buffers;
        candidates.clear();
        self.bt.opt_nodes_scratch = nodes;
        self.bt.opt_candidates_scratch = candidates;
        self.bt.opt_store_scratch = store;
        self.bt.opt_ll_price_scratch = ll_prices;
        self.bt.opt_ll_price_generation = ll_price_generations;
        self.bt.opt_ml_price_scratch = ml_prices;
        self.bt.opt_ml_price_generation = ml_price_generations;
        result
    }

    #[inline(always)]
    fn reset_opt_nodes(nodes: &mut [HcOptimalNode], start: usize, end: usize) {
        for node in &mut nodes[start..=end] {
            Self::reset_opt_node(node);
        }
    }

    #[inline(always)]
    fn reset_opt_node(node: &mut HcOptimalNode) {
        node.price = u32::MAX;
        // Donor only marks the slot as unreachable and not end-of-match here;
        // stale mlen is ignored while price is MAX and litlen is non-zero.
        node.litlen = u32::MAX;
    }

    #[inline(always)]
    fn add_price_delta(price: u32, add: u32, delta: i32) -> u32 {
        #[cfg(debug_assertions)]
        {
            let sum = price as i64 + add as i64 + delta as i64;
            debug_assert!((0..=u32::MAX as i64).contains(&sum));
        }
        price.wrapping_add(add).wrapping_add_signed(delta)
    }

    #[inline(always)]
    fn add_prices(lhs: u32, rhs: u32) -> u32 {
        let sum = lhs + rhs;
        debug_assert!(sum >= lhs);
        sum
    }

    #[inline(always)]
    fn cached_literal_price(
        profile: HcOptimalCostProfile,
        stats: &HcOptState,
        byte: u8,
        prices: &mut [u32; HC_MAX_LIT + 1],
        generations: &mut [u32; HC_MAX_LIT + 1],
        stamp: u32,
    ) -> u32 {
        // SAFETY: `byte as usize` is `0..256` and the fixed-size arrays are
        // `[u32; HC_MAX_LIT + 1 = 257]`, so the index is statically in bounds.
        // Each cached_*_price call sits inside the optimal parser per-byte
        // hot loop where these bounds checks are pure overhead.
        let idx = byte as usize;
        unsafe {
            if *generations.get_unchecked(idx) == stamp {
                return *prices.get_unchecked(idx);
            }
            let price = profile.literal_price(stats, byte);
            *prices.get_unchecked_mut(idx) = price;
            *generations.get_unchecked_mut(idx) = stamp;
            price
        }
    }

    #[inline(always)]
    fn cached_lit_length_price(
        profile: HcOptimalCostProfile,
        stats: &HcOptState,
        lit_len: usize,
        prices: &mut [u32],
        generations: &mut [u32],
        stamp: u32,
    ) -> u32 {
        if lit_len >= prices.len() {
            return profile.lit_length_price(stats, lit_len);
        }
        // SAFETY: the early-return above proves `lit_len < prices.len()`. The
        // matching `generations` slice is sized identically by the caller in
        // `build_optimal_plan_impl` (`opt_ll_price_scratch` /
        // `opt_ll_price_generation` are `resize`d together), so the same
        // index is in bounds for both.
        unsafe {
            if *generations.get_unchecked(lit_len) == stamp {
                return *prices.get_unchecked(lit_len);
            }
            let price = profile.lit_length_price(stats, lit_len);
            *prices.get_unchecked_mut(lit_len) = price;
            *generations.get_unchecked_mut(lit_len) = stamp;
            price
        }
    }

    #[inline(always)]
    fn cached_lit_length_delta_price(
        profile: HcOptimalCostProfile,
        stats: &HcOptState,
        lit_len: usize,
        prices: &mut [u32],
        generations: &mut [u32],
        stamp: u32,
    ) -> i32 {
        if lit_len == 0 {
            return profile.lit_length_price(stats, lit_len) as i32
                - profile.lit_length_price(stats, lit_len.saturating_sub(1)) as i32;
        }
        let price =
            Self::cached_lit_length_price(profile, stats, lit_len, prices, generations, stamp);
        let previous =
            Self::cached_lit_length_price(profile, stats, lit_len - 1, prices, generations, stamp);
        price as i32 - previous as i32
    }

    #[inline(always)]
    fn cached_match_length_price(
        profile: HcOptimalCostProfile,
        stats: &HcOptState,
        match_len: usize,
        prices: &mut [u32],
        generations: &mut [u32],
        stamp: u32,
    ) -> u32 {
        if match_len >= prices.len() {
            return profile.match_length_price(stats, match_len);
        }
        // SAFETY: see `cached_lit_length_price` — the caller co-sizes
        // `opt_ml_price_scratch` and `opt_ml_price_generation`, and the
        // early return proves `match_len < prices.len()`.
        unsafe {
            if *generations.get_unchecked(match_len) == stamp {
                return *prices.get_unchecked(match_len);
            }
            let price = profile.match_length_price(stats, match_len);
            *prices.get_unchecked_mut(match_len) = price;
            *generations.get_unchecked_mut(match_len) = stamp;
            price
        }
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
        self.table.ensure_tables();
        if self.uses_bt_matchfinder() {
            self.collect_optimal_candidates_initialized::<true>(
                abs_pos,
                current_abs_end,
                profile,
                query,
                out,
            );
        } else {
            self.collect_optimal_candidates_initialized::<false>(
                abs_pos,
                current_abs_end,
                profile,
                query,
                out,
            );
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
    fn collect_optimal_candidates_initialized<const USE_BT_MATCHFINDER: bool>(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.collect_optimal_candidates_initialized_neon::<USE_BT_MATCHFINDER>(
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
                    self.collect_optimal_candidates_initialized_avx2_bmi2::<USE_BT_MATCHFINDER>(
                        abs_pos,
                        current_abs_end,
                        profile,
                        query,
                        out,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.collect_optimal_candidates_initialized_sse42::<USE_BT_MATCHFINDER>(
                        abs_pos,
                        current_abs_end,
                        profile,
                        query,
                        out,
                    )
                },
                FastpathKernel::Scalar => self
                    .collect_optimal_candidates_initialized_scalar::<USE_BT_MATCHFINDER>(
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
            self.collect_optimal_candidates_initialized_scalar::<USE_BT_MATCHFINDER>(
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
    unsafe fn collect_optimal_candidates_initialized_neon<const USE_BT_MATCHFINDER: bool>(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
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
    unsafe fn collect_optimal_candidates_initialized_sse42<const USE_BT_MATCHFINDER: bool>(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
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
    unsafe fn collect_optimal_candidates_initialized_avx2_bmi2<const USE_BT_MATCHFINDER: bool>(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
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
    fn collect_optimal_candidates_initialized_scalar<const USE_BT_MATCHFINDER: bool>(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
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

    fn ldm_skip_raw_seq_store_bytes(&self, seq_store: &mut HcRawSeqStore, nb_bytes: usize) {
        let mut curr_pos = seq_store.pos_in_sequence.saturating_add(nb_bytes);
        while curr_pos > 0 && seq_store.pos < seq_store.size {
            let curr_seq = self.bt.ldm_sequences[seq_store.pos];
            let seq_len = curr_seq.lit_length.saturating_add(curr_seq.match_length);
            if curr_pos >= seq_len {
                curr_pos -= seq_len;
                seq_store.pos += 1;
            } else {
                seq_store.pos_in_sequence = curr_pos;
                break;
            }
        }
        if curr_pos == 0 || seq_store.pos == seq_store.size {
            seq_store.pos_in_sequence = 0;
        }
    }

    fn ldm_get_next_match_and_update_seq_store(
        &self,
        opt_ldm: &mut HcOptLdmState,
        curr_pos_in_block: usize,
        block_bytes_remaining: usize,
    ) {
        if opt_ldm.seq_store.size == 0 || opt_ldm.seq_store.pos >= opt_ldm.seq_store.size {
            opt_ldm.start_pos_in_block = usize::MAX;
            opt_ldm.end_pos_in_block = usize::MAX;
            return;
        }
        let curr_seq = self.bt.ldm_sequences[opt_ldm.seq_store.pos];
        let curr_block_end_pos = curr_pos_in_block.saturating_add(block_bytes_remaining);
        let literals_bytes_remaining = curr_seq
            .lit_length
            .saturating_sub(opt_ldm.seq_store.pos_in_sequence);
        let match_bytes_remaining = if literals_bytes_remaining == 0 {
            curr_seq.match_length.saturating_sub(
                opt_ldm
                    .seq_store
                    .pos_in_sequence
                    .saturating_sub(curr_seq.lit_length),
            )
        } else {
            curr_seq.match_length
        };
        if literals_bytes_remaining >= block_bytes_remaining {
            opt_ldm.start_pos_in_block = usize::MAX;
            opt_ldm.end_pos_in_block = usize::MAX;
            self.ldm_skip_raw_seq_store_bytes(&mut opt_ldm.seq_store, block_bytes_remaining);
            return;
        }
        opt_ldm.start_pos_in_block = curr_pos_in_block.saturating_add(literals_bytes_remaining);
        opt_ldm.end_pos_in_block = opt_ldm
            .start_pos_in_block
            .saturating_add(match_bytes_remaining);
        opt_ldm.offset = curr_seq.offset;
        if opt_ldm.end_pos_in_block > curr_block_end_pos {
            opt_ldm.end_pos_in_block = curr_block_end_pos;
            self.ldm_skip_raw_seq_store_bytes(
                &mut opt_ldm.seq_store,
                curr_block_end_pos.saturating_sub(curr_pos_in_block),
            );
        } else {
            self.ldm_skip_raw_seq_store_bytes(
                &mut opt_ldm.seq_store,
                literals_bytes_remaining.saturating_add(match_bytes_remaining),
            );
        }
    }

    fn ldm_maybe_add_match(
        &self,
        opt_ldm: &HcOptLdmState,
        curr_pos_in_block: usize,
        min_match: usize,
    ) -> Option<MatchCandidate> {
        let pos_diff = curr_pos_in_block.saturating_sub(opt_ldm.start_pos_in_block);
        let candidate_match_length = opt_ldm
            .end_pos_in_block
            .saturating_sub(opt_ldm.start_pos_in_block)
            .saturating_sub(pos_diff);
        if curr_pos_in_block < opt_ldm.start_pos_in_block
            || curr_pos_in_block >= opt_ldm.end_pos_in_block
            || candidate_match_length < min_match
        {
            return None;
        }
        Some(MatchCandidate {
            start: curr_pos_in_block,
            offset: opt_ldm.offset,
            match_len: candidate_match_length,
        })
    }

    fn ldm_process_match_candidate(
        &self,
        opt_ldm: &mut HcOptLdmState,
        curr_pos_in_block: usize,
        remaining_bytes: usize,
        min_match: usize,
    ) -> Option<MatchCandidate> {
        if opt_ldm.seq_store.size == 0 || opt_ldm.seq_store.pos >= opt_ldm.seq_store.size {
            return None;
        }
        if curr_pos_in_block >= opt_ldm.end_pos_in_block {
            if curr_pos_in_block > opt_ldm.end_pos_in_block {
                let pos_overshoot = curr_pos_in_block.saturating_sub(opt_ldm.end_pos_in_block);
                self.ldm_skip_raw_seq_store_bytes(&mut opt_ldm.seq_store, pos_overshoot);
            }
            self.ldm_get_next_match_and_update_seq_store(
                opt_ldm,
                curr_pos_in_block,
                remaining_bytes,
            );
        }
        self.ldm_maybe_add_match(opt_ldm, curr_pos_in_block, min_match)
    }

    fn prepare_ldm_candidates(&mut self, current_abs_start: usize, current_len: usize) {
        self.bt.ldm_sequences.clear();
        let _ = (current_abs_start, current_len);
        // Donor parity: btopt/btultra/btultra2 only merge LDM candidates when
        // a real ldmSeqStore exists (`enableLdm == ZSTD_ps_enable`).
        // This Rust encoder does not expose a donor-equivalent LDM producer or
        // runtime switch yet, so the ordinary level-22 path must remain LDM-free.
    }

    fn emit_optimal_plan(
        &mut self,
        current_len: usize,
        plan: &[HcOptimalSequence],
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        let current = self.table.window.back().unwrap().as_slice();
        if plan.is_empty() {
            handle_sequence(Sequence::Literals { literals: current });
            return;
        }

        let mut literals_start = 0usize;
        for item in plan {
            let lit_len = item.lit_len as usize;
            let match_len = item.match_len as usize;
            let start = literals_start.saturating_add(lit_len);
            if start < literals_start || start + match_len > current_len {
                continue;
            }
            let literals = &current[literals_start..start];
            handle_sequence(Sequence::Triple {
                literals,
                offset: item.offset as usize,
                match_len,
            });
            encode_offset_with_history(
                item.offset,
                literals.len() as u32,
                &mut self.table.offset_hist,
            );
            literals_start = start + match_len;
        }

        if literals_start < current_len {
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    fn uses_bt_matchfinder(&self) -> bool {
        matches!(
            self.parse_mode,
            HcParseMode::BtOpt | HcParseMode::BtUltra | HcParseMode::BtUltra2
        )
    }

    /// Cross-platform entry. Picks the kernel-specific variant so the BT walk
    /// body executes inside one `target_feature` umbrella and inlines the
    /// vectorized `count_match_from_indices` directly.
    #[inline(always)]
    fn bt_insert_step_no_rebase(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        // SAFETY: each branch verifies the target_feature requirement of the
        // callee — aarch64 NEON is baseline; x86 AVX2/BMI2 and SSE4.2 are
        // selected only when the runtime detector reports them present.
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.bt_insert_step_no_rebase_neon(abs_pos, current_abs_end, target_abs)
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.bt_insert_step_no_rebase_avx2_bmi2(abs_pos, current_abs_end, target_abs)
                },
                FastpathKernel::Sse42 => unsafe {
                    self.bt_insert_step_no_rebase_sse42(abs_pos, current_abs_end, target_abs)
                },
                FastpathKernel::Scalar => {
                    self.bt_insert_step_no_rebase_scalar(abs_pos, current_abs_end, target_abs)
                }
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.bt_insert_step_no_rebase_scalar(abs_pos, current_abs_end, target_abs)
        }
    }

    /// NEON-umbrella variant: body inlines `fastpath::neon::count_match_from_indices`.
    /// AArch64 only. Future x86 variants (`_sse42`, `_avx2_bmi2`) follow the
    /// same pattern with their respective umbrellas.
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    unsafe fn bt_insert_step_no_rebase_neon(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        bt_insert_step_no_rebase_body!(
            self,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::neon::count_match_from_indices
        )
    }

    /// SSE4.2-umbrella variant. Calls `fastpath::sse42::count_match_from_indices`.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    unsafe fn bt_insert_step_no_rebase_sse42(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        bt_insert_step_no_rebase_body!(
            self,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::sse42::count_match_from_indices
        )
    }

    /// AVX2+BMI2-umbrella variant. Calls
    /// `fastpath::avx2_bmi2::count_match_from_indices`.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    unsafe fn bt_insert_step_no_rebase_avx2_bmi2(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        bt_insert_step_no_rebase_body!(
            self,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::avx2_bmi2::count_match_from_indices
        )
    }

    /// Scalar fallback used on non-AArch64 targets (and when no SIMD kernel
    /// is selected). Routes through `fastpath::scalar::count_match_from_indices`
    /// directly so the call site remains the same shape as the SIMD variants.
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    fn bt_insert_step_no_rebase_scalar(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        bt_insert_step_no_rebase_body!(
            self,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::scalar::count_match_from_indices
        )
    }

    /// Cross-platform entry. Picks the kernel-specific variant so the BT-tree
    /// update loop runs inside the same `target_feature` umbrella as the per-
    /// position `bt_insert_step_no_rebase` it calls — eliminating one ABI
    /// barrier per fill iteration.
    #[inline(always)]
    fn bt_update_tree_until(&mut self, abs_pos: usize, current_abs_end: usize) {
        // SAFETY: each branch verifies the target_feature requirement of the
        // callee (see `bt_insert_step_no_rebase` dispatcher).
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.bt_update_tree_until_neon(abs_pos, current_abs_end)
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.bt_update_tree_until_avx2_bmi2(abs_pos, current_abs_end)
                },
                FastpathKernel::Sse42 => unsafe {
                    self.bt_update_tree_until_sse42(abs_pos, current_abs_end)
                },
                FastpathKernel::Scalar => {
                    self.bt_update_tree_until_scalar(abs_pos, current_abs_end)
                }
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.bt_update_tree_until_scalar(abs_pos, current_abs_end)
        }
    }

    /// NEON-umbrella variant: per-iteration `bt_insert_step_no_rebase_neon`
    /// inlines into the body because both share the `target_feature = "neon"`
    /// umbrella.
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    unsafe fn bt_update_tree_until_neon(&mut self, abs_pos: usize, current_abs_end: usize) {
        if self.table.skip_insert_until_abs < self.table.history_abs_start {
            self.table.skip_insert_until_abs = self.table.history_abs_start;
        }
        let mut update_abs = self.table.skip_insert_until_abs;
        while update_abs < abs_pos {
            let is_btultra2 = self.parse_mode == HcParseMode::BtUltra2;
            if !self
                .table
                .can_skip_rebase_check_at(update_abs, abs_pos, is_btultra2)
            {
                self.maybe_rebase_positions(update_abs);
            }
            // SAFETY: same NEON umbrella; direct call inlines the BT-walk body.
            let forward =
                unsafe { self.bt_insert_step_no_rebase_neon(update_abs, current_abs_end, abs_pos) };
            update_abs = update_abs.saturating_add(forward.max(1));
        }
        self.table.skip_insert_until_abs = abs_pos;
    }

    /// SSE4.2 umbrella variant.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    unsafe fn bt_update_tree_until_sse42(&mut self, abs_pos: usize, current_abs_end: usize) {
        if self.table.skip_insert_until_abs < self.table.history_abs_start {
            self.table.skip_insert_until_abs = self.table.history_abs_start;
        }
        let mut update_abs = self.table.skip_insert_until_abs;
        while update_abs < abs_pos {
            let is_btultra2 = self.parse_mode == HcParseMode::BtUltra2;
            if !self
                .table
                .can_skip_rebase_check_at(update_abs, abs_pos, is_btultra2)
            {
                self.maybe_rebase_positions(update_abs);
            }
            let forward = unsafe {
                self.bt_insert_step_no_rebase_sse42(update_abs, current_abs_end, abs_pos)
            };
            update_abs = update_abs.saturating_add(forward.max(1));
        }
        self.table.skip_insert_until_abs = abs_pos;
    }

    /// AVX2+BMI2 umbrella variant.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    unsafe fn bt_update_tree_until_avx2_bmi2(&mut self, abs_pos: usize, current_abs_end: usize) {
        if self.table.skip_insert_until_abs < self.table.history_abs_start {
            self.table.skip_insert_until_abs = self.table.history_abs_start;
        }
        let mut update_abs = self.table.skip_insert_until_abs;
        while update_abs < abs_pos {
            let is_btultra2 = self.parse_mode == HcParseMode::BtUltra2;
            if !self
                .table
                .can_skip_rebase_check_at(update_abs, abs_pos, is_btultra2)
            {
                self.maybe_rebase_positions(update_abs);
            }
            let forward = unsafe {
                self.bt_insert_step_no_rebase_avx2_bmi2(update_abs, current_abs_end, abs_pos)
            };
            update_abs = update_abs.saturating_add(forward.max(1));
        }
        self.table.skip_insert_until_abs = abs_pos;
    }

    /// Scalar fallback used on non-AArch64 targets.
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    fn bt_update_tree_until_scalar(&mut self, abs_pos: usize, current_abs_end: usize) {
        if self.table.skip_insert_until_abs < self.table.history_abs_start {
            self.table.skip_insert_until_abs = self.table.history_abs_start;
        }
        let mut update_abs = self.table.skip_insert_until_abs;
        while update_abs < abs_pos {
            let is_btultra2 = self.parse_mode == HcParseMode::BtUltra2;
            if !self
                .table
                .can_skip_rebase_check_at(update_abs, abs_pos, is_btultra2)
            {
                self.maybe_rebase_positions(update_abs);
            }
            let forward =
                self.bt_insert_step_no_rebase_scalar(update_abs, current_abs_end, abs_pos);
            update_abs = update_abs.saturating_add(forward.max(1));
        }
        self.table.skip_insert_until_abs = abs_pos;
    }

    /// Cross-platform entry. Picks the kernel-specific variant so the BT walk
    /// body executes inside one `target_feature` umbrella and inlines the
    /// vectorized `count_match_from_indices` directly. See
    /// `bt_insert_step_no_rebase` for the same dispatcher pattern.
    ///
    /// The on-encode hot path bypasses this dispatcher: when invoked from
    /// `collect_optimal_candidates_initialized_<kernel>` the per-kernel
    /// variant is called directly so the BT match collection inlines under
    /// the surrounding umbrella. This entry is kept for external / future
    /// callers that aren't yet under an umbrella.
    #[allow(dead_code)]
    #[inline(always)]
    fn bt_insert_and_collect_matches(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        // SAFETY: each branch verifies the target_feature requirement of the
        // callee (see `bt_insert_step_no_rebase` dispatcher).
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.bt_insert_and_collect_matches_neon(
                abs_pos,
                current_abs_end,
                profile,
                min_match_len,
                best_len_for_skip,
                out,
            )
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.bt_insert_and_collect_matches_avx2_bmi2(
                        abs_pos,
                        current_abs_end,
                        profile,
                        min_match_len,
                        best_len_for_skip,
                        out,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.bt_insert_and_collect_matches_sse42(
                        abs_pos,
                        current_abs_end,
                        profile,
                        min_match_len,
                        best_len_for_skip,
                        out,
                    )
                },
                FastpathKernel::Scalar => self.bt_insert_and_collect_matches_scalar(
                    abs_pos,
                    current_abs_end,
                    profile,
                    min_match_len,
                    best_len_for_skip,
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
            self.bt_insert_and_collect_matches_scalar(
                abs_pos,
                current_abs_end,
                profile,
                min_match_len,
                best_len_for_skip,
                out,
            )
        }
    }

    /// NEON-umbrella variant of `bt_insert_and_collect_matches`. Inlines
    /// `fastpath::neon::count_match_from_indices` via the shared body macro.
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    unsafe fn bt_insert_and_collect_matches_neon(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        bt_insert_and_collect_matches_body!(
            self,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::neon::count_match_from_indices,
        )
    }

    /// SSE4.2 umbrella variant of `bt_insert_and_collect_matches`.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    unsafe fn bt_insert_and_collect_matches_sse42(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        bt_insert_and_collect_matches_body!(
            self,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::sse42::count_match_from_indices,
        )
    }

    /// AVX2+BMI2 umbrella variant of `bt_insert_and_collect_matches`.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    unsafe fn bt_insert_and_collect_matches_avx2_bmi2(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        bt_insert_and_collect_matches_body!(
            self,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::avx2_bmi2::count_match_from_indices,
        )
    }

    /// Scalar fallback used on non-AArch64 targets.
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    fn bt_insert_and_collect_matches_scalar(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        bt_insert_and_collect_matches_body!(
            self,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::scalar::count_match_from_indices,
        )
    }

    fn update_hash3_until(&mut self, abs_pos: usize) {
        let is_btultra2 = self.parse_mode == HcParseMode::BtUltra2;
        if self.table.next_to_update3 < self.table.history_abs_start {
            self.table.next_to_update3 = self.table.history_abs_start;
        }
        if self.table.next_to_update3 >= abs_pos {
            return;
        }
        while self.table.next_to_update3 < abs_pos {
            if !self.table.can_skip_rebase_check_at(
                self.table.next_to_update3,
                abs_pos,
                is_btultra2,
            ) {
                self.maybe_rebase_positions(self.table.next_to_update3);
            }
            self.table
                .insert_hash3_only_no_rebase(self.table.next_to_update3);
            self.table.next_to_update3 = self.table.next_to_update3.saturating_add(1);
        }
    }

    /// Hot wrapper: every `insert_position*` call passes through here.
    /// The fast path is "no rebase needed" — a single
    /// [`MatchTable::needs_rebase`] check including the `BtUltra2`
    /// first-position skip. The actual table rebuild fires once per
    /// ~`u32::MAX` positions on rolling streams and never for typical
    /// single-shot inputs, so it lives in a separate `#[cold]`
    /// function on this struct (it still needs to drive the BT
    /// matchfinder, which can't move onto `MatchTable`). Inlining the
    /// hot wrapper lets the compiler fold the early return into the
    /// caller and keep the cold path off the i-cache.
    #[inline]
    fn maybe_rebase_positions(&mut self, abs_pos: usize) {
        let is_btultra2 = self.parse_mode == HcParseMode::BtUltra2;
        if self.table.needs_rebase(abs_pos, is_btultra2) {
            self.rebase_positions_cold(abs_pos);
        }
    }

    #[cold]
    #[inline(never)]
    fn rebase_positions_cold(&mut self, abs_pos: usize) {
        // Keep all live history addressable after rebase.
        self.table.position_base = self.table.history_abs_start;
        self.table.index_shift = 0;
        self.table.allow_zero_relative_position = true;
        self.table.hash_table.fill(HC_EMPTY);
        self.table.hash3_table.fill(HC_EMPTY);
        self.table.chain_table.fill(HC_EMPTY);

        let history_start = self.table.history_abs_start;
        // Rebuild only the already-inserted prefix. The caller inserts abs_pos
        // immediately after this, and later positions are added in-order.
        if self.uses_bt_matchfinder() {
            let rebuild_end = self.table.history_abs_end();
            let mut pos = history_start;
            while pos < abs_pos {
                let forward = self.bt_insert_step_no_rebase(pos, rebuild_end, abs_pos);
                pos = pos.saturating_add(forward.max(1));
            }
        } else {
            for pos in history_start..abs_pos {
                self.table.insert_position_no_rebase(pos);
            }
        }
        self.table.next_to_update3 = self.table.next_to_update3.max(abs_pos);
    }

    #[inline]
    fn insert_position(&mut self, abs_pos: usize) {
        self.maybe_rebase_positions(abs_pos);
        self.table.insert_position_no_rebase(abs_pos);
    }

    fn insert_positions(&mut self, start: usize, end: usize) {
        for pos in start..end {
            self.insert_position(pos);
        }
        self.table.next_to_update3 = self.table.next_to_update3.max(end);
    }

    fn insert_positions_with_step(&mut self, start: usize, end: usize, step: usize) {
        if step == 0 {
            return;
        }
        let mut pos = start;
        while pos < end {
            self.insert_position(pos);
            let next = pos.saturating_add(step);
            if next <= pos {
                break;
            }
            pos = next;
        }
    }
}

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

#[test]
fn driver_switches_backends_and_initializes_dfast_via_reset() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Default);
    assert_eq!(driver.active_backend, MatcherBackend::Dfast);
    assert_eq!(driver.window_size(), (1u64 << 22));

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
    assert_eq!(driver.window_size(), (1u64 << 17));
}

#[test]
fn driver_level4_selects_row_backend() {
    let mut driver = MatchGeneratorDriver::new(32, 2);
    driver.reset(CompressionLevel::Level(4));
    assert_eq!(driver.active_backend, MatcherBackend::Row);
}

#[test]
fn level_16_17_use_btopt_parse_mode() {
    let p16 = resolve_level_params(CompressionLevel::Level(16), None);
    let p17 = resolve_level_params(CompressionLevel::Level(17), None);
    assert_eq!(p16.backend, MatcherBackend::HashChain);
    assert_eq!(p17.backend, MatcherBackend::HashChain);
    assert_eq!(p16.hc.parse_mode, HcParseMode::BtOpt);
    assert_eq!(p17.hc.parse_mode, HcParseMode::BtOpt);
}

#[test]
fn level_18_19_use_btultra_parse_mode() {
    let p18 = resolve_level_params(CompressionLevel::Level(18), None);
    let p19 = resolve_level_params(CompressionLevel::Level(19), None);
    assert_eq!(p18.backend, MatcherBackend::HashChain);
    assert_eq!(p19.backend, MatcherBackend::HashChain);
    assert_eq!(p18.hc.parse_mode, HcParseMode::BtUltra);
    assert_eq!(p19.hc.parse_mode, HcParseMode::BtUltra);
}

#[test]
fn level_20_22_use_btultra2_parse_mode() {
    for level in 20..=22 {
        let params = resolve_level_params(CompressionLevel::Level(level), None);
        assert_eq!(params.backend, MatcherBackend::HashChain);
        assert_eq!(params.hc.parse_mode, HcParseMode::BtUltra2);
    }
}

#[test]
fn level22_uses_donor_target_length_and_large_input_tables() {
    let params = resolve_level_params(CompressionLevel::Level(22), None);
    assert_eq!(params.window_log, 27);
    assert_eq!(params.hc.hash_log, 25);
    assert_eq!(params.hc.chain_log, 27);
    assert_eq!(params.hc.search_depth, 1 << 9);
    assert_eq!(params.hc.target_len, 999);
}

#[test]
fn level22_source_size_hint_uses_donor_btultra2_tiers() {
    let p16k = resolve_level_params(CompressionLevel::Level(22), Some(16 * 1024));
    assert_eq!(p16k.window_log, 14);
    assert_eq!(p16k.hc.hash_log, 15);
    assert_eq!(p16k.hc.chain_log, 15);
    assert_eq!(p16k.hc.search_depth, 1 << 10);
    assert_eq!(p16k.hc.target_len, 999);

    let p128k = resolve_level_params(CompressionLevel::Level(22), Some(128 * 1024));
    assert_eq!(p128k.window_log, 17);
    assert_eq!(p128k.hc.hash_log, 17);
    assert_eq!(p128k.hc.chain_log, 18);
    assert_eq!(p128k.hc.search_depth, 1 << 11);
    assert_eq!(p128k.hc.target_len, 999);

    let p256k = resolve_level_params(CompressionLevel::Level(22), Some(256 * 1024));
    assert_eq!(p256k.window_log, 18);
    assert_eq!(p256k.hc.hash_log, 19);
    assert_eq!(p256k.hc.chain_log, 19);
    assert_eq!(p256k.hc.search_depth, 1 << 13);
    assert_eq!(p256k.hc.target_len, 999);
}

#[test]
fn level22_small_source_size_hint_matches_donor_cparams() {
    use zstd::zstd_safe::zstd_sys;

    let source_size = 15_027u64;
    let donor = unsafe { zstd_sys::ZSTD_getCParams(22, source_size, 0) };
    let params = resolve_level_params(CompressionLevel::Level(22), Some(source_size));

    assert_eq!(params.window_log as u32, donor.windowLog);
    assert_eq!(params.hc.chain_log as u32, donor.chainLog);
    assert_eq!(params.hc.hash_log as u32, donor.hashLog);
    assert_eq!(params.hc.search_depth as u32, 1u32 << donor.searchLog);
    assert_eq!(HC_OPT_MIN_MATCH_LEN as u32, donor.minMatch);
    assert_eq!(params.hc.target_len as u32, donor.targetLength);
}

#[test]
fn level22_small_source_uses_window_bounded_hash3_log() {
    let mut hc = HcMatchGenerator::new(1 << 14);
    hc.configure(BTULTRA2_HC_CONFIG_L22_16K, 14);
    assert_eq!(hc.table.hash3_log, 14);

    hc.configure(BTULTRA2_HC_CONFIG_L22, 27);
    assert_eq!(hc.table.hash3_log, HC3_HASH_LOG);
}

#[test]
fn btultra2_seed_pass_initializes_opt_state() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);
    let data: Vec<u8> = (0..32 * 1024).map(|i| (i % 251) as u8).collect();
    hc.table.add_data(data, |_| {});
    hc.start_matching(|_| {});
    assert!(
        hc.bt.opt_state.lit_length_sum > 0,
        "btultra2 first block should seed non-zero sequence statistics"
    );
    assert!(
        hc.bt.opt_state.off_code_sum > 0,
        "btultra2 first block should seed offset-code statistics"
    );
}

#[test]
fn btultra2_profile_disables_small_offset_handicap() {
    let p1 = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, false);
    let p2 = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, true);
    assert!(
        !p1.favor_small_offsets,
        "btultra2 primary profile should match donor opt2 offset pricing"
    );
    assert!(
        !p2.favor_small_offsets,
        "btultra2 secondary profile should match donor opt2 offset pricing"
    );
}

#[test]
fn btultra2_profile_is_single_pass_opt2() {
    let p1 = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, false);
    let p2 = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, true);
    assert_eq!(p1.max_chain_depth, p2.max_chain_depth);
    assert_eq!(p1.sufficient_match_len, p2.sufficient_match_len);
    assert_eq!(p1.accurate, p2.accurate);
    assert_eq!(p1.favor_small_offsets, p2.favor_small_offsets);
    assert!(
        p1.accurate,
        "btultra2 should use donor opt2 accurate pricing in the main pass"
    );
}

#[test]
fn btultra_profile_keeps_donor_search_depth_budget() {
    let p = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra, false);
    assert_eq!(
        p.max_chain_depth, 32,
        "btultra should not cap chain depth below donor opt2 search budget"
    );
}

#[test]
fn btopt_profile_keeps_donor_search_depth_budget() {
    let p = HcOptimalCostProfile::for_mode(HcParseMode::BtOpt, false);
    assert_eq!(
        p.max_chain_depth, 32,
        "btopt should not cap chain depth below donor btopt search budget"
    );
}

#[test]
fn sufficient_match_len_is_clamped_by_target_len() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);
    hc.hc.target_len = 13;
    let profile = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, true);
    assert_eq!(hc.sufficient_match_len_for_pass(profile), 13);
}

#[test]
fn opt_modes_use_target_len_as_sufficient_len() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.hc.target_len = 57;
    for (mode, pass2) in [
        (HcParseMode::BtOpt, false),
        (HcParseMode::BtUltra, false),
        (HcParseMode::BtUltra2, false),
        (HcParseMode::BtUltra2, true),
    ] {
        let profile = HcOptimalCostProfile::for_mode(mode, pass2);
        assert_eq!(hc.sufficient_match_len_for_pass(profile), 57);
    }
}

#[test]
fn sufficient_match_len_is_capped_by_opt_num() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.hc.target_len = usize::MAX / 2;
    let profile = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, true);
    assert_eq!(hc.sufficient_match_len_for_pass(profile), HC_OPT_NUM - 1);
}

#[test]
fn dictionary_entropy_seed_initializes_opt_state_from_tables() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);

    let huff = crate::huff0::huff0_encoder::HuffmanTable::build_from_data(
        b"aaabbbbccccddddeeeeefffffgggg",
    );
    let ll = crate::fse::fse_encoder::default_ll_table();
    let ml = crate::fse::fse_encoder::default_ml_table();
    let of = crate::fse::fse_encoder::default_of_table();
    hc.seed_dictionary_entropy(Some(&huff), Some(&ll), Some(&ml), Some(&of));

    hc.bt.opt_state.rescale_freqs(
        b"abcd",
        HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, false),
    );

    let base_ll_freqs: [u32; HC_MAX_LL + 1] = [
        4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1,
    ];

    assert_ne!(
        hc.bt.opt_state.lit_length_freq, base_ll_freqs,
        "dictionary entropy should override fallback LL bootstrap frequencies"
    );
    assert!(
        hc.bt.opt_state.match_length_freq.iter().any(|&v| v != 1),
        "dictionary entropy should seed non-uniform ML frequencies"
    );
    assert_ne!(
        hc.bt.opt_state.off_code_freq[0], 6,
        "dictionary entropy should override fallback OF bootstrap frequencies"
    );
}

#[test]
fn dictionary_fse_seed_applies_without_huffman_seed() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);

    let ll = crate::fse::fse_encoder::default_ll_table();
    let ml = crate::fse::fse_encoder::default_ml_table();
    let of = crate::fse::fse_encoder::default_of_table();
    hc.seed_dictionary_entropy(None, Some(&ll), Some(&ml), Some(&of));
    hc.bt.opt_state.rescale_freqs(
        b"abcd",
        HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, false),
    );

    let base_ll_freqs: [u32; HC_MAX_LL + 1] = [
        4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1,
    ];
    assert_ne!(
        hc.bt.opt_state.lit_length_freq, base_ll_freqs,
        "FSE seed should still override LL bootstrap frequencies without huffman seed"
    );
    assert!(
        hc.bt.opt_state.match_length_freq.iter().any(|&v| v != 1),
        "FSE seed should still seed non-uniform ML frequencies"
    );
    assert_ne!(
        hc.bt.opt_state.off_code_freq[0], 6,
        "FSE seed should still override OF bootstrap frequencies without huffman seed"
    );
}

#[test]
fn dictionary_seed_overrides_predef_price_mode_on_tiny_input() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);

    let ll = crate::fse::fse_encoder::default_ll_table();
    let ml = crate::fse::fse_encoder::default_ml_table();
    let of = crate::fse::fse_encoder::default_of_table();
    hc.seed_dictionary_entropy(None, Some(&ll), Some(&ml), Some(&of));
    hc.bt.opt_state.rescale_freqs(
        b"abc",
        HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, false),
    );
    assert!(
        matches!(hc.bt.opt_state.price_type, HcOptPriceType::Dynamic),
        "dictionary-seeded first block should stay in dynamic mode even for tiny src"
    );
}

#[test]
fn lit_length_price_blocksize_max_costs_one_extra_bit() {
    let profile_predef = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, false);
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

    let profile_dyn = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, true);
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
fn btultra2_seed_pass_disabled_when_dictionary_entropy_seed_present() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);
    let ll = crate::fse::fse_encoder::default_ll_table();
    let ml = crate::fse::fse_encoder::default_ml_table();
    let of = crate::fse::fse_encoder::default_of_table();
    hc.seed_dictionary_entropy(None, Some(&ll), Some(&ml), Some(&of));
    assert!(
        !hc.should_run_btultra2_seed_pass(HC_PREDEF_THRESHOLD + 1),
        "dictionary-seeded first block should skip btultra2 warmup pass"
    );
}

#[test]
fn btultra2_seed_pass_disabled_when_prefix_history_exists() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);
    hc.table.history_abs_start = 17;
    hc.table.window.push_back(b"abcdefghijklmnop".to_vec());
    assert!(
        !hc.should_run_btultra2_seed_pass(HC_PREDEF_THRESHOLD + 9),
        "btultra2 warmup must be first-block only (no prefix history)"
    );
}

#[test]
fn btultra2_seed_pass_disabled_for_tiny_block() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);
    assert!(
        !hc.should_run_btultra2_seed_pass(HC_PREDEF_THRESHOLD),
        "btultra2 warmup should not run at or below predefined threshold"
    );
}

#[test]
fn btultra2_seed_pass_disabled_after_stats_initialized() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);
    hc.bt.opt_state.lit_length_sum = 1;
    assert!(
        !hc.should_run_btultra2_seed_pass(HC_PREDEF_THRESHOLD + 32),
        "btultra2 warmup should run only for first block before stats are initialized"
    );
}

#[test]
fn btultra2_seed_pass_disabled_when_not_at_frame_start() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);
    // Simulate non-first block state: current block has no prefix in deque,
    // but total produced window already includes prior output.
    hc.table.window_size = HC_PREDEF_THRESHOLD + 64;
    hc.table
        .window
        .push_back(alloc::vec![b'A'; HC_PREDEF_THRESHOLD + 32]);
    assert!(
        !hc.should_run_btultra2_seed_pass(HC_PREDEF_THRESHOLD + 32),
        "btultra2 warmup must not run after frame start"
    );
}

#[test]
fn btultra2_seed_pass_disabled_when_ldm_sequences_exist() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);
    hc.table.window_size = HC_PREDEF_THRESHOLD + 64;
    hc.table
        .window
        .push_back(alloc::vec![b'A'; HC_PREDEF_THRESHOLD + 64]);
    hc.bt.ldm_sequences.push(HcRawSeq {
        lit_length: 8,
        offset: 16,
        match_length: 32,
    });
    assert!(
        !hc.should_run_btultra2_seed_pass(HC_PREDEF_THRESHOLD + 32),
        "btultra2 warmup must not run when LDM already produced sequences"
    );
}

#[test]
fn literal_price_uses_eight_bits_when_literals_uncompressed() {
    let profile = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, false);
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
fn dictionary_huffman_seed_ignored_when_literals_uncompressed() {
    let mut stats = HcOptState::new();
    stats.set_literals_compressed_for_tests(false);
    let huff = crate::huff0::huff0_encoder::HuffmanTable::build_from_data(
        b"aaaaabbbbcccddeeff00112233445566778899",
    );
    let ll = crate::fse::fse_encoder::default_ll_table();
    let ml = crate::fse::fse_encoder::default_ml_table();
    let of = crate::fse::fse_encoder::default_of_table();
    stats.seed_dictionary_entropy(Some(&huff), Some(&ll), Some(&ml), Some(&of));
    stats.rescale_freqs(
        b"abcd",
        HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, false),
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
    hc.insert_positions(0, abs_pos);

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
    hc.insert_positions(0, abs_pos);
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
    hc.insert_positions(0, abs_pos);
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

#[test]
fn hc_collect_optimal_candidates_uses_hash3_when_chain_depth_zero() {
    let mut hc = HcMatchGenerator::new(256);
    hc.table.history = b"abcde1234abcdeZZZZ".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;
    hc.table.position_base = 0;
    hc.hc.search_depth = 0;
    let abs_pos = 9usize; // second "abcde"
    hc.table.ensure_tables();
    hc.insert_positions(0, abs_pos);
    // Donor hash3 has an independent nextToUpdate3 cursor; main-table
    // insertion does not imply the HC3 side table has been filled.
    hc.table.next_to_update3 = 0;

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
            reps: [1, 2, 3],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );

    assert!(
        out.iter()
            .any(|candidate| candidate.offset == 9 && candidate.match_len >= HC_MIN_MATCH_LEN),
        "hash3 candidate should supply at least one valid match when chain search is disabled"
    );
}

#[test]
fn hc_collect_optimal_candidates_hash3_updates_skipped_prefix_positions() {
    let mut hc = HcMatchGenerator::new(256);
    hc.table.history = b"abcdeZabcdeYY".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;
    hc.table.position_base = 0;
    hc.hc.search_depth = 0;
    hc.table.ensure_tables();
    // Simulate donor-like nextToUpdate3 path: no explicit hash/chain insertions
    // were done for prefix positions, so hash3 must fill them on demand.
    hc.table.next_to_update3 = 0;

    let abs_pos = 6usize; // second "abcde"
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
            reps: [1, 2, 3],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );

    assert!(
        out.iter()
            .any(|candidate| candidate.offset == 6 && candidate.match_len >= HC_MIN_MATCH_LEN),
        "hash3 incremental update should surface prefix match even without explicit insert_positions"
    );
}

#[test]
fn hc_hash3_tail_match_advances_update_cursor_on_early_return() {
    let mut hc = HcMatchGenerator::new(256);
    hc.table.history = b"abcdeZabcde".to_vec();
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;
    hc.table.position_base = 0;
    hc.hc.search_depth = 0;
    hc.table.ensure_tables();
    hc.table.next_to_update3 = 0;

    let abs_pos = 6usize; // second "abcde", match reaches current end
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
            reps: [1, 2, 3],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );

    assert!(
        hc.table.next_to_update3 >= abs_pos,
        "tail-reaching hash3 lookup should fill the update cursor to current position"
    );
    assert!(
        hc.table.skip_insert_until_abs > abs_pos,
        "tail-reaching hash3 early return should request skipping current-position hash insertion"
    );
}

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
    let mut hc = HcMatchGenerator::new(256);
    hc.table.history = alloc::vec![0u8; 160];
    for i in 0..64 {
        hc.table.history[i] = b'a' + (i % 7) as u8;
    }
    for i in 64..160 {
        hc.table.history[i] = b'k' + (i % 5) as u8;
    }
    let abs_pos = 96usize;
    for i in 0..24 {
        hc.table.history[abs_pos + i] = hc.table.history[16 + i];
    }
    hc.table.history_start = 0;
    hc.table.history_abs_start = 0;
    hc.table.position_base = 0;
    hc.hc.search_depth = 32;
    hc.table.ensure_tables();
    hc.insert_positions(0, abs_pos);

    let profile = HcOptimalCostProfile {
        max_chain_depth: 32,
        sufficient_match_len: usize::MAX / 2,
        accurate: true,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();

    hc.parse_mode = HcParseMode::BtUltra2;
    hc.table.dictionary_limit_abs = Some(64);
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

    hc.parse_mode = HcParseMode::BtUltra;
    hc.table.skip_insert_until_abs = 0;
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

    driver.reset(CompressionLevel::Level(2));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);
    let full_tables = driver.dfast_matcher().short_hash.len();
    assert_eq!(full_tables, 1 << DFAST_HASH_BITS);

    driver.set_source_size_hint(1024);
    driver.reset(CompressionLevel::Level(2));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"xyzxyzxyzxyz");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);
    let hinted_tables = driver.dfast_matcher().short_hash.len();

    assert_eq!(driver.window_size(), 1 << MIN_HINTED_WINDOW_LOG);
    assert_eq!(hinted_tables, 1 << MIN_HINTED_WINDOW_LOG);
    assert!(
        hinted_tables < full_tables,
        "tiny source hint should reduce dfast table footprint"
    );
}

#[test]
fn driver_small_source_hint_shrinks_row_hash_tables() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Level(4));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);
    let full_rows = driver.row_matcher().row_heads.len();
    assert_eq!(full_rows, 1 << (ROW_HASH_BITS - ROW_LOG));

    driver.set_source_size_hint(1024);
    driver.reset(CompressionLevel::Level(4));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"xyzxyzxyzxyz");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);
    let hinted_rows = driver.row_matcher().row_heads.len();

    assert_eq!(driver.window_size(), 1 << MIN_HINTED_WINDOW_LOG);
    assert_eq!(
        hinted_rows,
        1 << ((MIN_HINTED_WINDOW_LOG as usize) - ROW_LOG)
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

#[test]
fn driver_unhinted_level2_keeps_default_dfast_hash_table_size() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Level(2));
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);

    let table_len = driver.dfast_matcher().short_hash.len();
    assert_eq!(
        table_len,
        1 << DFAST_HASH_BITS,
        "unhinted Level(2) should keep default dfast table size"
    );
}

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
        .match_generator
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

    // Initialize at Best — allocates large HC tables (2M hash, 1M chain).
    driver.reset(CompressionLevel::Best);
    assert_eq!(driver.window_size(), (1u64 << 24));

    // Feed data so tables are actually allocated via ensure_tables().
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);

    // Switch to Fastest — must release HC tables.
    driver.reset(CompressionLevel::Fastest);
    assert_eq!(driver.window_size(), (1u64 << 17));

    // HC matcher should have empty tables after backend switch.
    let hc = driver.hc_match_generator.as_ref().unwrap();
    assert!(
        hc.table.hash_table.is_empty(),
        "HC hash_table should be released after switching away from Best"
    );
    assert!(
        hc.table.chain_table.is_empty(),
        "HC chain_table should be released after switching away from Best"
    );
}

#[test]
fn driver_better_to_best_resizes_hc_tables() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    // Initialize at Better — allocates small HC tables (1M hash, 512K chain).
    driver.reset(CompressionLevel::Better);
    assert_eq!(driver.window_size(), (1u64 << 23));

    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"abcabcabcabc");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);

    let hc = driver.hc_match_generator.as_ref().unwrap();
    let better_hash_len = hc.table.hash_table.len();
    let better_chain_len = hc.table.chain_table.len();

    // Switch to Best — must resize to larger tables.
    driver.reset(CompressionLevel::Best);
    assert_eq!(driver.window_size(), (1u64 << 24));

    // Feed data to trigger ensure_tables with new sizes.
    let mut space = driver.get_next_space();
    space[..12].copy_from_slice(b"xyzxyzxyzxyz");
    space.truncate(12);
    driver.commit_space(space);
    driver.skip_matching_with_hint(None);

    let hc = driver.hc_match_generator.as_ref().unwrap();
    assert!(
        hc.table.hash_table.len() > better_hash_len,
        "Best hash_table ({}) should be larger than Better ({})",
        hc.table.hash_table.len(),
        better_hash_len
    );
    assert!(
        hc.table.chain_table.len() > better_chain_len,
        "Best chain_table ({}) should be larger than Better ({})",
        hc.table.chain_table.len(),
        better_chain_len
    );
}

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

    assert_eq!(driver.match_generator.offset_hist, [11, 7, 3]);
}

#[test]
fn hc_prime_with_empty_dictionary_disables_btultra2_seed_pass() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Better);

    driver.prime_with_dictionary(&[], [11, 7, 3]);

    assert_eq!(driver.hc_matcher().table.offset_hist, [11, 7, 3]);
    assert!(
        !driver
            .hc_matcher()
            .should_run_btultra2_seed_pass(HC_PREDEF_THRESHOLD + 1),
        "btultra2 warmup must stay disabled after dictionary priming, even when dict content is empty"
    );
}

#[test]
fn hc_prime_with_dictionary_disables_btultra2_seed_pass() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Better);

    driver.prime_with_dictionary(b"abcdefgh", [1, 4, 8]);

    assert!(
        !driver
            .hc_matcher()
            .should_run_btultra2_seed_pass(HC_PREDEF_THRESHOLD + 1),
        "btultra2 warmup must stay disabled after dictionary priming with content"
    );
}

#[test]
fn dfast_prime_with_dictionary_preserves_history_for_first_full_block() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(2));

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
fn prime_with_dictionary_does_not_reuse_tiny_suffix_store() {
    let mut driver = MatchGeneratorDriver::new(8, 2);
    driver.reset(CompressionLevel::Fastest);

    // This dictionary leaves a 1-byte tail chunk (capacity=1 suffix table),
    // which should never be committed to the matcher window.
    driver.prime_with_dictionary(b"abcdefghi", [1, 4, 8]);

    assert!(
        driver
            .match_generator
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

    let before = driver.match_generator.max_window_size;
    // One full slice plus a 1-byte tail that cannot be committed.
    driver.prime_with_dictionary(b"abcdefghi", [1, 4, 8]);

    assert_eq!(
        driver.match_generator.max_window_size,
        before + 8,
        "retention budget must account only for dictionary bytes actually committed to history"
    );
}

#[test]
fn dfast_prime_with_dictionary_counts_four_byte_tail_budget() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(2));

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
    driver.reset(CompressionLevel::Level(4));

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
    driver.reset(CompressionLevel::Level(4));

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
    driver.reset(CompressionLevel::Level(4));
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

#[test]
fn row_get_last_space_and_reset_to_fastest_clears_window() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(4));

    let mut space = driver.get_next_space();
    space.clear();
    space.extend_from_slice(b"row-data");
    driver.commit_space(space);

    assert_eq!(driver.get_last_space(), b"row-data");

    driver.reset(CompressionLevel::Fastest);
    assert_eq!(driver.active_backend, MatcherBackend::Simple);
    assert!(driver.row_matcher().window.is_empty());
}

/// Ensures switching from Row to Simple returns pooled buffers and row tables.
#[test]
fn driver_reset_from_row_backend_reclaims_row_buffer_pool() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(4));
    assert_eq!(driver.active_backend, MatcherBackend::Row);

    // Ensure the row matcher option is initialized so reset() executes
    // the Row backend retirement path.
    let _ = driver.row_matcher();
    let mut space = driver.get_next_space();
    space.extend_from_slice(b"row-data-to-recycle");
    driver.commit_space(space);

    let before_pool = driver.vec_pool.len();
    driver.reset(CompressionLevel::Fastest);

    assert_eq!(driver.active_backend, MatcherBackend::Simple);
    let row = driver
        .row_match_generator
        .as_ref()
        .expect("row matcher should remain allocated after switch");
    assert!(row.row_heads.is_empty());
    assert!(row.row_positions.is_empty());
    assert!(row.row_tags.is_empty());
    assert!(
        driver.vec_pool.len() >= before_pool,
        "row reset should recycle row history buffers"
    );
}

/// Guards the optional row backend retirement path when no row matcher was allocated.
#[test]
fn driver_reset_from_row_backend_tolerates_missing_row_matcher() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.active_backend = MatcherBackend::Row;
    driver.row_match_generator = None;

    driver.reset(CompressionLevel::Fastest);

    assert_eq!(driver.active_backend, MatcherBackend::Simple);
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
    assert_eq!(best.match_len, ROW_MIN_MATCH_LEN);

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
    assert_eq!(best.match_len, ROW_MIN_MATCH_LEN);

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
    hc.configure(HC_CONFIG, 22);
    let bytes = [b'a', b'b', b'c', b'd'];
    let read32 = u32::from_le_bytes(bytes);
    let expected = ((read32.wrapping_mul(HC_PRIME4BYTES)) >> (32 - hc.table.hash_log)) as usize;
    assert_eq!(hc.table.hash_position(&bytes), expected);
}

#[test]
fn btultra2_main_hash_uses_donor_hash4_formula() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG_L22, 27);
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
    matcher.history = b"abcde".to_vec();
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
fn hc_reset_refills_existing_tables_with_empty_sentinel() {
    let mut hc = HcMatchGenerator::new(32);
    hc.table.add_data(b"abcdeabcde".to_vec(), |_| {});
    hc.table.ensure_tables();
    assert!(!hc.table.hash_table.is_empty());
    assert!(!hc.table.chain_table.is_empty());
    hc.table.hash_table.fill(123);
    hc.table.chain_table.fill(456);

    hc.reset(|_| {});

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
        let block_len = crate::encoding::frame_compressor::donor_optimal_block_size(
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
    matcher.configure(BTULTRA2_HC_CONFIG_L22, 20);
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

    hc.insert_positions(0, 9);

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

    hc.insert_positions_with_step(0, 16, 4);

    assert_eq!(
        hc.table.next_to_update3, 0,
        "sparse insert_positions_with_step must not mark skipped positions as hash3-updated"
    );
}

#[test]
fn prime_with_dictionary_budget_shrinks_after_simple_eviction() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Fastest);
    // Use a small live window so dictionary-primed slices are evicted
    // quickly and budget retirement can be asserted deterministically.
    driver.match_generator.max_window_size = 8;
    driver.reported_window_size = 8;

    let base_window = driver.match_generator.max_window_size;
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    assert_eq!(driver.match_generator.max_window_size, base_window + 24);

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
        driver.match_generator.max_window_size, base_window,
        "retired dictionary budget must not remain reusable for live history"
    );
}

#[test]
fn prime_with_dictionary_budget_shrinks_after_dfast_eviction() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Level(2));
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
    driver.reset(CompressionLevel::Better);

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
    driver.reset(CompressionLevel::Better);
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

    matcher.maybe_rebase_positions(abs_pos);

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

#[test]
fn suffix_store_with_single_slot_does_not_panic_on_keying() {
    let mut suffixes = SuffixStore::with_capacity(1);
    suffixes.insert(b"abcde", 0);
    assert!(suffixes.contains_key(b"abcde"));
    assert_eq!(suffixes.get(b"abcde"), Some(0));
}

#[test]
fn fastest_reset_uses_interleaved_hash_fill_step() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Uncompressed);
    assert_eq!(driver.match_generator.hash_fill_step, 1);

    driver.reset(CompressionLevel::Fastest);
    assert_eq!(driver.match_generator.hash_fill_step, FAST_HASH_FILL_STEP);

    // Better uses the HashChain backend with lazy2; verify that the backend switch
    // happened and the lazy_depth is configured correctly.
    driver.reset(CompressionLevel::Better);
    assert_eq!(driver.active_backend, MatcherBackend::HashChain);
    assert_eq!(driver.window_size(), (1u64 << 23));
    assert_eq!(driver.hc_matcher().hc.lazy_depth, 2);
}

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

#[test]
fn dfast_trim_to_window_callback_reports_evicted_len_not_capacity() {
    let mut matcher = DfastMatchGenerator::new(16);

    let mut first = Vec::with_capacity(64);
    first.extend_from_slice(b"abcdefgh");
    matcher.add_data(first, |_| {});

    let mut second = Vec::with_capacity(64);
    second.extend_from_slice(b"ijklmnop");
    matcher.add_data(second, |_| {});

    matcher.max_window_size = 8;

    let mut observed_evicted_len = None;
    matcher.trim_to_window(|data| {
        observed_evicted_len = Some(data.len());
    });

    assert_eq!(
        observed_evicted_len,
        Some(8),
        "trim callback must report evicted byte length, not backing capacity"
    );
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
    let long_hash = matcher.hash8(&live[target_rel..]);
    assert!(
        matcher.long_hash[long_hash].contains(&target_abs_pos),
        "dense skip must seed long-hash entry for newly hashable boundary start"
    );
}

#[test]
fn dfast_seed_remaining_hashable_starts_seeds_last_short_hash_positions() {
    let mut matcher = DfastMatchGenerator::new(1 << 20);
    let block = deterministic_high_entropy_bytes(0x13F0_9A6D_55CE_7B21, 64);
    matcher.add_data(block, |_| {});
    matcher.ensure_hash_tables();

    let current_len = matcher.window.back().unwrap().len();
    let current_abs_start = matcher.history_abs_start + matcher.window_size - current_len;
    let seed_start = current_len - DFAST_MIN_MATCH_LEN;
    matcher.seed_remaining_hashable_starts(current_abs_start, current_len, seed_start);

    let target_abs_pos = current_abs_start + current_len - 4;
    let target_rel = target_abs_pos - matcher.history_abs_start;
    let live = matcher.live_history();
    assert!(
        target_rel + 4 <= live.len(),
        "fixture must leave the last short-hash start valid"
    );
    let short_hash = matcher.hash4(&live[target_rel..]);
    assert!(
        matcher.short_hash[short_hash].contains(&target_abs_pos),
        "tail seeding must include the last 4-byte-hashable start"
    );
}

#[test]
fn dfast_seed_remaining_hashable_starts_handles_pos_at_block_end() {
    let mut matcher = DfastMatchGenerator::new(1 << 20);
    let block = deterministic_high_entropy_bytes(0x7BB2_DA91_441E_C0EF, 64);
    matcher.add_data(block, |_| {});
    matcher.ensure_hash_tables();

    let current_len = matcher.window.back().unwrap().len();
    let current_abs_start = matcher.history_abs_start + matcher.window_size - current_len;
    matcher.seed_remaining_hashable_starts(current_abs_start, current_len, current_len);

    let target_abs_pos = current_abs_start + current_len - 4;
    let target_rel = target_abs_pos - matcher.history_abs_start;
    let live = matcher.live_history();
    assert!(
        target_rel + 4 <= live.len(),
        "fixture must leave the last short-hash start valid"
    );
    let short_hash = matcher.hash4(&live[target_rel..]);
    assert!(
        matcher.short_hash[short_hash].contains(&target_abs_pos),
        "tail seeding must still include the last 4-byte-hashable start when pos is at block end"
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

    assert!(saw_triple, "fixture must emit at least one match");
    assert_eq!(rebuilt, data);
}
