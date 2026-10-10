//! Values and interfaces shared between the encoding side
//! and the decoding side.

// --- FRAMES ---
/// This magic number is included at the start of a single Zstandard frame
pub const MAGIC_NUM: u32 = 0xFD2F_B528;
/// Window size refers to the minimum amount of memory needed to decode any given frame.
///
/// The minimum window size is defined as 1 KB
pub const MIN_WINDOW_SIZE: u64 = 1024;
/// Window size refers to the minimum amount of memory needed to decode any given frame.
///
/// The maximum window size allowed by the spec is 3.75TB
pub const MAX_WINDOW_SIZE: u64 = (1 << 41) + 7 * (1 << 38);

// --- BLOCKS ---
/// While the spec limits block size to 128KB, the implementation uses
/// 128kibibytes
///
/// <https://github.com/facebook/zstd/blob/eca205fc7849a61ab287492931a04960ac58e031/doc/educational_decoder/zstd_decompress.c#L28-L29>
pub const MAX_BLOCK_SIZE: u32 = 128 * 1024;
/// Smallest accepted block-size target (upstream `ZSTD_TARGETCBLOCKSIZE_MIN`):
/// below this the per-block header overhead dominates any latency benefit.
/// Re-exported at the crate root as the single source of truth; the C ABI
/// parameter bounds import it from there.
pub const MIN_TARGET_BLOCK_SIZE: u32 = 1340;

/// The decoder's default window ceiling (128 MiB = `1 << 27`), upstream zstd's
/// default `ZSTD_d_windowLogMax` (`ZSTD_WINDOWLOG_LIMIT_DEFAULT = 27`). On the
/// paths where the decoder holds the window itself, a frame advertising a
/// larger one is refused to bound that allocation on untrusted input, until the
/// caller raises the ceiling (`FrameDecoder::set_max_window_size`). Every frame
/// our encoder emits by default (up to `window_log 27` at level 22) fits it.
pub const MAXIMUM_ALLOWED_WINDOW_SIZE: u64 = 1 << 27;

/// The largest window this build decodes on any path, however far the ceiling
/// is raised: upstream `ZSTD_WINDOWLOG_MAX`, `1 << 31` on 64-bit targets and
/// `1 << 30` on 32-bit ones, where a larger window could not be addressed.
#[cfg(target_pointer_width = "64")]
pub const MAX_DECODER_WINDOW_SIZE: u64 = 1 << 31;
/// The largest window this build decodes on any path, however far the ceiling
/// is raised: upstream `ZSTD_WINDOWLOG_MAX`, `1 << 31` on 64-bit targets and
/// `1 << 30` on 32-bit ones, where a larger window could not be addressed.
#[cfg(not(target_pointer_width = "64"))]
pub const MAX_DECODER_WINDOW_SIZE: u64 = 1 << 30;
