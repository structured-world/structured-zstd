//! Matching algorithm used find repeated parts in the original data
//!
//! The Zstd format relies on finden repeated sequences of data and compressing these sequences as instructions to the decoder.
//! A sequence basically tells the decoder "Go back X bytes and copy Y bytes to the end of your decode buffer".
//!
//! The task here is to efficiently find matches in the already encoded data for the current suffix of the not yet encoded data.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
use core::arch::aarch64::{
    __crc32d, uint8x16_t, vceqq_u8, vgetq_lane_u64, vld1q_u8, vreinterpretq_u64_u8,
};
#[cfg(target_arch = "x86")]
use core::arch::x86::{
    __m128i, __m256i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm256_cmpeq_epi8,
    _mm256_loadu_si256, _mm256_movemask_epi8,
};
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{
    __m128i, __m256i, _mm_cmpeq_epi8, _mm_crc32_u64, _mm_loadu_si128, _mm_movemask_epi8,
    _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8,
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
const DFAST_SHORT_HASH_LOOKAHEAD: usize = 4;
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
const HASH_MIX_PRIME: u64 = 0x9E37_79B1_85EB_CA87;
const HC_PRIME3BYTES: u32 = 506_832_829;
const HC_PRIME4BYTES: u32 = 2_654_435_761;

const HC_HASH_LOG: usize = 20;
const HC_CHAIN_LOG: usize = 19;
const HC3_HASH_LOG: usize = 17;
const HC3_MAX_OFFSET: usize = 1 << 18;
const HC_SEARCH_DEPTH: usize = 16;
const HC_MIN_MATCH_LEN: usize = 4;
const HC_OPT_MIN_MATCH_LEN: usize = HC_FORMAT_MINMATCH;
const HC_TARGET_LEN: usize = 48;
// Positions are stored as (relative_pos + 1) so that 0 is a safe empty
// sentinel that can never collide with any valid position.
const HC_EMPTY: u32 = 0;

// Maximum search depth across all HC-based levels. Used to size the
// fixed-length candidate array returned by chain_candidates().
const MAX_HC_SEARCH_DEPTH: usize = 512;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
enum HashMixKernel {
    Scalar = 0,
    #[cfg(target_arch = "x86_64")]
    X86Sse42 = 1,
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    Aarch64Crc = 2,
}

#[inline(always)]
fn hash_mix_u64_with_kernel(value: u64, kernel: HashMixKernel) -> u64 {
    match kernel {
        HashMixKernel::Scalar => value.wrapping_mul(HASH_MIX_PRIME),
        #[cfg(target_arch = "x86_64")]
        HashMixKernel::X86Sse42 => {
            // SAFETY: runtime/static detection selected this kernel.
            unsafe { hash_mix_u64_sse42(value) }
        }
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        HashMixKernel::Aarch64Crc => {
            // SAFETY: runtime/static detection selected this kernel.
            unsafe { hash_mix_u64_crc(value) }
        }
    }
}

#[inline(always)]
fn detect_hash_mix_kernel() -> HashMixKernel {
    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    if is_x86_feature_detected!("sse4.2") {
        return HashMixKernel::X86Sse42;
    }

    #[cfg(all(feature = "std", target_arch = "aarch64", target_endian = "little"))]
    if is_aarch64_feature_detected!("crc") {
        return HashMixKernel::Aarch64Crc;
    }

    #[cfg(all(not(feature = "std"), target_arch = "x86_64"))]
    if cfg!(target_feature = "sse4.2") {
        return HashMixKernel::X86Sse42;
    }

    #[cfg(all(
        not(feature = "std"),
        target_arch = "aarch64",
        target_endian = "little"
    ))]
    if cfg!(target_feature = "crc") {
        return HashMixKernel::Aarch64Crc;
    }

    HashMixKernel::Scalar
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn hash_mix_u64_sse42(value: u64) -> u64 {
    let crc = _mm_crc32_u64(0, value);
    ((crc << 32) ^ value.rotate_left(13)).wrapping_mul(HASH_MIX_PRIME)
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[target_feature(enable = "crc")]
unsafe fn hash_mix_u64_crc(value: u64) -> u64 {
    // Feed the full 64-bit lane through ARM CRC32 and then mix back with a
    // rotated copy of the source to keep dispersion in the upper bits used by
    // hash table indexing.
    let crc = __crc32d(0, value) as u64;
    ((crc << 32) ^ value.rotate_left(17)).wrapping_mul(HASH_MIX_PRIME)
}

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

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum HcParseMode {
    Lazy2,
    BtOpt,
    BtUltra,
    BtUltra2,
}

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
                matcher.offset_hist = offset_hist;
                matcher.mark_dictionary_primed();
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
        if self.active_backend == MatcherBackend::HashChain {
            self.hc_matcher_mut()
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
    fn common_prefix_len_scalar(a: &[u8], b: &[u8], off: usize, max: usize) -> usize {
        Self::common_prefix_len_scalar_ptr(a.as_ptr(), b.as_ptr(), off, max)
    }

    #[inline(always)]
    fn common_prefix_len_scalar_ptr(
        lhs: *const u8,
        rhs: *const u8,
        mut off: usize,
        max: usize,
    ) -> usize {
        let chunk = core::mem::size_of::<usize>();
        while off + chunk <= max {
            let lhs_word = unsafe { core::ptr::read_unaligned(lhs.add(off) as *const usize) };
            let rhs_word = unsafe { core::ptr::read_unaligned(rhs.add(off) as *const usize) };
            let diff = lhs_word ^ rhs_word;
            if diff != 0 {
                return off + Self::mismatch_byte_index(diff);
            }
            off += chunk;
        }
        while off < max {
            if unsafe { *lhs.add(off) != *rhs.add(off) } {
                break;
            }
            off += 1;
        }
        off
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
    hash_mix_kernel: HashMixKernel,
    use_fast_loop: bool,
    // Lazy match lookahead depth (internal tuning parameter).
    lazy_depth: u8,
}

#[derive(Copy, Clone, Debug)]
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
            hash_mix_kernel: detect_hash_mix_kernel(),
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
            .saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN)
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
        let mut pos = 1usize;
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
        let mut pos = 1usize;
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
        let boundary_tail_start = current_len.saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN);
        let mut seed_pos = pos.min(current_len).min(boundary_tail_start);
        while seed_pos + DFAST_SHORT_HASH_LOOKAHEAD <= current_len {
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
        (hash_mix_u64_with_kernel(value, self.hash_mix_kernel) >> (64 - self.hash_bits)) as usize
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
    hash_mix_kernel: HashMixKernel,
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
            hash_mix_kernel: detect_hash_mix_kernel(),
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
        let hash = hash_mix_u64_with_kernel(value, self.hash_mix_kernel);
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
    index_shift: usize,
    offset_hist: [u32; 3],
    hash_table: Vec<u32>,
    hash3_table: Vec<u32>,
    chain_table: Vec<u32>,
    lazy_depth: u8,
    hash_log: usize,
    chain_log: usize,
    search_depth: usize,
    target_len: usize,
    parse_mode: HcParseMode,
    ldm_sequences: Vec<HcRawSeq>,
    next_to_update3: usize,
    hash3_log: usize,
    skip_insert_until_abs: usize,
    dictionary_limit_abs: Option<usize>,
    dictionary_primed_for_frame: bool,
    allow_zero_relative_position: bool,
    opt_state: HcOptState,
    opt_nodes_scratch: Vec<HcOptimalNode>,
    opt_candidates_scratch: Vec<MatchCandidate>,
    opt_store_scratch: Vec<HcOptimalNode>,
    opt_segment_plan_scratch: Vec<HcOptimalSequence>,
    opt_seed_plan_scratch: Vec<HcOptimalSequence>,
    opt_ll_price_scratch: Vec<u32>,
    opt_ll_price_generation: Vec<u32>,
    opt_ll_price_stamp: u32,
    opt_lit_price_scratch: [u32; HC_MAX_LIT + 1],
    opt_lit_price_generation: [u32; HC_MAX_LIT + 1],
    opt_lit_price_stamp: u32,
    opt_ml_price_scratch: Vec<u32>,
    opt_ml_price_generation: Vec<u32>,
    opt_ml_price_stamp: u32,
}

#[derive(Copy, Clone, Debug)]
struct HcOptimalNode {
    price: u32,
    off: u32,
    mlen: u32,
    litlen: u32,
    reps: [u32; 3],
}

impl Default for HcOptimalNode {
    fn default() -> Self {
        Self {
            price: u32::MAX,
            off: 0,
            mlen: 0,
            // Donor parity: uninitialized DP slots use litlen != 0
            // (C code uses !0) so they are never treated as end-of-match.
            litlen: u32::MAX,
            reps: [1, 4, 8],
        }
    }
}

#[derive(Copy, Clone)]
struct HcOptimalSequence {
    offset: usize,
    match_len: usize,
    lit_len: usize,
}

#[derive(Copy, Clone)]
struct HcRawSeq {
    lit_length: usize,
    offset: usize,
    match_length: usize,
}

#[derive(Copy, Clone, Default)]
struct HcRawSeqStore {
    pos: usize,
    pos_in_sequence: usize,
    size: usize,
}

#[derive(Copy, Clone)]
struct HcOptLdmState {
    seq_store: HcRawSeqStore,
    start_pos_in_block: usize,
    end_pos_in_block: usize,
    offset: usize,
}

impl Default for HcOptLdmState {
    fn default() -> Self {
        Self {
            seq_store: HcRawSeqStore::default(),
            start_pos_in_block: usize::MAX,
            end_pos_in_block: usize::MAX,
            offset: 0,
        }
    }
}

#[derive(Copy, Clone)]
struct HcCandidateQuery {
    reps: [u32; 3],
    lit_len: usize,
    ldm_candidate: Option<MatchCandidate>,
}

#[derive(Copy, Clone)]
struct HcOptimalPlanState {
    reps: [u32; 3],
    litlen: usize,
    profile: HcOptimalCostProfile,
}

struct HcOptimalPlanBuffers {
    nodes: Vec<HcOptimalNode>,
    candidates: Vec<MatchCandidate>,
    store: Vec<HcOptimalNode>,
    ll_prices: Vec<u32>,
    ll_price_generations: Vec<u32>,
    ml_prices: Vec<u32>,
    ml_price_generations: Vec<u32>,
}

#[derive(Copy, Clone)]
enum HcOptPriceType {
    Dynamic,
    Predefined,
}

#[derive(Clone)]
struct HcDictEntropySeed {
    has_lit: bool,
    has_ll: bool,
    has_ml: bool,
    has_of: bool,
    lit_bits: [u8; HC_MAX_LIT + 1],
    ll_bits: [u8; HC_MAX_LL + 1],
    ml_bits: [u8; HC_MAX_ML + 1],
    of_bits: [u8; HC_MAX_OFF + 1],
}

const HC_MAX_LIT: usize = 255;
const HC_MAX_LL: usize = 35;
const HC_MAX_ML: usize = 52;
const HC_MAX_OFF: usize = 31;
const HC_LITFREQ_ADD: u32 = 2;
const HC_PREDEF_THRESHOLD: usize = 8;
const HC_BITCOST_MULTIPLIER: u32 = 1 << 8;
const HC_BLOCKSIZE_MAX: usize = crate::common::MAX_BLOCK_SIZE as usize;
const HC_OPT_NUM: usize = 1 << 12;
const HC_FORMAT_MINMATCH: usize = 3;
#[derive(Clone)]
struct HcOptState {
    lit_freq: [u32; HC_MAX_LIT + 1],
    lit_length_freq: [u32; HC_MAX_LL + 1],
    match_length_freq: [u32; HC_MAX_ML + 1],
    off_code_freq: [u32; HC_MAX_OFF + 1],
    lit_sum: u32,
    lit_length_sum: u32,
    match_length_sum: u32,
    off_code_sum: u32,
    lit_sum_base_price: u32,
    lit_length_sum_base_price: u32,
    match_length_sum_base_price: u32,
    off_code_sum_base_price: u32,
    price_type: HcOptPriceType,
    literals_compressed: bool,
    dictionary_seed: Option<HcDictEntropySeed>,
}

impl HcOptState {
    fn new() -> Self {
        Self {
            lit_freq: [0; HC_MAX_LIT + 1],
            lit_length_freq: [0; HC_MAX_LL + 1],
            match_length_freq: [0; HC_MAX_ML + 1],
            off_code_freq: [0; HC_MAX_OFF + 1],
            lit_sum: 0,
            lit_length_sum: 0,
            match_length_sum: 0,
            off_code_sum: 0,
            lit_sum_base_price: 0,
            lit_length_sum_base_price: 0,
            match_length_sum_base_price: 0,
            off_code_sum_base_price: 0,
            price_type: HcOptPriceType::Dynamic,
            literals_compressed: true,
            dictionary_seed: None,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn bit_weight(stat: u32) -> u32 {
        let hb = 31u32.saturating_sub((stat + 1).leading_zeros());
        hb.saturating_mul(HC_BITCOST_MULTIPLIER)
    }

    fn frac_weight(raw_stat: u32) -> u32 {
        let stat = raw_stat + 1;
        let hb = 31u32.saturating_sub(stat.leading_zeros());
        let b_weight = hb.saturating_mul(HC_BITCOST_MULTIPLIER);
        let f_weight = (stat << 8) >> hb;
        b_weight.saturating_add(f_weight)
    }

    fn weight(stat: u32, accurate: bool) -> u32 {
        if accurate {
            Self::frac_weight(stat)
        } else {
            Self::bit_weight(stat)
        }
    }

    fn downscale_stats(table: &mut [u32], shift: u32, base1: bool) -> u32 {
        let mut sum = 0u32;
        for stat in table {
            let base = if base1 { 1 } else { u32::from(*stat > 0) };
            let new_stat = base + (*stat >> shift);
            *stat = new_stat;
            sum = sum.saturating_add(new_stat);
        }
        sum
    }

    fn scale_stats(table: &mut [u32], log_target: u32) -> u32 {
        let prev_sum = table.iter().copied().sum::<u32>();
        let factor = prev_sum >> log_target;
        if factor <= 1 {
            return prev_sum;
        }
        let shift = 31u32.saturating_sub(factor.leading_zeros());
        Self::downscale_stats(table, shift, true)
    }

    fn set_base_prices(&mut self, accurate: bool) {
        self.lit_sum_base_price = if self.literals_compressed() {
            Self::weight(self.lit_sum, accurate)
        } else {
            0
        };
        self.lit_length_sum_base_price = Self::weight(self.lit_length_sum, accurate);
        self.match_length_sum_base_price = Self::weight(self.match_length_sum, accurate);
        self.off_code_sum_base_price = Self::weight(self.off_code_sum, accurate);
    }

    fn literals_compressed(&self) -> bool {
        self.literals_compressed
    }

    #[cfg(test)]
    fn set_literals_compressed_for_tests(&mut self, enabled: bool) {
        self.literals_compressed = enabled;
    }

    fn seed_dictionary_entropy(
        &mut self,
        huff: Option<&crate::huff0::huff0_encoder::HuffmanTable>,
        ll: Option<&crate::fse::fse_encoder::FSETable>,
        ml: Option<&crate::fse::fse_encoder::FSETable>,
        of: Option<&crate::fse::fse_encoder::FSETable>,
    ) {
        if huff.is_none() && ll.is_none() && ml.is_none() && of.is_none() {
            self.dictionary_seed = None;
            return;
        }
        let mut lit_bits = [0u8; HC_MAX_LIT + 1];
        if let Some(huff) = huff {
            for (sym, slot) in lit_bits.iter_mut().enumerate() {
                *slot = huff.num_bits_for_symbol(sym as u8).unwrap_or(0);
            }
        }
        let mut ll_bits = [0u8; HC_MAX_LL + 1];
        if let Some(ll) = ll {
            for (sym, slot) in ll_bits.iter_mut().enumerate() {
                *slot = ll.max_num_bits_for_symbol(sym as u8).unwrap_or(0);
            }
        }
        let mut ml_bits = [0u8; HC_MAX_ML + 1];
        if let Some(ml) = ml {
            for (sym, slot) in ml_bits.iter_mut().enumerate() {
                *slot = ml.max_num_bits_for_symbol(sym as u8).unwrap_or(0);
            }
        }
        let mut of_bits = [0u8; HC_MAX_OFF + 1];
        if let Some(of) = of {
            for (sym, slot) in of_bits.iter_mut().enumerate() {
                *slot = of.max_num_bits_for_symbol(sym as u8).unwrap_or(0);
            }
        }
        self.dictionary_seed = Some(HcDictEntropySeed {
            has_lit: huff.is_some(),
            has_ll: ll.is_some(),
            has_ml: ml.is_some(),
            has_of: of.is_some(),
            lit_bits,
            ll_bits,
            ml_bits,
            of_bits,
        });
    }

    fn apply_seeded_table<const N: usize>(
        table: &mut [u32; N],
        bits: &[u8; N],
        scale_log: u8,
    ) -> u32 {
        let mut sum = 0u32;
        for (slot, &bit_cost) in table.iter_mut().zip(bits.iter()) {
            let value = if bit_cost == 0 || bit_cost >= scale_log {
                1
            } else {
                1u32 << (scale_log - bit_cost)
            };
            *slot = value;
            sum = sum.saturating_add(value);
        }
        sum
    }

    fn lit_code_and_bits(lit_len: usize) -> (usize, u32) {
        let ll = lit_len.min(131_071) as u32;
        let (code, _, extra_bits) = match ll {
            0..=15 => (ll as u8, 0, 0),
            16..=17 => (16, ll - 16, 1),
            18..=19 => (17, ll - 18, 1),
            20..=21 => (18, ll - 20, 1),
            22..=23 => (19, ll - 22, 1),
            24..=27 => (20, ll - 24, 2),
            28..=31 => (21, ll - 28, 2),
            32..=39 => (22, ll - 32, 3),
            40..=47 => (23, ll - 40, 3),
            48..=63 => (24, ll - 48, 4),
            64..=127 => (25, ll - 64, 6),
            128..=255 => (26, ll - 128, 7),
            256..=511 => (27, ll - 256, 8),
            512..=1023 => (28, ll - 512, 9),
            1024..=2047 => (29, ll - 1024, 10),
            2048..=4095 => (30, ll - 2048, 11),
            4096..=8191 => (31, ll - 4096, 12),
            8192..=16383 => (32, ll - 8192, 13),
            16384..=32767 => (33, ll - 16384, 14),
            32768..=65535 => (34, ll - 32768, 15),
            _ => (35, ll - 65536, 16),
        };
        (code as usize, extra_bits as u32)
    }

    fn ml_code_and_bits(match_len: usize) -> (usize, u32) {
        let ml = match_len.clamp(3, 131_074) as u32;
        let (code, _, extra_bits) = match ml {
            3..=34 => (ml as u8 - 3, 0, 0),
            35..=36 => (32, ml - 35, 1),
            37..=38 => (33, ml - 37, 1),
            39..=40 => (34, ml - 39, 1),
            41..=42 => (35, ml - 41, 1),
            43..=46 => (36, ml - 43, 2),
            47..=50 => (37, ml - 47, 2),
            51..=58 => (38, ml - 51, 3),
            59..=66 => (39, ml - 59, 3),
            67..=82 => (40, ml - 67, 4),
            83..=98 => (41, ml - 83, 4),
            99..=130 => (42, ml - 99, 5),
            131..=258 => (43, ml - 131, 7),
            259..=514 => (44, ml - 259, 8),
            515..=1026 => (45, ml - 515, 9),
            1027..=2050 => (46, ml - 1027, 10),
            2051..=4098 => (47, ml - 2051, 11),
            4099..=8194 => (48, ml - 4099, 12),
            8195..=16386 => (49, ml - 8195, 13),
            16387..=32770 => (50, ml - 16387, 14),
            32771..=65538 => (51, ml - 32771, 15),
            _ => (52, ml - 65539, 16),
        };
        (code as usize, extra_bits as u32)
    }

    fn rescale_freqs(&mut self, src: &[u8], profile: HcOptimalCostProfile) {
        self.price_type = HcOptPriceType::Dynamic;
        if self.lit_length_sum == 0 {
            if src.len() <= HC_PREDEF_THRESHOLD {
                self.price_type = HcOptPriceType::Predefined;
            }
            if let Some(seed) = self.dictionary_seed.take() {
                if seed.has_lit || seed.has_ll || seed.has_ml || seed.has_of {
                    self.price_type = HcOptPriceType::Dynamic;
                }
                if seed.has_lit && self.literals_compressed() {
                    self.lit_sum = Self::apply_seeded_table(&mut self.lit_freq, &seed.lit_bits, 11);
                } else {
                    if self.literals_compressed() {
                        self.lit_freq.fill(0);
                        for &byte in src {
                            self.lit_freq[byte as usize] =
                                self.lit_freq[byte as usize].saturating_add(1);
                        }
                        self.lit_sum = Self::downscale_stats(&mut self.lit_freq, 8, false);
                        if self.lit_sum == 0 {
                            self.lit_freq[0] = 1;
                            self.lit_sum = 1;
                        }
                    } else {
                        self.lit_freq.fill(0);
                        self.lit_sum = 0;
                    }
                }

                if seed.has_ll {
                    self.lit_length_sum =
                        Self::apply_seeded_table(&mut self.lit_length_freq, &seed.ll_bits, 10);
                } else {
                    let base_ll_freqs: [u32; HC_MAX_LL + 1] = [
                        4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                    ];
                    self.lit_length_freq = base_ll_freqs;
                    self.lit_length_sum = base_ll_freqs.iter().copied().sum();
                }

                if seed.has_ml {
                    self.match_length_sum =
                        Self::apply_seeded_table(&mut self.match_length_freq, &seed.ml_bits, 10);
                } else {
                    self.match_length_freq.fill(1);
                    self.match_length_sum = (HC_MAX_ML + 1) as u32;
                }

                if seed.has_of {
                    self.off_code_sum =
                        Self::apply_seeded_table(&mut self.off_code_freq, &seed.of_bits, 10);
                } else {
                    let base_off_freqs: [u32; HC_MAX_OFF + 1] = [
                        6, 2, 1, 1, 2, 3, 4, 4, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                        1, 1, 1, 1, 1, 1, 1,
                    ];
                    self.off_code_freq = base_off_freqs;
                    self.off_code_sum = base_off_freqs.iter().copied().sum();
                }
            } else {
                if self.literals_compressed() {
                    self.lit_freq.fill(0);
                    for &byte in src {
                        self.lit_freq[byte as usize] =
                            self.lit_freq[byte as usize].saturating_add(1);
                    }
                    self.lit_sum = Self::downscale_stats(&mut self.lit_freq, 8, false);
                    if self.lit_sum == 0 {
                        self.lit_freq[0] = 1;
                        self.lit_sum = 1;
                    }
                } else {
                    self.lit_freq.fill(0);
                    self.lit_sum = 0;
                }

                let base_ll_freqs: [u32; HC_MAX_LL + 1] = [
                    4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                    1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                ];
                self.lit_length_freq = base_ll_freqs;
                self.lit_length_sum = base_ll_freqs.iter().copied().sum();

                self.match_length_freq.fill(1);
                self.match_length_sum = (HC_MAX_ML + 1) as u32;

                let base_off_freqs: [u32; HC_MAX_OFF + 1] = [
                    6, 2, 1, 1, 2, 3, 4, 4, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                    1, 1, 1, 1, 1, 1,
                ];
                self.off_code_freq = base_off_freqs;
                self.off_code_sum = base_off_freqs.iter().copied().sum();
            }
        } else {
            if self.literals_compressed() {
                self.lit_sum = Self::scale_stats(&mut self.lit_freq, 12);
            }
            self.lit_length_sum = Self::scale_stats(&mut self.lit_length_freq, 11);
            self.match_length_sum = Self::scale_stats(&mut self.match_length_freq, 11);
            self.off_code_sum = Self::scale_stats(&mut self.off_code_freq, 11);
        }
        self.set_base_prices(profile.accurate);
    }

    fn update_stats(&mut self, lit_len: usize, literals: &[u8], off_base: u32, match_len: usize) {
        if self.literals_compressed() {
            for &byte in literals.iter().take(lit_len) {
                self.lit_freq[byte as usize] =
                    self.lit_freq[byte as usize].saturating_add(HC_LITFREQ_ADD);
            }
            self.lit_sum = self
                .lit_sum
                .saturating_add((lit_len as u32).saturating_mul(HC_LITFREQ_ADD));
        }

        let (ll_code, _) = Self::lit_code_and_bits(lit_len);
        self.lit_length_freq[ll_code] = self.lit_length_freq[ll_code].saturating_add(1);
        self.lit_length_sum = self.lit_length_sum.saturating_add(1);

        let off_code =
            (31u32.saturating_sub(off_base.max(1).leading_zeros()) as usize).min(HC_MAX_OFF);
        self.off_code_freq[off_code] = self.off_code_freq[off_code].saturating_add(1);
        self.off_code_sum = self.off_code_sum.saturating_add(1);

        let (ml_code, _) = Self::ml_code_and_bits(match_len);
        self.match_length_freq[ml_code] = self.match_length_freq[ml_code].saturating_add(1);
        self.match_length_sum = self.match_length_sum.saturating_add(1);
    }
}

#[derive(Copy, Clone)]
struct HcOptimalCostProfile {
    max_chain_depth: usize,
    sufficient_match_len: usize,
    accurate: bool,
    favor_small_offsets: bool,
}

impl HcOptimalCostProfile {
    fn for_mode(mode: HcParseMode, pass2: bool) -> Self {
        match mode {
            HcParseMode::Lazy2 => Self {
                max_chain_depth: 8,
                sufficient_match_len: 32,
                accurate: false,
                favor_small_offsets: true,
            },
            HcParseMode::BtOpt => Self {
                max_chain_depth: 32,
                sufficient_match_len: usize::MAX,
                accurate: false,
                favor_small_offsets: true,
            },
            HcParseMode::BtUltra => Self {
                max_chain_depth: 32,
                sufficient_match_len: usize::MAX,
                accurate: true,
                favor_small_offsets: false,
            },
            HcParseMode::BtUltra2 => {
                let _ = pass2;
                Self {
                    max_chain_depth: 512,
                    sufficient_match_len: usize::MAX,
                    accurate: true,
                    // Donor opt2 path doesn't apply the small-offset
                    // decompression-speed handicap.
                    favor_small_offsets: false,
                }
            }
        }
    }

    fn literal_price(&self, stats: &HcOptState, byte: u8) -> u32 {
        if !stats.literals_compressed() {
            return 8 * HC_BITCOST_MULTIPLIER;
        }
        if matches!(stats.price_type, HcOptPriceType::Predefined) {
            return 6 * HC_BITCOST_MULTIPLIER;
        }
        let lit_max = stats
            .lit_sum_base_price
            .saturating_sub(HC_BITCOST_MULTIPLIER);
        let mut lit_weight = HcOptState::weight(stats.lit_freq[byte as usize], self.accurate);
        if lit_weight > lit_max {
            lit_weight = lit_max;
        }
        stats.lit_sum_base_price.saturating_sub(lit_weight)
    }

    fn lit_length_price(&self, stats: &HcOptState, lit_len: usize) -> u32 {
        if lit_len == HC_BLOCKSIZE_MAX {
            // Donor parity: ZSTD_litLengthPrice() handles the non-representable
            // BLOCKSIZE_MAX literal-length by charging one extra bit over the
            // largest encodable litLength symbol.
            return HC_BITCOST_MULTIPLIER
                + self.lit_length_price(stats, HC_BLOCKSIZE_MAX.saturating_sub(1));
        }
        if matches!(stats.price_type, HcOptPriceType::Predefined) {
            return HcOptState::weight(lit_len as u32, self.accurate);
        }
        let (ll_code, ll_bits) = HcOptState::lit_code_and_bits(lit_len);
        ll_bits.saturating_mul(HC_BITCOST_MULTIPLIER) + stats.lit_length_sum_base_price
            - HcOptState::weight(stats.lit_length_freq[ll_code], self.accurate)
    }

    #[inline(always)]
    fn offset_price(&self, stats: &HcOptState, off_base: u32) -> u32 {
        let off_code = 31u32.saturating_sub(off_base.max(1).leading_zeros());
        if matches!(stats.price_type, HcOptPriceType::Predefined) {
            return (16 + off_code).saturating_mul(HC_BITCOST_MULTIPLIER);
        }
        let mut price = off_code.saturating_mul(HC_BITCOST_MULTIPLIER)
            + (stats.off_code_sum_base_price
                - HcOptState::weight(stats.off_code_freq[off_code as usize], self.accurate));
        if self.favor_small_offsets && off_code >= 20 {
            price = price.saturating_add(
                (off_code - 19)
                    .saturating_mul(2)
                    .saturating_mul(HC_BITCOST_MULTIPLIER),
            );
        }
        price
    }

    #[inline(always)]
    fn match_length_price(&self, stats: &HcOptState, match_len: usize) -> u32 {
        // Donor parity: mlBase is computed from format MINMATCH (3).
        let ml_base = match_len.saturating_sub(HC_FORMAT_MINMATCH);
        if matches!(stats.price_type, HcOptPriceType::Predefined) {
            return HcOptState::weight(ml_base as u32, self.accurate);
        }
        let (ml_code, ml_bits) = HcOptState::ml_code_and_bits(match_len);
        ml_bits.saturating_mul(HC_BITCOST_MULTIPLIER)
            + (stats.match_length_sum_base_price
                - HcOptState::weight(stats.match_length_freq[ml_code], self.accurate))
    }

    #[inline(always)]
    fn match_price_from_parts(&self, off_price: u32, ml_price: u32, stats: &HcOptState) -> u32 {
        let price = off_price.saturating_add(ml_price);
        if matches!(stats.price_type, HcOptPriceType::Predefined) {
            price
        } else {
            price.saturating_add(HC_BITCOST_MULTIPLIER / 5)
        }
    }
}

impl HcMatchGenerator {
    fn donor_opt_start_cursor_and_litlen(&self, current_abs_start: usize) -> (usize, usize) {
        let start_cursor = usize::from(current_abs_start == self.history_abs_start);
        (start_cursor, start_cursor)
    }

    fn sufficient_match_len_for_pass(&self, profile: HcOptimalCostProfile) -> usize {
        profile
            .sufficient_match_len
            .min(self.target_len)
            .clamp(HC_OPT_MIN_MATCH_LEN, HC_OPT_NUM - 1)
    }

    fn should_run_btultra2_seed_pass(&self, current_len: usize) -> bool {
        self.parse_mode == HcParseMode::BtUltra2
            && self.opt_state.lit_length_sum == 0
            && self.opt_state.dictionary_seed.is_none()
            && !self.dictionary_primed_for_frame
            && self.ldm_sequences.is_empty()
            && self.window_size == current_len
            && self.history_abs_start == 0
            && self.window.len() == 1
            && current_len > HC_PREDEF_THRESHOLD
    }

    fn mark_dictionary_primed(&mut self) {
        self.dictionary_primed_for_frame = true;
    }

    fn set_dictionary_limit_from_primed_bytes(&mut self, primed_len: usize) {
        self.dictionary_limit_abs = if primed_len == 0 {
            None
        } else {
            Some(self.history_abs_start.saturating_add(primed_len))
        };
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
            max_window_size,
            window: VecDeque::new(),
            window_size: 0,
            history: Vec::new(),
            history_start: 0,
            history_abs_start: 0,
            position_base: 0,
            index_shift: 0,
            offset_hist: [1, 4, 8],
            hash_table: Vec::new(),
            hash3_table: Vec::new(),
            chain_table: Vec::new(),
            lazy_depth: 2,
            hash_log: HC_HASH_LOG,
            chain_log: HC_CHAIN_LOG,
            search_depth: HC_SEARCH_DEPTH,
            target_len: HC_TARGET_LEN,
            parse_mode: HcParseMode::Lazy2,
            ldm_sequences: Vec::new(),
            next_to_update3: 0,
            hash3_log: HC3_HASH_LOG,
            skip_insert_until_abs: 0,
            dictionary_limit_abs: None,
            dictionary_primed_for_frame: false,
            allow_zero_relative_position: false,
            opt_state: HcOptState::new(),
            opt_nodes_scratch: Vec::new(),
            opt_candidates_scratch: Vec::new(),
            opt_store_scratch: Vec::new(),
            opt_segment_plan_scratch: Vec::new(),
            opt_seed_plan_scratch: Vec::new(),
            opt_ll_price_scratch: Vec::new(),
            opt_ll_price_generation: Vec::new(),
            opt_ll_price_stamp: 0,
            opt_lit_price_scratch: [0; HC_MAX_LIT + 1],
            opt_lit_price_generation: [0; HC_MAX_LIT + 1],
            opt_lit_price_stamp: 0,
            opt_ml_price_scratch: Vec::new(),
            opt_ml_price_generation: Vec::new(),
            opt_ml_price_stamp: 0,
        }
    }

    fn configure(&mut self, config: HcConfig, window_log: u8) {
        let resize = self.hash_log != config.hash_log || self.chain_log != config.chain_log;
        self.hash_log = config.hash_log;
        self.chain_log = config.chain_log;
        self.hash3_log = if config.parse_mode == HcParseMode::BtUltra2 {
            HC3_HASH_LOG.min(window_log as usize)
        } else {
            0
        };
        self.search_depth = if matches!(
            config.parse_mode,
            HcParseMode::BtOpt | HcParseMode::BtUltra | HcParseMode::BtUltra2
        ) {
            config.search_depth
        } else {
            config.search_depth.min(MAX_HC_SEARCH_DEPTH)
        };
        self.target_len = config.target_len;
        self.parse_mode = config.parse_mode;
        if resize && !self.hash_table.is_empty() {
            // Force reallocation on next ensure_tables() call.
            self.hash_table.clear();
            self.hash3_table.clear();
            self.chain_table.clear();
        }
    }

    fn seed_dictionary_entropy(
        &mut self,
        huff: Option<&crate::huff0::huff0_encoder::HuffmanTable>,
        ll: Option<&crate::fse::fse_encoder::FSETable>,
        ml: Option<&crate::fse::fse_encoder::FSETable>,
        of: Option<&crate::fse::fse_encoder::FSETable>,
    ) {
        self.opt_state.seed_dictionary_entropy(huff, ll, ml, of);
    }

    fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        self.window_size = 0;
        self.history.clear();
        self.history_start = 0;
        self.history_abs_start = 0;
        self.position_base = 0;
        self.index_shift = 0;
        self.offset_hist = [1, 4, 8];
        self.ldm_sequences.clear();
        self.next_to_update3 = 0;
        self.skip_insert_until_abs = 0;
        self.dictionary_limit_abs = None;
        self.dictionary_primed_for_frame = false;
        self.allow_zero_relative_position = false;
        self.opt_state.reset();
        self.opt_nodes_scratch.clear();
        self.opt_candidates_scratch.clear();
        self.opt_store_scratch.clear();
        self.opt_segment_plan_scratch.clear();
        self.opt_seed_plan_scratch.clear();
        self.opt_ll_price_scratch.clear();
        self.opt_ll_price_generation.clear();
        self.opt_ll_price_stamp = 0;
        self.opt_lit_price_scratch = [0; HC_MAX_LIT + 1];
        self.opt_lit_price_generation = [0; HC_MAX_LIT + 1];
        self.opt_lit_price_stamp = 0;
        self.opt_ml_price_scratch.clear();
        self.opt_ml_price_generation.clear();
        self.opt_ml_price_stamp = 0;
        if !self.hash_table.is_empty() {
            self.hash_table.fill(HC_EMPTY);
            self.hash3_table.fill(HC_EMPTY);
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
        self.next_to_update3 = self.next_to_update3.max(self.history_abs_start);
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
    fn backfill_boundary_positions(&mut self, current_abs_start: usize, current_abs_end: usize) {
        let backfill_start = current_abs_start
            .saturating_sub(3)
            .max(self.history_abs_start);
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
        let gap = current_abs_start.saturating_sub(self.skip_insert_until_abs);
        if gap > 384 {
            self.skip_insert_until_abs = current_abs_start - (gap - 384).min(192);
        }
    }

    fn skip_matching(&mut self, incompressible_hint: Option<bool>) {
        self.ensure_tables();
        let current_len = self.window.back().unwrap().len();
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
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

    fn bt_insert_sparse_incompressible_block(
        &mut self,
        current_abs_start: usize,
        current_abs_end: usize,
    ) {
        let mut pos = current_abs_start;
        while pos < current_abs_end {
            self.maybe_rebase_positions(pos);
            let _ = self.bt_insert_step_no_rebase(pos, current_abs_end, current_abs_end);
            self.insert_hash3_only_no_rebase(pos);
            let next = pos.saturating_add(INCOMPRESSIBLE_SKIP_STEP);
            if next <= pos {
                break;
            }
            pos = next;
        }

        let dense_tail = HC_MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
        let tail_start = current_abs_end
            .saturating_sub(dense_tail)
            .max(self.history_abs_start)
            .max(current_abs_start);
        for pos in tail_start..current_abs_end {
            if (pos - current_abs_start).is_multiple_of(INCOMPRESSIBLE_SKIP_STEP) {
                continue;
            }
            self.maybe_rebase_positions(pos);
            let _ = self.bt_insert_step_no_rebase(pos, current_abs_end, current_abs_end);
            self.insert_hash3_only_no_rebase(pos);
        }

        self.skip_insert_until_abs = self.skip_insert_until_abs.max(current_abs_end);
        self.next_to_update3 = self.next_to_update3.max(current_abs_end);
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
        self.ensure_tables();

        let current_len = self.window.back().unwrap().len();
        if current_len == 0 {
            return;
        }

        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        self.backfill_boundary_positions(current_abs_start, current_abs_end);

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

    fn start_matching_optimal(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        self.ensure_tables();
        let current_len = self.window.back().unwrap().len();
        if current_len == 0 {
            return;
        }
        let current_ptr = self.window.back().unwrap().as_ptr();
        // `start_matching_optimal()` mutates tables/state but never mutates or
        // reorders `self.window`, so this block slice remains valid for the
        // duration of the routine and avoids cloning the full block.
        let current = unsafe { core::slice::from_raw_parts(current_ptr, current_len) };

        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        self.apply_limited_update_after_long_match(current_abs_start);
        let hash3_start_cursor = self.skip_insert_until_abs.max(self.history_abs_start);
        self.backfill_boundary_positions(current_abs_start, current_abs_end);
        self.next_to_update3 = hash3_start_cursor;
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
        let mut opt_state = core::mem::replace(&mut self.opt_state, HcOptState::new());
        opt_state.rescale_freqs(current, profile);
        let mut best_plan = core::mem::take(&mut self.opt_segment_plan_scratch);
        best_plan.clear();
        let mut plan_reps = self.offset_hist;
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
        self.opt_segment_plan_scratch = best_plan;
        self.opt_state = opt_state;
    }

    fn run_btultra2_seed_pass(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
    ) {
        let seed_profile = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, true);
        let mut opt_state = core::mem::replace(&mut self.opt_state, HcOptState::new());
        opt_state.rescale_freqs(current, seed_profile);
        let mut seed_reps = self.offset_hist;
        let (mut cursor, mut seed_litlen) =
            self.donor_opt_start_cursor_and_litlen(current_abs_start);
        let mut seed_literals_cursor = 0usize;
        let mut seed_plan = core::mem::take(&mut self.opt_seed_plan_scratch);
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
        self.opt_seed_plan_scratch = seed_plan;
        self.opt_state = opt_state;

        // Donor initStats_ultra keeps the collected entropy statistics but
        // invalidates the first-pass matchfinder history before the real pass.
        self.position_base = self.history_abs_start;
        self.index_shift = current_len;
        self.next_to_update3 = current_abs_start;
        self.skip_insert_until_abs = current_abs_start;
        // Donor `ZSTD_initStats_ultra()` invalidates the first scan by moving
        // `window.base` back by `srcSize`, making the real pass start at
        // `curr == srcSize` instead of 0. Position 0 is therefore a valid
        // table entry in the second pass even though raw C tables reserve
        // value 0 as empty during an unshifted first pass.
        self.allow_zero_relative_position = true;
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
            let start = literals_start.saturating_add(item.lit_len);
            if start < *literals_start || start + item.match_len > current_len {
                continue;
            }
            let literals = &current[*literals_start..start];
            let (off_base, next_reps) =
                Self::encode_offset_with_reps(item.offset as u32, literals.len(), *reps);
            opt_state.update_stats(literals.len(), literals, off_base, item.match_len);
            *reps = next_reps;
            *literals_start = start + item.match_len;
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
        let current_abs_end = current_abs_start + current_len;
        let min_match_len = HC_OPT_MIN_MATCH_LEN;
        let frontier_limit = current_len.min(HC_OPT_NUM.saturating_sub(1));
        let initial_reps = initial_state.reps;
        let initial_litlen = initial_state.litlen;
        let mut profile = initial_state.profile;
        profile.sufficient_match_len = self.sufficient_match_len_for_pass(profile);
        let mut nodes = core::mem::take(&mut self.opt_nodes_scratch);
        if nodes.len() < frontier_limit.saturating_add(2) {
            nodes.resize(frontier_limit.saturating_add(2), HcOptimalNode::default());
        }
        let mut candidates = core::mem::take(&mut self.opt_candidates_scratch);
        candidates.clear();
        if candidates.capacity() < MAX_HC_SEARCH_DEPTH {
            candidates.reserve_exact(MAX_HC_SEARCH_DEPTH - candidates.capacity());
        }
        let mut store = core::mem::take(&mut self.opt_store_scratch);
        store.clear();
        let mut ll_prices = core::mem::take(&mut self.opt_ll_price_scratch);
        let mut ll_price_generations = core::mem::take(&mut self.opt_ll_price_generation);
        if ll_prices.len() <= frontier_limit {
            ll_prices.resize(frontier_limit + 1, 0);
            ll_price_generations.resize(frontier_limit + 1, 0);
        }
        self.opt_ll_price_stamp = self.opt_ll_price_stamp.wrapping_add(1).max(1);
        let ll_price_stamp = self.opt_ll_price_stamp;
        self.opt_lit_price_stamp = self.opt_lit_price_stamp.wrapping_add(1).max(1);
        let lit_price_stamp = self.opt_lit_price_stamp;
        let mut ml_prices = core::mem::take(&mut self.opt_ml_price_scratch);
        let mut ml_price_generations = core::mem::take(&mut self.opt_ml_price_generation);
        if ml_prices.len() <= frontier_limit {
            ml_prices.resize(frontier_limit + 1, 0);
            ml_price_generations.resize(frontier_limit + 1, 0);
        }
        self.opt_ml_price_stamp = self.opt_ml_price_stamp.wrapping_add(1).max(1);
        let ml_price_stamp = self.opt_ml_price_stamp;
        nodes[0] = HcOptimalNode {
            price: Self::cached_lit_length_price(
                profile,
                stats,
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
        let ll0_price = Self::cached_lit_length_price(
            profile,
            stats,
            0,
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
                size: self.ldm_sequences.len(),
            },
            ..HcOptLdmState::default()
        };
        let has_ldm = !self.ldm_sequences.is_empty();
        if has_ldm {
            self.ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 0, current_len);
        }

        // Donor-like seed at rPos=0: initialize frontier with matches starting
        // at current position before entering the generic forward DP loop.
        if current_len >= min_match_len {
            let seed_ldm = if has_ldm {
                self.ldm_process_match_candidate(&mut opt_ldm, 0, current_len, min_match_len)
            } else {
                None
            };
            candidates.clear();
            self.collect_optimal_candidates(
                current_abs_start,
                current_abs_end,
                profile,
                HcCandidateQuery {
                    reps: initial_reps,
                    lit_len: initial_litlen,
                    ldm_candidate: seed_ldm,
                },
                &mut candidates,
            );
            if !candidates.is_empty() {
                last_pos = min_match_len.saturating_sub(1).min(frontier_limit);
                for p in 1..min_match_len.min(nodes.len()) {
                    Self::reset_opt_node(&mut nodes[p]);
                    nodes[p].litlen = initial_litlen.saturating_add(p) as u32;
                }
            }

            if let Some(candidate) = candidates.last() {
                let longest_len = candidate.match_len.min(current_len);
                if longest_len > sufficient_len {
                    let off_base = Self::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        initial_litlen,
                        initial_reps,
                    );
                    let off_price = profile.offset_price(stats, off_base);
                    let ml_price = Self::cached_match_length_price(
                        profile,
                        stats,
                        longest_len,
                        &mut ml_prices,
                        &mut ml_price_generations,
                        ml_price_stamp,
                    );
                    let seq_cost = ll0_price
                        .saturating_add(profile.match_price_from_parts(off_price, ml_price, stats));
                    let forced_price = nodes[0].price.saturating_add(seq_cost);
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
                        Self::reset_opt_nodes(&mut nodes, last_pos + 1, max_match_len);
                    }
                    let off_base = Self::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        initial_litlen,
                        initial_reps,
                    );
                    let off_price = profile.offset_price(stats, off_base);
                    for match_len in (start_len..=max_match_len).rev() {
                        let ml_price = Self::cached_match_length_price(
                            profile,
                            stats,
                            match_len,
                            &mut ml_prices,
                            &mut ml_price_generations,
                            ml_price_stamp,
                        );
                        let seq_cost = ll0_price.saturating_add(
                            profile.match_price_from_parts(off_price, ml_price, stats),
                        );
                        let next_cost = nodes[0].price.saturating_add(seq_cost);
                        if match_len > last_pos || next_cost < nodes[match_len].price {
                            nodes[match_len] = HcOptimalNode {
                                price: next_cost,
                                off: candidate.offset as u32,
                                mlen: match_len as u32,
                                litlen: 0,
                                reps: initial_reps,
                            };
                            if match_len > last_pos {
                                last_pos = match_len;
                            }
                        } else if self.parse_mode == HcParseMode::BtOpt {
                            break;
                        }
                    }
                    prev_max_len = prev_max_len.max(max_match_len);
                }
            }
        }
        while !seed_forced_shortest_path && pos <= last_pos && pos <= frontier_limit {
            // Donor order: fix opt[cur] from opt[cur-1] with one literal.
            let prev_node = nodes[pos - 1];
            if prev_node.price != u32::MAX {
                let lit_len = prev_node.litlen as usize + 1;
                let lit_price = Self::cached_literal_price(
                    profile,
                    stats,
                    current[pos - 1],
                    &mut self.opt_lit_price_scratch,
                    &mut self.opt_lit_price_generation,
                    lit_price_stamp,
                );
                let ll_delta = Self::cached_lit_length_price(
                    profile,
                    stats,
                    lit_len,
                    &mut ll_prices,
                    &mut ll_price_generations,
                    ll_price_stamp,
                ) as i64
                    - Self::cached_lit_length_price(
                        profile,
                        stats,
                        prev_node.litlen as usize,
                        &mut ll_prices,
                        &mut ll_price_generations,
                        ll_price_stamp,
                    ) as i64;
                let lit_cost = (prev_node.price as i64)
                    .saturating_add(lit_price as i64)
                    .saturating_add(ll_delta)
                    .clamp(0, u32::MAX as i64) as u32;
                if lit_cost <= nodes[pos].price {
                    let prev_match = nodes[pos];
                    nodes[pos] = prev_node;
                    nodes[pos].litlen = lit_len as u32;
                    nodes[pos].price = lit_cost;
                    let opt_level = matches!(
                        self.parse_mode,
                        HcParseMode::BtUltra | HcParseMode::BtUltra2
                    );
                    if opt_level
                        && prev_match.mlen > 0
                        && prev_match.litlen == 0
                        && pos < current_len
                    {
                        let ll0 = Self::cached_lit_length_price(
                            profile,
                            stats,
                            0,
                            &mut ll_prices,
                            &mut ll_price_generations,
                            ll_price_stamp,
                        );
                        let ll1 = Self::cached_lit_length_price(
                            profile,
                            stats,
                            1,
                            &mut ll_prices,
                            &mut ll_price_generations,
                            ll_price_stamp,
                        );
                        if ll1 < ll0 {
                            let with1literal = (prev_match.price as i64)
                                .saturating_add(Self::cached_literal_price(
                                    profile,
                                    stats,
                                    current[pos],
                                    &mut self.opt_lit_price_scratch,
                                    &mut self.opt_lit_price_generation,
                                    lit_price_stamp,
                                ) as i64)
                                .saturating_add(ll1 as i64 - ll0 as i64)
                                .clamp(0, u32::MAX as i64)
                                as u32;
                            let next_ll = Self::cached_lit_length_price(
                                profile,
                                stats,
                                lit_len.saturating_add(1),
                                &mut ll_prices,
                                &mut ll_price_generations,
                                ll_price_stamp,
                            );
                            let cur_ll = Self::cached_lit_length_price(
                                profile,
                                stats,
                                lit_len,
                                &mut ll_prices,
                                &mut ll_price_generations,
                                ll_price_stamp,
                            );
                            let with_more_literals = (lit_cost as i64)
                                .saturating_add(Self::cached_literal_price(
                                    profile,
                                    stats,
                                    current[pos],
                                    &mut self.opt_lit_price_scratch,
                                    &mut self.opt_lit_price_generation,
                                    lit_price_stamp,
                                ) as i64)
                                .saturating_add(next_ll as i64 - cur_ll as i64)
                                .clamp(0, u32::MAX as i64)
                                as u32;
                            let next = pos + 1;
                            if with1literal < with_more_literals && with1literal < nodes[next].price
                            {
                                let prev_pos = pos.saturating_sub(prev_match.mlen as usize);
                                if prev_pos <= pos {
                                    let prev_state = nodes[prev_pos];
                                    let (_, reps_after_match) = Self::encode_offset_with_reps(
                                        prev_match.off,
                                        prev_state.litlen as usize,
                                        prev_state.reps,
                                    );
                                    nodes[next] = prev_match;
                                    nodes[next].reps = reps_after_match;
                                    nodes[next].litlen = 1;
                                    nodes[next].price = with1literal;
                                    if next > last_pos {
                                        last_pos = next;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let mut base_node = nodes[pos];
            if base_node.price == u32::MAX {
                pos += 1;
                continue;
            }
            if base_node.mlen > 0 && base_node.litlen == 0 {
                let prev_pos = pos.saturating_sub(base_node.mlen as usize);
                if prev_pos <= pos {
                    let prev_state = nodes[prev_pos];
                    let (_, reps_after_match) = Self::encode_offset_with_reps(
                        base_node.off,
                        prev_state.litlen as usize,
                        prev_state.reps,
                    );
                    base_node.reps = reps_after_match;
                    nodes[pos].reps = reps_after_match;
                }
            }
            let base_cost = base_node.price;

            // Donor guard (`inr > ilimit` where `ilimit = iend - 8`):
            // near the block tail, skip expensive match probing and keep
            // literal-path progression only.
            if pos + 8 > current_len {
                pos += 1;
                continue;
            }

            // Donor guard: if we're at the current frontier, there's no
            // cheaper path to probe beyond this position yet.
            if pos == last_pos {
                break;
            }

            // donor-style early skip for opt-level path: if next literal-only
            // state is already nearly as good, skip expensive match probing.
            if self.parse_mode == HcParseMode::BtOpt
                && nodes[pos + 1].price <= base_cost.saturating_add(HC_BITCOST_MULTIPLIER / 2)
            {
                pos += 1;
                continue;
            }

            let abs_pos = current_abs_start + pos;
            let ldm_candidate = if has_ldm {
                self.ldm_process_match_candidate(
                    &mut opt_ldm,
                    pos,
                    current_len - pos,
                    min_match_len,
                )
            } else {
                None
            };
            candidates.clear();
            self.collect_optimal_candidates(
                abs_pos,
                current_abs_end,
                profile,
                HcCandidateQuery {
                    reps: base_node.reps,
                    lit_len: base_node.litlen as usize,
                    ldm_candidate,
                },
                &mut candidates,
            );
            let mut prev_max_len = min_match_len.saturating_sub(1);
            for candidate in candidates.iter() {
                let max_match_len = candidate
                    .match_len
                    .min(current_len - pos)
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
                    Self::reset_opt_nodes(&mut nodes, last_pos + 1, max_next);
                }
                let lit_len = base_node.litlen as usize;
                let off_base = Self::encode_offset_base_with_reps(
                    candidate.offset as u32,
                    lit_len,
                    base_node.reps,
                );
                let off_price = profile.offset_price(stats, off_base);
                // donor-style descending ML scan from best length to minimum.
                // For faster btopt-like mode (optLevel==0 equivalent) we stop
                // early once shorter lengths stop improving the current best.
                for match_len in (start_len..=max_match_len).rev() {
                    let next = pos + match_len;
                    let ml_price = Self::cached_match_length_price(
                        profile,
                        stats,
                        match_len,
                        &mut ml_prices,
                        &mut ml_price_generations,
                        ml_price_stamp,
                    );
                    let seq_cost = ll0_price
                        .saturating_add(profile.match_price_from_parts(off_price, ml_price, stats));
                    let next_cost = base_cost.saturating_add(seq_cost);
                    let improved = next > last_pos || next_cost < nodes[next].price;
                    if improved {
                        nodes[next] = HcOptimalNode {
                            price: next_cost,
                            off: candidate.offset as u32,
                            mlen: match_len as u32,
                            litlen: 0,
                            reps: base_node.reps,
                        };
                        if next > last_pos {
                            last_pos = next;
                        }
                    } else if self.parse_mode == HcParseMode::BtOpt {
                        break;
                    }
                }
                prev_max_len = prev_max_len.max(max_match_len);
            }

            if let Some(candidate) = candidates.last() {
                let longest_len = candidate.match_len.min(current_len - pos);
                if longest_len > sufficient_len
                    || pos + longest_len >= HC_OPT_NUM
                    || pos + longest_len >= current_len
                {
                    let lit_len = base_node.litlen as usize;
                    let off_base = Self::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        lit_len,
                        base_node.reps,
                    );
                    let off_price = profile.offset_price(stats, off_base);
                    let ml_price = Self::cached_match_length_price(
                        profile,
                        stats,
                        longest_len,
                        &mut ml_prices,
                        &mut ml_price_generations,
                        ml_price_stamp,
                    );
                    let seq_cost = ll0_price
                        .saturating_add(profile.match_price_from_parts(off_price, ml_price, stats));
                    let forced_price = base_cost.saturating_add(seq_cost);
                    let end_pos = (pos + longest_len).min(current_len);
                    let forced_state = HcOptimalNode {
                        price: forced_price,
                        off: candidate.offset as u32,
                        mlen: longest_len as u32,
                        litlen: 0,
                        reps: base_node.reps,
                    };
                    if end_pos < nodes.len() && forced_price < nodes[end_pos].price {
                        nodes[end_pos] = forced_state;
                    }
                    forced_end = Some(end_pos);
                    forced_end_state = Some(forced_state);
                    break;
                }
            }

            pos += 1;
        }

        if last_pos == 0 {
            if current_len == 0 {
                let price = nodes[0].price;
                return self.finish_optimal_plan(
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
            let lit_price = Self::cached_literal_price(
                profile,
                stats,
                current[0],
                &mut self.opt_lit_price_scratch,
                &mut self.opt_lit_price_generation,
                lit_price_stamp,
            );
            let next_litlen = initial_litlen.saturating_add(1);
            let ll_delta = Self::cached_lit_length_price(
                profile,
                stats,
                next_litlen,
                &mut ll_prices,
                &mut ll_price_generations,
                ll_price_stamp,
            ) as i64
                - Self::cached_lit_length_price(
                    profile,
                    stats,
                    initial_litlen,
                    &mut ll_prices,
                    &mut ll_price_generations,
                    ll_price_stamp,
                ) as i64;
            let price = (nodes[0].price as i64)
                .saturating_add(lit_price as i64)
                .saturating_add(ll_delta)
                .clamp(0, u32::MAX as i64) as u32;
            return self.finish_optimal_plan(
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

        // Donor parity: shortest-path commits up to the current frontier
        // (`last_pos`), not necessarily to the full segment end.
        let target_pos = forced_end.unwrap_or(last_pos.min(frontier_limit));
        let last_stretch = if let Some(forced_state) = forced_end_state {
            forced_state
        } else {
            nodes[target_pos]
        };
        if last_stretch.price == u32::MAX {
            return self.finish_optimal_plan(
                HcOptimalPlanBuffers {
                    nodes,
                    candidates,
                    store,
                    ll_prices,
                    ll_price_generations,
                    ml_prices,
                    ml_price_generations,
                },
                (u32::MAX, initial_reps, initial_litlen, current_len),
            );
        }

        if last_stretch.mlen == 0 {
            return self.finish_optimal_plan(
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
                    target_pos.min(current_len),
                ),
            );
        }

        let mut cur = target_pos.saturating_sub(last_stretch.mlen as usize);
        let end_reps = if last_stretch.litlen == 0 {
            let prev_state = nodes[cur];
            let (_, reps_after_match) = Self::encode_offset_with_reps(
                last_stretch.off,
                prev_state.litlen as usize,
                prev_state.reps,
            );
            reps_after_match
        } else {
            let tail_literals = last_stretch.litlen as usize;
            if cur < tail_literals {
                return self.finish_optimal_plan(
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
                        target_pos.min(current_len),
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

        // Donor stores stretches as "match followed by literals" during the
        // forward pass, then rewrites the selected path in-place into
        // "literals followed by match" sequences. If the last stretch carries
        // trailing literals, donor materializes a final literal-only entry and
        // leaves those bytes pending for the next match loop segment.
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
        for stretch in store.iter().take(store_end + 1).skip(store_start) {
            let llen = stretch.litlen as usize;
            let mlen = stretch.mlen as usize;
            if mlen == 0 {
                tail_literals = llen;
                continue;
            }
            out.push(HcOptimalSequence {
                offset: stretch.off as usize,
                match_len: mlen,
                lit_len: llen,
            });
            tail_literals = 0;
        }
        let result = (
            last_stretch.price,
            end_reps,
            if last_stretch.litlen > 0 {
                last_stretch.litlen as usize
            } else {
                tail_literals
            },
            target_pos.min(current_len),
        );
        self.finish_optimal_plan(
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
        self.opt_nodes_scratch = nodes;
        self.opt_candidates_scratch = candidates;
        self.opt_store_scratch = store;
        self.opt_ll_price_scratch = ll_prices;
        self.opt_ll_price_generation = ll_price_generations;
        self.opt_ml_price_scratch = ml_prices;
        self.opt_ml_price_generation = ml_price_generations;
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
        node.mlen = 0;
        node.litlen = u32::MAX;
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
        let idx = byte as usize;
        if generations[idx] == stamp {
            return prices[idx];
        }
        let price = profile.literal_price(stats, byte);
        prices[idx] = price;
        generations[idx] = stamp;
        price
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
        if generations[lit_len] == stamp {
            return prices[lit_len];
        }
        let price = profile.lit_length_price(stats, lit_len);
        prices[lit_len] = price;
        generations[lit_len] = stamp;
        price
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
        if generations[match_len] == stamp {
            return prices[match_len];
        }
        let price = profile.match_length_price(stats, match_len);
        prices[match_len] = price;
        generations[match_len] = stamp;
        price
    }

    fn collect_optimal_candidates(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        self.ensure_tables();
        let min_match_len = HC_OPT_MIN_MATCH_LEN;
        let reps = query.reps;
        let lit_len = query.lit_len;
        let ldm_candidate = query.ldm_candidate;
        out.clear();
        if abs_pos < self.skip_insert_until_abs {
            if let Some(ldm) = ldm_candidate {
                let mut best_len_for_skip = 0usize;
                let _ =
                    Self::push_candidate_ladder(out, &mut best_len_for_skip, ldm, min_match_len);
            }
            return;
        }
        if self.uses_bt_matchfinder() {
            self.bt_update_tree_until(abs_pos, current_abs_end);
        }
        let current_idx = abs_pos - self.history_abs_start;
        // Hash-based candidate lookup reads 4-byte prefixes.
        if current_idx + 4 > self.live_history().len() {
            if let Some(ldm) = ldm_candidate {
                let mut best_len_for_skip = 0usize;
                let _ =
                    Self::push_candidate_ladder(out, &mut best_len_for_skip, ldm, min_match_len);
            }
            return;
        }
        let mut best_len_for_skip = 0usize;
        let mut skip_further_match_search = false;
        let mut rep_len_candidate_found = false;
        self.for_each_repcode_candidate_with_reps(
            abs_pos,
            lit_len,
            reps,
            current_abs_end,
            min_match_len,
            |rep| {
                if rep.match_len >= min_match_len {
                    rep_len_candidate_found = true;
                }
                let _ =
                    Self::push_candidate_ladder(out, &mut best_len_for_skip, rep, min_match_len);
                if rep.match_len > profile.sufficient_match_len {
                    skip_further_match_search = true;
                }
                if abs_pos.saturating_add(rep.match_len) >= current_abs_end {
                    skip_further_match_search = true;
                }
            },
        );
        if !skip_further_match_search && best_len_for_skip < min_match_len {
            self.update_hash3_until(abs_pos);
            if let Some(hash3_candidate) =
                self.hash3_candidate(abs_pos, current_abs_end, min_match_len)
            {
                let _ = Self::push_candidate_ladder(
                    out,
                    &mut best_len_for_skip,
                    hash3_candidate,
                    min_match_len,
                );
                if !rep_len_candidate_found
                    && (hash3_candidate.match_len > profile.sufficient_match_len
                        || abs_pos.saturating_add(hash3_candidate.match_len) >= current_abs_end)
                {
                    self.skip_insert_until_abs = abs_pos.saturating_add(1);
                    skip_further_match_search = true;
                }
            }
        }
        if !skip_further_match_search && self.uses_bt_matchfinder() {
            self.bt_insert_and_collect_matches(
                abs_pos,
                current_abs_end,
                profile,
                min_match_len,
                &mut best_len_for_skip,
                out,
            );
        } else if !skip_further_match_search {
            self.insert_position(abs_pos);
            let max_chain_depth = profile.max_chain_depth.min(self.search_depth);
            let concat = &self.history[self.history_start..];
            // Donor model:
            //   bestLength starts from already found rep/hash3 candidates,
            //   matchEndIdx starts at curr + 8 + 1,
            //   and after BT walk nextToUpdate = matchEndIdx - 8.
            let mut match_end_abs = abs_pos.saturating_add(9);
            if max_chain_depth > 0 {
                for (visited, candidate_abs) in
                    self.chain_candidates(abs_pos).into_iter().enumerate()
                {
                    if visited >= max_chain_depth {
                        break;
                    }
                    if candidate_abs == usize::MAX {
                        break;
                    }
                    if candidate_abs < self.history_abs_start || candidate_abs >= abs_pos {
                        continue;
                    }
                    let candidate_idx = candidate_abs - self.history_abs_start;
                    let tail_limit = current_abs_end.saturating_sub(abs_pos);
                    let base = concat.as_ptr();
                    let match_len = MatchGenerator::common_prefix_len_scalar_ptr(
                        unsafe { base.add(candidate_idx) },
                        unsafe { base.add(current_idx) },
                        0,
                        tail_limit,
                    );
                    if match_len < min_match_len {
                        continue;
                    }
                    let offset = abs_pos - candidate_abs;
                    if Self::push_candidate_ladder(
                        out,
                        &mut best_len_for_skip,
                        MatchCandidate {
                            start: abs_pos,
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
                        || abs_pos.saturating_add(match_len) >= current_abs_end
                    {
                        break;
                    }
                }
            }
            // Donor parity: always carry forward nextToUpdate = matchEndIdx - 8
            // after BT insertion/match walk (non-rep/hash3-early-return path).
            self.skip_insert_until_abs = self
                .skip_insert_until_abs
                .max(match_end_abs.saturating_sub(8));
        }
        if let Some(ldm) = ldm_candidate {
            let _ = Self::push_candidate_ladder(out, &mut best_len_for_skip, ldm, min_match_len);
        }
    }

    fn push_candidate_ladder(
        out: &mut Vec<MatchCandidate>,
        best_len_for_skip: &mut usize,
        candidate: MatchCandidate,
        min_match_len: usize,
    ) -> bool {
        if candidate.match_len < min_match_len {
            return false;
        }
        if candidate.match_len > *best_len_for_skip {
            out.push(candidate);
            *best_len_for_skip = candidate.match_len;
            return true;
        }
        false
    }

    fn ldm_skip_raw_seq_store_bytes(&self, seq_store: &mut HcRawSeqStore, nb_bytes: usize) {
        let mut curr_pos = seq_store.pos_in_sequence.saturating_add(nb_bytes);
        while curr_pos > 0 && seq_store.pos < seq_store.size {
            let curr_seq = self.ldm_sequences[seq_store.pos];
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
        let curr_seq = self.ldm_sequences[opt_ldm.seq_store.pos];
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
        self.ldm_sequences.clear();
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
        let current = self.window.back().unwrap().as_slice();
        if plan.is_empty() {
            handle_sequence(Sequence::Literals { literals: current });
            return;
        }

        let mut literals_start = 0usize;
        for item in plan {
            let start = literals_start.saturating_add(item.lit_len);
            if start < literals_start || start + item.match_len > current_len {
                continue;
            }
            let literals = &current[literals_start..start];
            handle_sequence(Sequence::Triple {
                literals,
                offset: item.offset,
                match_len: item.match_len,
            });
            encode_offset_with_history(
                item.offset as u32,
                literals.len() as u32,
                &mut self.offset_hist,
            );
            literals_start = start + item.match_len;
        }

        if literals_start < current_len {
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    fn ensure_tables(&mut self) {
        if self.hash_table.is_empty() {
            self.hash_table = alloc::vec![HC_EMPTY; 1 << self.hash_log];
            self.hash3_table = alloc::vec![HC_EMPTY; 1 << HC3_HASH_LOG];
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

    fn uses_bt_matchfinder(&self) -> bool {
        matches!(
            self.parse_mode,
            HcParseMode::BtOpt | HcParseMode::BtUltra | HcParseMode::BtUltra2
        )
    }

    fn bt_log(&self) -> usize {
        self.chain_log.saturating_sub(1)
    }

    fn bt_mask(&self) -> usize {
        (1usize << self.bt_log()) - 1
    }

    fn bt_pair_index_for_abs(&self, abs_pos: usize) -> usize {
        2 * (abs_pos.saturating_add(self.index_shift) & self.bt_mask())
    }

    #[inline(always)]
    fn stored_abs_position_fast(
        stored: u32,
        position_base: usize,
        index_shift: usize,
    ) -> Option<usize> {
        if stored == HC_EMPTY {
            return None;
        }
        let shifted = position_base + (stored as usize - 1);
        if shifted < index_shift {
            return None;
        }
        Some(shifted - index_shift)
    }

    fn window_low_abs_for_target(&self, target_abs: usize) -> usize {
        let history_low = self.history_abs_start;
        let window_low = target_abs.saturating_sub(self.max_window_size);
        history_low.max(window_low)
    }

    fn bt_hash_mls(&self) -> usize {
        // Donor parity: even when `minMatch == 3` (btultra2), the main BT/HC
        // hash still goes through `ZSTD_hashPtr(..., mls)` which falls back to
        // the default `case 4` in `zstd_compress_internal.h`. The 3-byte path
        // is a separate HC3 side table only.
        4
    }

    #[inline(always)]
    fn count_match_from_indices(
        concat: &[u8],
        current_idx: usize,
        candidate_idx: usize,
        tail_limit: usize,
        seed_len: usize,
    ) -> usize {
        let seed = seed_len.min(tail_limit);
        if seed == tail_limit {
            return seed;
        }
        let remaining = tail_limit - seed;
        let base = concat.as_ptr();
        let extra = MatchGenerator::common_prefix_len_scalar_ptr(
            unsafe { base.add(candidate_idx + seed) },
            unsafe { base.add(current_idx + seed) },
            0,
            remaining,
        );
        seed + extra
    }

    fn bt_insert_step_no_rebase(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        let idx = abs_pos - self.history_abs_start;
        let concat = &self.history[self.history_start..];
        if idx + 8 > concat.len() {
            return 1;
        }
        let tail_limit = current_abs_end.saturating_sub(abs_pos);
        let hash = Self::hash_position_at(concat, idx, self.hash_log, self.bt_hash_mls());
        let Some(relative_pos) = self.relative_position(abs_pos) else {
            return 1;
        };
        let stored = relative_pos + 1;
        let bt_mask = self.bt_mask();
        let bt_low = abs_pos.saturating_sub(bt_mask);
        // Donor `ZSTD_insertBt1()` computes `windowLow` from the update target,
        // not from the position being inserted, because updateTree only needs
        // positions that stay in-window at the end of the batched fill.
        let window_low = self.window_low_abs_for_target(target_abs);
        let mut match_end_abs = abs_pos.saturating_add(9);
        let mut best_len = 8usize;
        let mut compares_left = self.search_depth;
        let mut common_length_smaller = 0usize;
        let mut common_length_larger = 0usize;
        let pair_idx = self.bt_pair_index_for_abs(abs_pos);
        let mut smaller_slot = pair_idx;
        let mut larger_slot = pair_idx + 1;
        let mut match_stored = self.hash_table[hash];
        self.hash_table[hash] = stored;

        while compares_left > 0 {
            let Some(candidate_abs) =
                Self::stored_abs_position_fast(match_stored, self.position_base, self.index_shift)
            else {
                break;
            };
            if candidate_abs < window_low || candidate_abs >= abs_pos {
                break;
            }
            compares_left -= 1;

            let next_pair_idx = self.bt_pair_index_for_abs(candidate_abs);
            let next_smaller = self.chain_table[next_pair_idx];
            let next_larger = self.chain_table[next_pair_idx + 1];
            let seed_len = common_length_smaller.min(common_length_larger);
            let candidate_idx = candidate_abs - self.history_abs_start;
            let match_len =
                Self::count_match_from_indices(concat, idx, candidate_idx, tail_limit, seed_len);

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
                self.chain_table[smaller_slot] = match_stored;
                common_length_smaller = match_len;
                if candidate_abs <= bt_low {
                    smaller_slot = usize::MAX;
                    break;
                }
                smaller_slot = next_pair_idx + 1;
                match_stored = next_larger;
            } else {
                self.chain_table[larger_slot] = match_stored;
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
            self.chain_table[smaller_slot] = HC_EMPTY;
        }
        if larger_slot != usize::MAX {
            self.chain_table[larger_slot] = HC_EMPTY;
        }

        let speed_positions = if best_len > 384 {
            (best_len - 384).min(192)
        } else {
            0
        };
        speed_positions.max(match_end_abs.saturating_sub(abs_pos.saturating_add(8)))
    }

    fn bt_update_tree_until(&mut self, abs_pos: usize, current_abs_end: usize) {
        if self.skip_insert_until_abs < self.history_abs_start {
            self.skip_insert_until_abs = self.history_abs_start;
        }
        let max_rel_no_rebase = (u32::MAX as usize).saturating_sub(2);
        let can_skip_rebase_checks = self.position_base == 0
            && self.index_shift == 0
            && abs_pos <= max_rel_no_rebase
            && (self.allow_zero_relative_position
                || self.skip_insert_until_abs > self.history_abs_start);
        let mut update_abs = self.skip_insert_until_abs;
        while update_abs < abs_pos {
            if !can_skip_rebase_checks {
                self.maybe_rebase_positions(update_abs);
            }
            let forward = self.bt_insert_step_no_rebase(update_abs, current_abs_end, abs_pos);
            update_abs = update_abs.saturating_add(forward.max(1));
        }
        self.skip_insert_until_abs = abs_pos;
    }

    fn bt_insert_and_collect_matches(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        let idx = abs_pos - self.history_abs_start;
        let concat = &self.history[self.history_start..];
        if idx + 8 > concat.len() {
            return;
        }
        let tail_limit = current_abs_end.saturating_sub(abs_pos);
        let hash = Self::hash_position_at(concat, idx, self.hash_log, self.bt_hash_mls());
        let Some(relative_pos) = self.relative_position(abs_pos) else {
            return;
        };
        let stored = relative_pos + 1;
        let bt_mask = self.bt_mask();
        let bt_low = abs_pos.saturating_sub(bt_mask);
        let window_low = self.window_low_abs_for_target(abs_pos);
        let mut match_end_abs = abs_pos.saturating_add(9);
        let mut compares_left = profile.max_chain_depth.min(self.search_depth);
        let mut common_length_smaller = 0usize;
        let mut common_length_larger = 0usize;
        let pair_idx = self.bt_pair_index_for_abs(abs_pos);
        let mut smaller_slot = pair_idx;
        let mut larger_slot = pair_idx + 1;
        let mut match_stored = self.hash_table[hash];
        self.hash_table[hash] = stored;
        // Donor semantics:
        // - `bestLength` starts at `lengthToBeat - 1`,
        // - rep/hash3 probing may raise it before BT traversal,
        // - BT then only reports strictly longer matches.
        // So after rep/hash3 we must not relax the threshold back down, or we
        // mutate `matchEndIdx` / `nextToUpdate` with non-donor equal-length BT
        // alternatives and drift the future tree evolution.
        let mut best_len = (*best_len_for_skip).max(min_match_len.saturating_sub(1));

        while compares_left > 0 {
            let Some(candidate_abs) =
                Self::stored_abs_position_fast(match_stored, self.position_base, self.index_shift)
            else {
                break;
            };
            if candidate_abs < window_low || candidate_abs >= abs_pos {
                break;
            }
            compares_left -= 1;

            let next_pair_idx = self.bt_pair_index_for_abs(candidate_abs);
            let next_smaller = self.chain_table[next_pair_idx];
            let next_larger = self.chain_table[next_pair_idx + 1];
            let seed_len = common_length_smaller.min(common_length_larger);
            let candidate_idx = candidate_abs - self.history_abs_start;
            let match_len =
                Self::count_match_from_indices(concat, idx, candidate_idx, tail_limit, seed_len);

            if match_len > best_len {
                let offset = abs_pos - candidate_abs;
                let accepted = Self::push_candidate_ladder(
                    out,
                    best_len_for_skip,
                    MatchCandidate {
                        start: abs_pos,
                        offset,
                        match_len,
                    },
                    min_match_len,
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
                self.chain_table[smaller_slot] = match_stored;
                common_length_smaller = match_len;
                if candidate_abs <= bt_low {
                    smaller_slot = usize::MAX;
                    break;
                }
                smaller_slot = next_pair_idx + 1;
                match_stored = next_larger;
            } else {
                self.chain_table[larger_slot] = match_stored;
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
            self.chain_table[smaller_slot] = HC_EMPTY;
        }
        if larger_slot != usize::MAX {
            self.chain_table[larger_slot] = HC_EMPTY;
        }
        self.skip_insert_until_abs = match_end_abs.saturating_sub(8);
    }

    fn hash_position_with_mls(data: &[u8], hash_log: usize, mls: usize) -> usize {
        let value = Self::read_le_u32(data);
        Self::hash_value_with_mls(value, hash_log, mls)
    }

    #[inline(always)]
    fn hash_position_at(data: &[u8], idx: usize, hash_log: usize, mls: usize) -> usize {
        debug_assert!(idx + 4 <= data.len());
        let value = unsafe { Self::read_le_u32_ptr(data.as_ptr().add(idx)) };
        Self::hash_value_with_mls(value, hash_log, mls)
    }

    #[inline(always)]
    fn hash_value_with_mls(value: u32, hash_log: usize, mls: usize) -> usize {
        match mls {
            3 => (((value << 8).wrapping_mul(HC_PRIME3BYTES)) >> (32 - hash_log)) as usize,
            _ => ((value.wrapping_mul(HC_PRIME4BYTES)) >> (32 - hash_log)) as usize,
        }
    }

    fn hash_position(&self, data: &[u8]) -> usize {
        Self::hash_position_with_mls(data, self.hash_log, 4)
    }

    #[cfg(test)]
    fn hash3_position(data: &[u8], hash_log: usize) -> usize {
        let value = Self::read_le_u32(data);
        (((value << 8).wrapping_mul(HC_PRIME3BYTES)) >> (32 - hash_log)) as usize
    }

    #[inline(always)]
    fn read_le_u32(data: &[u8]) -> u32 {
        debug_assert!(data.len() >= 4);
        unsafe { Self::read_le_u32_ptr(data.as_ptr()) }
    }

    #[inline(always)]
    unsafe fn read_le_u32_ptr(ptr: *const u8) -> u32 {
        unsafe { u32::from_le(core::ptr::read_unaligned(ptr as *const u32)) }
    }

    fn hash3_candidate(
        &self,
        abs_pos: usize,
        current_abs_end: usize,
        min_match_len: usize,
    ) -> Option<MatchCandidate> {
        if self.hash3_log == 0 {
            return None;
        }
        let idx = abs_pos.checked_sub(self.history_abs_start)?;
        let concat = self.live_history();
        if idx + 4 > concat.len() {
            return None;
        }
        let hash3 = Self::hash_position_at(concat, idx, self.hash3_log, 3);
        let entry = self.hash3_table.get(hash3).copied().unwrap_or(HC_EMPTY);
        let candidate_abs =
            Self::stored_abs_position_fast(entry, self.position_base, self.index_shift)?;
        if candidate_abs < self.history_abs_start || candidate_abs >= abs_pos {
            return None;
        }
        let offset = abs_pos - candidate_abs;
        if offset >= HC3_MAX_OFFSET {
            return None;
        }
        let candidate_idx = candidate_abs - self.history_abs_start;
        let tail_limit = current_abs_end.saturating_sub(abs_pos);
        let base = concat.as_ptr();
        let match_len = MatchGenerator::common_prefix_len_scalar_ptr(
            unsafe { base.add(candidate_idx) },
            unsafe { base.add(idx) },
            0,
            tail_limit,
        );
        (match_len >= min_match_len).then_some(MatchCandidate {
            start: abs_pos,
            offset,
            match_len,
        })
    }

    fn insert_hash3_only_no_rebase(&mut self, abs_pos: usize) {
        if self.hash3_log == 0 {
            return;
        }
        let idx = abs_pos - self.history_abs_start;
        let concat = &self.history[self.history_start..];
        if idx + 4 > concat.len() {
            return;
        }
        let Some(relative_pos) = self.relative_position(abs_pos) else {
            return;
        };
        let hash3 = Self::hash_position_at(concat, idx, self.hash3_log, 3);
        self.hash3_table[hash3] = relative_pos + 1;
    }

    fn update_hash3_until(&mut self, abs_pos: usize) {
        if self.next_to_update3 < self.history_abs_start {
            self.next_to_update3 = self.history_abs_start;
        }
        if self.next_to_update3 >= abs_pos {
            return;
        }
        let max_rel_no_rebase = (u32::MAX as usize).saturating_sub(2);
        let can_skip_rebase_checks = self.position_base == 0
            && self.index_shift == 0
            && abs_pos <= max_rel_no_rebase
            && (self.allow_zero_relative_position || self.next_to_update3 > self.history_abs_start);
        while self.next_to_update3 < abs_pos {
            if !can_skip_rebase_checks {
                self.maybe_rebase_positions(self.next_to_update3);
            }
            self.insert_hash3_only_no_rebase(self.next_to_update3);
            self.next_to_update3 = self.next_to_update3.saturating_add(1);
        }
    }

    fn relative_position(&self, abs_pos: usize) -> Option<u32> {
        let shifted_abs = abs_pos.checked_add(self.index_shift)?;
        let rel = shifted_abs.checked_sub(self.position_base)?;
        let rel_u32 = u32::try_from(rel).ok()?;
        // Donor parity: raw BT/HC tables use 0 as the empty sentinel, so the
        // very first absolute position in the first block (curr == 0) is not a
        // representable candidate index.
        if !self.allow_zero_relative_position && self.position_base == 0 && rel_u32 == 0 {
            return None;
        }
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
        self.index_shift = 0;
        self.allow_zero_relative_position = true;
        self.hash_table.fill(HC_EMPTY);
        self.hash3_table.fill(HC_EMPTY);
        self.chain_table.fill(HC_EMPTY);

        let history_start = self.history_abs_start;
        // Rebuild only the already-inserted prefix. The caller inserts abs_pos
        // immediately after this, and later positions are added in-order.
        if self.uses_bt_matchfinder() {
            let rebuild_end = self.history_abs_end();
            let mut pos = history_start;
            while pos < abs_pos {
                let forward = self.bt_insert_step_no_rebase(pos, rebuild_end, abs_pos);
                pos = pos.saturating_add(forward.max(1));
            }
        } else {
            for pos in history_start..abs_pos {
                self.insert_position_no_rebase(pos);
            }
        }
        self.next_to_update3 = self.next_to_update3.max(abs_pos);
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
        self.next_to_update3 = self.next_to_update3.max(end);
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
        let mut steps = 0;
        let max_chain_steps = self.search_depth;
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

    fn for_each_repcode_candidate_with_reps(
        &self,
        abs_pos: usize,
        lit_len: usize,
        reps: [u32; 3],
        current_abs_end: usize,
        min_match_len: usize,
        mut f: impl FnMut(MatchCandidate),
    ) {
        let rep_offsets: [Option<usize>; 3] = if lit_len == 0 {
            [
                Some(reps[1] as usize),
                Some(reps[2] as usize),
                (reps[0] > 1).then_some((reps[0] - 1) as usize),
            ]
        } else {
            [
                Some(reps[0] as usize),
                Some(reps[1] as usize),
                Some(reps[2] as usize),
            ]
        };
        let concat = self.live_history();
        let current_idx = abs_pos - self.history_abs_start;
        if current_idx + 4 > concat.len() {
            return;
        }
        let tail_limit = current_abs_end.saturating_sub(abs_pos);
        for rep in rep_offsets.into_iter().flatten() {
            if rep == 0 || rep > abs_pos {
                continue;
            }
            let candidate_pos = abs_pos - rep;
            if candidate_pos < self.history_abs_start {
                continue;
            }
            let candidate_idx = candidate_pos - self.history_abs_start;
            let match_len =
                MatchGenerator::common_prefix_len(&concat[candidate_idx..], &concat[current_idx..])
                    .min(tail_limit);
            if match_len < min_match_len {
                continue;
            }
            f(MatchCandidate {
                start: abs_pos,
                offset: rep,
                match_len,
            });
        }
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
    assert_eq!(hc.hash3_log, 14);

    hc.configure(BTULTRA2_HC_CONFIG_L22, 27);
    assert_eq!(hc.hash3_log, HC3_HASH_LOG);
}

#[test]
fn btultra2_seed_pass_initializes_opt_state() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG, 26);
    let data: Vec<u8> = (0..32 * 1024).map(|i| (i % 251) as u8).collect();
    hc.add_data(data, |_| {});
    hc.start_matching(|_| {});
    assert!(
        hc.opt_state.lit_length_sum > 0,
        "btultra2 first block should seed non-zero sequence statistics"
    );
    assert!(
        hc.opt_state.off_code_sum > 0,
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
    hc.target_len = 13;
    let profile = HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, true);
    assert_eq!(hc.sufficient_match_len_for_pass(profile), 13);
}

#[test]
fn opt_modes_use_target_len_as_sufficient_len() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.target_len = 57;
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
    hc.target_len = usize::MAX / 2;
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

    hc.opt_state.rescale_freqs(
        b"abcd",
        HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, false),
    );

    let base_ll_freqs: [u32; HC_MAX_LL + 1] = [
        4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1,
    ];

    assert_ne!(
        hc.opt_state.lit_length_freq, base_ll_freqs,
        "dictionary entropy should override fallback LL bootstrap frequencies"
    );
    assert!(
        hc.opt_state.match_length_freq.iter().any(|&v| v != 1),
        "dictionary entropy should seed non-uniform ML frequencies"
    );
    assert_ne!(
        hc.opt_state.off_code_freq[0], 6,
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
    hc.opt_state.rescale_freqs(
        b"abcd",
        HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, false),
    );

    let base_ll_freqs: [u32; HC_MAX_LL + 1] = [
        4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1,
    ];
    assert_ne!(
        hc.opt_state.lit_length_freq, base_ll_freqs,
        "FSE seed should still override LL bootstrap frequencies without huffman seed"
    );
    assert!(
        hc.opt_state.match_length_freq.iter().any(|&v| v != 1),
        "FSE seed should still seed non-uniform ML frequencies"
    );
    assert_ne!(
        hc.opt_state.off_code_freq[0], 6,
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
    hc.opt_state.rescale_freqs(
        b"abc",
        HcOptimalCostProfile::for_mode(HcParseMode::BtUltra2, false),
    );
    assert!(
        matches!(hc.opt_state.price_type, HcOptPriceType::Dynamic),
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
    hc.history_abs_start = 17;
    hc.window.push_back(b"abcdefghijklmnop".to_vec());
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
    hc.opt_state.lit_length_sum = 1;
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
    hc.window_size = HC_PREDEF_THRESHOLD + 64;
    hc.window
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
    hc.window_size = HC_PREDEF_THRESHOLD + 64;
    hc.window
        .push_back(alloc::vec![b'A'; HC_PREDEF_THRESHOLD + 64]);
    hc.ldm_sequences.push(HcRawSeq {
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
    hc.history = b"xxxxxxABCDEFABCDEF".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;

    let abs_pos = 12usize; // points at second "ABCDEF"
    let current_abs_end = hc.history.len();
    let reps = [6u32, 3u32, 9u32];

    let mut lit_pos_candidates = Vec::new();
    hc.for_each_repcode_candidate_with_reps(
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
    hc.for_each_repcode_candidate_with_reps(
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
    hc.search_depth = 0;
    hc.history = b"xyzxyzxyzxyz".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;

    let abs_pos = 6usize;
    let current_abs_end = hc.history.len();
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
    hc.history = b"aaaaaaaaaa".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;
    hc.position_base = 0;
    hc.search_depth = 32;
    let abs_pos = 6usize;
    hc.ensure_tables();
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
        hc.history.len(),
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
    hc.history = b"abcabcabcabcabcabcabcabc".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;
    hc.position_base = 0;
    hc.search_depth = 32;
    let abs_pos = 9usize;
    hc.ensure_tables();
    hc.insert_positions(0, abs_pos);
    hc.skip_insert_until_abs = 0;

    let profile = HcOptimalCostProfile {
        max_chain_depth: 32,
        sufficient_match_len: usize::MAX / 2,
        accurate: true,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();
    hc.collect_optimal_candidates(
        abs_pos,
        hc.history.len(),
        profile,
        HcCandidateQuery {
            reps: [1, 4, 8],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );

    assert!(
        hc.skip_insert_until_abs > abs_pos,
        "long chain match should advance skip window to avoid redundant immediate insertions"
    );
}

#[test]
fn hc_collect_optimal_candidates_chain_fast_skip_uses_match_end_minus_8() {
    let mut hc = HcMatchGenerator::new(128);
    hc.history = b"abcabcabcabcabcabcabcabc".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;
    hc.position_base = 0;
    hc.search_depth = 32;
    let abs_pos = 9usize;
    hc.ensure_tables();
    hc.insert_positions(0, abs_pos);
    hc.skip_insert_until_abs = 0;

    let profile = HcOptimalCostProfile {
        max_chain_depth: 32,
        sufficient_match_len: 10,
        accurate: true,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();
    hc.collect_optimal_candidates(
        abs_pos,
        hc.history.len(),
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
        hc.skip_insert_until_abs > abs_pos,
        "chain fast-skip must advance past current position"
    );
    assert!(
        hc.skip_insert_until_abs <= best_match_end.saturating_sub(8),
        "chain fast-skip must not exceed donor-style matchEndIdx - 8 bound"
    );
}

#[test]
fn hc_collect_optimal_candidates_advances_skip_window_on_plain_bt_path() {
    let mut hc = HcMatchGenerator::new(256);
    hc.history = b"abcdefghijklmnop".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;
    hc.position_base = 0;
    hc.search_depth = 0;
    hc.ensure_tables();

    let abs_pos = 8usize;
    hc.skip_insert_until_abs = 0;

    let profile = HcOptimalCostProfile {
        max_chain_depth: 0,
        sufficient_match_len: usize::MAX / 2,
        accurate: true,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();
    hc.collect_optimal_candidates(
        abs_pos,
        hc.history.len(),
        profile,
        HcCandidateQuery {
            reps: [1, 4, 8],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );

    assert_eq!(
        hc.skip_insert_until_abs,
        abs_pos.saturating_add(1),
        "plain BT path should advance skip window by 1 via donor matchEndIdx baseline"
    );
}

#[test]
fn hc_collect_optimal_candidates_uses_hash3_when_chain_depth_zero() {
    let mut hc = HcMatchGenerator::new(256);
    hc.history = b"abcde1234abcdeZZZZ".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;
    hc.position_base = 0;
    hc.search_depth = 0;
    let abs_pos = 9usize; // second "abcde"
    hc.ensure_tables();
    hc.insert_positions(0, abs_pos);
    // Donor hash3 has an independent nextToUpdate3 cursor; main-table
    // insertion does not imply the HC3 side table has been filled.
    hc.next_to_update3 = 0;

    let profile = HcOptimalCostProfile {
        max_chain_depth: 0,
        sufficient_match_len: usize::MAX / 2,
        accurate: true,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();
    hc.collect_optimal_candidates(
        abs_pos,
        hc.history.len(),
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
    hc.history = b"abcdeZabcdeYY".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;
    hc.position_base = 0;
    hc.search_depth = 0;
    hc.ensure_tables();
    // Simulate donor-like nextToUpdate3 path: no explicit hash/chain insertions
    // were done for prefix positions, so hash3 must fill them on demand.
    hc.next_to_update3 = 0;

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
        hc.history.len(),
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
    hc.history = b"abcdeZabcde".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;
    hc.position_base = 0;
    hc.search_depth = 0;
    hc.ensure_tables();
    hc.next_to_update3 = 0;

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
        hc.history.len(),
        profile,
        HcCandidateQuery {
            reps: [1, 2, 3],
            lit_len: 1,
            ldm_candidate: None,
        },
        &mut out,
    );

    assert!(
        hc.next_to_update3 >= abs_pos,
        "tail-reaching hash3 lookup should fill the update cursor to current position"
    );
    assert!(
        hc.skip_insert_until_abs > abs_pos,
        "tail-reaching hash3 early return should request skipping current-position hash insertion"
    );
}

#[test]
fn hc_ldm_candidates_are_merged_into_optimal_candidates() {
    let mut hc = HcMatchGenerator::new(512);
    hc.history = (0..256).map(|i| (i % 251) as u8).collect();
    hc.history_start = 0;
    hc.history_abs_start = 0;

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
    hc.history = alloc::vec![0u8; 160];
    for i in 0..64 {
        hc.history[i] = b'a' + (i % 7) as u8;
    }
    for i in 64..160 {
        hc.history[i] = b'k' + (i % 5) as u8;
    }
    let abs_pos = 96usize;
    for i in 0..24 {
        hc.history[abs_pos + i] = hc.history[16 + i];
    }
    hc.history_start = 0;
    hc.history_abs_start = 0;
    hc.position_base = 0;
    hc.search_depth = 32;
    hc.ensure_tables();
    hc.insert_positions(0, abs_pos);

    let profile = HcOptimalCostProfile {
        max_chain_depth: 32,
        sufficient_match_len: usize::MAX / 2,
        accurate: true,
        favor_small_offsets: false,
    };
    let mut out = Vec::new();

    hc.parse_mode = HcParseMode::BtUltra2;
    hc.dictionary_limit_abs = Some(64);
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
    hc.skip_insert_until_abs = 0;
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
fn hc_prime_with_empty_dictionary_disables_btultra2_seed_pass() {
    let mut driver = MatchGeneratorDriver::new(8, 1);
    driver.reset(CompressionLevel::Better);

    driver.prime_with_dictionary(&[], [11, 7, 3]);

    assert_eq!(driver.hc_matcher().offset_hist, [11, 7, 3]);
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
    let hash = hash_mix_u64_with_kernel(value, matcher.hash_mix_kernel);
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
    if !is_x86_feature_detected!("sse4.2") {
        return;
    }

    let kernel = detect_hash_mix_kernel();
    assert_eq!(kernel, HashMixKernel::X86Sse42);
    let v = 0x0123_4567_89AB_CDEFu64;
    let accelerated = unsafe { hash_mix_u64_sse42(v) };
    assert_eq!(hash_mix_u64_with_kernel(v, kernel), accelerated);
}

#[cfg(all(feature = "std", target_arch = "x86_64"))]
#[test]
fn hash_mix_scalar_path_can_be_forced_for_coverage_and_matches_formula() {
    let v = 0x0123_4567_89AB_CDEFu64;
    let expected = v.wrapping_mul(HASH_MIX_PRIME);
    let mixed = hash_mix_u64_with_kernel(v, HashMixKernel::Scalar);
    assert_eq!(mixed, expected);
}

#[cfg(all(feature = "std", target_arch = "aarch64", target_endian = "little"))]
#[test]
fn hash_mix_crc_path_is_available_and_matches_accelerated_impl_when_supported() {
    if !is_aarch64_feature_detected!("crc") {
        return;
    }

    let kernel = detect_hash_mix_kernel();
    assert_eq!(kernel, HashMixKernel::Aarch64Crc);
    let v = 0x0123_4567_89AB_CDEFu64;
    let accelerated = unsafe { hash_mix_u64_crc(v) };
    assert_eq!(hash_mix_u64_with_kernel(v, kernel), accelerated);
}

#[cfg(all(feature = "std", target_arch = "aarch64", target_endian = "little"))]
#[test]
fn hash_mix_scalar_path_can_be_forced_on_aarch64_and_matches_formula() {
    let v = 0x0123_4567_89AB_CDEFu64;
    let expected = v.wrapping_mul(HASH_MIX_PRIME);
    let mixed = hash_mix_u64_with_kernel(v, HashMixKernel::Scalar);
    assert_eq!(mixed, expected);
}

#[test]
fn hc_hash3_position_matches_donor_formula() {
    let bytes = [b'a', b'b', b'c', b'd'];
    let read32 = u32::from_le_bytes(bytes);
    let expected = (((read32 << 8).wrapping_mul(HC_PRIME3BYTES)) >> (32 - HC3_HASH_LOG)) as usize;
    assert_eq!(
        HcMatchGenerator::hash3_position(&bytes, HC3_HASH_LOG),
        expected
    );
}

#[test]
fn hc_hash_position_matches_donor_hash4_formula() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(HC_CONFIG, 22);
    let bytes = [b'a', b'b', b'c', b'd'];
    let read32 = u32::from_le_bytes(bytes);
    let expected = ((read32.wrapping_mul(HC_PRIME4BYTES)) >> (32 - hc.hash_log)) as usize;
    assert_eq!(hc.hash_position(&bytes), expected);
}

#[test]
fn btultra2_main_hash_uses_donor_hash4_formula() {
    let mut hc = HcMatchGenerator::new(1 << 20);
    hc.configure(BTULTRA2_HC_CONFIG_L22, 27);
    let bytes = [b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h'];
    let read32 = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    let expected = ((read32.wrapping_mul(HC_PRIME4BYTES)) >> (32 - hc.hash_log)) as usize;
    let actual = HcMatchGenerator::hash_position_with_mls(&bytes, hc.hash_log, hc.bt_hash_mls());
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
fn btultra2_sparse_skip_matching_preserves_tail_cross_block_match() {
    let mut matcher = HcMatchGenerator::new(1 << 20);
    matcher.configure(BTULTRA2_HC_CONFIG_L22, 20);
    let tail = b"Bt9kLm2Rp";
    let mut first = deterministic_high_entropy_bytes(0xA9C3_7F21_D4E8_510B, 4096);
    let tail_start = first.len() - tail.len();
    first[tail_start..].copy_from_slice(tail);
    matcher.add_data(first, |_| {});
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
fn hc_insert_positions_advances_next_to_update3_for_contiguous_range() {
    let mut hc = HcMatchGenerator::new(64);
    hc.history = b"abcdefghijklmnopqrstuvwxyz".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;
    hc.position_base = 0;
    hc.ensure_tables();
    hc.next_to_update3 = 0;

    hc.insert_positions(0, 9);

    assert_eq!(
        hc.next_to_update3, 9,
        "contiguous insert_positions should advance hash3 update cursor"
    );
}

#[test]
fn hc_insert_positions_with_step_keeps_next_to_update3_cursor_for_sparse_ranges() {
    let mut hc = HcMatchGenerator::new(64);
    hc.history = b"abcdefghijklmnopqrstuvwxyz".to_vec();
    hc.history_start = 0;
    hc.history_abs_start = 0;
    hc.position_base = 0;
    hc.ensure_tables();
    hc.next_to_update3 = 0;

    hc.insert_positions_with_step(0, 16, 4);

    assert_eq!(
        hc.next_to_update3, 0,
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
