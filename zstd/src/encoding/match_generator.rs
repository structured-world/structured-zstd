//! Matching algorithm used find repeated parts in the original data
//!
//! The Zstd format relies on finden repeated sequences of data and compressing these sequences as instructions to the decoder.
//! A sequence basically tells the decoder "Go back X bytes and copy Y bytes to the end of your decode buffer".
//!
//! The task here is to efficiently find matches in the already encoded data for the current suffix of the not yet encoded data.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
use core::arch::aarch64::{uint8x16_t, vceqq_u8, vgetq_lane_u64, vld1q_u8, vreinterpretq_u64_u8};
#[cfg(target_arch = "x86")]
use core::arch::x86::{
    __m128i, __m256i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm256_cmpeq_epi8,
    _mm256_loadu_si256, _mm256_movemask_epi8,
};
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{
    __m128i, __m256i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm256_cmpeq_epi8,
    _mm256_loadu_si256, _mm256_movemask_epi8,
};
use core::convert::TryInto;
use core::num::NonZeroUsize;

use super::BETTER_WINDOW_LOG;
use super::CompressionLevel;
use super::Matcher;
use super::Sequence;
use super::blocks::encode_offset_with_history;
use super::incompressible::{block_looks_incompressible, block_looks_incompressible_strict};
#[cfg(all(feature = "std", target_arch = "aarch64", target_endian = "little"))]
use std::arch::is_aarch64_feature_detected;
#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
use std::arch::is_x86_feature_detected;
#[cfg(feature = "std")]
use std::sync::OnceLock;

const MIN_MATCH_LEN: usize = 5;
const FAST_HASH_FILL_STEP: usize = 3;
const INCOMPRESSIBLE_SKIP_STEP: usize = 8;
const DFAST_MIN_MATCH_LEN: usize = 6;
const ROW_MIN_MATCH_LEN: usize = 6;
const DFAST_TARGET_LEN: usize = 48;
// Keep these aligned with the issue's zstd level-3/dfast target unless ratio
// measurements show we can shrink them without regressing acceptance tests.
const DFAST_HASH_BITS: usize = 20;
const DFAST_SEARCH_DEPTH: usize = 4;
const DFAST_EMPTY_SLOT: usize = usize::MAX;
const DFAST_SKIP_SEARCH_STRENGTH: usize = 6;
const DFAST_SKIP_STEP_GROWTH_INTERVAL: usize = 1 << DFAST_SKIP_SEARCH_STRENGTH;
const DFAST_LOCAL_SKIP_TRIGGER: usize = 256;
const DFAST_MAX_SKIP_STEP: usize = 8;
const DFAST_INCOMPRESSIBLE_SKIP_STEP: usize = 16;
const ROW_HASH_BITS: usize = 20;
const ROW_LOG: usize = 5;
const ROW_SEARCH_DEPTH: usize = 16;
const ROW_TARGET_LEN: usize = 48;
const ROW_TAG_BITS: usize = 8;
const ROW_EMPTY_SLOT: usize = usize::MAX;
const ROW_HASH_KEY_LEN: usize = 4;

const HC_HASH_LOG: usize = 20;
const HC_CHAIN_LOG: usize = 19;
const HC_SEARCH_DEPTH: usize = 16;
const HC_MIN_MATCH_LEN: usize = 5;
const HC_TARGET_LEN: usize = 48;
// Positions are stored as (relative_pos + 1) so that 0 is a safe empty
// sentinel that can never collide with any valid position.
const HC_EMPTY: u32 = 0;

// Maximum search depth across all HC-based levels. Used to size the
// fixed-length candidate array returned by chain_candidates().
const MAX_HC_SEARCH_DEPTH: usize = 32;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PrefixKernel {
    Scalar,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    X86Sse2,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    X86Avx2,
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    Aarch64Neon,
}

/// Bundled tuning knobs for the hash-chain matcher. Using a typed config
/// instead of positional `usize` args eliminates parameter-order hazards.
#[derive(Copy, Clone)]
struct HcConfig {
    hash_log: usize,
    chain_log: usize,
    search_depth: usize,
    target_len: usize,
}

#[derive(Copy, Clone)]
struct RowConfig {
    hash_bits: usize,
    row_log: usize,
    search_depth: usize,
    target_len: usize,
}

const HC_CONFIG: HcConfig = HcConfig {
    hash_log: HC_HASH_LOG,
    chain_log: HC_CHAIN_LOG,
    search_depth: HC_SEARCH_DEPTH,
    target_len: HC_TARGET_LEN,
};

/// Best-level: deeper search, larger tables, higher target length.
const BEST_HC_CONFIG: HcConfig = HcConfig {
    hash_log: 21,
    chain_log: 20,
    search_depth: 32,
    target_len: 128,
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
/// backend and tuning knobs.  Levels that require strategies this crate does
/// not implement (greedy, btopt, btultra) are approximated with the closest
/// available backend.
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
    /* 5 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 22, hash_fill_step: 1, lazy_depth: 1, hc: HcConfig { hash_log: 18, chain_log: 17, search_depth: 4,  target_len: 32  }, row: ROW_CONFIG },
    /* 6 */ LevelParams { backend: MatcherBackend::HashChain, window_log: BETTER_WINDOW_LOG, hash_fill_step: 1, lazy_depth: 1, hc: HcConfig { hash_log: 19, chain_log: 18, search_depth: 8,  target_len: 48  }, row: ROW_CONFIG },
    /* 7 */ LevelParams { backend: MatcherBackend::HashChain, window_log: BETTER_WINDOW_LOG, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 20, chain_log: 19, search_depth: 16, target_len: 48  }, row: ROW_CONFIG },
    /* 8 */ LevelParams { backend: MatcherBackend::HashChain, window_log: BETTER_WINDOW_LOG, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 20, chain_log: 19, search_depth: 24, target_len: 64  }, row: ROW_CONFIG },
    /* 9 */ LevelParams { backend: MatcherBackend::HashChain, window_log: BETTER_WINDOW_LOG, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 21, chain_log: 20, search_depth: 24, target_len: 64  }, row: ROW_CONFIG },
    /*10 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 24, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 21, chain_log: 20, search_depth: 28, target_len: 96  }, row: ROW_CONFIG },
    /*11 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 24, hash_fill_step: 1, lazy_depth: 2, hc: BEST_HC_CONFIG, row: ROW_CONFIG },
    /*12 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 25, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 22, chain_log: 21, search_depth: 32, target_len: 128 }, row: ROW_CONFIG },
    /*13 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 25, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 22, chain_log: 21, search_depth: 32, target_len: 160 }, row: ROW_CONFIG },
    /*14 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 25, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 22, chain_log: 22, search_depth: 32, target_len: 192 }, row: ROW_CONFIG },
    /*15 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 23, chain_log: 22, search_depth: 32, target_len: 192 }, row: ROW_CONFIG },
    /*16 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 23, chain_log: 22, search_depth: 32, target_len: 256 }, row: ROW_CONFIG },
    /*17 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 23, chain_log: 23, search_depth: 32, target_len: 256 }, row: ROW_CONFIG },
    /*18 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 23, chain_log: 23, search_depth: 32, target_len: 256 }, row: ROW_CONFIG },
    /*19 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 23, chain_log: 23, search_depth: 32, target_len: 256 }, row: ROW_CONFIG },
    /*20 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 23, chain_log: 23, search_depth: 32, target_len: 256 }, row: ROW_CONFIG },
    /*21 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 23, chain_log: 23, search_depth: 32, target_len: 256 }, row: ROW_CONFIG },
    /*22 */ LevelParams { backend: MatcherBackend::HashChain, window_log: 26, hash_fill_step: 1, lazy_depth: 2, hc: HcConfig { hash_log: 23, chain_log: 23, search_depth: 32, target_len: 256 }, row: ROW_CONFIG },
];

/// Smallest window_log the encoder will use regardless of source size.
const MIN_WINDOW_LOG: u8 = 10;
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

/// Resolve a [`CompressionLevel`] to internal tuning parameters,
/// optionally adjusted for a known source size.
fn resolve_level_params(level: CompressionLevel, source_size: Option<u64>) -> LevelParams {
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
                matcher.max_window_size = matcher.max_window_size.saturating_sub(reclaimed);
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
                    self.hc_matcher_mut().trim_to_window(|data| {
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
                        hc.hash_table = Vec::new();
                        hc.chain_table = Vec::new();
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
                hc.max_window_size = max_window_size;
                hc.lazy_depth = params.lazy_depth;
                hc.configure(params.hc);
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
            MatcherBackend::HashChain => self.hc_matcher_mut().offset_hist = offset_hist,
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
                matcher.max_window_size =
                    matcher.max_window_size.saturating_add(retained_dict_budget);
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
                    matcher.max_window_size = matcher
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
            MatcherBackend::HashChain => self.hc_matcher().get_last_space(),
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

/// This stores the index of a suffix of a string by hashing the first few bytes of that suffix
/// This means that collisions just overwrite and that you need to check validity after a get
struct SuffixStore {
    // We use NonZeroUsize to enable niche optimization here.
    // On store we do +1 and on get -1
    // This is ok since usize::MAX is never a valid offset
    slots: Vec<Option<NonZeroUsize>>,
    len_log: u32,
}

impl SuffixStore {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: alloc::vec![None; capacity],
            len_log: capacity.ilog2(),
        }
    }

    #[inline(always)]
    fn insert(&mut self, suffix: &[u8], idx: usize) {
        let key = self.key(suffix);
        self.slots[key] = Some(NonZeroUsize::new(idx + 1).unwrap());
    }

    #[inline(always)]
    fn contains_key(&self, suffix: &[u8]) -> bool {
        let key = self.key(suffix);
        self.slots[key].is_some()
    }

    #[inline(always)]
    fn get(&self, suffix: &[u8]) -> Option<usize> {
        let key = self.key(suffix);
        self.slots[key].map(|x| <NonZeroUsize as Into<usize>>::into(x) - 1)
    }

    #[inline(always)]
    fn key(&self, suffix: &[u8]) -> usize {
        // Capacity=1 yields len_log=0; shifting by 64 would panic.
        if self.len_log == 0 {
            return 0;
        }

        let s0 = suffix[0] as u64;
        let s1 = suffix[1] as u64;
        let s2 = suffix[2] as u64;
        let s3 = suffix[3] as u64;
        let s4 = suffix[4] as u64;

        const POLY: u64 = 0xCF3BCCDCABu64;

        let s0 = (s0 << 24).wrapping_mul(POLY);
        let s1 = (s1 << 32).wrapping_mul(POLY);
        let s2 = (s2 << 40).wrapping_mul(POLY);
        let s3 = (s3 << 48).wrapping_mul(POLY);
        let s4 = (s4 << 56).wrapping_mul(POLY);

        let index = s0 ^ s1 ^ s2 ^ s3 ^ s4;
        let index = index >> (64 - self.len_log);
        index as usize % self.slots.len()
    }
}

/// We keep a window of a few of these entries
/// All of these are valid targets for a match to be generated for
struct WindowEntry {
    data: Vec<u8>,
    /// Stores indexes into data
    suffixes: SuffixStore,
    /// Makes offset calculations efficient
    base_offset: usize,
}

pub(crate) struct MatchGenerator {
    max_window_size: usize,
    /// Data window we are operating on to find matches
    /// The data we want to find matches for is in the last slice
    window: Vec<WindowEntry>,
    window_size: usize,
    #[cfg(debug_assertions)]
    concat_window: Vec<u8>,
    /// Index in the last slice that we already processed
    suffix_idx: usize,
    /// Gets updated when a new sequence is returned to point right behind that sequence
    last_idx_in_sequence: usize,
    hash_fill_step: usize,
    offset_hist: [u32; 3],
}

impl MatchGenerator {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[inline(always)]
    const fn select_x86_prefix_kernel(has_avx2: bool, has_sse2: bool) -> PrefixKernel {
        if has_avx2 {
            return PrefixKernel::X86Avx2;
        }
        if has_sse2 {
            return PrefixKernel::X86Sse2;
        }
        PrefixKernel::Scalar
    }

    #[cfg(feature = "std")]
    #[inline(always)]
    fn detect_prefix_kernel() -> PrefixKernel {
        static KERNEL: OnceLock<PrefixKernel> = OnceLock::new();
        *KERNEL.get_or_init(|| {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            {
                let kernel = Self::select_x86_prefix_kernel(
                    is_x86_feature_detected!("avx2"),
                    is_x86_feature_detected!("sse2"),
                );
                if kernel != PrefixKernel::Scalar {
                    return kernel;
                }
            }
            #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
            {
                if is_aarch64_feature_detected!("neon") {
                    return PrefixKernel::Aarch64Neon;
                }
            }
            PrefixKernel::Scalar
        })
    }

    #[cfg(not(feature = "std"))]
    #[inline(always)]
    fn detect_prefix_kernel() -> PrefixKernel {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            let kernel = Self::select_x86_prefix_kernel(
                cfg!(target_feature = "avx2"),
                cfg!(target_feature = "sse2"),
            );
            if kernel != PrefixKernel::Scalar {
                return kernel;
            }
        }
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        {
            if cfg!(target_feature = "neon") {
                return PrefixKernel::Aarch64Neon;
            }
        }
        PrefixKernel::Scalar
    }

    #[inline(always)]
    #[cfg(target_endian = "little")]
    fn mismatch_byte_index(diff: usize) -> usize {
        diff.trailing_zeros() as usize / 8
    }

    #[inline(always)]
    #[cfg(target_endian = "big")]
    fn mismatch_byte_index(diff: usize) -> usize {
        diff.leading_zeros() as usize / 8
    }

    /// max_size defines how many bytes will be used at most in the window used for matching
    fn new(max_size: usize) -> Self {
        Self {
            max_window_size: max_size,
            window: Vec::new(),
            window_size: 0,
            #[cfg(debug_assertions)]
            concat_window: Vec::new(),
            suffix_idx: 0,
            last_idx_in_sequence: 0,
            hash_fill_step: 1,
            offset_hist: [1, 4, 8],
        }
    }

    fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>, SuffixStore)) {
        self.window_size = 0;
        #[cfg(debug_assertions)]
        self.concat_window.clear();
        self.suffix_idx = 0;
        self.last_idx_in_sequence = 0;
        self.offset_hist = [1, 4, 8];
        self.window.drain(..).for_each(|entry| {
            reuse_space(entry.data, entry.suffixes);
        });
    }

    /// Processes bytes in the current window until either a match is found or no more matches can be found
    /// * If a match is found handle_sequence is called with the Triple variant
    /// * If no more matches can be found but there are bytes still left handle_sequence is called with the Literals variant
    /// * If no more matches can be found and no more bytes are left this returns false
    fn next_sequence(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) -> bool {
        loop {
            let last_entry = self.window.last().unwrap();
            let data_slice = &last_entry.data;

            // We already reached the end of the window, check if we need to return a Literals{}
            if self.suffix_idx >= data_slice.len() {
                if self.last_idx_in_sequence != self.suffix_idx {
                    let literals = &data_slice[self.last_idx_in_sequence..];
                    self.last_idx_in_sequence = self.suffix_idx;
                    handle_sequence(Sequence::Literals { literals });
                    return true;
                } else {
                    return false;
                }
            }

            // If the remaining data is smaller than the minimum match length we can stop and return a Literals{}
            let data_slice = &data_slice[self.suffix_idx..];
            if data_slice.len() < MIN_MATCH_LEN {
                let last_idx_in_sequence = self.last_idx_in_sequence;
                self.last_idx_in_sequence = last_entry.data.len();
                self.suffix_idx = last_entry.data.len();
                handle_sequence(Sequence::Literals {
                    literals: &last_entry.data[last_idx_in_sequence..],
                });
                return true;
            }

            // This is the key we are looking to find a match for
            let key = &data_slice[..MIN_MATCH_LEN];
            let literals_len = self.suffix_idx - self.last_idx_in_sequence;

            // Look in each window entry
            let mut candidate = self.repcode_candidate(data_slice, literals_len);
            for match_entry in self.window.iter() {
                if let Some(match_index) = match_entry.suffixes.get(key) {
                    let match_slice = &match_entry.data[match_index..];

                    // Check how long the common prefix actually is
                    let match_len = Self::common_prefix_len(match_slice, data_slice);

                    // Collisions in the suffix store might make this check fail
                    if match_len >= MIN_MATCH_LEN {
                        let offset = match_entry.base_offset + self.suffix_idx - match_index;

                        // If we are in debug/tests make sure the match we found is actually at the offset we calculated
                        #[cfg(debug_assertions)]
                        {
                            let unprocessed = last_entry.data.len() - self.suffix_idx;
                            let start = self.concat_window.len() - unprocessed - offset;
                            let end = start + match_len;
                            let check_slice = &self.concat_window[start..end];
                            debug_assert_eq!(check_slice, &match_slice[..match_len]);
                        }

                        if let Some((old_offset, old_match_len)) = candidate {
                            if match_len > old_match_len
                                || (match_len == old_match_len && offset < old_offset)
                            {
                                candidate = Some((offset, match_len));
                            }
                        } else {
                            candidate = Some((offset, match_len));
                        }
                    }
                }
            }

            if let Some((offset, match_len)) = candidate {
                // For each index in the match we found we do not need to look for another match
                // But we still want them registered in the suffix store
                self.add_suffixes_till(self.suffix_idx + match_len, self.hash_fill_step);

                // All literals that were not included between this match and the last are now included here
                let last_entry = self.window.last().unwrap();
                let literals = &last_entry.data[self.last_idx_in_sequence..self.suffix_idx];

                // Update the indexes, all indexes upto and including the current index have been included in a sequence now
                self.suffix_idx += match_len;
                self.last_idx_in_sequence = self.suffix_idx;
                let _ = encode_offset_with_history(
                    offset as u32,
                    literals.len() as u32,
                    &mut self.offset_hist,
                );
                handle_sequence(Sequence::Triple {
                    literals,
                    offset,
                    match_len,
                });

                return true;
            }

            let last_entry = self.window.last_mut().unwrap();
            let key = &last_entry.data[self.suffix_idx..self.suffix_idx + MIN_MATCH_LEN];
            if !last_entry.suffixes.contains_key(key) {
                last_entry.suffixes.insert(key, self.suffix_idx);
            }
            self.suffix_idx += 1;
        }
    }

    /// Find the common prefix length between two byte slices
    #[inline(always)]
    fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
        let max = a.len().min(b.len());
        let mut off = 0usize;
        let lhs = a.as_ptr();
        let rhs = b.as_ptr();

        match Self::detect_prefix_kernel() {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            PrefixKernel::X86Avx2 => {
                off = unsafe { Self::prefix_len_simd_avx2(lhs, rhs, max) };
            }
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            PrefixKernel::X86Sse2 => {
                off = unsafe { Self::prefix_len_simd_sse2(lhs, rhs, max) };
            }
            #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
            PrefixKernel::Aarch64Neon => {
                off = unsafe { Self::prefix_len_simd_neon(lhs, rhs, max) };
            }
            PrefixKernel::Scalar => {}
        }

        Self::common_prefix_len_scalar(a, b, off, max)
    }

    #[inline(always)]
    fn common_prefix_len_scalar(a: &[u8], b: &[u8], mut off: usize, max: usize) -> usize {
        let chunk = core::mem::size_of::<usize>();
        let lhs = a.as_ptr();
        let rhs = b.as_ptr();
        while off + chunk <= max {
            let lhs_word = unsafe { core::ptr::read_unaligned(lhs.add(off) as *const usize) };
            let rhs_word = unsafe { core::ptr::read_unaligned(rhs.add(off) as *const usize) };
            let diff = lhs_word ^ rhs_word;
            if diff != 0 {
                return off + Self::mismatch_byte_index(diff);
            }
            off += chunk;
        }
        off + core::iter::zip(&a[off..max], &b[off..max])
            .take_while(|(x, y)| x == y)
            .count()
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse2")]
    unsafe fn prefix_len_simd_sse2(lhs: *const u8, rhs: *const u8, max: usize) -> usize {
        let mut off = 0usize;
        while off + 16 <= max {
            let a: __m128i = unsafe { _mm_loadu_si128(lhs.add(off).cast::<__m128i>()) };
            let b: __m128i = unsafe { _mm_loadu_si128(rhs.add(off).cast::<__m128i>()) };
            let eq = _mm_cmpeq_epi8(a, b);
            let mask = _mm_movemask_epi8(eq) as u32;
            if mask != 0xFFFF {
                return off + (!mask).trailing_zeros() as usize;
            }
            off += 16;
        }
        off
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2")]
    unsafe fn prefix_len_simd_avx2(lhs: *const u8, rhs: *const u8, max: usize) -> usize {
        let mut off = 0usize;
        while off + 32 <= max {
            let a: __m256i = unsafe { _mm256_loadu_si256(lhs.add(off).cast::<__m256i>()) };
            let b: __m256i = unsafe { _mm256_loadu_si256(rhs.add(off).cast::<__m256i>()) };
            let eq = _mm256_cmpeq_epi8(a, b);
            let mask = _mm256_movemask_epi8(eq) as u32;
            if mask != u32::MAX {
                return off + (!mask).trailing_zeros() as usize;
            }
            off += 32;
        }
        off
    }

    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    unsafe fn prefix_len_simd_neon(lhs: *const u8, rhs: *const u8, max: usize) -> usize {
        let mut off = 0usize;
        while off + 16 <= max {
            let a: uint8x16_t = unsafe { vld1q_u8(lhs.add(off)) };
            let b: uint8x16_t = unsafe { vld1q_u8(rhs.add(off)) };
            let eq = vceqq_u8(a, b);
            let lanes = vreinterpretq_u64_u8(eq);
            let low = vgetq_lane_u64(lanes, 0);
            if low != u64::MAX {
                let diff = low ^ u64::MAX;
                return off + Self::mismatch_byte_index(diff as usize);
            }
            let high = vgetq_lane_u64(lanes, 1);
            if high != u64::MAX {
                let diff = high ^ u64::MAX;
                return off + 8 + Self::mismatch_byte_index(diff as usize);
            }
            off += 16;
        }
        off
    }

    /// Process bytes and add the suffixes to the suffix store up to a specific index
    #[inline(always)]
    fn add_suffixes_till(&mut self, idx: usize, fill_step: usize) {
        let start = self.suffix_idx;
        let last_entry = self.window.last_mut().unwrap();
        if last_entry.data.len() < MIN_MATCH_LEN {
            return;
        }
        let insert_limit = idx.saturating_sub(MIN_MATCH_LEN - 1);
        if insert_limit > start {
            let data = last_entry.data.as_slice();
            let suffixes = &mut last_entry.suffixes;
            if fill_step == FAST_HASH_FILL_STEP {
                Self::add_suffixes_interleaved_fast(data, suffixes, start, insert_limit);
            } else {
                let mut pos = start;
                while pos < insert_limit {
                    Self::insert_suffix_if_absent(data, suffixes, pos);
                    pos += fill_step;
                }
            }
        }

        if idx >= start + MIN_MATCH_LEN {
            let tail_start = idx - MIN_MATCH_LEN;
            let tail_key = &last_entry.data[tail_start..tail_start + MIN_MATCH_LEN];
            if !last_entry.suffixes.contains_key(tail_key) {
                last_entry.suffixes.insert(tail_key, tail_start);
            }
        }
    }

    #[inline(always)]
    fn insert_suffix_if_absent(data: &[u8], suffixes: &mut SuffixStore, pos: usize) {
        debug_assert!(
            pos + MIN_MATCH_LEN <= data.len(),
            "insert_suffix_if_absent: pos {} + MIN_MATCH_LEN {} exceeds data.len() {}",
            pos,
            MIN_MATCH_LEN,
            data.len()
        );
        let key = &data[pos..pos + MIN_MATCH_LEN];
        if !suffixes.contains_key(key) {
            suffixes.insert(key, pos);
        }
    }

    #[inline(always)]
    fn add_suffixes_interleaved_fast(
        data: &[u8],
        suffixes: &mut SuffixStore,
        start: usize,
        insert_limit: usize,
    ) {
        let lane = FAST_HASH_FILL_STEP;
        let mut pos = start;

        // Pipeline-ish fill: compute and retire several hash positions per loop
        // so the fastest path keeps multiple independent hash lookups in flight.
        while pos + lane * 3 < insert_limit {
            let p0 = pos;
            let p1 = pos + lane;
            let p2 = pos + lane * 2;
            let p3 = pos + lane * 3;

            Self::insert_suffix_if_absent(data, suffixes, p0);
            Self::insert_suffix_if_absent(data, suffixes, p1);
            Self::insert_suffix_if_absent(data, suffixes, p2);
            Self::insert_suffix_if_absent(data, suffixes, p3);

            pos += lane * 4;
        }

        while pos < insert_limit {
            Self::insert_suffix_if_absent(data, suffixes, pos);
            pos += lane;
        }
    }

    fn repcode_candidate(&self, data_slice: &[u8], literals_len: usize) -> Option<(usize, usize)> {
        if literals_len != 0 {
            return None;
        }

        let reps = [
            Some(self.offset_hist[1] as usize),
            Some(self.offset_hist[2] as usize),
            (self.offset_hist[0] > 1).then_some((self.offset_hist[0] - 1) as usize),
        ];

        let mut best: Option<(usize, usize)> = None;
        let mut seen = [0usize; 3];
        let mut seen_len = 0usize;
        for offset in reps.into_iter().flatten() {
            if offset == 0 {
                continue;
            }
            if seen[..seen_len].contains(&offset) {
                continue;
            }
            seen[seen_len] = offset;
            seen_len += 1;

            let Some(match_len) = self.offset_match_len(offset, data_slice) else {
                continue;
            };
            if match_len < MIN_MATCH_LEN {
                continue;
            }

            if best.is_none_or(|(old_offset, old_len)| {
                match_len > old_len || (match_len == old_len && offset < old_offset)
            }) {
                best = Some((offset, match_len));
            }
        }
        best
    }

    fn offset_match_len(&self, offset: usize, data_slice: &[u8]) -> Option<usize> {
        if offset == 0 {
            return None;
        }

        let last_idx = self.window.len().checked_sub(1)?;
        let last_entry = &self.window[last_idx];
        let searchable_prefix = self.window_size - (last_entry.data.len() - self.suffix_idx);
        if offset > searchable_prefix {
            return None;
        }

        let mut remaining = offset;
        let (entry_idx, match_index) = if remaining <= self.suffix_idx {
            (last_idx, self.suffix_idx - remaining)
        } else {
            remaining -= self.suffix_idx;
            let mut found = None;
            for entry_idx in (0..last_idx).rev() {
                let len = self.window[entry_idx].data.len();
                if remaining <= len {
                    found = Some((entry_idx, len - remaining));
                    break;
                }
                remaining -= len;
            }
            found?
        };

        let match_entry = &self.window[entry_idx];
        let match_slice = &match_entry.data[match_index..];

        Some(Self::common_prefix_len(match_slice, data_slice))
    }

    /// Skip matching for the whole current window entry.
    ///
    /// When callers already know the block is incompressible, index positions
    /// sparsely and keep a dense tail so the next block still gets boundary
    /// matches.
    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        let len = self.window.last().unwrap().data.len();
        if incompressible_hint == Some(true) {
            let dense_tail = MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
            let sparse_end = len.saturating_sub(dense_tail);
            self.add_suffixes_till(sparse_end, INCOMPRESSIBLE_SKIP_STEP);
            self.suffix_idx = sparse_end;
            self.add_suffixes_till(len, 1);
        } else {
            self.add_suffixes_till(len, 1);
        }
        self.suffix_idx = len;
        self.last_idx_in_sequence = len;
    }

    /// Backward-compatible dense path used by tests.
    #[cfg(test)]
    fn skip_matching(&mut self) {
        self.skip_matching_with_hint(None);
    }

    /// Add a new window entry. Will panic if the last window entry hasn't been processed properly.
    /// If any resources are released by pushing the new entry they are returned via the callback
    fn add_data(
        &mut self,
        data: Vec<u8>,
        suffixes: SuffixStore,
        reuse_space: impl FnMut(Vec<u8>, SuffixStore),
    ) {
        assert!(
            self.window.is_empty() || self.suffix_idx == self.window.last().unwrap().data.len()
        );
        self.reserve(data.len(), reuse_space);
        #[cfg(debug_assertions)]
        self.concat_window.extend_from_slice(&data);

        if let Some(last_len) = self.window.last().map(|last| last.data.len()) {
            for entry in self.window.iter_mut() {
                entry.base_offset += last_len;
            }
        }

        let len = data.len();
        self.window.push(WindowEntry {
            data,
            suffixes,
            base_offset: 0,
        });
        self.window_size += len;
        self.suffix_idx = 0;
        self.last_idx_in_sequence = 0;
    }

    /// Reserve space for a new window entry
    /// If any resources are released by pushing the new entry they are returned via the callback
    fn reserve(&mut self, amount: usize, mut reuse_space: impl FnMut(Vec<u8>, SuffixStore)) {
        assert!(self.max_window_size >= amount);
        while self.window_size + amount > self.max_window_size {
            let removed = self.window.remove(0);
            self.window_size -= removed.data.len();
            #[cfg(debug_assertions)]
            self.concat_window.drain(0..removed.data.len());

            let WindowEntry {
                suffixes,
                data: leaked_vec,
                base_offset: _,
            } = removed;
            reuse_space(leaked_vec, suffixes);
        }
    }
}

struct DfastMatchGenerator {
    max_window_size: usize,
    window: VecDeque<Vec<u8>>,
    window_size: usize,
    // We keep a contiguous searchable history to avoid rebuilding and reseeding
    // the matcher state from disjoint block buffers on every block.
    history: Vec<u8>,
    history_start: usize,
    history_abs_start: usize,
    offset_hist: [u32; 3],
    short_hash: Vec<[usize; DFAST_SEARCH_DEPTH]>,
    long_hash: Vec<[usize; DFAST_SEARCH_DEPTH]>,
    hash_bits: usize,
    use_fast_loop: bool,
    // Lazy match lookahead depth (internal tuning parameter).
    lazy_depth: u8,
}

#[derive(Copy, Clone)]
struct MatchCandidate {
    start: usize,
    offset: usize,
    match_len: usize,
}

fn best_len_offset_candidate(
    lhs: Option<MatchCandidate>,
    rhs: Option<MatchCandidate>,
) -> Option<MatchCandidate> {
    match (lhs, rhs) {
        (None, other) | (other, None) => other,
        (Some(lhs), Some(rhs)) => {
            if rhs.match_len > lhs.match_len
                || (rhs.match_len == lhs.match_len && rhs.offset < lhs.offset)
            {
                Some(rhs)
            } else {
                Some(lhs)
            }
        }
    }
}

fn extend_backwards_shared(
    concat: &[u8],
    history_abs_start: usize,
    mut candidate_pos: usize,
    mut abs_pos: usize,
    mut match_len: usize,
    lit_len: usize,
) -> MatchCandidate {
    let min_abs_pos = abs_pos - lit_len;
    while abs_pos > min_abs_pos
        && candidate_pos > history_abs_start
        && concat[candidate_pos - history_abs_start - 1] == concat[abs_pos - history_abs_start - 1]
    {
        candidate_pos -= 1;
        abs_pos -= 1;
        match_len += 1;
    }
    MatchCandidate {
        start: abs_pos,
        offset: abs_pos - candidate_pos,
        match_len,
    }
}

fn repcode_candidate_shared(
    concat: &[u8],
    history_abs_start: usize,
    offset_hist: [u32; 3],
    abs_pos: usize,
    lit_len: usize,
    min_match_len: usize,
) -> Option<MatchCandidate> {
    let reps = if lit_len == 0 {
        [
            Some(offset_hist[1] as usize),
            Some(offset_hist[2] as usize),
            (offset_hist[0] > 1).then_some((offset_hist[0] - 1) as usize),
        ]
    } else {
        [
            Some(offset_hist[0] as usize),
            Some(offset_hist[1] as usize),
            Some(offset_hist[2] as usize),
        ]
    };

    let current_idx = abs_pos - history_abs_start;
    if current_idx + min_match_len > concat.len() {
        return None;
    }

    let mut best = None;
    for rep in reps.into_iter().flatten() {
        if rep == 0 || rep > abs_pos {
            continue;
        }
        let candidate_pos = abs_pos - rep;
        if candidate_pos < history_abs_start {
            continue;
        }
        let candidate_idx = candidate_pos - history_abs_start;
        let match_len =
            MatchGenerator::common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
        if match_len >= min_match_len {
            let candidate = extend_backwards_shared(
                concat,
                history_abs_start,
                candidate_pos,
                abs_pos,
                match_len,
                lit_len,
            );
            best = best_len_offset_candidate(best, Some(candidate));
        }
    }
    best
}

#[derive(Copy, Clone)]
struct LazyMatchConfig {
    target_len: usize,
    min_match_len: usize,
    lazy_depth: u8,
    history_abs_end: usize,
}

fn pick_lazy_match_shared(
    abs_pos: usize,
    lit_len: usize,
    best: Option<MatchCandidate>,
    config: LazyMatchConfig,
    mut best_match_at: impl FnMut(usize, usize) -> Option<MatchCandidate>,
) -> Option<MatchCandidate> {
    let best = best?;
    if best.match_len >= config.target_len
        || abs_pos + 1 + config.min_match_len > config.history_abs_end
    {
        return Some(best);
    }

    let next = best_match_at(abs_pos + 1, lit_len + 1);
    if let Some(next) = next
        && (next.match_len > best.match_len
            || (next.match_len == best.match_len && next.offset < best.offset))
    {
        return None;
    }

    if config.lazy_depth >= 2 && abs_pos + 2 + config.min_match_len <= config.history_abs_end {
        let next2 = best_match_at(abs_pos + 2, lit_len + 2);
        if let Some(next2) = next2
            && next2.match_len > best.match_len + 1
        {
            return None;
        }
    }

    Some(best)
}

impl DfastMatchGenerator {
    // Keep a short dense tail at block boundaries for two related reasons:
    // 1) insert_position() needs short (4-byte) and long (8-byte) lookahead,
    //    so appending a new block can make starts from the previous block newly
    //    hashable and require backfill;
    // 2) we also need enough trailing bytes from the previous block to preserve
    //    cross-block matching for the minimum match length.
    const BOUNDARY_DENSE_TAIL_LEN: usize = DFAST_MIN_MATCH_LEN + 3;

    fn new(max_window_size: usize) -> Self {
        Self {
            max_window_size,
            window: VecDeque::new(),
            window_size: 0,
            history: Vec::new(),
            history_start: 0,
            history_abs_start: 0,
            offset_hist: [1, 4, 8],
            short_hash: Vec::new(),
            long_hash: Vec::new(),
            hash_bits: DFAST_HASH_BITS,
            use_fast_loop: false,
            lazy_depth: 1,
        }
    }

    fn set_hash_bits(&mut self, bits: usize) {
        let clamped = bits.clamp(MIN_WINDOW_LOG as usize, DFAST_HASH_BITS);
        if self.hash_bits != clamped {
            self.hash_bits = clamped;
            self.short_hash = Vec::new();
            self.long_hash = Vec::new();
        }
    }

    fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        self.window_size = 0;
        self.history.clear();
        self.history_start = 0;
        self.history_abs_start = 0;
        self.offset_hist = [1, 4, 8];
        if !self.short_hash.is_empty() {
            self.short_hash.fill([DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]);
            self.long_hash.fill([DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]);
        }
        for mut data in self.window.drain(..) {
            data.resize(data.capacity(), 0);
            reuse_space(data);
        }
    }

    fn get_last_space(&self) -> &[u8] {
        self.window.back().unwrap().as_slice()
    }

    fn add_data(&mut self, data: Vec<u8>, mut reuse_space: impl FnMut(Vec<u8>)) {
        assert!(data.len() <= self.max_window_size);
        while self.window_size + data.len() > self.max_window_size {
            let removed = self.window.pop_front().unwrap();
            self.window_size -= removed.len();
            self.history_start += removed.len();
            self.history_abs_start += removed.len();
            reuse_space(removed);
        }
        self.compact_history();
        self.history.extend_from_slice(&data);
        self.window_size += data.len();
        self.window.push_back(data);
    }

    fn trim_to_window(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        while self.window_size > self.max_window_size {
            let removed = self.window.pop_front().unwrap();
            self.window_size -= removed.len();
            self.history_start += removed.len();
            self.history_abs_start += removed.len();
            reuse_space(removed);
        }
    }

    fn skip_matching(&mut self, incompressible_hint: Option<bool>) {
        self.ensure_hash_tables();
        let current_len = self.window.back().unwrap().len();
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        let tail_start = current_abs_start.saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN);
        if tail_start < current_abs_start {
            self.insert_positions(tail_start, current_abs_start);
        }

        let used_sparse = incompressible_hint
            .unwrap_or_else(|| self.block_looks_incompressible(current_abs_start, current_abs_end));
        if used_sparse {
            self.insert_positions_with_step(
                current_abs_start,
                current_abs_end,
                DFAST_INCOMPRESSIBLE_SKIP_STEP,
            );
        } else {
            self.insert_positions(current_abs_start, current_abs_end);
        }

        // Seed the tail densely only after sparse insertion so the next block
        // can match across the boundary without rehashing the full block twice.
        if used_sparse {
            let tail_start = current_abs_end
                .saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN)
                .max(current_abs_start);
            if tail_start < current_abs_end {
                self.insert_positions(tail_start, current_abs_end);
            }
        }
    }

    fn skip_matching_dense(&mut self) {
        self.ensure_hash_tables();
        let current_len = self.window.back().unwrap().len();
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        let backfill_start = current_abs_start
            .saturating_sub(3)
            .max(self.history_abs_start);
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }
        self.insert_positions(current_abs_start, current_abs_end);
    }

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        self.ensure_hash_tables();

        let current_len = self.window.back().unwrap().len();
        if current_len == 0 {
            return;
        }

        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        if self.use_fast_loop {
            self.start_matching_fast_loop(current_abs_start, current_len, &mut handle_sequence);
            return;
        }
        self.start_matching_general(current_abs_start, current_len, &mut handle_sequence);
    }

    fn start_matching_general(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        let use_adaptive_skip =
            self.block_looks_incompressible(current_abs_start, current_abs_start + current_len);
        let mut pos = 0usize;
        let mut literals_start = 0usize;
        let mut skip_step = 1usize;
        let mut next_skip_growth_pos = DFAST_SKIP_STEP_GROWTH_INTERVAL;
        let mut miss_run = 0usize;
        while pos + DFAST_MIN_MATCH_LEN <= current_len {
            let abs_pos = current_abs_start + pos;
            let lit_len = pos - literals_start;

            let best = self.best_match(abs_pos, lit_len);
            if let Some(candidate) = self.pick_lazy_match(abs_pos, lit_len, best) {
                let start = self.emit_candidate(
                    current_abs_start,
                    &mut literals_start,
                    candidate,
                    handle_sequence,
                );
                pos = start + candidate.match_len;
                skip_step = 1;
                next_skip_growth_pos = pos.saturating_add(DFAST_SKIP_STEP_GROWTH_INTERVAL);
                miss_run = 0;
            } else {
                self.insert_position(abs_pos);
                miss_run = miss_run.saturating_add(1);
                let use_local_adaptive_skip = miss_run >= DFAST_LOCAL_SKIP_TRIGGER;
                if use_adaptive_skip || use_local_adaptive_skip {
                    let skip_cap = if use_adaptive_skip {
                        DFAST_MAX_SKIP_STEP
                    } else {
                        2
                    };
                    if pos >= next_skip_growth_pos {
                        skip_step = (skip_step + 1).min(skip_cap);
                        next_skip_growth_pos =
                            next_skip_growth_pos.saturating_add(DFAST_SKIP_STEP_GROWTH_INTERVAL);
                    }
                    pos = pos.saturating_add(skip_step);
                } else {
                    pos += 1;
                }
            }
        }

        self.seed_remaining_hashable_starts(current_abs_start, current_len, pos);
        self.emit_trailing_literals(literals_start, handle_sequence);
    }

    fn start_matching_fast_loop(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        let block_is_strict_incompressible = self
            .block_looks_incompressible_strict(current_abs_start, current_abs_start + current_len);
        let mut pos = 0usize;
        let mut literals_start = 0usize;
        let mut skip_step = 1usize;
        let mut next_skip_growth_pos = DFAST_SKIP_STEP_GROWTH_INTERVAL;
        let mut miss_run = 0usize;
        while pos + DFAST_MIN_MATCH_LEN <= current_len {
            let ip0 = pos;
            let ip1 = ip0.saturating_add(1);
            let ip2 = ip0.saturating_add(2);
            let ip3 = ip0.saturating_add(3);

            let abs_ip0 = current_abs_start + ip0;
            let lit_len_ip0 = ip0 - literals_start;

            if ip2 + DFAST_MIN_MATCH_LEN <= current_len {
                let abs_ip2 = current_abs_start + ip2;
                let lit_len_ip2 = ip2 - literals_start;
                if let Some(rep) = self.repcode_candidate(abs_ip2, lit_len_ip2)
                    && rep.start >= current_abs_start + literals_start
                    && rep.start <= abs_ip2
                {
                    let start = self.emit_candidate(
                        current_abs_start,
                        &mut literals_start,
                        rep,
                        handle_sequence,
                    );
                    pos = start + rep.match_len;
                    skip_step = 1;
                    next_skip_growth_pos = pos.saturating_add(DFAST_SKIP_STEP_GROWTH_INTERVAL);
                    miss_run = 0;
                    continue;
                }
            }

            let best = self.best_match(abs_ip0, lit_len_ip0);
            if let Some(candidate) = best {
                let start = self.emit_candidate(
                    current_abs_start,
                    &mut literals_start,
                    candidate,
                    handle_sequence,
                );
                pos = start + candidate.match_len;
                skip_step = 1;
                next_skip_growth_pos = pos.saturating_add(DFAST_SKIP_STEP_GROWTH_INTERVAL);
                miss_run = 0;
            } else {
                self.insert_position(abs_ip0);
                if ip1 + 4 <= current_len {
                    self.insert_position(current_abs_start + ip1);
                }
                if ip2 + 4 <= current_len {
                    self.insert_position(current_abs_start + ip2);
                }
                if ip3 + 4 <= current_len {
                    self.insert_position(current_abs_start + ip3);
                }
                miss_run = miss_run.saturating_add(1);
                if block_is_strict_incompressible || miss_run >= DFAST_LOCAL_SKIP_TRIGGER {
                    let skip_cap = DFAST_MAX_SKIP_STEP;
                    if pos >= next_skip_growth_pos {
                        skip_step = (skip_step + 1).min(skip_cap);
                        next_skip_growth_pos =
                            next_skip_growth_pos.saturating_add(DFAST_SKIP_STEP_GROWTH_INTERVAL);
                    }
                    pos = pos.saturating_add(skip_step);
                } else {
                    skip_step = 1;
                    next_skip_growth_pos = pos.saturating_add(DFAST_SKIP_STEP_GROWTH_INTERVAL);
                    pos += 1;
                }
            }
        }

        self.seed_remaining_hashable_starts(current_abs_start, current_len, pos);
        self.emit_trailing_literals(literals_start, handle_sequence);
    }

    fn seed_remaining_hashable_starts(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        pos: usize,
    ) {
        let mut seed_pos = pos.min(current_len);
        while seed_pos + DFAST_MIN_MATCH_LEN <= current_len {
            self.insert_position(current_abs_start + seed_pos);
            seed_pos += 1;
        }
    }

    fn emit_candidate(
        &mut self,
        current_abs_start: usize,
        literals_start: &mut usize,
        candidate: MatchCandidate,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) -> usize {
        self.insert_positions(
            current_abs_start + *literals_start,
            candidate.start + candidate.match_len,
        );
        let current = self.window.back().unwrap().as_slice();
        let start = candidate.start - current_abs_start;
        let literals = &current[*literals_start..start];
        handle_sequence(Sequence::Triple {
            literals,
            offset: candidate.offset,
            match_len: candidate.match_len,
        });
        let _ = encode_offset_with_history(
            candidate.offset as u32,
            literals.len() as u32,
            &mut self.offset_hist,
        );
        *literals_start = start + candidate.match_len;
        start
    }

    fn emit_trailing_literals(
        &self,
        literals_start: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        if literals_start < self.window.back().unwrap().len() {
            let current = self.window.back().unwrap().as_slice();
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    fn ensure_hash_tables(&mut self) {
        let table_len = 1usize << self.hash_bits;
        if self.short_hash.len() != table_len {
            // This is intentionally lazy so Fastest/Uncompressed never pay the
            // ~dfast-level memory cost. The current size tracks the issue's
            // zstd level-3 style parameters rather than a generic low-memory preset.
            self.short_hash = alloc::vec![[DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]; table_len];
            self.long_hash = alloc::vec![[DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]; table_len];
        }
    }

    fn compact_history(&mut self) {
        if self.history_start == 0 {
            return;
        }
        if self.history_start >= self.max_window_size
            || self.history_start * 2 >= self.history.len()
        {
            self.history.drain(..self.history_start);
            self.history_start = 0;
        }
    }

    fn live_history(&self) -> &[u8] {
        &self.history[self.history_start..]
    }

    fn history_abs_end(&self) -> usize {
        self.history_abs_start + self.live_history().len()
    }

    fn best_match(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let rep = self.repcode_candidate(abs_pos, lit_len);
        let hash = self.hash_candidate(abs_pos, lit_len);
        best_len_offset_candidate(rep, hash)
    }

    fn pick_lazy_match(
        &self,
        abs_pos: usize,
        lit_len: usize,
        best: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        pick_lazy_match_shared(
            abs_pos,
            lit_len,
            best,
            LazyMatchConfig {
                target_len: DFAST_TARGET_LEN,
                min_match_len: DFAST_MIN_MATCH_LEN,
                lazy_depth: self.lazy_depth,
                history_abs_end: self.history_abs_end(),
            },
            |next_pos, next_lit_len| self.best_match(next_pos, next_lit_len),
        )
    }

    fn repcode_candidate(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        repcode_candidate_shared(
            self.live_history(),
            self.history_abs_start,
            self.offset_hist,
            abs_pos,
            lit_len,
            DFAST_MIN_MATCH_LEN,
        )
    }

    fn hash_candidate(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let concat = self.live_history();
        let current_idx = abs_pos - self.history_abs_start;
        let mut best = None;
        for candidate_pos in self.long_candidates(abs_pos) {
            if candidate_pos < self.history_abs_start || candidate_pos >= abs_pos {
                continue;
            }
            let candidate_idx = candidate_pos - self.history_abs_start;
            let match_len =
                MatchGenerator::common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
            if match_len >= DFAST_MIN_MATCH_LEN {
                let candidate = self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len);
                best = best_len_offset_candidate(best, Some(candidate));
                if best.is_some_and(|best| best.match_len >= DFAST_TARGET_LEN) {
                    return best;
                }
            }
        }

        for candidate_pos in self.short_candidates(abs_pos) {
            if candidate_pos < self.history_abs_start || candidate_pos >= abs_pos {
                continue;
            }
            let candidate_idx = candidate_pos - self.history_abs_start;
            let match_len =
                MatchGenerator::common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
            if match_len >= DFAST_MIN_MATCH_LEN {
                let candidate = self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len);
                best = best_len_offset_candidate(best, Some(candidate));
                if best.is_some_and(|best| best.match_len >= DFAST_TARGET_LEN) {
                    return best;
                }
            }
        }
        best
    }

    fn extend_backwards(
        &self,
        candidate_pos: usize,
        abs_pos: usize,
        match_len: usize,
        lit_len: usize,
    ) -> MatchCandidate {
        extend_backwards_shared(
            self.live_history(),
            self.history_abs_start,
            candidate_pos,
            abs_pos,
            match_len,
            lit_len,
        )
    }

    fn insert_positions(&mut self, start: usize, end: usize) {
        let start = start.max(self.history_abs_start);
        let end = end.min(self.history_abs_end());
        for pos in start..end {
            self.insert_position(pos);
        }
    }

    fn insert_positions_with_step(&mut self, start: usize, end: usize, step: usize) {
        let start = start.max(self.history_abs_start);
        let end = end.min(self.history_abs_end());
        if step <= 1 {
            self.insert_positions(start, end);
            return;
        }
        let mut pos = start;
        while pos < end {
            self.insert_position(pos);
            pos = pos.saturating_add(step);
        }
    }

    fn insert_position(&mut self, pos: usize) {
        let idx = pos - self.history_abs_start;
        let short = {
            let concat = self.live_history();
            (idx + 4 <= concat.len()).then(|| self.hash4(&concat[idx..]))
        };
        if let Some(short) = short {
            let bucket = &mut self.short_hash[short];
            if bucket[0] != pos {
                bucket.copy_within(0..DFAST_SEARCH_DEPTH - 1, 1);
                bucket[0] = pos;
            }
        }

        let long = {
            let concat = self.live_history();
            (idx + 8 <= concat.len()).then(|| self.hash8(&concat[idx..]))
        };
        if let Some(long) = long {
            let bucket = &mut self.long_hash[long];
            if bucket[0] != pos {
                bucket.copy_within(0..DFAST_SEARCH_DEPTH - 1, 1);
                bucket[0] = pos;
            }
        }
    }

    fn short_candidates(&self, pos: usize) -> impl Iterator<Item = usize> + '_ {
        let concat = self.live_history();
        let idx = pos - self.history_abs_start;
        (idx + 4 <= concat.len())
            .then(|| self.short_hash[self.hash4(&concat[idx..])])
            .into_iter()
            .flatten()
            .filter(|candidate| *candidate != DFAST_EMPTY_SLOT)
    }

    fn long_candidates(&self, pos: usize) -> impl Iterator<Item = usize> + '_ {
        let concat = self.live_history();
        let idx = pos - self.history_abs_start;
        (idx + 8 <= concat.len())
            .then(|| self.long_hash[self.hash8(&concat[idx..])])
            .into_iter()
            .flatten()
            .filter(|candidate| *candidate != DFAST_EMPTY_SLOT)
    }

    fn hash4(&self, data: &[u8]) -> usize {
        let value = u32::from_le_bytes(data[..4].try_into().unwrap()) as u64;
        self.hash_index(value)
    }

    fn hash8(&self, data: &[u8]) -> usize {
        let value = u64::from_le_bytes(data[..8].try_into().unwrap());
        self.hash_index(value)
    }

    fn block_looks_incompressible(&self, start: usize, end: usize) -> bool {
        let live = self.live_history();
        if start >= end || start < self.history_abs_start {
            return false;
        }
        let start_idx = start - self.history_abs_start;
        let end_idx = end - self.history_abs_start;
        if end_idx > live.len() {
            return false;
        }
        let block = &live[start_idx..end_idx];
        block_looks_incompressible(block)
    }

    fn block_looks_incompressible_strict(&self, start: usize, end: usize) -> bool {
        let live = self.live_history();
        if start >= end || start < self.history_abs_start {
            return false;
        }
        let start_idx = start - self.history_abs_start;
        let end_idx = end - self.history_abs_start;
        if end_idx > live.len() {
            return false;
        }
        let block = &live[start_idx..end_idx];
        block_looks_incompressible_strict(block)
    }

    fn hash_index(&self, value: u64) -> usize {
        const PRIME: u64 = 0x9E37_79B1_85EB_CA87;
        ((value.wrapping_mul(PRIME)) >> (64 - self.hash_bits)) as usize
    }
}

struct RowMatchGenerator {
    max_window_size: usize,
    window: VecDeque<Vec<u8>>,
    window_size: usize,
    history: Vec<u8>,
    history_start: usize,
    history_abs_start: usize,
    offset_hist: [u32; 3],
    row_hash_log: usize,
    row_log: usize,
    search_depth: usize,
    target_len: usize,
    lazy_depth: u8,
    row_heads: Vec<u8>,
    row_positions: Vec<usize>,
    row_tags: Vec<u8>,
}

impl RowMatchGenerator {
    fn new(max_window_size: usize) -> Self {
        Self {
            max_window_size,
            window: VecDeque::new(),
            window_size: 0,
            history: Vec::new(),
            history_start: 0,
            history_abs_start: 0,
            offset_hist: [1, 4, 8],
            row_hash_log: ROW_HASH_BITS - ROW_LOG,
            row_log: ROW_LOG,
            search_depth: ROW_SEARCH_DEPTH,
            target_len: ROW_TARGET_LEN,
            lazy_depth: 1,
            row_heads: Vec::new(),
            row_positions: Vec::new(),
            row_tags: Vec::new(),
        }
    }

    fn set_hash_bits(&mut self, bits: usize) {
        let clamped = bits.clamp(self.row_log + 1, ROW_HASH_BITS);
        let row_hash_log = clamped.saturating_sub(self.row_log);
        if self.row_hash_log != row_hash_log {
            self.row_hash_log = row_hash_log;
            self.row_heads.clear();
            self.row_positions.clear();
            self.row_tags.clear();
        }
    }

    fn configure(&mut self, config: RowConfig) {
        self.row_log = config.row_log.clamp(4, 6);
        self.search_depth = config.search_depth;
        self.target_len = config.target_len;
        self.set_hash_bits(config.hash_bits.max(self.row_log + 1));
    }

    fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        self.window_size = 0;
        self.history.clear();
        self.history_start = 0;
        self.history_abs_start = 0;
        self.offset_hist = [1, 4, 8];
        self.row_heads.fill(0);
        self.row_positions.fill(ROW_EMPTY_SLOT);
        self.row_tags.fill(0);
        for mut data in self.window.drain(..) {
            data.resize(data.capacity(), 0);
            reuse_space(data);
        }
    }

    fn get_last_space(&self) -> &[u8] {
        self.window.back().unwrap().as_slice()
    }

    fn add_data(&mut self, data: Vec<u8>, mut reuse_space: impl FnMut(Vec<u8>)) {
        assert!(data.len() <= self.max_window_size);
        while self.window_size + data.len() > self.max_window_size {
            let removed = self.window.pop_front().unwrap();
            self.window_size -= removed.len();
            self.history_start += removed.len();
            self.history_abs_start += removed.len();
            reuse_space(removed);
        }
        self.compact_history();
        self.history.extend_from_slice(&data);
        self.window_size += data.len();
        self.window.push_back(data);
    }

    fn trim_to_window(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        while self.window_size > self.max_window_size {
            let removed = self.window.pop_front().unwrap();
            self.window_size -= removed.len();
            self.history_start += removed.len();
            self.history_abs_start += removed.len();
            reuse_space(removed);
        }
    }

    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        self.ensure_tables();
        let current_len = self.window.back().unwrap().len();
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        let backfill_start = self.backfill_start(current_abs_start);
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }
        if incompressible_hint == Some(true) {
            self.insert_positions_with_step(
                current_abs_start,
                current_abs_end,
                INCOMPRESSIBLE_SKIP_STEP,
            );
            let dense_tail = ROW_MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
            let tail_start = current_abs_end
                .saturating_sub(dense_tail)
                .max(current_abs_start);
            for pos in tail_start..current_abs_end {
                if !(pos - current_abs_start).is_multiple_of(INCOMPRESSIBLE_SKIP_STEP) {
                    self.insert_position(pos);
                }
            }
        } else {
            self.insert_positions(current_abs_start, current_abs_end);
        }
    }

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        self.ensure_tables();

        let current_len = self.window.back().unwrap().len();
        if current_len == 0 {
            return;
        }
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let backfill_start = self.backfill_start(current_abs_start);
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }

        let mut pos = 0usize;
        let mut literals_start = 0usize;
        while pos + ROW_MIN_MATCH_LEN <= current_len {
            let abs_pos = current_abs_start + pos;
            let lit_len = pos - literals_start;

            let best = self.best_match(abs_pos, lit_len);
            if let Some(candidate) = self.pick_lazy_match(abs_pos, lit_len, best) {
                self.insert_positions(abs_pos, candidate.start + candidate.match_len);
                let current = self.window.back().unwrap().as_slice();
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
                    &mut self.offset_hist,
                );
                pos = start + candidate.match_len;
                literals_start = pos;
            } else {
                self.insert_position(abs_pos);
                pos += 1;
            }
        }

        while pos + ROW_HASH_KEY_LEN <= current_len {
            self.insert_position(current_abs_start + pos);
            pos += 1;
        }

        if literals_start < current_len {
            let current = self.window.back().unwrap().as_slice();
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    fn ensure_tables(&mut self) {
        let row_count = 1usize << self.row_hash_log;
        let row_entries = 1usize << self.row_log;
        let total = row_count * row_entries;
        if self.row_positions.len() != total {
            self.row_heads = alloc::vec![0; row_count];
            self.row_positions = alloc::vec![ROW_EMPTY_SLOT; total];
            self.row_tags = alloc::vec![0; total];
        }
    }

    fn compact_history(&mut self) {
        if self.history_start == 0 {
            return;
        }
        if self.history_start >= self.max_window_size
            || self.history_start * 2 >= self.history.len()
        {
            self.history.drain(..self.history_start);
            self.history_start = 0;
        }
    }

    fn live_history(&self) -> &[u8] {
        &self.history[self.history_start..]
    }

    fn history_abs_end(&self) -> usize {
        self.history_abs_start + self.live_history().len()
    }

    fn hash_and_row(&self, abs_pos: usize) -> Option<(usize, u8)> {
        let idx = abs_pos - self.history_abs_start;
        let concat = self.live_history();
        if idx + ROW_HASH_KEY_LEN > concat.len() {
            return None;
        }
        let value =
            u32::from_le_bytes(concat[idx..idx + ROW_HASH_KEY_LEN].try_into().unwrap()) as u64;
        const PRIME: u64 = 0x9E37_79B1_85EB_CA87;
        let hash = value.wrapping_mul(PRIME);
        let total_bits = self.row_hash_log + ROW_TAG_BITS;
        let combined = hash >> (u64::BITS as usize - total_bits);
        let row_mask = (1usize << self.row_hash_log) - 1;
        let row = ((combined >> ROW_TAG_BITS) as usize) & row_mask;
        let tag = combined as u8;
        Some((row, tag))
    }

    fn backfill_start(&self, current_abs_start: usize) -> usize {
        current_abs_start
            .saturating_sub(ROW_HASH_KEY_LEN - 1)
            .max(self.history_abs_start)
    }

    fn best_match(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let rep = self.repcode_candidate(abs_pos, lit_len);
        let row = self.row_candidate(abs_pos, lit_len);
        best_len_offset_candidate(rep, row)
    }

    fn pick_lazy_match(
        &self,
        abs_pos: usize,
        lit_len: usize,
        best: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        pick_lazy_match_shared(
            abs_pos,
            lit_len,
            best,
            LazyMatchConfig {
                target_len: self.target_len,
                min_match_len: ROW_MIN_MATCH_LEN,
                lazy_depth: self.lazy_depth,
                history_abs_end: self.history_abs_end(),
            },
            |next_pos, next_lit_len| self.best_match(next_pos, next_lit_len),
        )
    }

    fn repcode_candidate(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        repcode_candidate_shared(
            self.live_history(),
            self.history_abs_start,
            self.offset_hist,
            abs_pos,
            lit_len,
            ROW_MIN_MATCH_LEN,
        )
    }

    fn row_candidate(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let concat = self.live_history();
        let current_idx = abs_pos - self.history_abs_start;
        if current_idx + ROW_MIN_MATCH_LEN > concat.len() {
            return None;
        }

        let (row, tag) = self.hash_and_row(abs_pos)?;
        let row_entries = 1usize << self.row_log;
        let row_mask = row_entries - 1;
        let row_base = row << self.row_log;
        let head = self.row_heads[row] as usize;
        let max_walk = self.search_depth.min(row_entries);

        let mut best = None;
        for i in 0..max_walk {
            let slot = (head + i) & row_mask;
            let idx = row_base + slot;
            if self.row_tags[idx] != tag {
                continue;
            }
            let candidate_pos = self.row_positions[idx];
            if candidate_pos == ROW_EMPTY_SLOT
                || candidate_pos < self.history_abs_start
                || candidate_pos >= abs_pos
            {
                continue;
            }
            let candidate_idx = candidate_pos - self.history_abs_start;
            let match_len =
                MatchGenerator::common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
            if match_len >= ROW_MIN_MATCH_LEN {
                let candidate = self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len);
                best = best_len_offset_candidate(best, Some(candidate));
                if best.is_some_and(|best| best.match_len >= self.target_len) {
                    return best;
                }
            }
        }
        best
    }

    fn extend_backwards(
        &self,
        candidate_pos: usize,
        abs_pos: usize,
        match_len: usize,
        lit_len: usize,
    ) -> MatchCandidate {
        extend_backwards_shared(
            self.live_history(),
            self.history_abs_start,
            candidate_pos,
            abs_pos,
            match_len,
            lit_len,
        )
    }

    fn insert_positions(&mut self, start: usize, end: usize) {
        for pos in start..end {
            self.insert_position(pos);
        }
    }

    fn insert_positions_with_step(&mut self, start: usize, end: usize, step: usize) {
        if step <= 1 {
            self.insert_positions(start, end);
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

    fn insert_position(&mut self, abs_pos: usize) {
        let Some((row, tag)) = self.hash_and_row(abs_pos) else {
            return;
        };
        let row_entries = 1usize << self.row_log;
        let row_mask = row_entries - 1;
        let row_base = row << self.row_log;
        let head = self.row_heads[row] as usize;
        let next = head.wrapping_sub(1) & row_mask;
        self.row_heads[row] = next as u8;
        self.row_tags[row_base + next] = tag;
        self.row_positions[row_base + next] = abs_pos;
    }
}

struct HcMatchGenerator {
    max_window_size: usize,
    window: VecDeque<Vec<u8>>,
    window_size: usize,
    history: Vec<u8>,
    history_start: usize,
    history_abs_start: usize,
    position_base: usize,
    offset_hist: [u32; 3],
    hash_table: Vec<u32>,
    chain_table: Vec<u32>,
    lazy_depth: u8,
    hash_log: usize,
    chain_log: usize,
    search_depth: usize,
    target_len: usize,
}

impl HcMatchGenerator {
    fn new(max_window_size: usize) -> Self {
        Self {
            max_window_size,
            window: VecDeque::new(),
            window_size: 0,
            history: Vec::new(),
            history_start: 0,
            history_abs_start: 0,
            position_base: 0,
            offset_hist: [1, 4, 8],
            hash_table: Vec::new(),
            chain_table: Vec::new(),
            lazy_depth: 2,
            hash_log: HC_HASH_LOG,
            chain_log: HC_CHAIN_LOG,
            search_depth: HC_SEARCH_DEPTH,
            target_len: HC_TARGET_LEN,
        }
    }

    fn configure(&mut self, config: HcConfig) {
        let resize = self.hash_log != config.hash_log || self.chain_log != config.chain_log;
        self.hash_log = config.hash_log;
        self.chain_log = config.chain_log;
        self.search_depth = config.search_depth.min(MAX_HC_SEARCH_DEPTH);
        self.target_len = config.target_len;
        if resize && !self.hash_table.is_empty() {
            // Force reallocation on next ensure_tables() call.
            self.hash_table.clear();
            self.chain_table.clear();
        }
    }

    fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        self.window_size = 0;
        self.history.clear();
        self.history_start = 0;
        self.history_abs_start = 0;
        self.position_base = 0;
        self.offset_hist = [1, 4, 8];
        if !self.hash_table.is_empty() {
            self.hash_table.fill(HC_EMPTY);
            self.chain_table.fill(HC_EMPTY);
        }
        for mut data in self.window.drain(..) {
            data.resize(data.capacity(), 0);
            reuse_space(data);
        }
    }

    fn get_last_space(&self) -> &[u8] {
        self.window.back().unwrap().as_slice()
    }

    // History duplicates window data for O(1) contiguous access during match
    // finding (common_prefix_len, extend_backwards). Same pattern as
    // DfastMatchGenerator. Peak: ~2x window size for data buffers + 6 MB tables.
    fn add_data(&mut self, data: Vec<u8>, mut reuse_space: impl FnMut(Vec<u8>)) {
        assert!(data.len() <= self.max_window_size);
        while self.window_size + data.len() > self.max_window_size {
            let removed = self.window.pop_front().unwrap();
            self.window_size -= removed.len();
            self.history_start += removed.len();
            self.history_abs_start += removed.len();
            reuse_space(removed);
        }
        self.compact_history();
        self.history.extend_from_slice(&data);
        self.window_size += data.len();
        self.window.push_back(data);
    }

    fn trim_to_window(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        while self.window_size > self.max_window_size {
            let removed = self.window.pop_front().unwrap();
            self.window_size -= removed.len();
            self.history_start += removed.len();
            self.history_abs_start += removed.len();
            reuse_space(removed);
        }
    }

    /// Backfill positions from the tail of the previous slice that couldn't be
    /// hashed at the time (insert_position needs 4 bytes of lookahead).
    fn backfill_boundary_positions(&mut self, current_abs_start: usize) {
        let backfill_start = current_abs_start
            .saturating_sub(3)
            .max(self.history_abs_start);
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }
    }

    fn skip_matching(&mut self, incompressible_hint: Option<bool>) {
        self.ensure_tables();
        let current_len = self.window.back().unwrap().len();
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        self.backfill_boundary_positions(current_abs_start);
        if incompressible_hint == Some(true) {
            self.insert_positions_with_step(
                current_abs_start,
                current_abs_end,
                INCOMPRESSIBLE_SKIP_STEP,
            );
            let dense_tail = HC_MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
            let tail_start = current_abs_end
                .saturating_sub(dense_tail)
                .max(self.history_abs_start);
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

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        self.ensure_tables();

        let current_len = self.window.back().unwrap().len();
        if current_len == 0 {
            return;
        }

        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        self.backfill_boundary_positions(current_abs_start);

        let mut pos = 0usize;
        let mut literals_start = 0usize;
        while pos + HC_MIN_MATCH_LEN <= current_len {
            let abs_pos = current_abs_start + pos;
            let lit_len = pos - literals_start;

            let best = self.find_best_match(abs_pos, lit_len);
            if let Some(candidate) = self.pick_lazy_match(abs_pos, lit_len, best) {
                self.insert_positions(abs_pos, candidate.start + candidate.match_len);
                let current = self.window.back().unwrap().as_slice();
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
                    &mut self.offset_hist,
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
            let current = self.window.back().unwrap().as_slice();
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    fn ensure_tables(&mut self) {
        if self.hash_table.is_empty() {
            self.hash_table = alloc::vec![HC_EMPTY; 1 << self.hash_log];
            self.chain_table = alloc::vec![HC_EMPTY; 1 << self.chain_log];
        }
    }

    fn compact_history(&mut self) {
        if self.history_start == 0 {
            return;
        }
        if self.history_start >= self.max_window_size
            || self.history_start * 2 >= self.history.len()
        {
            self.history.drain(..self.history_start);
            self.history_start = 0;
        }
    }

    fn live_history(&self) -> &[u8] {
        &self.history[self.history_start..]
    }

    fn history_abs_end(&self) -> usize {
        self.history_abs_start + self.live_history().len()
    }

    fn hash_position(&self, data: &[u8]) -> usize {
        let value = u32::from_le_bytes(data[..4].try_into().unwrap()) as u64;
        const PRIME: u64 = 0x9E37_79B1_85EB_CA87;
        ((value.wrapping_mul(PRIME)) >> (64 - self.hash_log)) as usize
    }

    fn relative_position(&self, abs_pos: usize) -> Option<u32> {
        let rel = abs_pos.checked_sub(self.position_base)?;
        let rel_u32 = u32::try_from(rel).ok()?;
        // Positions are stored as (relative_pos + 1), with 0 reserved as the
        // empty sentinel. So the raw relative position itself must stay
        // strictly below u32::MAX.
        (rel_u32 < u32::MAX).then_some(rel_u32)
    }

    fn maybe_rebase_positions(&mut self, abs_pos: usize) {
        let needs_rebase = self
            .relative_position(abs_pos)
            .is_none_or(|relative| relative >= u32::MAX - 1);
        if !needs_rebase {
            return;
        }

        // Keep all live history addressable after rebase.
        self.position_base = self.history_abs_start;
        self.hash_table.fill(HC_EMPTY);
        self.chain_table.fill(HC_EMPTY);

        let history_start = self.history_abs_start;
        // Rebuild only the already-inserted prefix. The caller inserts abs_pos
        // immediately after this, and later positions are added in-order.
        for pos in history_start..abs_pos {
            self.insert_position_no_rebase(pos);
        }
    }

    fn insert_position(&mut self, abs_pos: usize) {
        self.maybe_rebase_positions(abs_pos);
        self.insert_position_no_rebase(abs_pos);
    }

    fn insert_position_no_rebase(&mut self, abs_pos: usize) {
        let idx = abs_pos - self.history_abs_start;
        let concat = self.live_history();
        if idx + 4 > concat.len() {
            return;
        }
        let hash = self.hash_position(&concat[idx..]);
        let Some(relative_pos) = self.relative_position(abs_pos) else {
            return;
        };
        let stored = relative_pos + 1;
        let chain_idx = relative_pos as usize & ((1 << self.chain_log) - 1);
        let prev = self.hash_table[hash];
        self.chain_table[chain_idx] = prev;
        self.hash_table[hash] = stored;
    }

    fn insert_positions(&mut self, start: usize, end: usize) {
        for pos in start..end {
            self.insert_position(pos);
        }
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

    // Fixed-size stack array is intentional: it avoids heap allocation on
    // the hot path and the sentinel loop exits at self.search_depth.
    fn chain_candidates(&self, abs_pos: usize) -> [usize; MAX_HC_SEARCH_DEPTH] {
        let mut buf = [usize::MAX; MAX_HC_SEARCH_DEPTH];
        let idx = abs_pos - self.history_abs_start;
        let concat = self.live_history();
        if idx + 4 > concat.len() {
            return buf;
        }
        let hash = self.hash_position(&concat[idx..]);
        let chain_mask = (1 << self.chain_log) - 1;

        let mut cur = self.hash_table[hash];
        let mut filled = 0;
        // Follow chain up to search_depth valid candidates, skipping stale
        // entries (evicted from window) instead of stopping at them.
        // Stored values are (relative_pos + 1); decode with wrapping_sub(1)
        // and recover absolute position via position_base + relative.
        // Break on self-loops (masked chain_idx collision at periodicity).
        // Cap total steps at 4x search depth to bound time spent skipping
        // stale entries while still finding valid candidates deeper in chain.
        let mut steps = 0;
        let max_chain_steps = self.search_depth * 4;
        while filled < self.search_depth && steps < max_chain_steps {
            if cur == HC_EMPTY {
                break;
            }
            let candidate_rel = cur.wrapping_sub(1) as usize;
            let candidate_abs = self.position_base + candidate_rel;
            let next = self.chain_table[candidate_rel & chain_mask];
            steps += 1;
            if next == cur {
                // Self-loop: two positions share chain_idx, stop to avoid
                // spinning on the same candidate forever.
                if candidate_abs >= self.history_abs_start && candidate_abs < abs_pos {
                    buf[filled] = candidate_abs;
                }
                break;
            }
            cur = next;
            if candidate_abs < self.history_abs_start || candidate_abs >= abs_pos {
                continue;
            }
            buf[filled] = candidate_abs;
            filled += 1;
        }
        buf
    }

    fn find_best_match(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let rep = self.repcode_candidate(abs_pos, lit_len);
        let hash = self.hash_chain_candidate(abs_pos, lit_len);
        Self::better_candidate(rep, hash)
    }

    fn hash_chain_candidate(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let concat = self.live_history();
        let current_idx = abs_pos - self.history_abs_start;
        if current_idx + HC_MIN_MATCH_LEN > concat.len() {
            return None;
        }

        let mut best: Option<MatchCandidate> = None;
        for candidate_abs in self.chain_candidates(abs_pos) {
            if candidate_abs == usize::MAX {
                break;
            }
            let candidate_idx = candidate_abs - self.history_abs_start;
            let match_len =
                MatchGenerator::common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
            if match_len >= HC_MIN_MATCH_LEN {
                let candidate = self.extend_backwards(candidate_abs, abs_pos, match_len, lit_len);
                best = Self::better_candidate(best, Some(candidate));
                if best.is_some_and(|b| b.match_len >= self.target_len) {
                    return best;
                }
            }
        }
        best
    }

    fn repcode_candidate(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let reps = if lit_len == 0 {
            [
                Some(self.offset_hist[1] as usize),
                Some(self.offset_hist[2] as usize),
                (self.offset_hist[0] > 1).then_some((self.offset_hist[0] - 1) as usize),
            ]
        } else {
            [
                Some(self.offset_hist[0] as usize),
                Some(self.offset_hist[1] as usize),
                Some(self.offset_hist[2] as usize),
            ]
        };

        let concat = self.live_history();
        let current_idx = abs_pos - self.history_abs_start;
        if current_idx + HC_MIN_MATCH_LEN > concat.len() {
            return None;
        }

        let mut best = None;
        for rep in reps.into_iter().flatten() {
            if rep == 0 || rep > abs_pos {
                continue;
            }
            let candidate_pos = abs_pos - rep;
            if candidate_pos < self.history_abs_start {
                continue;
            }
            let candidate_idx = candidate_pos - self.history_abs_start;
            let match_len =
                MatchGenerator::common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
            if match_len >= HC_MIN_MATCH_LEN {
                let candidate = self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len);
                best = Self::better_candidate(best, Some(candidate));
            }
        }
        best
    }

    fn extend_backwards(
        &self,
        mut candidate_pos: usize,
        mut abs_pos: usize,
        mut match_len: usize,
        lit_len: usize,
    ) -> MatchCandidate {
        let concat = self.live_history();
        let min_abs_pos = abs_pos - lit_len;
        while abs_pos > min_abs_pos
            && candidate_pos > self.history_abs_start
            && concat[candidate_pos - self.history_abs_start - 1]
                == concat[abs_pos - self.history_abs_start - 1]
        {
            candidate_pos -= 1;
            abs_pos -= 1;
            match_len += 1;
        }
        MatchCandidate {
            start: abs_pos,
            offset: abs_pos - candidate_pos,
            match_len,
        }
    }

    fn better_candidate(
        lhs: Option<MatchCandidate>,
        rhs: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        match (lhs, rhs) {
            (None, other) | (other, None) => other,
            (Some(lhs), Some(rhs)) => {
                let lhs_gain = Self::match_gain(lhs.match_len, lhs.offset);
                let rhs_gain = Self::match_gain(rhs.match_len, rhs.offset);
                if rhs_gain > lhs_gain {
                    Some(rhs)
                } else {
                    Some(lhs)
                }
            }
        }
    }

    fn match_gain(match_len: usize, offset: usize) -> i32 {
        debug_assert!(
            offset > 0,
            "zstd offsets are 1-indexed, offset=0 is invalid"
        );
        let offset_bits = 32 - (offset as u32).leading_zeros() as i32;
        (match_len as i32) * 4 - offset_bits
    }

    // Lazy lookahead queries pos+1/pos+2 before they are inserted into hash
    // tables — matching C zstd behavior. Seeding before comparing would let a
    // position match against itself, changing semantics.
    fn pick_lazy_match(
        &self,
        abs_pos: usize,
        lit_len: usize,
        best: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        let best = best?;
        if best.match_len >= self.target_len
            || abs_pos + 1 + HC_MIN_MATCH_LEN > self.history_abs_end()
        {
            return Some(best);
        }

        let current_gain = Self::match_gain(best.match_len, best.offset) + 4;

        // Lazy check: evaluate pos+1
        let next = self.find_best_match(abs_pos + 1, lit_len + 1);
        if let Some(next) = next {
            let next_gain = Self::match_gain(next.match_len, next.offset);
            if next_gain > current_gain {
                return None;
            }
        }

        // Lazy2 check: also evaluate pos+2
        if self.lazy_depth >= 2 && abs_pos + 2 + HC_MIN_MATCH_LEN <= self.history_abs_end() {
            let next2 = self.find_best_match(abs_pos + 2, lit_len + 2);
            if let Some(next2) = next2 {
                let next2_gain = Self::match_gain(next2.match_len, next2.offset);
                // Must beat current gain + extra literal cost
                if next2_gain > current_gain + 4 {
                    return None;
                }
            }
        }

        Some(best)
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
        hc.hash_table.is_empty(),
        "HC hash_table should be released after switching away from Best"
    );
    assert!(
        hc.chain_table.is_empty(),
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
    let better_hash_len = hc.hash_table.len();
    let better_chain_len = hc.chain_table.len();

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
        hc.hash_table.len() > better_hash_len,
        "Best hash_table ({}) should be larger than Better ({})",
        hc.hash_table.len(),
        better_hash_len
    );
    assert!(
        hc.chain_table.len() > better_chain_len,
        "Best chain_table ({}) should be larger than Better ({})",
        hc.chain_table.len(),
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
                MatchGenerator::common_prefix_len(a, &b),
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
                    MatchGenerator::common_prefix_len(a, &altered),
                    scalar_reference(a, &altered),
                    "total_len={total_len} start={start} mismatch={mismatch}"
                );
            }

            if len > 0 {
                let mismatch = len - 1;
                let mut altered = b.clone();
                altered[mismatch] ^= 0xA5;
                assert_eq!(
                    MatchGenerator::common_prefix_len(a, &altered),
                    scalar_reference(a, &altered),
                    "tail mismatch total_len={total_len} start={start} mismatch={mismatch}"
                );
            }
        }
    }

    let long = alloc::vec![0xAB; 320];
    let shorter = alloc::vec![0xAB; 137];
    assert_eq!(
        MatchGenerator::common_prefix_len(&long, &shorter),
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

/// Verifies row/tag extraction uses the high bits of the multiplicative hash.
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
    const PRIME: u64 = 0x9E37_79B1_85EB_CA87;
    let hash = value.wrapping_mul(PRIME);
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
    hc.history = b"abc".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;
    hc.ensure_tables();

    let candidates = hc.chain_candidates(0);
    assert!(candidates.iter().all(|&pos| pos == usize::MAX));
}

#[test]
fn hc_reset_refills_existing_tables_with_empty_sentinel() {
    let mut hc = HcMatchGenerator::new(32);
    hc.add_data(b"abcdeabcde".to_vec(), |_| {});
    hc.ensure_tables();
    assert!(!hc.hash_table.is_empty());
    assert!(!hc.chain_table.is_empty());
    hc.hash_table.fill(123);
    hc.chain_table.fill(456);

    hc.reset(|_| {});

    assert!(hc.hash_table.iter().all(|&v| v == HC_EMPTY));
    assert!(hc.chain_table.iter().all(|&v| v == HC_EMPTY));
}

#[test]
fn hc_start_matching_returns_early_for_empty_current_block() {
    let mut hc = HcMatchGenerator::new(32);
    hc.add_data(Vec::new(), |_| {});
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

#[test]
fn hc_sparse_skip_matching_preserves_tail_cross_block_match() {
    let mut matcher = HcMatchGenerator::new(1 << 22);
    let tail = b"Qz9kLm2Rp";
    let mut first = deterministic_high_entropy_bytes(0xD1B5_4A32_9C77_0E19, 4096);
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
fn hc_sparse_skip_matching_does_not_reinsert_sparse_tail_positions() {
    let mut matcher = HcMatchGenerator::new(1 << 22);
    let first = deterministic_high_entropy_bytes(0xC2B2_AE3D_27D4_EB4F, 4096);
    matcher.add_data(first.clone(), |_| {});
    matcher.skip_matching(Some(true));

    let current_len = first.len();
    let current_abs_start = matcher.history_abs_start + matcher.window_size - current_len;
    let current_abs_end = current_abs_start + current_len;
    let dense_tail = HC_MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
    let tail_start = current_abs_end
        .saturating_sub(dense_tail)
        .max(matcher.history_abs_start)
        .max(current_abs_start);

    let overlap_pos = (tail_start..current_abs_end)
        .find(|&pos| (pos - current_abs_start).is_multiple_of(INCOMPRESSIBLE_SKIP_STEP))
        .expect("fixture should contain at least one sparse-grid overlap in dense tail");

    let rel = matcher
        .relative_position(overlap_pos)
        .expect("overlap position should be representable as relative position");
    let chain_idx = rel as usize & ((1 << matcher.chain_log) - 1);
    assert_ne!(
        matcher.chain_table[chain_idx],
        rel + 1,
        "sparse-grid tail positions must not be reinserted (self-loop chain entry)"
    );
}

#[test]
fn hc_compact_history_drains_when_threshold_crossed() {
    let mut hc = HcMatchGenerator::new(8);
    hc.history = b"abcdefghijklmnopqrstuvwxyz".to_vec();
    hc.history_start = 16;
    hc.compact_history();
    assert_eq!(hc.history_start, 0);
    assert_eq!(hc.history, b"qrstuvwxyz");
}

#[test]
fn hc_insert_position_no_rebase_returns_when_relative_pos_unavailable() {
    let mut hc = HcMatchGenerator::new(32);
    hc.history = b"abcdefghijklmnop".to_vec();
    hc.history_abs_start = 0;
    hc.position_base = 1;
    hc.ensure_tables();
    let before_hash = hc.hash_table.clone();
    let before_chain = hc.chain_table.clone();

    hc.insert_position_no_rebase(0);

    assert_eq!(hc.hash_table, before_hash);
    assert_eq!(hc.chain_table, before_chain);
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
    driver.hc_matcher_mut().max_window_size = 8;
    driver.reported_window_size = 8;

    let base_window = driver.hc_matcher().max_window_size;
    driver.prime_with_dictionary(b"abcdefghABCDEFGHijklmnop", [1, 4, 8]);
    assert_eq!(driver.hc_matcher().max_window_size, base_window + 24);

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
        driver.hc_matcher().max_window_size,
        base_window,
        "retired dictionary budget must not remain reusable for live history"
    );
}

#[test]
fn hc_rebases_positions_after_u32_boundary() {
    let mut matcher = HcMatchGenerator::new(64);
    matcher.add_data(b"abcdeabcdeabcde".to_vec(), |_| {});
    matcher.ensure_tables();
    matcher.position_base = 0;
    let history_abs_start: usize = match (u64::from(u32::MAX) + 64).try_into() {
        Ok(value) => value,
        Err(_) => return,
    };
    // Simulate a long-running stream where absolute history positions crossed
    // the u32 range. Before #51 this disabled HC inserts entirely.
    matcher.history_abs_start = history_abs_start;
    matcher.skip_matching(None);
    assert_eq!(
        matcher.position_base, matcher.history_abs_start,
        "rebase should anchor to the oldest live absolute position"
    );

    assert!(
        matcher.hash_table.iter().any(|entry| *entry != HC_EMPTY),
        "HC hash table should still be populated after crossing u32 boundary"
    );

    // Verify rebasing preserves candidate lookup, not just table population.
    let abs_pos = matcher.history_abs_start + 10;
    let candidates = matcher.chain_candidates(abs_pos);
    assert!(
        candidates.iter().any(|candidate| *candidate != usize::MAX),
        "chain_candidates should return valid matches after rebase"
    );
}

#[test]
fn hc_rebase_rebuilds_only_inserted_prefix() {
    let mut matcher = HcMatchGenerator::new(64);
    matcher.add_data(b"abcdeabcdeabcde".to_vec(), |_| {});
    matcher.ensure_tables();
    matcher.position_base = 0;
    let history_abs_start: usize = match (u64::from(u32::MAX) + 64).try_into() {
        Ok(value) => value,
        Err(_) => return,
    };
    matcher.history_abs_start = history_abs_start;
    let abs_pos = matcher.history_abs_start + 6;

    let mut expected = HcMatchGenerator::new(64);
    expected.add_data(b"abcdeabcdeabcde".to_vec(), |_| {});
    expected.ensure_tables();
    expected.history_abs_start = history_abs_start;
    expected.position_base = expected.history_abs_start;
    expected.hash_table.fill(HC_EMPTY);
    expected.chain_table.fill(HC_EMPTY);
    for pos in expected.history_abs_start..abs_pos {
        expected.insert_position_no_rebase(pos);
    }

    matcher.maybe_rebase_positions(abs_pos);

    assert_eq!(
        matcher.position_base, matcher.history_abs_start,
        "rebase should still anchor to the oldest live absolute position"
    );
    assert_eq!(
        matcher.hash_table, expected.hash_table,
        "rebase must rebuild only positions already inserted before abs_pos"
    );
    assert_eq!(
        matcher.chain_table, expected.chain_table,
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
    assert_eq!(driver.hc_matcher().lazy_depth, 2);
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
