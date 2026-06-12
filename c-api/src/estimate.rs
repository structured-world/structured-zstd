//! `ZSTD_estimate*` — memory-budget estimates for contexts and streams.
//!
//! The figures are derived from the codec's own per-level tuning table
//! (`estimated_compression_workspace_bytes`), so they track this library's
//! real allocations rather than replicating upstream's numbers; like
//! upstream they are budget upper bounds, not exact accounting.

use core::ffi::c_int;

use codec::encoding::{CompressionLevel, estimated_compression_workspace_bytes};

use crate::cdict::ZSTD_compressionParameters;
use crate::context::{ZSTD_CCtx, ZSTD_DCtx};

/// 128 KiB — the format's maximum block size; bounds per-block staging.
const BLOCK_SIZE_MAX: usize = 128 * 1024;

/// `ZSTD_WINDOWLOG_MAX` for the target's pointer width (upstream: 30 on
/// 32-bit, 31 on 64-bit). Inputs past it are rejected with an encoded error.
const WINDOW_LOG_MAX: core::ffi::c_uint = if usize::BITS >= 64 { 31 } else { 30 };

/// Hash/chain table-log ceiling: upstream's 30, additionally bounded so the
/// `4 << log` table-size term fits `usize` on 32-bit targets.
const TABLE_LOG_MAX: core::ffi::c_uint = if usize::BITS >= 64 { 30 } else { 29 };

/// `size_t ZSTD_estimateCCtxSize(int maxCompressionLevel)` — budget for a
/// one-shot compression context at the given level.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_estimateCCtxSize(max_compression_level: c_int) -> usize {
    core::mem::size_of::<ZSTD_CCtx>()
        + estimated_compression_workspace_bytes(CompressionLevel::from_level(max_compression_level))
}

/// `size_t ZSTD_estimateCCtxSize_usingCParams(ZSTD_compressionParameters
/// cParams)` — budget from explicit compression parameters: window history
/// plus the hash + chain tables the parameters request, plus block staging.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_estimateCCtxSize_usingCParams(cparams: ZSTD_compressionParameters) -> usize {
    // Out-of-range parameters are an error, not a number to clamp: upstream
    // validates cParams here and returns an encoded error code (the size_t
    // ABI carries them — test with ZSTD_isError). The bounds below also
    // prove the plain arithmetic cannot overflow: `TABLE_LOG_MAX` keeps
    // `4 << log` inside `usize` on 32-bit targets too, where the upstream
    // 30-bit table-log ceiling alone would let `4 << 30` wrap.
    if cparams.windowLog > WINDOW_LOG_MAX
        || cparams.hashLog > TABLE_LOG_MAX
        || cparams.chainLog > TABLE_LOG_MAX
        || !(1..=9).contains(&cparams.strategy)
    {
        return crate::error::encode(crate::error::ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
    }
    let window = 1usize << cparams.windowLog.max(10);
    let hash = if cparams.hashLog == 0 {
        0
    } else {
        4usize << cparams.hashLog
    };
    let chain = if cparams.chainLog == 0 {
        0
    } else {
        4usize << cparams.chainLog
    };
    // Binary-tree strategies retain the optimal-parser workspace (and, for
    // btultra/btultra2, the HC3 side table) on top of the hash/chain tables.
    // No cast on purpose: this crate only targets platforms where C's
    // `unsigned int` is 32-bit (it emits the host `libzstd.so.1`), so
    // `c_uint == u32` here and clippy rejects a redundant conversion. On a
    // hypothetical 16-bit `c_uint` target this line fails to compile — a
    // loud signal beats a silent widening.
    let bt =
        codec::encoding::estimated_bt_strategy_extra_bytes(cparams.strategy, cparams.windowLog);
    // The per-field bounds keep each TERM inside usize, but on a 32-bit
    // target the SUM can still wrap (e.g. windowLog 30 + hashLog 29 +
    // chainLog 29) — an under-reported budget is worse than an error for a
    // sizing contract, so overflow comes back encoded.
    let Some(total) = core::mem::size_of::<ZSTD_CCtx>()
        .checked_add(window)
        .and_then(|v| v.checked_add(hash))
        .and_then(|v| v.checked_add(chain))
        .and_then(|v| v.checked_add(bt))
        .and_then(|v| v.checked_add(3 * BLOCK_SIZE_MAX))
    else {
        return crate::error::encode(crate::error::ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
    };
    total
}

/// `size_t ZSTD_estimateCStreamSize_usingCParams(ZSTD_compressionParameters
/// cParams)` — streaming budget from explicit compression parameters: the
/// one-shot estimate plus the streaming input/output staging buffers.
/// Invalid parameters propagate as an encoded error (test with
/// `ZSTD_isError`).
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_estimateCStreamSize_usingCParams(
    cparams: ZSTD_compressionParameters,
) -> usize {
    let base = ZSTD_estimateCCtxSize_usingCParams(cparams);
    if crate::error::result_is_error(base) {
        return base;
    }
    base + 2 * BLOCK_SIZE_MAX
}

/// `size_t ZSTD_estimateDCtxSize(void)` — budget for a decompression
/// context before any frame allocates its window (the window itself is
/// sized per frame; see `ZSTD_estimateDStreamSize`).
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_estimateDCtxSize() -> usize {
    // Entropy tables (Huffman + 3×FSE) plus the per-block literal staging
    // dominate the pre-window footprint.
    core::mem::size_of::<ZSTD_DCtx>() + 192 * 1024
}

/// `size_t ZSTD_estimateCStreamSize(int maxCompressionLevel)` — budget for a
/// streaming compression context: the one-shot estimate plus the streaming
/// input/output staging buffers.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_estimateCStreamSize(max_compression_level: c_int) -> usize {
    // `from_level` clamps the level and the per-level estimate is bounded by
    // the largest real table set (a few hundred MiB), so plain addition
    // provably cannot overflow.
    ZSTD_estimateCCtxSize(max_compression_level) + 2 * BLOCK_SIZE_MAX
}

/// `size_t ZSTD_estimateDStreamSize(size_t maxWindowSize)` — budget for a
/// streaming decompression context decoding frames up to `maxWindowSize`:
/// the window history buffer plus one output block plus the fixed tables.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_estimateDStreamSize(max_window_size: usize) -> usize {
    // A window beyond the format's maximum is invalid input, not a number
    // to absorb: return an encoded error (size_t ABI, test with
    // ZSTD_isError) so a garbage SIZE_MAX cannot wrap into an
    // underestimate. The bound also proves the plain addition in range.
    if max_window_size > (1usize << WINDOW_LOG_MAX) {
        return crate::error::encode(crate::error::ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
    }
    ZSTD_estimateDCtxSize() + max_window_size + BLOCK_SIZE_MAX
}
