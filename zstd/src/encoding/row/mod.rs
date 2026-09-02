//! Row-based match finder (level 4 default backend).
//!
//! Upstream zstd parity: mirrors the `ZSTD_row_*` family in `zstd_lazy.c`. The
//! row hash splits each bucket into `1 << row_log` slots (16 / 32 / 64),
//! each tagged with a 1-byte hash so the search can skip most slots
//! without touching the position table.
//!
//! Extracted from `match_generator.rs` as part of #111 Phase 1b
//! (structural split). Mechanical move — names, fields, and bodies
//! are preserved; visibility on the relocated items was opened to
//! `pub(crate)` so `match_generator` can keep dispatching to
//! `RowMatchGenerator` through the `row::` import path.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::Sequence;
use super::blocks::encode_offset_with_history;
use super::dict_attach::DictAttach;
use super::levels::config::{RowConfig, RowDictPlan};
use super::match_generator::{
    ROW_EMPTY_SLOT, ROW_HASH_BITS, ROW_HASH_KEY_LEN, ROW_LOG, ROW_MIN_MATCH_LEN, ROW_SEARCH_DEPTH,
    ROW_TAG_BITS, ROW_TARGET_LEN,
};

/// Upstream zstd lazy-parse bounds (`zstd_lazy.c`): the row parse stops
/// `8 + ZSTD_ROW_HASH_CACHE_SIZE` bytes before the block end, a miss steps
/// by `(ip - anchor) >> kSearchStrength + 1`, and steps above
/// `kLazySkippingStep` stop indexing the skipped-over positions.
const LAZY_ROW_ILIMIT_MARGIN: usize = 16;
const LAZY_HC_ILIMIT_MARGIN: usize = 8;
const LAZY_SEARCH_STRENGTH: u32 = 8;
const LAZY_SKIPPING_STEP: usize = 8;

/// Upstream zstd `ZSTD_ROW_HASH_CACHE_SIZE`: the row hash cache runs this
/// many positions ahead of the parse, prefetching each position's row.
const ROW_HASH_CACHE_SIZE: usize = 8;
/// Upstream zstd `ZSTD_row_update_internal` gap rule: a gap of more than
/// `ROW_UPDATE_SKIP_THRESHOLD` un-indexed positions indexes only its first
/// `ROW_UPDATE_MAX_START` and last `ROW_UPDATE_MAX_END`.
const ROW_UPDATE_SKIP_THRESHOLD: usize = 384;
const ROW_UPDATE_MAX_START: usize = 96;
const ROW_UPDATE_MAX_END: usize = 32;
/// Hash-cache sentinel for a position too close to the end to hash.
const ROW_CACHE_NONE: u32 = u32::MAX;

/// Upstream zstd hash multipliers per key width (`zstd_compress_internal.h`
/// `prime4bytes` / `prime5bytes` / `prime6bytes`): `ZSTD_hash4` multiplies
/// the 32-bit key, `ZSTD_hash5` / `ZSTD_hash6` shift the 8-byte read so the
/// key occupies the top 40 / 48 bits and multiply; the salt is XORed into
/// the product and the row + tag are its top `hashLog + 8` bits.
pub(crate) const ROW_HASH_PRIME4: u32 = 2_654_435_761;
pub(crate) const ROW_HASH_PRIME5: u64 = 889_523_592_379;
pub(crate) const ROW_HASH_PRIME6: u64 = 227_718_039_650_203;

/// Upstream zstd `ZSTD_bitmix` (zstd_compress.c:1971, XXH3 rrmxmx shape).
const fn zstd_bitmix(mut val: u64, len: u64) -> u64 {
    val ^= val.rotate_right(49) ^ val.rotate_right(24);
    val = val.wrapping_mul(0x9FB2_1C65_1E98_DF25);
    val ^= (val >> 35).wrapping_add(len);
    val = val.wrapping_mul(0x9FB2_1C65_1E98_DF25);
    val ^ (val >> 28)
}

/// Row hash salt of a freshly created upstream context:
/// `ZSTD_advanceHashSalt` on `hashSalt = hashSaltEntropy = 0`
/// (zstd_compress.c:1980, run by `ZSTD_reset_matchState` for a CCtx). A
/// fixed salt keeps row assignment identical to a fresh `ZSTD_CCtx`; the
/// per-reset re-salting upstream does on context reuse only serves to
/// defeat stale tags, which the position floor rejects here anyway.
pub(crate) const ROW_HASH_SALT: u64 = zstd_bitmix(0, 8) ^ zstd_bitmix(0, 4);

/// Upstream `ZSTD_WINDOW_START_INDEX`: the binary tree stores a position
/// as `abs + 2`, so `0` is "no node" and `1` ([`BT_UNSORTED_MARK`]) the
/// not-yet-sorted mark of a lazily inserted node.
const BT_IDX_BASE: usize = 2;
/// Upstream `ZSTD_DUBT_UNSORTED_MARK`.
const BT_UNSORTED_MARK: u32 = 1;
/// A tree link "pointer" whose write is discarded (upstream `dummy32`).
const BT_DISCARD: usize = usize::MAX;

/// [`LazyFinder`] as the const parameter of the lazy monoliths: each
/// finder is compiled into its own monolith (the others fold away), so a
/// tier carries one parse per finder instead of three finders in every
/// parse; the chain and tree monoliths ignore `ROW_LOG` and are
/// instantiated once.
const FINDER_ROWS: u8 = 0;
const FINDER_CHAIN: u8 = 1;
const FINDER_TREE: u8 = 2;

/// The match finder the lazy parse searches (upstream `searchMethod_e`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LazyFinder {
    /// `ZSTD_RowFindBestMatch`: the SIMD-tagged rows.
    Rows,
    /// `ZSTD_HcFindBestMatch`: the hash chain (window of 2^14 or less).
    Chain,
    /// `ZSTD_BtFindBestMatch`: the lazily-sorted binary tree (btlazy2).
    Tree,
}

/// Per-parse view of the live history: raw base pointer + length + absolute
/// start, hoisted ONCE per block so the hot loops read no `Option` branch
/// and no `Vec` header per access. Valid for the whole parse: the history
/// buffer is never reallocated while a block is being scanned.
#[derive(Clone, Copy)]
struct RowScan {
    base: *const u8,
    len: usize,
    hist_start: usize,
    /// Block-constant search parameters hoisted with the view so the hot
    /// loops never re-read them through `&mut self`: the salt of the live
    /// row hash, the match-distance bound (upstream `maxDist`), the lowest
    /// valid match index (upstream `window.lowLimit`), the prefix start
    /// (upstream `dictLimit`: the dictionary lies below it), whether the
    /// dictionary is still valid for this block (upstream `loadedDictEnd !=
    /// 0`: no distance bound on the search) and whether it is attached
    /// (upstream `dictMatchState`, its own tables probed after the prefix).
    salt: u64,
    search_window: usize,
    low_limit: usize,
    prefix_start: usize,
    dict_frame: bool,
    attached: bool,
}

impl RowScan {
    /// Upstream `ZSTD_getLowestMatchIndex`: the whole history down to
    /// `lowLimit` while a dictionary is valid, else at most `maxDist` back.
    #[inline(always)]
    fn window_low(&self, pos: usize) -> usize {
        if self.dict_frame {
            self.low_limit
        } else {
            self.low_limit.max(pos.saturating_sub(self.search_window))
        }
    }
}

/// Upstream zstd `ZSTD_highbit32(offBase)` term of the lazy gain formula:
/// `offBase` is `offset + 3` for a real offset and 1 for repcode 1
/// (`off == 0` here), whose highbit is 0.
/// Upstream `ZSTD_highbit32` (`val != 0`).
#[inline(always)]
fn highbit32(val: u32) -> i64 {
    debug_assert!(val != 0);
    i64::from(31 - val.leading_zeros())
}

/// Upstream `offBase` of the best match so far in the binary-tree walks:
/// `999999999` (the lazy parse's "no match yet" value) before the first
/// accepted match, else `offset + 3` (`OFFSET_TO_OFFBASE`).
#[inline(always)]
fn bt_best_offbase(best_len: usize, best_off: usize) -> u32 {
    if best_len == 0 {
        999_999_999
    } else {
        (best_off + 3) as u32
    }
}

#[inline(always)]
fn offbase_highbit(off: usize) -> i64 {
    if off == 0 {
        0
    } else {
        i64::from(31 - ((off + 3) as u32).leading_zeros())
    }
}

/// Unaligned little-endian 4-byte read (upstream zstd `MEM_read32`).
///
/// # Safety
/// `p..p + 4` must be readable.
#[inline(always)]
unsafe fn rd32(p: *const u8) -> u32 {
    u32::from_le_bytes(unsafe { p.cast::<[u8; 4]>().read_unaligned() })
}

/// Immutable row-hash dictionary index (upstream zstd `ZSTD_RowFindBestMatch`'s
/// `dictMatchState` probe). Built once over the dictionary region and probed as
/// ONE fixed-width row (`<= row_entries` tag-matched candidates) AFTER the live
/// row, so the dict search is bounded (unlike a hash-chain walk) and never
/// re-indexed per frame. `positions` store CONCAT indices (history_start-
/// relative, floor-rebase-invariant); `ROW_EMPTY_SLOT = u32::MAX` marks empty.
#[derive(Debug, Default, Clone)]
pub(crate) struct RowDictTables {
    pub(crate) heads: Vec<u8>,
    pub(crate) positions: Vec<u32>,
    pub(crate) tags: Vec<u8>,
    /// Hash-chain form of the same index (upstream CDict `hashTable` /
    /// `chainTable`) when the dictionary's cParams resolve to the chain
    /// finder (concat-indexed positions, `ROW_EMPTY_SLOT` = empty), or its
    /// sorted binary tree when they resolve to btlazy2 (`concat +
    /// BT_IDX_BASE`, 0 = none).
    pub(crate) hc_hash: Vec<u32>,
    pub(crate) hc_chain: Vec<u32>,
    /// Geometry the index was built with (the CDict's cParams): `hash_log`
    /// (`hashLog`), `chain_log`, `row_log` (`clamp(searchLog, 4, 6)`); the
    /// row form uses `hash_log - row_log` row bits.
    pub(crate) hash_log: usize,
    pub(crate) chain_log: usize,
    pub(crate) row_log: usize,
    pub(crate) use_row: bool,
    pub(crate) use_bt: bool,
}
use super::match_table::helpers::{
    INCOMPRESSIBLE_SKIP_STEP, best_len_offset_candidate, extend_backwards_shared,
    repcode_candidate_shared,
};
use super::match_table::storage::REBASE_RESET_FLOOR_CEILING;
use super::opt::types::MatchCandidate;

// The row probe reuses the shared `fastpath::FastpathKernel` selection so each
// per-tier `#[target_feature]` probe can inline BOTH the tag-match mask AND the
// matching `fastpath::<tier>::common_prefix_len_ptr` (the tiers must share a
// feature set for the cpl to inline — `Sse42` is a superset of the SSE2 mask
// intrinsics, `Avx2Bmi2` of the AVX2 mask). `select_kernel()` does the runtime
// detect once per process via a `OnceLock`.
use super::fastpath::FastpathKernel;

/// Compile-time row tag-match kernel. Each ZST monomorphises the per-row
/// tag compare so the search hot loop drops the runtime `RowTagKernel`
/// enum branch (one predictable branch + all-tiers' inlined SIMD bodies
/// per position become a single tier's body, no branch). The bare
/// dispatchers select the impl once per block from the runtime-detected
/// `tag_kernel`, so an impl is only instantiated/used on a CPU that
/// supports its ISA — the same contract `RowTagKernel::detect` upholds for
/// the enum's `unsafe` SIMD calls.
pub(crate) trait RowTags: Copy {
    /// Run the row match probe (live row + dict dual-probe) for this kernel.
    /// Forwards to the matcher's per-tier `#[target_feature]` probe method whose
    /// body expands the tier's `row_tag_mask_*!` SIMD inline (no function call),
    /// so the vector tag-match inlines straight-line under the kernel's feature
    /// umbrella instead of crossing the `#[target_feature]` ABI boundary on every
    /// probe (which it does even for baseline NEON/SSE2 — see `fastpath` module
    /// docs). Runtime kernel selection happens once at the `dispatch_tag_kernel!`
    /// site, never inside the per-position hot loop.
    ///
    /// # Safety
    /// The caller (via `dispatch_tag_kernel!`) only selects a kernel whose ISA
    /// `RowTagKernel::detect` confirmed present, upholding the per-tier
    /// `#[target_feature]` contract.
    unsafe fn probe<const ROW_LOG: usize>(
        matcher: &RowMatchGenerator,
        abs_pos: usize,
        lit_len: usize,
        hash: Option<(usize, u8)>,
    ) -> Option<MatchCandidate>;
}

// On wasm32+simd128 the row tier is the compile-time `Simd128Tags`, so the
// scalar fallback ZST is never constructed there (it stays the fallback on
// every other target, and on scalar-only wasm builds).
#[cfg_attr(
    all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ),
    allow(dead_code)
)]
#[derive(Copy, Clone)]
struct ScalarTags;
impl RowTags for ScalarTags {
    #[inline]
    unsafe fn probe<const ROW_LOG: usize>(
        matcher: &RowMatchGenerator,
        abs_pos: usize,
        lit_len: usize,
        hash: Option<(usize, u8)>,
    ) -> Option<MatchCandidate> {
        // Scalar has no target feature; the probe body runs as-is.
        unsafe { matcher.row_probe_scalar::<ROW_LOG>(abs_pos, lit_len, hash) }
    }
}

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
#[derive(Copy, Clone)]
struct Sse42Tags;
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
impl RowTags for Sse42Tags {
    #[inline]
    unsafe fn probe<const ROW_LOG: usize>(
        matcher: &RowMatchGenerator,
        abs_pos: usize,
        lit_len: usize,
        hash: Option<(usize, u8)>,
    ) -> Option<MatchCandidate> {
        // SAFETY: dispatched only when `tag_kernel == Sse42` (SSE4.2 confirmed).
        unsafe { matcher.row_probe_sse42::<ROW_LOG>(abs_pos, lit_len, hash) }
    }
}

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
#[derive(Copy, Clone)]
struct Avx2Bmi2Tags;
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
impl RowTags for Avx2Bmi2Tags {
    #[inline]
    unsafe fn probe<const ROW_LOG: usize>(
        matcher: &RowMatchGenerator,
        abs_pos: usize,
        lit_len: usize,
        hash: Option<(usize, u8)>,
    ) -> Option<MatchCandidate> {
        // SAFETY: dispatched only when `tag_kernel == Avx2Bmi2` (AVX2+BMI2 confirmed).
        unsafe { matcher.row_probe_avx2bmi2::<ROW_LOG>(abs_pos, lit_len, hash) }
    }
}

#[cfg(all(
    target_arch = "aarch64",
    target_endian = "little",
    feature = "kernel-neon"
))]
#[derive(Copy, Clone)]
struct NeonTags;
#[cfg(all(
    target_arch = "aarch64",
    target_endian = "little",
    feature = "kernel-neon"
))]
impl RowTags for NeonTags {
    #[inline]
    unsafe fn probe<const ROW_LOG: usize>(
        matcher: &RowMatchGenerator,
        abs_pos: usize,
        lit_len: usize,
        hash: Option<(usize, u8)>,
    ) -> Option<MatchCandidate> {
        // SAFETY: dispatched only when `tag_kernel == Neon` (NEON confirmed).
        unsafe { matcher.row_probe_neon::<ROW_LOG>(abs_pos, lit_len, hash) }
    }
}

// WebAssembly fixed-128-bit SIMD tier. wasm has no runtime CPUID, so this is
// compile-time only: present (and dispatched-to) exactly when the build enables
// `simd128`, selected directly in `dispatch_tag_kernel!` rather than via the
// runtime `FastpathKernel` (which carries no wasm tier).
#[cfg(all(
    target_arch = "wasm32",
    target_feature = "simd128",
    feature = "kernel-simd128"
))]
#[derive(Copy, Clone)]
struct Simd128Tags;
#[cfg(all(
    target_arch = "wasm32",
    target_feature = "simd128",
    feature = "kernel-simd128"
))]
impl RowTags for Simd128Tags {
    #[inline]
    unsafe fn probe<const ROW_LOG: usize>(
        matcher: &RowMatchGenerator,
        abs_pos: usize,
        lit_len: usize,
        hash: Option<(usize, u8)>,
    ) -> Option<MatchCandidate> {
        // wasm simd128 is compile-time; `row_probe_simd128` needs no
        // `#[target_feature]` and the intrinsics inline directly.
        unsafe { matcher.row_probe_simd128::<ROW_LOG>(abs_pos, lit_len, hash) }
    }
}

/// Resolve the runtime `tag_kernel` (`FastpathKernel`) to a `RowTags` ZST once,
/// then call a `*_k::<K>` method that binds the `row_log` const. The kernel
/// branch runs once per block (cold), so the per-position hot loop is fully
/// monomorphised over the selected tier — no runtime kernel enum inside.
macro_rules! dispatch_tag_kernel {
    ($self:ident . $k_method:ident ( $($arg:expr),* )) => {{
        // wasm32 has no runtime CPUID: when the build enables `simd128`, the
        // tier is resolved at compile time straight to `Simd128Tags`, so the
        // runtime `FastpathKernel` match (native-only) is cfg'd out entirely.
        #[cfg(all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        ))]
        {
            $self.$k_method::<Simd128Tags>($($arg),*)
        }
        #[cfg(not(all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        )))]
        {
            match $self.tag_kernel {
                #[cfg(all(
                    any(target_arch = "x86", target_arch = "x86_64"),
                    feature = "kernel-avx2"
                ))]
                FastpathKernel::Avx2Bmi2 => $self.$k_method::<Avx2Bmi2Tags>($($arg),*),
                #[cfg(all(
                    any(target_arch = "x86", target_arch = "x86_64"),
                    feature = "kernel-sse"
                ))]
                FastpathKernel::Sse2 | FastpathKernel::Sse42 => {
                    $self.$k_method::<Sse42Tags>($($arg),*)
                }
                #[cfg(all(
                    target_arch = "aarch64",
                    target_endian = "little",
                    feature = "kernel-neon"
                ))]
                FastpathKernel::Neon => $self.$k_method::<NeonTags>($($arg),*),
                FastpathKernel::Scalar => $self.$k_method::<ScalarTags>($($arg),*),
            }
        }
    }};
}

/// Upstream zstd `ZSTD_RowFindBestMatch` (zstd_lazy.c:1141) at one position:
/// the row is walked newest-first until the first entry below the window
/// floor, the in-window candidates are buffered (and their data prefetched),
/// then each is gated by a 4-byte compare at the current best length
/// (`match + ml - 3`) before the full count. Lengths are FORWARD only (the
/// catch-up belongs to the parse). Evaluates to `(ml, offset)`: `ml == 3`
/// means nothing found (upstream's `ml = 4-1`), `offset` is the distance.
/// Leftover attempts fund the dictionary row (upstream `dictMatchState`).
macro_rules! row_find_best_match {
    ($m:expr, $ctx:ident, $abs_pos:expr, $row:expr, $tag:expr, $rl:expr, $use_mask:literal, $maskmac:ident, $cpl:path) => {{
        let hist_start = $ctx.hist_start;
        let cur_idx = $abs_pos - hist_start;
        // SAFETY: the parse searches only positions >= 16 bytes before the
        // block end, so `cur_idx + 16 <= $ctx.len`.
        let cur_ptr = unsafe { $ctx.base.add(cur_idx) };
        let limit = $ctx.len - cur_idx;
        let row_entries = 1usize << $rl;
        let row_mask = row_entries - 1;
        let row_base = $row << $rl;
        // SAFETY (table indexing below): `$row < row_heads.len()` and
        // `row_base + row_entries <= row_tags.len() == row_positions.len()`
        // by the `ensure_tables` sizing (`row` is masked to `row_hash_log`
        // bits, `row_log == $rl`), the same argument as `insert_at`.
        debug_assert_eq!($rl, $m.row_log);
        debug_assert!(row_base + row_entries <= $m.row_positions.len());
        let head = unsafe { *$m.row_heads.get_unchecked($row) } as usize;
        let window_low = $ctx.window_low($abs_pos);
        let budget = $m.search_depth.min(row_entries);
        let mut attempts = budget;
        let mut ml = 3usize;
        let mut best_off = 0usize;
        let entries_bits: u64 = if row_entries >= 64 {
            u64::MAX
        } else {
            (1u64 << row_entries) - 1
        };
        let mut buf = [0u32; 64];
        let mut n = 0usize;
        {
            #[allow(unused_mut)]
            let mut pending: u64 = if $use_mask {
                // SAFETY: see the table-indexing note above.
                let tags = unsafe {
                    core::slice::from_raw_parts($m.row_tags.as_ptr().add(row_base), row_entries)
                };
                let m = $maskmac!(tags, $tag) & entries_bits;
                if head == 0 {
                    m
                } else {
                    ((m >> head) | (m << (row_entries - head))) & entries_bits
                }
            } else {
                0
            };
            #[allow(unused_mut)]
            let mut scan = 0usize;
            while attempts > 0 {
                let slot_opt = if $use_mask {
                    if pending == 0 {
                        None
                    } else {
                        let i = pending.trailing_zeros() as usize;
                        pending &= pending - 1;
                        Some((head + i) & row_mask)
                    }
                } else {
                    let mut found = None;
                    while scan < row_entries {
                        let s = (head + scan) & row_mask;
                        scan += 1;
                        if $m.row_tags[row_base + s] == $tag {
                            found = Some(s);
                            break;
                        }
                    }
                    found
                };
                let Some(slot) = slot_opt else { break };
                // Upstream `if (matchPos == 0) continue`: slot 0 is never an
                // entry (the tag byte there is upstream's head), and the
                // skip does not spend an attempt.
                if slot == 0 {
                    continue;
                }
                // SAFETY: `slot < row_entries`; see the table-indexing note.
                let raw = unsafe { *$m.row_positions.get_unchecked(row_base + slot) };
                // Newest-first order: the first empty / below-floor entry ends
                // the walk (upstream `if (matchIndex < lowLimit) break`).
                if raw == ROW_EMPTY_SLOT || (raw as usize) < window_low {
                    break;
                }
                if (raw as usize) >= $abs_pos {
                    continue;
                }
                #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
                {
                    #[cfg(target_arch = "x86")]
                    use core::arch::x86::{_MM_HINT_T0, _mm_prefetch};
                    #[cfg(target_arch = "x86_64")]
                    use core::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
                    // SAFETY: prefetch is a hint; the candidate lies in the live
                    // history.
                    unsafe {
                        _mm_prefetch($ctx.base.add(raw as usize - hist_start).cast(), _MM_HINT_T0);
                    }
                }
                // SAFETY: `n < attempts_at_entry <= row_entries <= 64`.
                unsafe { *buf.get_unchecked_mut(n) = raw };
                n += 1;
                attempts -= 1;
            }
        }
        // A copied dictionary is upstream's `extDict` segment: its candidates
        // are gated at the match head (`MEM_read32(match) == MEM_read32(ip)`)
        // instead of at the current best length.
        let prefix_start = $ctx.prefix_start;
        for &raw in &buf[..n] {
            let cand_idx = raw as usize - hist_start;
            // SAFETY: `cand_idx < cur_idx`; the gate reads 4 bytes at
            // `+ ml - 3` where `ml + 1 <= limit` (the count breaks out of the
            // loop at `ml == limit`), so both reads stay inside `concat`.
            unsafe {
                let cand_ptr = $ctx.base.add(cand_idx);
                let gate = if (raw as usize) >= prefix_start {
                    rd32(cand_ptr.add(ml - 3)) == rd32(cur_ptr.add(ml - 3))
                } else {
                    rd32(cand_ptr) == rd32(cur_ptr)
                };
                if gate {
                    let cml = $cpl(cand_ptr, cur_ptr, limit);
                    if cml > ml {
                        ml = cml;
                        best_off = $abs_pos - raw as usize;
                        if cml == limit {
                            break;
                        }
                    }
                }
            }
        }
        // Attached dictionary (upstream `dictMatchState`): ONE bounded row of
        // the CDict's own tables, addressed by the unsalted hash of the
        // position at the dictionary's row width (`ZSTD_hashPtr(ip,
        // dms->rowHashLog + 8, mls)`), walked with the attempts the live row
        // left (`nbAttempts` is shared), each entry gated by a 4-byte compare
        // at the match head. `dict.row_log == $rl` (the frame keeps the
        // CDict's `searchLog`).
        if ml < limit
            && attempts > 0
            && $ctx.attached
            && let Some(dict) = $m.dict.table()
            && dict.use_row
        {
            debug_assert_eq!(dict.row_log, $rl);
            let mut dattempts = attempts;
            let dict_end = $m.dict.region_len();
            let dict_row_hash_log = dict.hash_log - $rl;
            // SAFETY: `cur_idx + 16 <= $ctx.len`.
            let dcombined = unsafe {
                RowMatchGenerator::key_hash_raw(
                    $ctx.base,
                    $ctx.len,
                    cur_idx,
                    $m.row_hash_mls,
                    dict_row_hash_log + ROW_TAG_BITS,
                    0,
                )
            };
            let drow = ((dcombined >> ROW_TAG_BITS) as usize) & ((1usize << dict_row_hash_log) - 1);
            let dtag = dcombined as u8;
            let drow_base = drow << $rl;
            let dhead = dict.heads[drow] as usize;
            #[allow(unused_mut)]
            let mut dpending: u64 = if $use_mask {
                let m =
                    $maskmac!(&dict.tags[drow_base..drow_base + row_entries], dtag) & entries_bits;
                if dhead == 0 {
                    m
                } else {
                    ((m >> dhead) | (m << (row_entries - dhead))) & entries_bits
                }
            } else {
                0
            };
            #[allow(unused_mut)]
            let mut dscan = 0usize;
            while dattempts > 0 {
                let slot_opt = if $use_mask {
                    if dpending == 0 {
                        None
                    } else {
                        let i = dpending.trailing_zeros() as usize;
                        dpending &= dpending - 1;
                        Some((dhead + i) & row_mask)
                    }
                } else {
                    let mut found = None;
                    while dscan < row_entries {
                        let s = (dhead + dscan) & row_mask;
                        dscan += 1;
                        if dict.tags[drow_base + s] == dtag {
                            found = Some(s);
                            break;
                        }
                    }
                    found
                };
                let Some(slot) = slot_opt else { break };
                if slot == 0 {
                    continue;
                }
                let dp = dict.positions[drow_base + slot];
                if dp == ROW_EMPTY_SLOT {
                    break;
                }
                dattempts -= 1;
                let dp = dp as usize;
                debug_assert!(dp + 8 <= dict_end);
                // SAFETY: `dp + 8 <= dict_end <= cur_idx`, `cur_idx + 4 <= $ctx.len`.
                unsafe {
                    let dptr = $ctx.base.add(dp);
                    if rd32(dptr) == rd32(cur_ptr) {
                        let cml = $cpl(dptr, cur_ptr, limit);
                        if cml > ml {
                            ml = cml;
                            best_off = $abs_pos - (hist_start + dp);
                            if cml == limit {
                                break;
                            }
                        }
                    }
                }
            }
        }
        (ml, best_off)
    }};
}

/// Upstream zstd `ZSTD_searchMax` for the row lazy parse: index the not yet
/// indexed gap up to `$p` (skipped while lazy-skipping; a long gap keeps only
/// its 96 head + 32 tail positions), search `$p`, then index `$p` itself.
/// `$ntu` is the parse's `nextToUpdate` local, `$skip` its `lazySkipping`.
macro_rules! lazy_search_at {
    ($m:expr, $ctx:ident, $p:expr, $ntu:ident, $skip:ident, $cache:ident, $rl:expr, $finder:expr, $use_mask:literal, $maskmac:ident, $cpl:path) => {{
        let p = $p;
        if $finder == FINDER_TREE {
            dubt_find_best_match!($m, $ctx, p, $ntu, $cpl)
        } else if $finder == FINDER_CHAIN {
            hc_find_best_match!($m, $ctx, p, $ntu, $skip, $cpl)
        } else {
            let (row, tag) = if $skip {
                // Lazy skipping: the skipped-over gap is not indexed and the cache
                // is not maintained; the position is hashed directly.
                row_cache_hash!($m, $ctx, $rl, p)
            } else {
                if p - $ntu > ROW_UPDATE_SKIP_THRESHOLD {
                    row_cache_insert_range!(
                        $m,
                        $ctx,
                        $cache,
                        $rl,
                        $ntu,
                        $ntu + ROW_UPDATE_MAX_START
                    );
                    row_cache_fill!($m, $ctx, $cache, $rl, p - ROW_UPDATE_MAX_END);
                    row_cache_insert_range!($m, $ctx, $cache, $rl, p - ROW_UPDATE_MAX_END, p);
                } else {
                    row_cache_insert_range!($m, $ctx, $cache, $rl, $ntu, p);
                }
                row_cache_next!($m, $ctx, $cache, $rl, p)
            };
            let r = if row != ROW_CACHE_NONE {
                let row = row as usize;
                let r = row_find_best_match!($m, $ctx, p, row, tag, $rl, $use_mask, $maskmac, $cpl);
                $m.insert_at::<$rl>(p, row, tag);
                r
            } else {
                (3usize, 0usize)
            };
            $ntu = p + 1;
            r
        }
    }};
}

/// Upstream zstd `ZSTD_HcFindBestMatch` (zstd_lazy.c:667, noDict) with its
/// `ZSTD_insertAndFindFirstIndex_internal` update: every position of the
/// gap `[$ntu, p)` is chained (only the first one while lazy-skipping), then
/// the chain of `p`'s bucket is walked newest-first, each link gated by the
/// 4-byte compare at the current best length, at most `search_depth` links
/// and never past `p - chainSize`. `p` itself is chained by the next
/// search's gap, as upstream. Evaluates to `(ml, offset)` like
/// `row_find_best_match!`.
macro_rules! hc_find_best_match {
    ($m:expr, $ctx:ident, $p:expr, $ntu:ident, $skip:ident, $cpl:path) => {{
        let p = $p;
        let hist_start = $ctx.hist_start;
        let chain_mask = (1usize << $m.hc_chain_log) - 1;
        let cur_idx = p - hist_start;
        // SAFETY: the parse searches only positions >= 16 bytes before the
        // block end.
        let cur_ptr = unsafe { $ctx.base.add(cur_idx) };
        let limit = $ctx.len - cur_idx;
        {
            let mut idx = $ntu;
            while idx < p {
                let h = $m.hc_hash_at($ctx, idx);
                $m.hc_chain[idx & chain_mask] = $m.hc_hash[h];
                $m.hc_hash[h] = idx as u32;
                idx += 1;
                if $skip {
                    break;
                }
            }
            $ntu = p;
        }
        let window_low = $ctx.window_low(p);
        let min_chain = p.saturating_sub(chain_mask + 1);
        let mut attempts = $m.search_depth;
        let mut ml = 3usize;
        let mut best_off = 0usize;
        // A copied dictionary is upstream's `extDict` segment: its candidates
        // are gated at the match head instead of at the current best length.
        let prefix_start = $ctx.prefix_start;
        let mut match_index = $m.hc_hash[$m.hc_hash_at($ctx, p)];
        while match_index != ROW_EMPTY_SLOT && (match_index as usize) >= window_low && attempts > 0
        {
            let cand = match_index as usize;
            // The floor advances past every indexed position between frames,
            // so a link never points at or past the searched position.
            debug_assert!(cand < p);
            // SAFETY: `cand < p`; the gate reads 4 bytes at `+ ml - 3` with
            // `ml + 1 <= limit` (the count breaks out at `ml == limit`).
            unsafe {
                let cand_ptr = $ctx.base.add(cand - hist_start);
                let gate = if cand >= prefix_start {
                    rd32(cand_ptr.add(ml - 3)) == rd32(cur_ptr.add(ml - 3))
                } else {
                    rd32(cand_ptr) == rd32(cur_ptr)
                };
                if gate {
                    let cml = $cpl(cand_ptr, cur_ptr, limit);
                    if cml > ml {
                        ml = cml;
                        best_off = p - cand;
                        if cml == limit {
                            break;
                        }
                    }
                }
            }
            if cand <= min_chain {
                break;
            }
            match_index = $m.hc_chain[cand & chain_mask];
            attempts -= 1;
        }
        // Attached dictionary (upstream `dictMatchState`): the CDict's own
        // chain, entered through its hash table at its `hashLog` (unsalted,
        // the frame's key width), walked with the remaining attempts, each
        // link gated by a 4-byte compare at the match head and never past
        // `dictSize - chainSize`.
        if ml < limit
            && attempts > 0
            && $ctx.attached
            && let Some(dict) = $m.dict.table()
            && !dict.use_row
            && !dict.use_bt
        {
            let dict_end = $m.dict.region_len();
            let dchain_mask = (1usize << dict.chain_log) - 1;
            let dmin_chain = dict_end.saturating_sub(dchain_mask + 1);
            // SAFETY: `cur_idx + 16 <= $ctx.len`.
            let dh = unsafe {
                RowMatchGenerator::key_hash_raw(
                    $ctx.base,
                    $ctx.len,
                    cur_idx,
                    $m.row_hash_mls,
                    dict.hash_log,
                    0,
                )
            } as usize;
            let mut dmi = dict.hc_hash[dh];
            while dmi != ROW_EMPTY_SLOT && attempts > 0 {
                let dp = dmi as usize;
                debug_assert!(dp + 8 <= dict_end);
                // SAFETY: `dp + 8 <= dict_end <= cur_idx`, `cur_idx + 4 <= $ctx.len`.
                unsafe {
                    let dptr = $ctx.base.add(dp);
                    if rd32(dptr) == rd32(cur_ptr) {
                        let cml = $cpl(dptr, cur_ptr, limit);
                        if cml > ml {
                            ml = cml;
                            best_off = p - (hist_start + dp);
                            if cml == limit {
                                break;
                            }
                        }
                    }
                }
                if dp <= dmin_chain {
                    break;
                }
                dmi = dict.hc_chain[dp & dchain_mask];
                attempts -= 1;
            }
        }
        (ml, best_off)
    }};
}

/// Upstream zstd `ZSTD_updateDUBT`: link every position of `[$from, $to)`
/// into its hash bucket as an UNSORTED tree node (the bucket head becomes
/// the node's next-candidate link, its other link the sort mark); the next
/// search of that bucket sorts them in one batch (`dubt_insert1!`).
macro_rules! dubt_update {
    ($m:expr, $ctx:ident, $from:expr, $to:expr) => {{
        let bt_mask = (1usize << ($m.hc_chain_log - 1)) - 1;
        let mut idx = $from;
        while idx < $to {
            let h = $m.hc_hash_at($ctx, idx);
            let ci = idx + BT_IDX_BASE;
            let node = 2 * (ci & bt_mask);
            $m.hc_chain[node] = $m.hc_hash[h];
            $m.hc_chain[node + 1] = BT_UNSORTED_MARK;
            $m.hc_hash[h] = ci as u32;
            idx += 1;
        }
    }};
}

/// Upstream zstd `ZSTD_insertDUBT1`: sort one linked-but-unsorted node
/// `$curr` (tree index) into the sorted tree below it, at most
/// `$nb_compares` compares, never past `$bt_low`; the sort walk is bounded
/// by the plain window distance (upstream reads `lowLimit` / `maxDist` here,
/// not the dictionary rule). A node whose match runs to the block end is
/// dropped from the tree (upstream: "no way to know if inf or sup").
macro_rules! dubt_insert1 {
    ($m:expr, $ctx:ident, $curr:expr, $nb_compares:expr, $bt_low:expr, $cpl:path) => {{
        let bt_mask = (1usize << ($m.hc_chain_log - 1)) - 1;
        let hist_start = $ctx.hist_start;
        let curr: usize = $curr;
        let cur_abs = curr - BT_IDX_BASE;
        debug_assert!(cur_abs >= $ctx.low_limit);
        // The node's bytes must still be resident: eviction raises
        // `low_limit` past everything it drops, and the sort floors on it.
        debug_assert!(
            cur_abs >= hist_start && cur_abs - hist_start <= $ctx.len,
            "sorting a node whose bytes were evicted (abs {cur_abs}, history starts {hist_start})",
        );
        let cur_idx = cur_abs - hist_start;
        // SAFETY: `cur_abs` was linked by `dubt_update!` from a searched
        // position of this or an earlier block, so it lies in the live
        // history at least 9 bytes before the block end; every compare
        // below stops at `iend_rem`.
        let ip = unsafe { $ctx.base.add(cur_idx) };
        let iend_rem = $ctx.len - cur_idx;
        let window_low = $ctx
            .low_limit
            .max(cur_abs.saturating_sub($ctx.search_window))
            + BT_IDX_BASE;
        let mut smaller_ptr = 2 * (curr & bt_mask);
        let mut larger_ptr = smaller_ptr + 1;
        let mut match_index = $m.hc_chain[smaller_ptr] as usize;
        let mut common_smaller = 0usize;
        let mut common_larger = 0usize;
        let mut nb = $nb_compares;
        while nb > 0 && match_index > window_low {
            // Sorted nodes hold only earlier positions.
            debug_assert!(match_index < curr, "sort walk reached a future node");
            let next_ptr = 2 * (match_index & bt_mask);
            let mut ml = common_smaller.min(common_larger);
            let m_idx = match_index - BT_IDX_BASE - hist_start;
            // SAFETY: `m_idx < cur_idx` (an older node above the window
            // floor); the reads stop at `iend_rem`.
            let (smaller, at_end) = unsafe {
                let mptr = $ctx.base.add(m_idx);
                ml += $cpl(mptr.add(ml), ip.add(ml), iend_rem - ml);
                if ml == iend_rem {
                    (false, true)
                } else {
                    (*mptr.add(ml) < *ip.add(ml), false)
                }
            };
            if at_end {
                break;
            }
            if smaller {
                $m.hc_chain[smaller_ptr] = match_index as u32;
                common_smaller = ml;
                if match_index <= $bt_low {
                    smaller_ptr = BT_DISCARD;
                    break;
                }
                smaller_ptr = next_ptr + 1;
                match_index = $m.hc_chain[next_ptr + 1] as usize;
            } else {
                $m.hc_chain[larger_ptr] = match_index as u32;
                common_larger = ml;
                if match_index <= $bt_low {
                    larger_ptr = BT_DISCARD;
                    break;
                }
                larger_ptr = next_ptr;
                match_index = $m.hc_chain[next_ptr] as usize;
            }
            nb -= 1;
        }
        if smaller_ptr != BT_DISCARD {
            $m.hc_chain[smaller_ptr] = 0;
        }
        if larger_ptr != BT_DISCARD {
            $m.hc_chain[larger_ptr] = 0;
        }
    }};
}

/// Upstream zstd `ZSTD_BtFindBestMatch` (`ZSTD_updateDUBT` +
/// `ZSTD_DUBT_findBestMatch`, plus `ZSTD_DUBT_findBetterDictMatch` over an
/// attached dictionary's sorted tree): a position inside the skipped area
/// (`ip < nextToUpdate`) is not searched; else the gap `[$ntu, p)` is linked
/// unsorted, the bucket's unsorted nodes are sorted in one batch, the tree
/// is walked for the longest match (a longer match replaces the best one
/// only past the upstream offset-cost margin), `p` is inserted on the way,
/// and `nextToUpdate` jumps past the longest match seen (`matchEndIdx - 8`).
/// Evaluates to `(ml, offset)`; `ml < 4` means nothing found.
macro_rules! dubt_find_best_match {
    ($m:expr, $ctx:ident, $p:expr, $ntu:ident, $cpl:path) => {{
        let p = $p;
        if p < $ntu {
            (0usize, 0usize)
        } else {
            dubt_update!($m, $ctx, $ntu, p);
            let hist_start = $ctx.hist_start;
            let bt_mask = (1usize << ($m.hc_chain_log - 1)) - 1;
            let cur_idx = p - hist_start;
            // SAFETY: the parse searches only positions >= 8 bytes before
            // the block end.
            let ip = unsafe { $ctx.base.add(cur_idx) };
            let limit = $ctx.len - cur_idx;
            let h = $m.hc_hash_at($ctx, p);
            let curr = p + BT_IDX_BASE;
            let window_low = $ctx.window_low(p) + BT_IDX_BASE;
            let bt_low = curr.saturating_sub(bt_mask);
            let unsort_limit = bt_low.max(window_low);
            let mut match_index = $m.hc_hash[h] as usize;
            let mut next_candidate = 2 * (match_index & bt_mask);
            let mut unsorted_mark = next_candidate + 1;
            let mut nb_compares = $m.search_depth;
            let mut nb_candidates = nb_compares;
            let mut previous_candidate = 0usize;
            // Reach the end of the unsorted candidate list, reversing it
            // through the sort-mark slots.
            while match_index > unsort_limit
                && $m.hc_chain[unsorted_mark] == BT_UNSORTED_MARK
                && nb_candidates > 1
            {
                $m.hc_chain[unsorted_mark] = previous_candidate as u32;
                previous_candidate = match_index;
                match_index = $m.hc_chain[next_candidate] as usize;
                next_candidate = 2 * (match_index & bt_mask);
                unsorted_mark = next_candidate + 1;
                nb_candidates -= 1;
            }
            // Nullify the last candidate if it is still unsorted (upstream:
            // costs a little ratio, buys speed).
            if match_index > unsort_limit && $m.hc_chain[unsorted_mark] == BT_UNSORTED_MARK {
                $m.hc_chain[next_candidate] = 0;
                $m.hc_chain[unsorted_mark] = 0;
            }
            // Batch-sort the stacked candidates, oldest first.
            match_index = previous_candidate;
            while match_index != 0 {
                let next_idx = $m.hc_chain[2 * (match_index & bt_mask) + 1] as usize;
                dubt_insert1!($m, $ctx, match_index, nb_candidates, unsort_limit, $cpl);
                match_index = next_idx;
                nb_candidates += 1;
            }
            // Find the longest match, inserting `p` on the way.
            let mut common_smaller = 0usize;
            let mut common_larger = 0usize;
            let mut smaller_ptr = 2 * (curr & bt_mask);
            let mut larger_ptr = smaller_ptr + 1;
            let mut match_end_idx = curr + 8 + 1;
            let mut best_len = 0usize;
            let mut best_off = 0usize;
            match_index = $m.hc_hash[h] as usize;
            $m.hc_hash[h] = curr as u32;
            while nb_compares > 0 && match_index > window_low {
                // Tree nodes hold only earlier positions.
                debug_assert!(match_index < curr, "tree walk reached a future node");
                let next_ptr = 2 * (match_index & bt_mask);
                let mut ml = common_smaller.min(common_larger);
                let m_idx = match_index - BT_IDX_BASE - hist_start;
                // SAFETY: `m_idx < cur_idx` (an older node above the window
                // floor); the reads stop at `limit`.
                let (smaller, at_end) = unsafe {
                    let mptr = $ctx.base.add(m_idx);
                    ml += $cpl(mptr.add(ml), ip.add(ml), limit - ml);
                    if ml == limit {
                        (false, true)
                    } else {
                        (*mptr.add(ml) < *ip.add(ml), false)
                    }
                };
                if ml > best_len {
                    if ml > match_end_idx - match_index {
                        match_end_idx = match_index + ml;
                    }
                    if 4 * (ml as i64 - best_len as i64)
                        > highbit32((curr - match_index + 1) as u32)
                            - highbit32(bt_best_offbase(best_len, best_off))
                    {
                        best_len = ml;
                        best_off = curr - match_index;
                    }
                    if at_end {
                        // Upstream: no further compares, the dictionary
                        // tree is not probed either.
                        nb_compares = 0;
                        break;
                    }
                }
                if at_end {
                    break;
                }
                if smaller {
                    $m.hc_chain[smaller_ptr] = match_index as u32;
                    common_smaller = ml;
                    if match_index <= bt_low {
                        smaller_ptr = BT_DISCARD;
                        break;
                    }
                    smaller_ptr = next_ptr + 1;
                    match_index = $m.hc_chain[next_ptr + 1] as usize;
                } else {
                    $m.hc_chain[larger_ptr] = match_index as u32;
                    common_larger = ml;
                    if match_index <= bt_low {
                        larger_ptr = BT_DISCARD;
                        break;
                    }
                    larger_ptr = next_ptr;
                    match_index = $m.hc_chain[next_ptr] as usize;
                }
                nb_compares -= 1;
            }
            if smaller_ptr != BT_DISCARD {
                $m.hc_chain[smaller_ptr] = 0;
            }
            if larger_ptr != BT_DISCARD {
                $m.hc_chain[larger_ptr] = 0;
            }
            // Attached dictionary (upstream `dictMatchState`): walk the
            // CDict's sorted tree with the compares left, entered through
            // its hash table at its `hashLog`; the offset-cost margin reads
            // `offBase + 1` there.
            if $ctx.attached
                && nb_compares > 0
                && let Some(dict) = $m.dict.table()
                && dict.use_bt
            {
                let dict_end = $m.dict.region_len();
                let dbt_mask = (1usize << (dict.chain_log - 1)) - 1;
                let dhigh = dict_end + BT_IDX_BASE;
                let dlow = BT_IDX_BASE;
                let dbt_low = if dbt_mask >= dhigh - dlow {
                    dlow
                } else {
                    dhigh - dbt_mask
                };
                // SAFETY: `cur_idx + 8 <= $ctx.len`.
                let dh = unsafe {
                    RowMatchGenerator::key_hash_raw(
                        $ctx.base,
                        $ctx.len,
                        cur_idx,
                        $m.row_hash_mls,
                        dict.hash_log,
                        0,
                    )
                } as usize;
                let mut dmi = dict.hc_hash[dh] as usize;
                let mut common_smaller = 0usize;
                let mut common_larger = 0usize;
                while nb_compares > 0 && dmi > dlow {
                    let next_ptr = 2 * (dmi & dbt_mask);
                    let mut ml = common_smaller.min(common_larger);
                    let d_concat = dmi - BT_IDX_BASE;
                    debug_assert!(d_concat < dict_end);
                    // SAFETY: the dictionary occupies concat `[0, dict_end)`
                    // below the prefix; the reads stop at `limit`.
                    let (smaller, at_end) = unsafe {
                        let mptr = $ctx.base.add(d_concat);
                        ml += $cpl(mptr.add(ml), ip.add(ml), limit - ml);
                        if ml == limit {
                            (false, true)
                        } else {
                            (*mptr.add(ml) < *ip.add(ml), false)
                        }
                    };
                    if ml > best_len {
                        let dist = p - (hist_start + d_concat);
                        if 4 * (ml as i64 - best_len as i64)
                            > highbit32((dist + 1) as u32)
                                - highbit32(bt_best_offbase(best_len, best_off) + 1)
                        {
                            best_len = ml;
                            best_off = dist;
                        }
                        if at_end {
                            break;
                        }
                    }
                    if at_end {
                        break;
                    }
                    if dmi <= dbt_low {
                        break;
                    }
                    if smaller {
                        common_smaller = ml;
                        dmi = dict.hc_chain[next_ptr + 1] as usize;
                    } else {
                        common_larger = ml;
                        dmi = dict.hc_chain[next_ptr] as usize;
                    }
                    nb_compares -= 1;
                }
            }
            debug_assert!(match_end_idx >= 8 + BT_IDX_BASE);
            $ntu = match_end_idx - 8 - BT_IDX_BASE;
            (best_len, best_off)
        }
    }};
}

/// Hash one position for the row cache and prefetch its row (upstream zstd
/// `ZSTD_row_fillHashCache` / `ZSTD_row_nextCachedHash` body): `(row, tag)`
/// with `row == ROW_CACHE_NONE` past the hashable end.
macro_rules! row_cache_hash {
    ($m:expr, $ctx:ident, $rl:expr, $pos:expr) => {{
        match $m.row_hash_at($ctx, $pos) {
            Some((row, tag)) => {
                $m.prefetch_row::<$rl>(row);
                (row as u32, tag)
            }
            None => (ROW_CACHE_NONE, 0u8),
        }
    }};
}

/// Upstream zstd `ZSTD_row_fillHashCache`: (re)load the cache for the
/// positions `$from .. $from + ROW_HASH_CACHE_SIZE`.
macro_rules! row_cache_fill {
    ($m:expr, $ctx:ident, $cache:ident, $rl:expr, $from:expr) => {{
        let from = $from;
        for pos in from..from + ROW_HASH_CACHE_SIZE {
            $cache[pos & (ROW_HASH_CACHE_SIZE - 1)] = row_cache_hash!($m, $ctx, $rl, pos);
        }
    }};
}

/// Upstream zstd `ZSTD_row_nextCachedHash`: take `$pos`'s cached hash and
/// replace it with the hash of `$pos + ROW_HASH_CACHE_SIZE` (prefetching
/// that row). Valid only while positions are consumed in sequence from the
/// last fill, exactly as upstream.
macro_rules! row_cache_next {
    ($m:expr, $ctx:ident, $cache:ident, $rl:expr, $pos:expr) => {{
        let pos = $pos;
        let slot = pos & (ROW_HASH_CACHE_SIZE - 1);
        let cur = $cache[slot];
        $cache[slot] = row_cache_hash!($m, $ctx, $rl, pos + ROW_HASH_CACHE_SIZE);
        cur
    }};
}

/// Upstream zstd `ZSTD_row_update_internalImpl` with the cache: index every
/// position of `$from .. $to` through the hash cache.
macro_rules! row_cache_insert_range {
    ($m:expr, $ctx:ident, $cache:ident, $rl:expr, $from:expr, $to:expr) => {{
        for pos in $from..$to {
            let (row, tag) = row_cache_next!($m, $ctx, $cache, $rl, pos);
            if row != ROW_CACHE_NONE {
                $m.insert_at::<$rl>(pos, row as usize, tag);
            }
        }
    }};
}

/// The lazy row parse BODY: upstream zstd `ZSTD_compressBlock_lazy_generic`
/// (zstd_lazy.c:1516) for the row-hash search, expanded per SIMD tier so the
/// search splices (`lazy_search_at!`, upstream's `FORCE_INLINE_TEMPLATE
/// ZSTD_searchMax`) inline into the parse loop as ONE monolith. Same
/// decisions as upstream: repcode 1 probed one byte ahead, gain-weighted
/// depth-1/depth-2 lookahead (each position searched once), catch-up on the
/// chosen match only, immediate-repcode loop after every stored sequence,
/// `>> 8` miss acceleration with lazy skipping past step 8.
macro_rules! lazy_parse_body {
    ($m:expr, $handle:expr, $rl:expr, $finder:expr, $use_mask:literal, $maskmac:ident, $cpl:path) => {{
        #[allow(unused_labels)]
        'parse: {
            debug_assert!($finder != FINDER_ROWS || $rl == $m.row_log);
            debug_assert_eq!(
                $finder,
                match $m.finder {
                    LazyFinder::Rows => FINDER_ROWS,
                    LazyFinder::Chain => FINDER_CHAIN,
                    LazyFinder::Tree => FINDER_TREE,
                }
            );
            $m.ensure_tables();

            let (current_abs_start, current_len) = $m.current_block_range();
            if current_len == 0 {
                break 'parse;
            }
            let block_end = current_abs_start + current_len;
            $m.enter_block(current_abs_start, block_end);
            let scan = $m.scan_ctx();
            let hist_start = scan.hist_start;
            let depth = $m.lazy_depth;
            // Upstream `ilimit`: `iend - 8 - ZSTD_ROW_HASH_CACHE_SIZE` for the
            // row search, `iend - 8` for the hash chain and the binary tree.
            let ilimit = block_end.saturating_sub(if $finder == FINDER_ROWS {
                LAZY_ROW_ILIMIT_MARGIN
            } else {
                LAZY_HC_ILIMIT_MARGIN
            });
            let mut anchor = current_abs_start;
            // Upstream skips the first byte of a frame: `ip += (dictAndPrefixLength
            // == 0)` on a plain / attached-dictionary frame (nothing precedes it),
            // `ip += (ip == prefixStart)` on a copied-dictionary frame (upstream
            // `extDict`, the dictionary being the segment below the prefix).
            let first_byte = if scan.dict_frame && !scan.attached {
                scan.prefix_start
            } else {
                hist_start
            };
            let mut ip = current_abs_start + usize::from(current_abs_start == first_byte);
            // Upstream `nextToUpdate`: resume where the previous block
            // stopped indexing (a lazy block leaves its last <= 16 bytes,
            // now readable with the full 8-byte key; the tree leaves it past
            // the longest match seen). A cursor the window floor moved past
            // restarts at the floor (upstream's `nextToUpdate < lowLimit`
            // clamp).
            let mut next_to_update = $m.lazy_next_to_update.max(scan.low_limit);
            // Upstream `ZSTD_buildSeqStore` "limited update after a very long
            // match" (zstd_compress.c:3296): a block starting more than 384
            // positions past the cursor indexes at most the last 192 + 384 of
            // the gap.
            if current_abs_start > next_to_update + 384 {
                next_to_update =
                    current_abs_start - (current_abs_start - next_to_update - 384).min(192);
            }
            let mut lazy_skipping = false;
            // Upstream `ZSTD_row_fillHashCache(ms, base, rowLog, mls, nextToUpdate, ilimit)`.
            let mut hash_cache = [(ROW_CACHE_NONE, 0u8); ROW_HASH_CACHE_SIZE];
            if $finder == FINDER_ROWS {
                row_cache_fill!($m, scan, hash_cache, $rl, next_to_update);
            }
            // Upstream `rep[0..2]` entering the block: carried from the
            // previous lazy block, else the frame / dictionary history.
            // Repcodes reaching below the window are disabled for the block
            // (upstream `maxRep`, remembered in `offset_saved` so an unused
            // one is handed back at the end); the bound also keeps every rep
            // read inside the live history since `ip` only grows.
            let [rep_in_1, rep_in_2] = $m
                .lazy_reps
                .unwrap_or([$m.offset_hist[0] as usize, $m.offset_hist[1] as usize]);
            // Repcode validity. Plain frame (upstream `noDict`): reps reaching
            // below the window are disabled for the whole block (`maxRep`,
            // remembered in `offset_saved`). Dictionary frame (upstream
            // `extDict` / `dictMatchState`): no block-start clamp; every rep
            // probe checks the window floor at its own position (the
            // attached dictionary has none: its reps reach the dictionary
            // start) and `ZSTD_index_overlap_check` (a source in the last 3
            // bytes below the frame's prefix, straddling the dictionary
            // boundary, is not taken).
            let dict_frame = scan.dict_frame;
            let attached = scan.attached;
            let prefix_start = scan.prefix_start;
            let search_window = scan.search_window;
            let rep_ok = |pos: usize, off: usize| -> bool {
                if !dict_frame {
                    // The block-start clamp below keeps every carried rep
                    // in-window for the whole block (the window floor never
                    // advances faster than the position) and searched
                    // offsets are in-window by construction.
                    return off != 0;
                }
                if off == 0 || off > pos {
                    return false;
                }
                let cand = pos - off;
                let floor = if attached {
                    hist_start
                } else {
                    scan.window_low(pos)
                };
                cand >= floor && !(cand < prefix_start && cand + 3 >= prefix_start)
            };
            let mut offset_1 = rep_in_1;
            let mut offset_2 = rep_in_2;
            let mut offset_saved_1 = 0usize;
            let mut offset_saved_2 = 0usize;
            if !dict_frame {
                // Upstream `ZSTD_getLowestPrefixIndex`: the prefix start or
                // `maxDist` back, whichever is nearer.
                let max_rep = ip - prefix_start.max(ip.saturating_sub(search_window));
                if offset_2 > max_rep {
                    offset_saved_2 = offset_2;
                    offset_2 = 0;
                }
                if offset_1 > max_rep {
                    offset_saved_1 = offset_1;
                    offset_1 = 0;
                }
            }

            while ip < ilimit {
                let mut match_length = 0usize;
                // `0` = repcode 1 (`offset_1`), else a real distance.
                let mut off = 0usize;
                let mut start = ip + 1;
                {
                    let base = scan.base;
                    // SAFETY: `ip + 1 + 4 <= block_end` (`ip < ilimit`), and
                    // `offset_1 <= max_rep` keeps the rep source in history.
                    unsafe {
                        let cur = base.add(ip + 1 - hist_start);
                        if rep_ok(ip + 1, offset_1) && rd32(cur.sub(offset_1)) == rd32(cur) {
                            match_length =
                                $cpl(cur.add(4).sub(offset_1), cur.add(4), block_end - (ip + 5))
                                    + 4;
                        }
                    }
                }
                // Upstream: a depth-0 rep hit is stored without searching.
                if !(match_length >= 4 && depth == 0) {
                    let (ml2, off2) = lazy_search_at!(
                        $m,
                        scan,
                        ip,
                        next_to_update,
                        lazy_skipping,
                        hash_cache,
                        $rl,
                        $finder,
                        $use_mask,
                        $maskmac,
                        $cpl
                    );
                    if ml2 > match_length {
                        match_length = ml2;
                        start = ip;
                        off = off2;
                    }
                    if match_length < 4 {
                        // Upstream: `step = ((ip - anchor) >> kSearchStrength) + 1`
                        // and lazy skipping past `step > 8` on a plain / attached
                        // frame; the `extDict` body (copied dictionary) counts
                        // the `+ 1` outside the skipping test, one 256-byte
                        // stretch later.
                        let gap = (ip - anchor) >> LAZY_SEARCH_STRENGTH;
                        ip += gap + 1;
                        lazy_skipping = if dict_frame && !attached {
                            gap > LAZY_SKIPPING_STEP
                        } else {
                            gap + 1 > LAZY_SKIPPING_STEP
                        };
                        continue;
                    }
                    if depth >= 1 {
                        while ip < ilimit {
                            ip += 1;
                            {
                                let base = scan.base;
                                // SAFETY: `ip + 4 <= block_end`, `offset_1 <= max_rep`.
                                unsafe {
                                    let cur = base.add(ip - hist_start);
                                    if rep_ok(ip, offset_1) && rd32(cur) == rd32(cur.sub(offset_1))
                                    {
                                        let ml_rep = $cpl(
                                            cur.add(4).sub(offset_1),
                                            cur.add(4),
                                            block_end - (ip + 4),
                                        ) + 4;
                                        let gain2 = (ml_rep * 3) as i64;
                                        let gain1 =
                                            (match_length * 3) as i64 - offbase_highbit(off) + 1;
                                        if ml_rep >= 4 && gain2 > gain1 {
                                            match_length = ml_rep;
                                            off = 0;
                                            start = ip;
                                        }
                                    }
                                }
                            }
                            let (ml2, off2) = lazy_search_at!(
                                $m,
                                scan,
                                ip,
                                next_to_update,
                                lazy_skipping,
                                hash_cache,
                                $rl,
                                $finder,
                                $use_mask,
                                $maskmac,
                                $cpl
                            );
                            let gain2 = (ml2 * 4) as i64 - offbase_highbit(off2);
                            let gain1 = (match_length * 4) as i64 - offbase_highbit(off) + 4;
                            if ml2 >= 4 && gain2 > gain1 {
                                match_length = ml2;
                                off = off2;
                                start = ip;
                                continue;
                            }
                            if depth == 2 && ip < ilimit {
                                ip += 1;
                                {
                                    let base = scan.base;
                                    // SAFETY: as above.
                                    unsafe {
                                        let cur = base.add(ip - hist_start);
                                        if rep_ok(ip, offset_1)
                                            && rd32(cur) == rd32(cur.sub(offset_1))
                                        {
                                            let ml_rep = $cpl(
                                                cur.add(4).sub(offset_1),
                                                cur.add(4),
                                                block_end - (ip + 4),
                                            ) + 4;
                                            let gain2 = (ml_rep * 4) as i64;
                                            let gain1 = (match_length * 4) as i64
                                                - offbase_highbit(off)
                                                + 1;
                                            if ml_rep >= 4 && gain2 > gain1 {
                                                match_length = ml_rep;
                                                off = 0;
                                                start = ip;
                                            }
                                        }
                                    }
                                }
                                let (ml2, off2) = lazy_search_at!(
                                    $m,
                                    scan,
                                    ip,
                                    next_to_update,
                                    lazy_skipping,
                                    hash_cache,
                                    $rl,
                                    $finder,
                                    $use_mask,
                                    $maskmac,
                                    $cpl
                                );
                                let gain2 = (ml2 * 4) as i64 - offbase_highbit(off2);
                                let gain1 = (match_length * 4) as i64 - offbase_highbit(off) + 7;
                                if ml2 >= 4 && gain2 > gain1 {
                                    match_length = ml2;
                                    off = off2;
                                    start = ip;
                                    continue;
                                }
                            }
                            break;
                        }
                    }
                }
                if off != 0 {
                    // Catch-up: absorb preceding literal bytes that also match,
                    // never past `anchor` nor below the match's segment start
                    // (upstream `mStart`: the prefix start for a prefix match,
                    // the dictionary start for a dictionary match).
                    let concat = $m.live_history();
                    let m_start = if start - off >= prefix_start {
                        prefix_start
                    } else {
                        hist_start
                    };
                    while start > anchor
                        && start - off > m_start
                        && concat[start - 1 - hist_start] == concat[start - 1 - off - hist_start]
                    {
                        start -= 1;
                        match_length += 1;
                    }
                    offset_2 = offset_1;
                    offset_1 = off;
                }
                {
                    let concat = $m.live_history();
                    let literals = &concat[anchor - hist_start..start - hist_start];
                    $handle(Sequence::Triple {
                        literals,
                        offset: offset_1,
                        match_len: match_length,
                    });
                    let _ = encode_offset_with_history(
                        offset_1 as u32,
                        (start - anchor) as u32,
                        &mut $m.offset_hist,
                    );
                }
                anchor = start + match_length;
                ip = anchor;
                if lazy_skipping {
                    // A match ends lazy skipping; the row cache is stale, refill it.
                    if $finder == FINDER_ROWS {
                        row_cache_fill!($m, scan, hash_cache, $rl, next_to_update);
                    }
                    lazy_skipping = false;
                }

                // Immediate repcode: `offset_2` right after the stored match,
                // swapping the two reps on every hit.
                while ip <= ilimit && rep_ok(ip, offset_2) {
                    let base = scan.base;
                    // SAFETY: `ip + 4 <= block_end`, `offset_2 <= max_rep`.
                    let rep_len = unsafe {
                        let cur = base.add(ip - hist_start);
                        if rd32(cur) != rd32(cur.sub(offset_2)) {
                            break;
                        }
                        $cpl(cur.add(4).sub(offset_2), cur.add(4), block_end - (ip + 4)) + 4
                    };
                    core::mem::swap(&mut offset_1, &mut offset_2);
                    {
                        let concat = $m.live_history();
                        $handle(Sequence::Triple {
                            literals: &concat[ip - hist_start..ip - hist_start],
                            offset: offset_1,
                            match_len: rep_len,
                        });
                        let _ = encode_offset_with_history(offset_1 as u32, 0, &mut $m.offset_hist);
                    }
                    ip += rep_len;
                    anchor = ip;
                }
            }

            // The un-indexed tail (upstream leaves it to the next block's
            // first search, which then reads full 8-byte keys across the
            // boundary) is carried in `lazy_next_to_update`.
            $m.lazy_next_to_update = next_to_update;
            // Upstream's rep save: a disabled `offset_1` that became valid
            // rotates the saved one into `rep[1]`; unused disabled reps are
            // restored as they were.
            let offset_saved_2 = if offset_saved_1 != 0 && offset_1 != 0 {
                offset_saved_1
            } else {
                offset_saved_2
            };
            $m.lazy_reps = Some([
                if offset_1 != 0 {
                    offset_1
                } else {
                    offset_saved_1
                },
                if offset_2 != 0 {
                    offset_2
                } else {
                    offset_saved_2
                },
            ]);
            if anchor < block_end {
                let concat = $m.live_history();
                $handle(Sequence::Literals {
                    literals: &concat[anchor - hist_start..],
                });
            }
        }
    }};
}

/// Per-tier lazy kernels (see `gen_greedy_monolith` — same SIMD pairing).
macro_rules! gen_lazy_monolith {
    ($name:ident, $use_mask:literal, $maskmac:ident, $cpl:path $(, $tf:literal)?) => {
        $(#[target_feature(enable = $tf)])?
        // wasm32+simd128 resolves the dispatch at compile time to the
        // simd128 kernel, leaving the scalar monolith uncalled there
        // (same shape as the `ScalarTags` allowance).
        #[cfg_attr(
            all(
                target_arch = "wasm32",
                target_feature = "simd128",
                feature = "kernel-simd128"
            ),
            allow(dead_code)
        )]
        #[allow(unused_unsafe)]
        unsafe fn $name<K: RowTags, const ROW_LOG: usize, const FINDER: u8>(
            &mut self,
            mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
        ) {
            lazy_parse_body!(self, handle_sequence, ROW_LOG, FINDER, $use_mask, $maskmac, $cpl)
        }
    };
}

/// Bind the runtime finder and (for rows) `row_log` to the const parameters
/// of a lazy monolith: rows per `ROW_LOG` 4..=6, the chain and the tree
/// once each (they ignore `ROW_LOG`). Cold, once per block.
macro_rules! dispatch_lazy {
    ($self:ident . $m:ident :: <$k:ty> ( $($arg:expr),* )) => {
        match $self.finder {
            LazyFinder::Rows => match $self.row_log {
                4 => $self.$m::<$k, 4, FINDER_ROWS>($($arg),*),
                5 => $self.$m::<$k, 5, FINDER_ROWS>($($arg),*),
                6 => $self.$m::<$k, 6, FINDER_ROWS>($($arg),*),
                _ => unreachable!("row_log is clamped to 4..=6 in configure()"),
            },
            LazyFinder::Chain => $self.$m::<$k, 4, FINDER_CHAIN>($($arg),*),
            LazyFinder::Tree => $self.$m::<$k, 4, FINDER_TREE>($($arg),*),
        }
    };
}

/// Bind the runtime `row_log` (clamped 4..=6) to the const `ROW_LOG` of a
/// `*_rl::<K, ROW_LOG>` hot loop. Mirrors the upstream zstd's per-rowLog variant
/// table; the branch is cold (once per block / call).
macro_rules! dispatch_row_log {
    ($self:ident . $rl_method:ident :: <$k:ty> ( $($arg:expr),* )) => {
        match $self.row_log {
            4 => $self.$rl_method::<$k, 4>($($arg),*),
            5 => $self.$rl_method::<$k, 5>($($arg),*),
            6 => $self.$rl_method::<$k, 6>($($arg),*),
            _ => unreachable!("row_log is clamped to 4..=6 in configure()"),
        }
    };
}

/// Row tag-match mask kernels as `macro_rules!` bodies (upstream zstd
/// `ZSTD_row_getMatchMask`). Per the SW-Rust SIMD rule, the SIMD body is a macro
/// expanded at the call site inside each per-kernel `#[target_feature]` probe so
/// the vector compare + movemask inline straight-line — `#[inline(always)]` +
/// `#[target_feature]` on a function is forbidden (rust-lang/rust#145574), so a
/// function call would otherwise cross the feature ABI boundary on every probe.
/// Each expands to a `u64` bitmask: bit `j` set iff `tags[j] == tag`. The
/// `row_tag_match_mask_*` wrapper fns below reuse these macros so the
/// bit-identity tests exercise the exact same code the hot path runs.
macro_rules! row_tag_mask_scalar {
    ($tags:expr, $tag:expr) => {{
        let tags: &[u8] = $tags;
        let tag: u8 = $tag;
        let mut mask = 0u64;
        for (j, &t) in tags.iter().enumerate() {
            if t == tag {
                mask |= 1u64 << j;
            }
        }
        mask
    }};
}

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
macro_rules! row_tag_mask_sse2 {
    ($tags:expr, $tag:expr) => {{
        #[cfg(target_arch = "x86")]
        use core::arch::x86::{_mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8};
        #[cfg(target_arch = "x86_64")]
        use core::arch::x86_64::{
            _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
        };
        let tags: &[u8] = $tags;
        let needle = _mm_set1_epi8($tag as i8);
        let mut mask = 0u64;
        let mut off = 0;
        while off + 16 <= tags.len() {
            // SAFETY: `off + 16 <= tags.len()`, so the 16-byte load is in bounds;
            // the enclosing fn carries `#[target_feature(enable = "sse2")]`.
            let v = unsafe { _mm_loadu_si128(tags.as_ptr().add(off) as *const _) };
            let eq = _mm_cmpeq_epi8(v, needle);
            mask |= (_mm_movemask_epi8(eq) as u16 as u64) << off;
            off += 16;
        }
        mask
    }};
}

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
macro_rules! row_tag_mask_avx2 {
    ($tags:expr, $tag:expr) => {{
        #[cfg(target_arch = "x86")]
        use core::arch::x86::{
            _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8, _mm256_cmpeq_epi8,
            _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
        };
        #[cfg(target_arch = "x86_64")]
        use core::arch::x86_64::{
            _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8, _mm256_cmpeq_epi8,
            _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
        };
        let tags: &[u8] = $tags;
        let tag = $tag;
        let needle = _mm256_set1_epi8(tag as i8);
        let mut mask = 0u64;
        let mut off = 0;
        while off + 32 <= tags.len() {
            // SAFETY: `off + 32 <= tags.len()`; enclosing fn is `target_feature(avx2)`.
            let v = unsafe { _mm256_loadu_si256(tags.as_ptr().add(off) as *const _) };
            let eq = _mm256_cmpeq_epi8(v, needle);
            mask |= (_mm256_movemask_epi8(eq) as u32 as u64) << off;
            off += 32;
        }
        if off + 16 <= tags.len() {
            let needle16 = _mm_set1_epi8(tag as i8);
            // SAFETY: `off + 16 <= tags.len()`; enclosing fn is `target_feature(avx2)`.
            let v = unsafe { _mm_loadu_si128(tags.as_ptr().add(off) as *const _) };
            let eq = _mm_cmpeq_epi8(v, needle16);
            mask |= (_mm_movemask_epi8(eq) as u16 as u64) << off;
        }
        mask
    }};
}

#[cfg(all(
    target_arch = "aarch64",
    target_endian = "little",
    feature = "kernel-neon"
))]
macro_rules! row_tag_mask_neon {
    ($tags:expr, $tag:expr) => {{
        use core::arch::aarch64::{
            vceqq_u8, vdupq_n_u8, vgetq_lane_u8, vld1q_u8, vreinterpretq_u8_u64,
            vreinterpretq_u16_u8, vreinterpretq_u32_u16, vreinterpretq_u64_u32, vshrq_n_u8,
            vsraq_n_u16, vsraq_n_u32, vsraq_n_u64,
        };
        let tags: &[u8] = $tags;
        let needle = vdupq_n_u8($tag);
        let mut mask = 0u64;
        let mut off = 0;
        while off + 16 <= tags.len() {
            // SAFETY: `off + 16 <= tags.len()`; enclosing fn is `target_feature(neon)`.
            let v = unsafe { vld1q_u8(tags.as_ptr().add(off)) };
            let eq = vceqq_u8(v, needle);
            let high = vshrq_n_u8(eq, 7);
            let paired16 = vreinterpretq_u32_u16(vsraq_n_u16(
                vreinterpretq_u16_u8(high),
                vreinterpretq_u16_u8(high),
                7,
            ));
            let paired32 = vreinterpretq_u64_u32(vsraq_n_u32(paired16, paired16, 14));
            let paired64 = vreinterpretq_u8_u64(vsraq_n_u64(paired32, paired32, 28));
            let bits =
                (vgetq_lane_u8(paired64, 0) as u64) | ((vgetq_lane_u8(paired64, 8) as u64) << 8);
            mask |= bits << off;
            off += 16;
        }
        mask
    }};
}

// WebAssembly `simd128` tag-match mask: `i8x16_eq` against the broadcast tag,
// then `i8x16_bitmask` (wasm's direct 16-lane-to-16-bit movemask) over each
// 16-byte chunk. Shape mirrors the SSE2 kernel; `tags.len()` is a multiple of
// 16 so no scalar tail is needed. Compiled only under `target_feature =
// "simd128"`, so the intrinsics are available without a `#[target_feature]`
// attribute (no runtime detection on wasm); only `v128_load` is `unsafe`.
#[cfg(all(
    target_arch = "wasm32",
    target_feature = "simd128",
    feature = "kernel-simd128"
))]
macro_rules! row_tag_mask_simd128 {
    ($tags:expr, $tag:expr) => {{
        use core::arch::wasm32::{i8x16_bitmask, i8x16_eq, i8x16_splat, v128_load};
        let tags: &[u8] = $tags;
        let needle = i8x16_splat($tag as i8);
        let mut mask = 0u64;
        let mut off = 0;
        while off + 16 <= tags.len() {
            // SAFETY: `off + 16 <= tags.len()`, so the 16-byte unaligned load is
            // in bounds.
            let v = unsafe { v128_load(tags.as_ptr().add(off) as *const _) };
            let eq = i8x16_eq(v, needle);
            mask |= (i8x16_bitmask(eq) as u64) << off;
            off += 16;
        }
        mask
    }};
}

/// Emit a per-kernel row match probe method on `RowMatchGenerator`. The body is
/// written ONCE here and stamped per tier under that tier's `#[target_feature]`
/// umbrella; the tag-match SIMD is expanded inline via the `$maskmac` macro (not
/// a function call), so the vector compare inlines straight-line — no
/// `#[target_feature]` ABI boundary on the per-probe hot path. Runtime kernel
/// selection happens once at the `dispatch_tag_kernel!` site; this method is the
/// per-tier monomorphised hot loop with no kernel branch inside it.
///
/// `$use_mask` is the compile-time bitmask-vs-byte-compare choice; `$maskmac` is
/// the tier's `row_tag_mask_*!`; the optional `$tf` is the `target_feature`.
/// Mirrors the former generic `row_candidate_rl`: live row probe, dict
/// dual-probe, speculative tail gate.
/// The row probe BODY as a macro, expanded both into the per-tier
/// `row_probe_*` functions (non-kernel callers) and directly into the
/// per-tier parse kernels (`greedy_*` / `lazy_*`) where a function-call
/// boundary — non-inlinable across `#[target_feature]` without an
/// `inline(always)` the compiler forbids there — cost a call with operand
/// spills per position. Early exits use the labeled block (`break 'probe`)
/// because a `return` inside a macro body would return from the EXPANSION
/// SITE's function.
macro_rules! row_probe_body {
    ($m:expr, $abs_pos:expr, $lit_len:expr, $hash:expr, $seed:expr, $rl:expr, $use_mask:literal, $maskmac:ident, $cpl:path) => {{
        #[allow(unused_labels)]
        'probe: {
            debug_assert_eq!($rl, $m.row_log);
            let mls = $m.mls;
            let concat = $m.live_history();
            let current_idx = $abs_pos - $m.history_abs_start;
            if current_idx + mls > concat.len() {
                break 'probe None;
            }

            // `hash` carries the (row, tag) the greedy loop already
            // computed for this position (and prefetched the row for);
            // recompute only on the uncarried paths.
            let (row, tag) = match $hash.or_else(|| $m.hash_and_row($abs_pos)) {
                Some(rt) => rt,
                None => break 'probe None,
            };
            let row_entries = 1usize << $rl;
            let row_mask = row_entries - 1;
            let row_base = row << $rl;
            let head = $m.row_heads[row] as usize;
            let max_walk = $m.search_depth.min(row_entries);

            // Prefetch the dict row before the live scan (upstream zstd
            // prefetches the dictMatchState rows up front,
            // zstd_lazy.c:1200 `ZSTD_row_prefetch`), hiding the dict-table
            // load latency behind the live row's work.
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            if let Some(dict) = $m.dict.table() {
                #[cfg(target_arch = "x86")]
                use core::arch::x86::{_MM_HINT_T0, _mm_prefetch};
                #[cfg(target_arch = "x86_64")]
                use core::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
                let drow_base = row << $rl;
                // SAFETY: prefetch is a hint and never faults; indexes are in
                // bounds by the dict-table sizing.
                unsafe {
                    _mm_prefetch(dict.tags.as_ptr().add(drow_base).cast(), _MM_HINT_T0);
                    _mm_prefetch(dict.positions.as_ptr().add(drow_base).cast(), _MM_HINT_T0);
                }
            }

            // SIMD tiers precompute the full bitmask once (the tag-match
            // intrinsic inlines under this method's `#[target_feature]`); the
            // scalar tier (`USE_MASK == false`) const-folds this away and does
            // an on-the-fly per-slot byte compare in the loop.
            let tag_match = if $use_mask {
                $maskmac!(&$m.row_tags[row_base..row_base + row_entries], tag)
            } else {
                0
            };

            // Seeded with the rep candidate (when present) so the tail-gate
            // below prunes row candidates against the rep length from the
            // first hit, and the rep value need not stay live separately
            // across the probe. Merge stays byte-identical: the seed is the
            // permanent lhs, exactly as the former trailing
            // `best_len_offset_candidate(rep, row)` merge made it.
            let mut best: Option<MatchCandidate> = $seed;
            // Upstream zstd `ZSTD_RowFindBestMatch` mask iteration: rotate the tag
            // mask into head (newest-first) order once, then visit ONLY the
            // set bits via tzcnt + clear-lowest. The former per-slot loop
            // burned slot arithmetic + a bit test on EVERY entry (rows are
            // 16-64 wide, typical tag hits 0-2) — ~14% of L10 wall time.
            // `max_walk` bounds ATTEMPTED candidates (upstream zstd `nbAttempts`
            // decrements per mask hit, not per scanned slot), so a depth
            // below the row width searches up to `depth` hits across the
            // WHOLE row — upstream zstd semantics on both the SIMD and scalar tiers
            // (the scalar arm advances to the next on-the-fly tag hit so
            // its visit order and attempt accounting stay bit-identical to
            // the mask tiers).
            let entries_bits: u64 = if row_entries >= 64 {
                u64::MAX
            } else {
                (1u64 << row_entries) - 1
            };
            #[allow(unused_mut)]
            let mut pending: u64 = if $use_mask {
                let m = tag_match & entries_bits;
                if head == 0 {
                    m
                } else {
                    ((m >> head) | (m << (row_entries - head))) & entries_bits
                }
            } else {
                0
            };
            #[allow(unused_mut)]
            let mut scan = 0usize;
            let mut attempts = 0usize;
            while attempts < max_walk {
                let slot_opt = if $use_mask {
                    if pending == 0 {
                        None
                    } else {
                        let i = pending.trailing_zeros() as usize;
                        pending &= pending - 1;
                        Some((head + i) & row_mask)
                    }
                } else {
                    let mut found = None;
                    while scan < row_entries {
                        let s = (head + scan) & row_mask;
                        scan += 1;
                        if $m.row_tags[row_base + s] == tag {
                            found = Some(s);
                            break;
                        }
                    }
                    found
                };
                let Some(slot) = slot_opt else { break };
                attempts += 1;
                let idx = row_base + slot;
                let raw_pos = $m.row_positions[idx];
                if raw_pos == ROW_EMPTY_SLOT {
                    continue;
                }
                let candidate_pos = raw_pos as usize;
                // Lower bound = window low. Owned: `history_abs_start` (eviction
                // floor) is always >= `abs_pos - max_window_size` (window_size <=
                // max_window_size), so the `max` picks it — byte-identical to the
                // pre-window_low check. Borrowed (history_abs_start forced to 0 in
                // set_borrowed_window): the `max` picks `abs_pos - max_window_size`,
                // capping the offset to the advertised window so an over-window
                // in-place scan never emits an unresolvable offset.
                let window_low = $m
                    .history_abs_start
                    .max($abs_pos.saturating_sub($m.max_window_size));
                if candidate_pos < window_low || candidate_pos >= $abs_pos {
                    continue;
                }
                let candidate_idx = candidate_pos - $m.history_abs_start;
                // NOTE: upstream zstd's 4-byte head gate (`MEM_read32(match)
                // == MEM_read32(ip)`, zstd_lazy.c:1265) was measured NEGATIVE
                // here both unconditionally (+7% on match-dense z000033 L6,
                // flat control) and best-gated (+3%); the row walk visits few
                // false tag hits and the SIMD prefix compare's first vector
                // already serves as the cheap reject. The tail gate below is
                // the selective filter that pays.
                // Speculative tail gate (HC `hash_chain_candidate` parity):
                // a 4-byte compare at the length the candidate must reach to
                // outgrow `best` proves whether the full `common_prefix_len`
                // can pay off. Gated on offset-monotonicity since the row walk
                // is not offset-ordered. Ratio-neutral.
                if let Some(b) = best {
                    let new_offset = $abs_pos - candidate_pos;
                    if new_offset >= b.offset
                        && let Some(tail_off) = b.match_len.checked_sub($lit_len + 3)
                    {
                        let m_end = candidate_idx + tail_off + 4;
                        let i_end = current_idx + tail_off + 4;
                        if i_end > concat.len()
                            || m_end > concat.len()
                            || concat[candidate_idx + tail_off..m_end]
                                != concat[current_idx + tail_off..i_end]
                        {
                            continue;
                        }
                    }
                }
                // Per-tier `common_prefix_len_ptr` expanded inline (same feature
                // umbrella as this probe) — no `dispatch_common_prefix_len_ptr`
                // runtime match + `#[target_feature]` call per candidate. `max =
                // concat.len() - current_idx` since `candidate_idx < current_idx`.
                let match_len = unsafe {
                    $cpl(
                        concat.as_ptr().add(candidate_idx),
                        concat.as_ptr().add(current_idx),
                        concat.len() - current_idx,
                    )
                };
                if match_len >= mls {
                    let candidate =
                        $m.extend_backwards(candidate_pos, $abs_pos, match_len, $lit_len);
                    best = best_len_offset_candidate(best, Some(candidate));
                    if best.is_some_and(|b| current_idx + b.match_len >= concat.len()) {
                        break 'probe best;
                    }
                }
            }

            // Dict dual-probe (upstream zstd `ZSTD_RowFindBestMatch` `dictMatchState`):
            // one bounded immutable dict row (concat-indexed positions).
            // The candidate budget is SHARED with the live row (upstream
            // zstd decrements one `nbAttempts` across both rows,
            // zstd_lazy.c:1308): the dict probe only spends what the live
            // walk left over.
            // Match upstream zstd's effective dict search depth: the CDict path
            // adjusts cParams (dict-size-aware searchLog) so the dict probe runs
            // deeper than the bare level searchLog — measured nbAttempts >= 16 to
            // surface the long dict match on the per-label-dict fixtures, vs the
            // 8 from L6's searchLog=3. Floor the dict budget at 16 (a full
            // rowLog-4 row); the dpending mask still bounds it to the row width.
            let dict_budget = max_walk.max(16);
            if attempts < dict_budget
                && let Some(dict) = $m.dict.table()
            {
                let dict_walk = dict_budget - attempts;
                let dict_end = $m.dict.region_len();
                let drow_base = row << $rl;
                let dhead = dict.heads[row] as usize;
                let dtag_match = if $use_mask {
                    $maskmac!(&dict.tags[drow_base..drow_base + row_entries], tag)
                } else {
                    0
                };
                // Same upstream zstd mask iteration as the live row above.
                #[allow(unused_mut)]
                let mut dpending: u64 = if $use_mask {
                    let m = dtag_match & entries_bits;
                    if dhead == 0 {
                        m
                    } else {
                        ((m >> dhead) | (m << (row_entries - dhead))) & entries_bits
                    }
                } else {
                    0
                };
                #[allow(unused_mut)]
                let mut dscan = 0usize;
                let mut dattempts = 0usize;
                while dattempts < dict_walk {
                    let slot_opt = if $use_mask {
                        if dpending == 0 {
                            None
                        } else {
                            let i = dpending.trailing_zeros() as usize;
                            dpending &= dpending - 1;
                            Some((dhead + i) & row_mask)
                        }
                    } else {
                        let mut found = None;
                        while dscan < row_entries {
                            let s = (dhead + dscan) & row_mask;
                            dscan += 1;
                            if dict.tags[drow_base + s] == tag {
                                found = Some(s);
                                break;
                            }
                        }
                        found
                    };
                    let Some(slot) = slot_opt else { break };
                    dattempts += 1;
                    let didx = drow_base + slot;
                    let dp = dict.positions[didx];
                    if dp == ROW_EMPTY_SLOT {
                        continue;
                    }
                    let dp = dp as usize;
                    if dp >= dict_end || dp + mls > concat.len() {
                        continue;
                    }
                    let cand_abs = $m.history_abs_start + dp;
                    if let Some(b) = best {
                        let new_offset = $abs_pos - cand_abs;
                        if new_offset >= b.offset
                            && let Some(tail_off) = b.match_len.checked_sub($lit_len + 3)
                        {
                            let m_end = dp + tail_off + 4;
                            let i_end = current_idx + tail_off + 4;
                            if i_end > concat.len()
                                || m_end > concat.len()
                                || concat[dp + tail_off..m_end]
                                    != concat[current_idx + tail_off..i_end]
                            {
                                continue;
                            }
                        }
                    }
                    let match_len = unsafe {
                        $cpl(
                            concat.as_ptr().add(dp),
                            concat.as_ptr().add(current_idx),
                            concat.len() - current_idx,
                        )
                    };
                    if match_len >= mls {
                        let candidate =
                            $m.extend_backwards(cand_abs, $abs_pos, match_len, $lit_len);
                        best = best_len_offset_candidate(best, Some(candidate));
                        if best.is_some_and(|b| current_idx + b.match_len >= concat.len()) {
                            break 'probe best;
                        }
                    }
                }
            }
            best
        }
    }};
}

macro_rules! gen_row_probe {
    ($name:ident, $use_mask:literal, $maskmac:ident, $cpl:path $(, $tf:literal)?) => {
        $(#[target_feature(enable = $tf)])?
        // `#[inline]` hint (NOT always — forbidden with target_feature):
        // the per-tier parse umbrella enables the same features, so the
        // probe is inlinable there and LLVM takes the single-call-site
        // hint, merging probe + parse loop into one body.
        #[inline]
        #[allow(unused_unsafe)]
        // wasm32+simd128 selects `row_probe_simd128` at compile time, leaving
        // `row_probe_scalar` (the only other tier compiled on wasm) unused.
        #[cfg_attr(
            all(
                target_arch = "wasm32",
                target_feature = "simd128",
                feature = "kernel-simd128"
            ),
            allow(dead_code)
        )]
        unsafe fn $name<const ROW_LOG: usize>(
            &self,
            abs_pos: usize,
            lit_len: usize,
            hash: Option<(usize, u8)>,
        ) -> Option<MatchCandidate> {
            // Standalone probe: the caller merges the rep candidate, so the
            // probe itself starts unseeded.
            row_probe_body!(self, abs_pos, lit_len, hash, None, ROW_LOG, $use_mask, $maskmac, $cpl)
        }
    };
}

// Reference mask wrappers for the bit-identity tests (`tag_mask_tests`). The
// `row_tag_mask_*!` macros are the production source of truth (expanded inline
// in the per-kernel `row_probe_*` methods); these fns just give the tests a
// callable handle to assert SIMD == scalar. Gated to the same cfg as the test
// module so they carry no weight in production builds.
#[cfg(all(
    test,
    any(
        all(
            feature = "std",
            any(target_arch = "x86", target_arch = "x86_64"),
            feature = "kernel-sse"
        ),
        all(
            target_arch = "aarch64",
            target_endian = "little",
            feature = "kernel-neon"
        )
    )
))]
fn row_tag_match_mask_scalar(tags: &[u8], tag: u8) -> u64 {
    row_tag_mask_scalar!(tags, tag)
}

/// # Safety
/// Caller must ensure SSE2 is available (checked by `RowTagKernel::detect`).
#[cfg(all(
    test,
    feature = "std",
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
#[target_feature(enable = "sse2")]
unsafe fn row_tag_match_mask_sse2(tags: &[u8], tag: u8) -> u64 {
    row_tag_mask_sse2!(tags, tag)
}

/// # Safety
/// Caller must ensure AVX2 is available (checked by `RowTagKernel::detect`).
#[cfg(all(
    test,
    feature = "std",
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
#[target_feature(enable = "avx2")]
unsafe fn row_tag_match_mask_avx2(tags: &[u8], tag: u8) -> u64 {
    row_tag_mask_avx2!(tags, tag)
}

/// # Safety
/// Caller must ensure NEON is available (baseline on aarch64; checked by
/// `RowTagKernel::detect`).
#[cfg(all(
    test,
    target_arch = "aarch64",
    target_endian = "little",
    feature = "kernel-neon"
))]
#[target_feature(enable = "neon")]
unsafe fn row_tag_match_mask_neon(tags: &[u8], tag: u8) -> u64 {
    row_tag_mask_neon!(tags, tag)
}

#[derive(Clone)]
pub(crate) struct RowMatchGenerator {
    pub(crate) max_window_size: usize,
    /// Per-committed-block lengths of the live window, mirroring the
    /// `HashChain` backend's `chunk_lens`. The block bytes themselves live
    /// only in the contiguous `history` mirror; the input buffers are handed
    /// straight back to the caller's pool in `add_data` rather than retained
    /// here. Retaining them (the old `VecDeque<Vec<u8>>`) held a full
    /// `block_capacity`-sized buffer per committed block, which on a heavily
    /// pre-split frame ballooned the window to many times the live byte count.
    pub(crate) chunk_lens: VecDeque<usize>,
    pub(crate) window_size: usize,
    pub(crate) history: Vec<u8>,
    pub(crate) history_start: usize,
    pub(crate) history_abs_start: usize,
    pub(crate) offset_hist: [u32; 3],
    /// The finder the lazy parse searches: rows, the hash chain (upstream
    /// `ZSTD_resolveRowMatchFinderMode`: a window of 2^14 or less) or the
    /// binary tree (btlazy2). Resolved in `configure`.
    finder: LazyFinder,
    /// Hash-chain / binary-tree tables (upstream `hashTable` / `chainTable`,
    /// `hashLog` / `chainLog` wide) used by the chain and tree finders. The
    /// chain holds absolute positions with `ROW_EMPTY_SLOT` = empty; the tree
    /// holds `abs + BT_IDX_BASE` with 0 = none (`hc_layout` records which).
    hc_chain_log: usize,
    hc_hash: Vec<u32>,
    hc_chain: Vec<u32>,
    hc_layout: LazyFinder,
    /// Upstream `ms->loadedDictEnd` (absolute): the dictionary end while the
    /// dictionary is still valid for the block being parsed (its whole
    /// content may be matched, no distance bound), 0 otherwise.
    loaded_dict_end: usize,
    /// Upstream `window.lowLimit` (absolute): the lowest index a match may
    /// reach; raised as the window slides (`ZSTD_window_enforceMaxDist`).
    low_limit: usize,
    /// Upstream `window.dictLimit` (absolute): the prefix start, the
    /// dictionary (copied: `extDict`) lying below it; equals the frame start
    /// on a plain frame and is raised with `low_limit` once the window has
    /// slid past the dictionary.
    prefix_low: usize,
    /// Salt XORed into the live row hash: a fresh context's
    /// [`ROW_HASH_SALT`], or 0 after a dictionary's tables were copied in
    /// (upstream `ZSTD_resetCCtx_byCopyingCDict` inherits the CDict's salt,
    /// which is always 0).
    hash_salt: u64,
    /// The dictionary plan of the current frame (upstream
    /// `ZSTD_resetCCtx_usingCDict`), `None` without a dictionary.
    dict_plan: Option<RowDictPlan>,
    /// `1 << windowLog` of the frame: the match distance bound (upstream
    /// `maxDistance`). `max_window_size` additionally grows by a primed
    /// dictionary's length for eviction only.
    search_window: usize,
    pub(crate) row_hash_log: usize,
    pub(crate) row_log: usize,
    pub(crate) search_depth: usize,
    pub(crate) target_len: usize,
    /// Regular-search min-match floor (upstream zstd `cParams.minMatch`). A row
    /// candidate must extend to >= `mls` bytes to be accepted. Hoisted to
    /// a local in the parse loops so the per-position compare reads a
    /// register, not this field. Default `ROW_MIN_MATCH_LEN` (5).
    pub(crate) mls: usize,
    /// Hash key width in bytes, `mls` bounded to 4..=6 (upstream zstd
    /// `ZSTD_hashPtrSalted`'s `mls` switch). Kept in sync by `configure`.
    row_hash_mls: u32,
    /// Upstream `nextToUpdate` carried across blocks by the lazy parse: the
    /// first position not yet indexed. The block after a lazy block indexes
    /// the previous tail from here (with the block's data now readable
    /// past it, as upstream), instead of a per-block backfill.
    lazy_next_to_update: usize,
    /// Upstream `rep[0..2]` as the lazy parse left them at the end of the
    /// previous block (`ZSTD_compressBlock_lazy_generic` saves `offset_1` /
    /// `offset_2`, restoring a window-disabled rep it never used). `None`
    /// until the first lazy block of a frame: then the reps come from
    /// `offset_hist` (frame start or dictionary). Kept apart from
    /// `offset_hist` because the block encoder may encode a searched
    /// offset that equals a rep as a repcode (no rotation) where upstream
    /// stores it raw and rotates, so the encoder-side history and
    /// upstream's parse-side reps legitimately differ.
    lazy_reps: Option<[usize; 2]>,
    pub(crate) lazy_depth: u8,
    /// Cached fastpath kernel for the per-candidate `common_prefix_len`
    /// compare, resolved once per matcher so the rep probe skips the
    /// `select_kernel()` atomic on every input byte.
    pub(crate) cpl_kernel: crate::encoding::fastpath::FastpathKernel,
    pub(crate) row_heads: Vec<u8>,
    // Absolute match positions, one per row slot. Stored as `u32` (not
    // `usize`): this is the largest match-finder array, and `u32` halves its
    // footprint vs the upstream zstd-parity `U32` layout. `ROW_EMPTY_SLOT == u32::MAX`
    // is the empty sentinel, so every stored position must stay strictly below
    // it. On a long stream the cumulative absolute cursor would cross `u32::MAX`
    // even while the live window is bounded; `add_data` rebases the coordinate
    // origin down to the oldest live byte before that happens (see
    // [`Self::rebase_positions`]), keeping positions representable without
    // capping frame length.
    pub(crate) row_positions: Vec<u32>,
    pub(crate) row_tags: Vec<u8>,
    /// Cached tag-match SIMD kernel; CPU features are fixed per process, so
    /// resolve once instead of querying per `row_candidate` call. On
    /// wasm32+simd128 the tier is compile-time (`dispatch_tag_kernel!` selects
    /// `Simd128Tags` directly), so the field is unread there.
    #[cfg_attr(
        all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        ),
        allow(dead_code)
    )]
    tag_kernel: FastpathKernel,
    /// Attached immutable dictionary row index (upstream zstd `dictMatchState`). `Some`
    /// activates the bounded dict probe in `row_candidate_rl`; built once and
    /// cached across frames via `DictAttach`, invalidated on eviction / resize.
    pub(crate) dict: DictAttach<RowDictTables>,
    /// Borrowed (no-copy) one-shot input window: `(ptr, len)` into the
    /// caller's slice. When set, the borrowed scan reads candidate/cursor
    /// bytes straight from here instead of the owned `history` mirror, so an
    /// over-window one-shot input is matched in place (no input->mirror copy).
    /// Raw pointer: the slice must stay live until `clear_borrowed_window` /
    /// `reset` (same contract as the Dfast/Simple borrowed backends).
    pub(crate) borrowed_input: Option<(*const u8, usize)>,
    /// Active borrowed block range `[start, end)` within `borrowed_input`,
    /// staged before each borrowed scan so `live_history()` exposes
    /// `[0, end)` and the parse loop scans `[start, end)`.
    pub(crate) borrowed_block: Option<(usize, usize)>,
    /// Furthest input offset any block of the current borrowed frame
    /// reached: the next `reset` advances the coordinate floor past it, so
    /// the positions this frame indexed can never resurface as another
    /// frame's (the owned path gets the same from its history length).
    borrowed_extent: usize,
}

impl RowMatchGenerator {
    pub(crate) fn new(max_window_size: usize) -> Self {
        Self {
            max_window_size,
            chunk_lens: VecDeque::new(),
            window_size: 0,
            history: Vec::new(),
            history_start: 0,
            history_abs_start: 0,
            offset_hist: [1, 4, 8],
            finder: LazyFinder::Rows,
            hc_chain_log: ROW_HASH_BITS,
            hc_hash: Vec::new(),
            hc_chain: Vec::new(),
            hc_layout: LazyFinder::Chain,
            loaded_dict_end: 0,
            low_limit: 0,
            prefix_low: 0,
            hash_salt: ROW_HASH_SALT,
            dict_plan: None,
            search_window: 0,
            row_hash_log: ROW_HASH_BITS - ROW_LOG,
            row_log: ROW_LOG,
            search_depth: ROW_SEARCH_DEPTH,
            target_len: ROW_TARGET_LEN,
            mls: ROW_MIN_MATCH_LEN,
            row_hash_mls: ROW_MIN_MATCH_LEN.clamp(4, 6) as u32,
            lazy_next_to_update: 0,
            lazy_reps: None,
            lazy_depth: 1,
            cpl_kernel: crate::encoding::fastpath::select_kernel(),
            row_heads: Vec::new(),
            row_positions: Vec::new(),
            row_tags: Vec::new(),
            tag_kernel: crate::encoding::fastpath::select_kernel(),
            dict: DictAttach::new(),
            borrowed_input: None,
            borrowed_block: None,
            borrowed_extent: 0,
        }
    }

    /// Heap bytes this matcher owns: history, the row head/position/tag tables,
    /// the chunk-length deque, and any attached dictionary row index.
    pub(crate) fn heap_size(&self) -> usize {
        let u32_sz = core::mem::size_of::<u32>();
        self.chunk_lens.capacity() * core::mem::size_of::<usize>()
            + self.history.capacity()
            + self.row_heads.capacity()
            + self.row_positions.capacity() * u32_sz
            + self.row_tags.capacity()
            + (self.hc_hash.capacity() + self.hc_chain.capacity()) * u32_sz
            + self.dict.table().map_or(0, |t| {
                t.heads.capacity()
                    + t.positions.capacity() * u32_sz
                    + t.tags.capacity()
                    + (t.hc_hash.capacity() + t.hc_chain.capacity()) * u32_sz
            })
    }

    /// Effective row hash width currently configured (`row_hash_log +
    /// row_log`). The primed-snapshot key records THIS value — the
    /// configured request may exceed the [`ROW_HASH_BITS`] cap below, and
    /// keying on the request while the tables use the clamped width forces
    /// needless dictionary re-primes.
    pub(crate) fn hash_bits(&self) -> usize {
        self.row_hash_log + self.row_log
    }

    /// Whether the parse searches the hash chain (window <= 2^14, upstream
    /// `ZSTD_resolveRowMatchFinderMode`) instead of rows.
    #[cfg(test)]
    pub(crate) fn uses_hash_chain(&self) -> bool {
        self.finder == LazyFinder::Chain
    }

    /// Whether the parse searches the binary tree (btlazy2).
    #[cfg(test)]
    pub(crate) fn uses_binary_tree(&self) -> bool {
        self.finder == LazyFinder::Tree
    }

    /// Hash-chain / tree table width (`chainLog`) applied by `configure`.
    #[cfg(test)]
    pub(crate) fn hc_chain_log(&self) -> usize {
        self.hc_chain_log
    }

    /// Upstream `ZSTD_checkDictValidity` + `ZSTD_window_enforceMaxDist`,
    /// run before every parsed block: a dictionary reaching further back than
    /// the window from the block end is dropped for good (`loadedDictEnd =
    /// 0`, the attached tables are no longer probed), and once the block
    /// start is more than a window past the dictionary end the window floor
    /// rises to `block_start - maxDist`, taking the prefix start with it (a
    /// copied dictionary is then out of reach and the frame is upstream's
    /// plain `noDict`).
    fn enter_block(&mut self, block_start: usize, block_end: usize) {
        if self.loaded_dict_end != 0 && block_end > self.loaded_dict_end + self.search_window {
            self.loaded_dict_end = 0;
        }
        if block_start > self.search_window + self.loaded_dict_end {
            let new_low = block_start - self.search_window;
            if self.low_limit < new_low {
                self.low_limit = new_low;
            }
            if self.prefix_low < self.low_limit {
                self.prefix_low = self.low_limit;
            }
            self.loaded_dict_end = 0;
        }
    }

    pub(crate) fn set_hash_bits(&mut self, bits: usize) {
        // The level's (source-size-adjusted) hashLog as upstream applies it:
        // a narrower table changes row assignment and eviction, and with it
        // the candidate set every search sees.
        let clamped = bits.max(self.row_log + 1);
        let row_hash_log = clamped.saturating_sub(self.row_log);
        if self.row_hash_log != row_hash_log {
            self.row_hash_log = row_hash_log;
            self.row_heads.clear();
            self.row_positions.clear();
            self.row_tags.clear();
            // NOTE: do NOT invalidate the dict here. `set_hash_bits` is called
            // twice per frame during level setup (once from `configure` with
            // the level's `hash_bits`, once with the hint-resolved table bits),
            // so `row_hash_log` oscillates every frame even when the level is
            // unchanged. Invalidating here would drop the CDict cache on every
            // frame. The dict is rebuilt by `prime_dict_rows` AFTER setup (final
            // shape), and `prime_dict_rows` self-invalidates a cached index whose
            // shape no longer matches — so a genuine level change is handled
            // there, while the per-frame oscillation is ignored.
        }
    }

    pub(crate) fn configure(&mut self, config: RowConfig) {
        self.row_log = config.row_log.clamp(4, 6);
        self.search_depth = config.search_depth;
        self.target_len = config.target_len;
        // Clamp the min-match floor to >= the hash key width (a shorter
        // floor can't be satisfied: the hash only surfaces candidates
        // sharing the 4-byte key) and a sane upper bound.
        self.mls = config.mls.clamp(ROW_HASH_KEY_LEN, 7);
        self.row_hash_mls = self.mls.clamp(4, 6) as u32;
        // A btlazy2 level (its own or the CDict's strategy) searches the
        // binary tree. Otherwise upstream `ZSTD_resolveRowMatchFinderMode`:
        // rows only above a 2^14 window; the window is source-size-adjusted,
        // so inputs of 16 KiB or less search the hash chain. A dictionary
        // frame inherits the CDict's decision (`ZSTD_resetCCtx_usingCDict`:
        // "cdict overrides"), and a copied dictionary brings the CDict's
        // salt (0) with its tables.
        self.finder = if config.bt {
            LazyFinder::Tree
        } else {
            let rows = match self.dict_plan {
                Some(plan) => plan.use_row,
                None => self.max_window_size > (1usize << 14),
            };
            if rows {
                LazyFinder::Rows
            } else {
                LazyFinder::Chain
            }
        };
        self.hash_salt = match self.dict_plan {
            Some(plan) if !plan.attach => 0,
            _ => ROW_HASH_SALT,
        };
        // Taken before dictionary priming inflates `max_window_size`.
        self.search_window = self.max_window_size;
        self.hc_chain_log = config.chain_log.max(1);
        self.set_hash_bits(config.hash_bits.max(self.row_log + 1));
    }

    /// Replace the repeat-offset history (dictionary priming): the lazy
    /// parse's carried reps restart from it at the next block.
    pub(crate) fn set_offset_hist(&mut self, offset_hist: [u32; 3]) {
        self.offset_hist = offset_hist;
        self.lazy_reps = None;
    }

    /// Install the frame's dictionary plan (upstream
    /// `ZSTD_resetCCtx_usingCDict`) before [`Self::configure`], which reads
    /// it for the match-finder and salt; `None` for a plain frame.
    pub(crate) fn set_dict_plan(&mut self, plan: Option<RowDictPlan>) {
        if self.dict_plan != plan {
            // The prepared dictionary index was built for another plan.
            self.dict.invalidate();
        }
        self.dict_plan = plan;
    }

    pub(crate) fn reset(&mut self) {
        // Floor-advance reset (same shape as the dfast/HC backends): instead
        // of re-zeroing the row tables per frame (a multi-MiB memset that
        // dominated small/medium-frame encode), advance the absolute
        // coordinate floor past everything ever inserted. Stale entries all
        // hold positions below the new floor, so the probes' existing
        // `candidate_pos < self.history_abs_start` window check rejects them
        // without any clearing — the upstream zstd's persistent-index design. Stale
        // TAGS can still produce the occasional false mask hit whose
        // candidate then fails the window check; the upstream zstd's tag table
        // persists across frames with the same behaviour.
        // Past everything the previous frame indexed: its owned history, or
        // the extent of its borrowed input.
        let next_floor = self.history_abs_start
            + (self.history.len() - self.history_start).max(self.borrowed_extent);
        self.borrowed_extent = 0;
        self.window_size = 0;
        self.history.clear();
        self.history_start = 0;
        self.offset_hist = [1, 4, 8];
        // Clear borrowed-window state so a following OWNED frame's
        // `current_block_range()` / `live_history()` read the owned mirror,
        // not a stale borrowed range. A borrowed frame re-arms via
        // `set_borrowed_window` after this reset.
        self.borrowed_input = None;
        self.borrowed_block = None;
        let tables_allocated = !self.row_positions.is_empty() || !self.hc_hash.is_empty();
        if next_floor <= REBASE_RESET_FLOOR_CEILING && tables_allocated {
            self.history_abs_start = next_floor;
        } else {
            // Bounded fallback: rewind the coordinate space and zero the
            // tables so the absolute cursor cannot climb without bound
            // (mirrors dfast; the u32 packing is separately kept in range
            // by `rebase_positions` in `add_data`).
            self.history_abs_start = 0;
            self.row_heads.fill(0);
            self.row_positions.fill(ROW_EMPTY_SLOT);
            self.row_tags.fill(0);
            let empty = self.hc_empty_slot();
            self.hc_hash.fill(empty);
            self.hc_chain.fill(empty);
        }
        // Nothing of the previous frame is indexed for the new one, and the
        // lazy reps restart from the frame's initial history. The window
        // starts at the frame start with no dictionary (a dictionary frame
        // primes these right after).
        self.lazy_next_to_update = self.history_abs_start;
        self.lazy_reps = None;
        self.loaded_dict_end = 0;
        self.low_limit = self.history_abs_start;
        self.prefix_low = self.history_abs_start;
        // Block buffers are returned to the caller's pool per block in
        // `add_data`, so there is nothing window-side to recycle here.
        self.chunk_lens.clear();
    }

    pub(crate) fn get_last_space(&self) -> &[u8] {
        if let (Some((ptr, _total)), Some((block_start, block_end))) =
            (self.borrowed_input, self.borrowed_block)
        {
            // Borrowed window: the active block is the in-place input range
            // `[block_start, block_end)`, staged before the scan so the emit
            // pipeline's pre-scan `get_last_space().len()` reserve is correct.
            // SAFETY: borrowed liveness contract; `block_start <= block_end <=
            // buffer len` (validated when staged).
            return unsafe {
                core::slice::from_raw_parts(ptr.add(block_start), block_end - block_start)
            };
        }
        let last = *self.chunk_lens.back().unwrap();
        &self.history[self.history.len() - last..]
    }

    pub(crate) fn add_data(&mut self, data: Vec<u8>, mut reuse_space: impl FnMut(Vec<u8>)) {
        assert!(data.len() <= self.max_window_size);
        super::match_table::storage::check_stream_abs_headroom(
            self.history_abs_start,
            self.window_size,
            data.len(),
        );
        // Row stores absolute match positions as `u32` (with `u32::MAX` the
        // empty sentinel). On a long stream the cumulative absolute cursor
        // crosses the u32 range even while the live window stays bounded, so
        // rebase the coordinate origin down to the oldest live byte before the
        // upcoming block's positions would overflow. Cold path — fires at most
        // once per ~4 GiB of stream, and one rebase always suffices because the
        // live window is far smaller than u32::MAX. `check_stream_abs_headroom`
        // above already guards the 32-bit-target `usize` overflow separately.
        if self.history_abs_start + self.window_size + data.len()
            >= u32::MAX as usize - 1 - BT_IDX_BASE
        {
            self.rebase_positions();
        }
        if self.window_size + data.len() > self.max_window_size {
            // Eviction advances `history_start`, staling the dict row index's
            // concat positions — drop the attach (dict slid within/out window).
            self.dict.invalidate();
            // Cap the history buffer near the live window instead of letting
            // the Vec power-of-two double to ~2x window on long streams. Once
            // eviction starts, reserve exactly (window + window/4 + one block)
            // so the buffer grows linearly to that ceiling; `compact_history`'s
            // quarter-window drain then keeps `len` under it, so the Vec never
            // reallocates again. Only fires in the eviction regime (large
            // inputs that fill the window) — small frames keep their tight
            // data-sized buffer untouched.
            let target = self.max_window_size
                + (self.max_window_size >> 2)
                + crate::common::MAX_BLOCK_SIZE as usize;
            if target > self.history.len() && self.history.capacity() < target {
                self.history.reserve_exact(target - self.history.len());
            }
        }
        while self.window_size + data.len() > self.max_window_size {
            let removed_len = self.chunk_lens.pop_front().unwrap();
            self.window_size -= removed_len;
            self.history_start += removed_len;
            self.history_abs_start += removed_len;
        }
        // Evicted bytes are gone from `history`, so the valid-data floor
        // rises with them (upstream `window.lowLimit`: the oldest byte the
        // buffer still holds). The distance-only window floor
        // (`pos - search_window`) can trail the eviction by up to a block, and
        // the DUBT walks floor on `low_limit` — without this they would
        // dereference evicted positions that are still inside the advertised
        // window.
        if self.low_limit < self.history_abs_start {
            self.low_limit = self.history_abs_start;
        }
        if self.prefix_low < self.low_limit {
            self.prefix_low = self.low_limit;
        }
        self.compact_history();
        let added = data.len();
        self.history.extend_from_slice(&data);
        self.window_size += added;
        self.chunk_lens.push_back(added);
        // The bytes now live in `history`; return the input buffer to the
        // caller's pool instead of holding a second copy in the window.
        reuse_space(data);
    }

    pub(crate) fn trim_to_window(&mut self) {
        if self.window_size > self.max_window_size {
            self.dict.invalidate();
        }
        while self.window_size > self.max_window_size {
            let removed_len = self.chunk_lens.pop_front().unwrap();
            self.window_size -= removed_len;
            self.history_start += removed_len;
            self.history_abs_start += removed_len;
        }
        // Same valid-data floor raise as `add_data`'s eviction loop.
        if self.low_limit < self.history_abs_start {
            self.low_limit = self.history_abs_start;
        }
        if self.prefix_low < self.low_limit {
            self.prefix_low = self.low_limit;
        }
    }

    /// Rebase the absolute coordinate origin down to the oldest live byte so
    /// stored `u32` match positions stay representable on long (multi-GiB)
    /// streams. Cold path, driven from [`Self::add_data`] when the cursor
    /// nears `u32::MAX`. Subtracts the current `history_abs_start` from every
    /// live `row_positions` entry; entries older than the new origin (already
    /// unreachable through the `candidate_pos < history_abs_start` read guard)
    /// collapse to `ROW_EMPTY_SLOT`. The shift is uniform across the origin and
    /// every stored position, so every match offset is preserved and matching
    /// is unaffected. `row_heads` (slot cursors) and `row_tags` (hash tags)
    /// hold no absolute positions and are left untouched.
    fn rebase_positions(&mut self) {
        let delta = self.history_abs_start;
        if delta == 0 {
            return;
        }
        let rebase_abs = |slot: &mut u32| {
            if *slot == ROW_EMPTY_SLOT {
                return;
            }
            let abs = *slot as usize;
            *slot = if abs < delta {
                ROW_EMPTY_SLOT
            } else {
                (abs - delta) as u32
            };
        };
        self.row_positions.iter_mut().for_each(rebase_abs);
        if self.hc_layout == LazyFinder::Tree {
            // Tree indices are `abs + BT_IDX_BASE`; 0 (none) and the unsorted
            // mark are kept (upstream `ZSTD_reduceTable_btlazy2`).
            let rebase_tree = |slot: &mut u32| {
                let v = *slot as usize;
                if v < BT_IDX_BASE {
                    return;
                }
                let abs = v - BT_IDX_BASE;
                *slot = if abs < delta {
                    0
                } else {
                    (abs - delta + BT_IDX_BASE) as u32
                };
            };
            self.hc_hash.iter_mut().for_each(rebase_tree);
            self.hc_chain.iter_mut().for_each(rebase_tree);
        } else {
            self.hc_hash.iter_mut().for_each(rebase_abs);
            self.hc_chain.iter_mut().for_each(rebase_abs);
        }
        self.lazy_next_to_update = self.lazy_next_to_update.saturating_sub(delta);
        self.low_limit = self.low_limit.saturating_sub(delta);
        self.prefix_low = self.prefix_low.saturating_sub(delta);
        if self.loaded_dict_end != 0 {
            self.loaded_dict_end = self.loaded_dict_end.saturating_sub(delta);
        }
        self.history_abs_start -= delta;
    }

    pub(crate) fn skip_matching_with_hint_rl<const ROW_LOG: usize>(
        &mut self,
        incompressible_hint: Option<bool>,
    ) {
        debug_assert_eq!(ROW_LOG, self.row_log);
        self.ensure_tables();
        let (current_abs_start, current_len) = self.current_block_range();
        let current_abs_end = current_abs_start + current_len;
        if self.finder != LazyFinder::Rows {
            // The chain / tree hold no rows to seed; the next lazy block
            // links the skipped block's tail from the carried cursor.
            self.lazy_next_to_update = current_abs_end.saturating_sub(ROW_HASH_KEY_LEN - 1);
            return;
        }
        let backfill_start = self.backfill_start(current_abs_start);
        if backfill_start < current_abs_start {
            self.insert_positions::<ROW_LOG>(backfill_start, current_abs_start);
        }
        match incompressible_hint {
            Some(true) => {
                // Sparse step + dense tail: caller declared the block
                // unlikely to compress, so we seed only every
                // `INCOMPRESSIBLE_SKIP_STEP` position plus a small tail to
                // keep cross-block continuity at the boundary.
                self.insert_positions_with_step::<ROW_LOG>(
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
                        self.insert_position::<ROW_LOG>(pos);
                    }
                }
            }
            Some(false) => {
                // Dense seeding requested by the caller: the entire
                // skipped range must remain queryable so subsequent
                // blocks can match into it. Currently only used by the
                // dictionary-priming path (upstream zstd's
                // `ZSTD_loadDictionaryContent` does the same dense fill
                // via `ZSTD_row_update_internalImpl` over every dict
                // byte), but the semantic is "dense fill on demand" and
                // future fast-paths (e.g. an RLE / raw-block emitter
                // that still wants cross-block matches into the skipped
                // bytes) can reuse it without rewording the contract.
                self.insert_positions::<ROW_LOG>(current_abs_start, current_abs_end);
            }
            None => {
                // Upstream zstd parity: a plain `skip_matching` (no hint) leaves
                // the row table untouched for the skipped range. Upstream zstd's
                // `ZSTD_row_fillHashCache` only pre-fills the next-scan
                // cache (8 positions of lookahead for SIMD prefetch); it
                // does NOT retroactively insert every byte of a skipped
                // block.
                //
                // Boundary handling: the `backfill_start` insert above
                // covers the `ROW_HASH_KEY_LEN - 1` bytes immediately
                // BEFORE `current_abs_start` (i.e. the previous block's
                // tail), keeping the current block's start hashable as
                // a cross-block match target. The CURRENT skipped
                // block's tail (the `ROW_HASH_KEY_LEN - 1` bytes ending
                // at `current_abs_end`) is itself backfilled lazily —
                // by the NEXT call's own `backfill_start` insert when
                // that call's `current_abs_start` lands at
                // `current_abs_end`. So a parse of block N+1 sees
                // block N's tail in the row table but not its
                // interior, matching upstream zstd.
                //
                // Trade: cross-block matches into a skipped block's
                // interior are lost (rare in practice — `skip_matching`
                // is called on blocks the driver upstream identified as
                // not worth scanning), but the per-block O(block_size)
                // `insert_position` storm is gone. On the L4 large-log-
                // stream bench (~104 MB / 800 blocks) the prior dense
                // fill dominated ~25% of Rust self-time at 131K inserts
                // per block × 800 = ~104M inserts.
            }
        }
        // A following lazy block gap-fills from here: the skipped block's
        // last `ROW_HASH_KEY_LEN - 1` bytes, the same tail the next call's
        // `backfill_start` insert covers.
        self.lazy_next_to_update = current_abs_end.saturating_sub(ROW_HASH_KEY_LEN - 1);
    }
    /// Upstream zstd-parity greedy parse for `lazy_depth == 0` (level 5).
    ///
    /// Mirrors `ZSTD_compressBlock_lazy_generic` (`zstd_lazy.c:1560`) with
    /// `depth == 0`, `dictMode == ZSTD_noDict`. The structural features
    /// that distinguish this greedy parse from the lazy parse in
    /// [`Self::start_matching`] (which `lazy_depth >= 1` strategies use):
    ///
    /// 1. **Default `start = pos + 1`**: each iteration first probes the
    ///    repcode bank at `abs_pos + 1` (treating one literal byte as
    ///    already committed). Upstream zstd's `start = ip + 1; matchLength = 0;
    ///    offBase = REPCODE1_TO_OFFBASE;` at the top of the loop body.
    ///    Only if a regular match at `abs_pos` is strictly longer does
    ///    `start` slide back to `abs_pos`. This trades one literal byte
    ///    for an unconditional repcode probe, which is the algorithmic
    ///    reason the strategy is called "greedy" — it greedily picks the
    ///    cheaper repcode encoding (4-5 bits) over a longer-offset
    ///    regular match (9-13 bits) whenever the rep hit is close to
    ///    matching the regular match's length.
    ///
    /// 2. **Hybrid commit, not upstream zstd's pure `goto _storeSequence`**:
    ///    upstream zstd's depth-0 path jumps to `_storeSequence` on the first
    ///    repcode hit and skips the regular search at `abs_pos`. We
    ///    deviate here — both the rep probe at `abs_pos + 1` *and* the
    ///    regular `row_candidate(abs_pos, ..)` are evaluated each
    ///    iteration, and the longer match wins (ties go to rep for
    ///    cheaper encoding via [`best_len_offset_candidate`]). Upstream zstd
    ///    can afford pure commit-on-first-rep because it recovers any
    ///    ratio loss via superblock-level entropy sharing, which we
    ///    don't replicate yet, so the hybrid form avoids a measured
    ///    ratio cliff on decodecorpus. (The row accept floor itself now
    ///    matches upstream zstd's `minMatch = 5` via `ROW_MIN_MATCH_LEN`; the
    ///    remaining un-replicated piece is the cross-block entropy
    ///    sharing, not the match-length threshold.) The hybrid form
    ///    still skips the upstream zstd `lazy_depth == 1` lookahead probe
    ///    that [`start_matching`] above runs unconditionally — the
    ///    speed shape stays upstream zstd-like.
    ///
    /// 3. **Skip-step grows with literal-run length**: on a miss upstream zstd
    ///    advances `ip += ((ip - anchor) >> kSearchStrength) + 1` with
    ///    `kSearchStrength = 8`. The plain matcher steps by 1 — denser
    ///    hash inserts (mild ratio benefit), but the upstream zstd parity skip
    ///    halves the per-byte work on incompressible runs (the
    ///    `lazySkipping` mode in upstream zstd is an extension of the same idea).
    ///
    /// Upstream zstd has an immediate-rep loop after store that probes
    /// `offset_2` for back-to-back hits. It is omitted here: the
    /// main-loop rep probe at `abs_pos + 1` already evaluates all
    /// three rep slots (rep1, rep2, rep3 + the upstream zstd `ll0` fallback)
    /// via [`repcode_candidate_shared`], so the inner-loop slot
    /// upstream zstd's single-rep design would catch is already covered by
    /// the next main-loop iteration. Confirmed dead-on-arrival via a
    /// `panic!` probe across the full 528-test suite + benchmark
    /// matrix (never fires).
    ///
    /// Catch-up backwards extension is already absorbed into the
    /// `MatchCandidate.start` field by `extend_backwards_shared`
    /// (called from `row_candidate` and `repcode_candidate_shared`),
    /// so we don't redo it explicitly.
    ///
    /// `pick_lazy_match` is intentionally not called here — depth == 0
    /// means "no lookahead", emit the first viable hit.
    pub(crate) fn ensure_tables(&mut self) {
        let row_count = 1usize << self.row_hash_log;
        let row_entries = 1usize << self.row_log;
        // Only the active finder's tables are held: the chain / tree
        // finders never read the rows (tens of MiB at the btlazy2 levels).
        let total = if self.finder == LazyFinder::Rows {
            row_count * row_entries
        } else {
            0
        };
        if total == 0 {
            self.row_heads = Vec::new();
            self.row_positions = Vec::new();
            self.row_tags = Vec::new();
        } else if self.row_positions.len() != total {
            // Resize in place: `set_hash_bits` width changes `clear()` the
            // vecs but keep their capacity. The previous `vec![..]` form
            // re-allocated all three tables on every width change — three
            // malloc/free pairs (~40 KiB) per hinted frame while the
            // configure→hint width pair disagreed, which allocator-slow
            // targets (musl) amplified into the dominant per-frame cost.
            self.row_heads.clear();
            self.row_heads.resize(row_count, 0);
            self.row_positions.clear();
            self.row_positions.resize(total, ROW_EMPTY_SLOT);
            self.row_tags.clear();
            self.row_tags.resize(total, 0);
        }
        if self.finder != LazyFinder::Rows {
            // Chain / tree mode: `hashTable` is `1 << hashLog` wide (the full
            // row hash width, no tag bits), `chainTable` `1 << chainLog` (the
            // tree: two links per node over `chainLog - 1` bits). The two
            // finders use different empty conventions, so tables laid out for
            // the other one are refilled.
            let hash_len = 1usize << (self.row_hash_log + self.row_log);
            let chain_len = 1usize << self.hc_chain_log;
            let empty = self.hc_empty_slot();
            let relayout = self.hc_layout != self.finder;
            if self.hc_hash.len() != hash_len || relayout {
                self.hc_hash.clear();
                self.hc_hash.resize(hash_len, empty);
            }
            if self.hc_chain.len() != chain_len || relayout {
                self.hc_chain.clear();
                self.hc_chain.resize(chain_len, empty);
            }
            self.hc_layout = self.finder;
        } else {
            // Rows mode never reads the chain / tree tables; a reused
            // compressor coming back from a btlazy2 level (same Row storage,
            // no backend swap) must not retain them (tens of MiB) alongside
            // the live row tables.
            self.hc_hash = Vec::new();
            self.hc_chain = Vec::new();
        }
    }

    /// Combined length of the chain / tree tables. Test-only.
    #[cfg(test)]
    pub(crate) fn hc_tables_len(&self) -> usize {
        self.hc_hash.len() + self.hc_chain.len()
    }

    /// The absolute coordinate floor. Test-only.
    #[cfg(test)]
    pub(crate) fn abs_floor(&self) -> usize {
        self.history_abs_start
    }

    /// Force the absolute coordinate floor (simulates a long-lived reused
    /// compressor whose cumulative cursor nears `u32::MAX`). Test-only.
    #[cfg(test)]
    pub(crate) fn set_abs_floor(&mut self, floor: usize) {
        self.history_abs_start = floor;
        self.low_limit = floor;
        self.prefix_low = floor;
        self.lazy_next_to_update = floor;
    }

    /// The empty-slot value of the chain / tree tables for the active finder.
    fn hc_empty_slot(&self) -> u32 {
        if self.finder == LazyFinder::Tree {
            0
        } else {
            ROW_EMPTY_SLOT
        }
    }

    fn compact_history(&mut self) {
        if self.history_start == 0 {
            return;
        }
        // Drain the (unreachable) dead prefix once it reaches a quarter window
        // so the buffer stays near `window + window/4` rather than growing to
        // ~2x window before the old full-window trigger fired. Paired with the
        // one-time `reserve_exact` in `add_data`, this keeps the Vec at a fixed
        // ~1.25x-window capacity on long streams. The drain memmoves the live
        // window, so a quarter-window trigger bounds the write amplification
        // (~4x the eviction stride) while closing most of the peak gap.
        if self.history_start >= (self.max_window_size >> 2)
            || self.history_start * 2 >= self.history.len()
        {
            self.history.drain(..self.history_start);
            self.history_start = 0;
        }
    }

    pub(crate) fn live_history(&self) -> &[u8] {
        // Borrowed one-shot: candidate/cursor bytes live in the caller's
        // input slice, not the owned mirror. Expose `[0, block_end)` so the
        // scan reads every prior byte in place (no input->mirror copy). The
        // branch is loop-invariant for a whole scan and inlines.
        if let Some((_start, end)) = self.borrowed_block {
            let (ptr, total) = self
                .borrowed_input
                .expect("borrowed_block set without a registered borrowed window");
            debug_assert!(
                end <= total,
                "borrowed block end {end} exceeds window {total}"
            );
            // SAFETY: `ptr` is the registered borrowed window's start (live by
            // the `set_borrowed_window` contract) and `end <= total` bytes are
            // in bounds.
            return unsafe { core::slice::from_raw_parts(ptr, end) };
        }
        &self.history[self.history_start..]
    }

    fn history_abs_end(&self) -> usize {
        self.history_abs_start + self.live_history().len()
    }

    /// Register the borrowed input window (the whole caller slice). Borrowed
    /// blocks staged after this read their bytes from `buffer` in place.
    ///
    /// The coordinate floor (`history_abs_start`, advanced by `reset` past
    /// every position the previous frame indexed) is kept: input offset `o`
    /// is absolute position `floor + o`, so a stale table entry of an
    /// earlier frame lies below the window floor and is never taken, and
    /// the chain / tree walks never meet a position at or past the one they
    /// search (offset 0 "self-matches" from a zeroed floor were a corrupt
    /// frame). The owned history is unused while borrowed (no `add_data`
    /// copy).
    ///
    /// # Safety
    /// `buffer` must stay live and unmodified until `clear_borrowed_window`
    /// or `reset` — the matcher stores a raw pointer into it.
    pub(crate) unsafe fn set_borrowed_window(&mut self, buffer: &[u8]) {
        // Same `u32` headroom guard as `add_data`: the borrowed reuse path
        // advances the coordinate floor per frame without ever committing
        // through `add_data`, so after ~4 GiB of cumulative reused frames the
        // inserted positions would wrap `u32` and every candidate would read
        // as stale. Rebase the origin down before this frame's positions are
        // stored (`rebase_positions` zeroes the floor; the previous frame's
        // stale entries collapse to empty, exactly what the advanced floor
        // already made them).
        // u64 arithmetic: on a 32-bit target the sum itself overflows
        // `usize` right where the guard must fire.
        if self.history_abs_start as u64 + buffer.len() as u64
            >= u32::MAX as u64 - 1 - BT_IDX_BASE as u64
        {
            self.rebase_positions();
        }
        self.borrowed_input = Some((buffer.as_ptr(), buffer.len()));
        self.borrowed_block = None;
        self.borrowed_extent = 0;
        // No dictionary on the borrowed path; the window bounds start at
        // the floor `reset` set.
        self.loaded_dict_end = 0;
    }

    pub(crate) fn clear_borrowed_window(&mut self) {
        self.borrowed_input = None;
        self.borrowed_block = None;
    }

    /// Stage `[block_start, block_end)` as the active borrowed block before a
    /// scan so `live_history()` / `current_block_range()` report it.
    pub(crate) fn stage_borrowed_block(&mut self, block_start: usize, block_end: usize) {
        let (_ptr, total) = self
            .borrowed_input
            .expect("stage_borrowed_block requires a registered borrowed window");
        assert!(
            block_start <= block_end && block_end <= total,
            "borrowed block bounds out of range: start={block_start} end={block_end} total={total}",
        );
        self.borrowed_block = Some((block_start, block_end));
        self.borrowed_extent = self.borrowed_extent.max(block_end);
    }

    /// `(current_abs_start, current_len)` for the active scan. Borrowed: the
    /// staged block range, at the coordinate floor. Owned: derived from the
    /// last committed chunk in the live window.
    fn current_block_range(&self) -> (usize, usize) {
        if let Some((start, end)) = self.borrowed_block {
            (self.history_abs_start + start, end - start)
        } else {
            let current_len = *self.chunk_lens.back().unwrap();
            (
                self.history_abs_start + self.window_size - current_len,
                current_len,
            )
        }
    }

    /// Row hash key at `idx`: `key_len` bytes (upstream zstd `mls`, 5-6 on the row
    /// levels) via one masked 8-byte read, degrading to the 4-byte key in
    /// the last <8 bytes of the window. Shared by the live hash and the
    /// dictionary row-index build — the two MUST bucket identically or
    /// dict-region probes go blind.
    ///
    /// The degradation is per window STATE: within one window a position
    /// hashes identically in the probe and the insert. The last <8
    /// positions of a pre-primed dictionary are a separate, unfixable
    /// case — the bytes following them exist only at probe time, so no
    /// fixed build-time key (4-byte, zero-padded, or otherwise) can match
    /// the probe's real-byte key there. Those few dict-tail entries stay
    /// unreachable, mirroring the upstream zstd, whose dictionary load also stops
    /// hashing short of the dictionary end.
    /// Upstream zstd `ZSTD_hash4/5/6` of the `mls`-byte key at `base[idx..]`
    /// with `salt` XORed into the product, reduced to the top `bits`:
    /// `ZSTD_hashPtrSalted` for the row tables, `ZSTD_hashPtr` (salt 0) for
    /// the hash chain.
    ///
    /// # Safety
    /// `idx + ROW_HASH_KEY_LEN <= len` and `base` must point to `len` bytes.
    #[inline(always)]
    unsafe fn key_hash_raw(
        base: *const u8,
        len: usize,
        idx: usize,
        mls: u32,
        bits: usize,
        salt: u64,
    ) -> u64 {
        debug_assert!(bits <= 32);
        // SAFETY: `idx + 4 <= len`; the 8-byte read is taken only when it fits.
        unsafe {
            let p = base.add(idx);
            if mls == 4 || idx + 8 > len {
                let v = rd32(p).wrapping_mul(ROW_HASH_PRIME4) ^ (salt as u32);
                u64::from(v >> (32 - bits))
            } else {
                let v = u64::from_le_bytes(p.cast::<[u8; 8]>().read_unaligned());
                let h = if mls == 5 {
                    (v << 24).wrapping_mul(ROW_HASH_PRIME5)
                } else {
                    (v << 16).wrapping_mul(ROW_HASH_PRIME6)
                };
                (h ^ salt) >> (64 - bits)
            }
        }
    }

    /// Hash-chain bucket of `abs_pos` (upstream `ZSTD_hashPtr(p, hashLog, mls)`).
    /// Callers pass positions at least `ROW_HASH_KEY_LEN` bytes before the end.
    #[inline(always)]
    fn hc_hash_at(&self, ctx: RowScan, abs_pos: usize) -> usize {
        let idx = abs_pos - ctx.hist_start;
        debug_assert!(idx + ROW_HASH_KEY_LEN <= ctx.len);
        let bits = self.row_hash_log + self.row_log;
        // SAFETY: caller contract, `ctx` views the live history.
        unsafe { Self::key_hash_raw(ctx.base, ctx.len, idx, self.row_hash_mls, bits, 0) as usize }
    }

    /// The live history as a [`RowScan`] for one parse.
    #[inline(always)]
    fn scan_ctx(&self) -> RowScan {
        let history = self.live_history();
        let dict_frame = self.loaded_dict_end != 0;
        RowScan {
            base: history.as_ptr(),
            len: history.len(),
            hist_start: self.history_abs_start,
            salt: self.hash_salt,
            search_window: self.search_window,
            low_limit: self.low_limit,
            prefix_start: self.prefix_low,
            dict_frame,
            attached: dict_frame && self.dict_plan.is_some_and(|p| p.attach),
        }
    }

    /// `(row, tag)` of `abs_pos` through a hoisted [`RowScan`]; `None` when
    /// fewer than `ROW_HASH_KEY_LEN` bytes follow the position.
    #[inline(always)]
    fn row_hash_at(&self, ctx: RowScan, abs_pos: usize) -> Option<(usize, u8)> {
        let idx = abs_pos - ctx.hist_start;
        if idx + ROW_HASH_KEY_LEN > ctx.len {
            return None;
        }
        let total_bits = self.row_hash_log + ROW_TAG_BITS;
        // SAFETY: `idx + ROW_HASH_KEY_LEN <= ctx.len`, `ctx` views the live history.
        let combined = unsafe {
            Self::key_hash_raw(
                ctx.base,
                ctx.len,
                idx,
                self.row_hash_mls,
                total_bits,
                ctx.salt,
            )
        };
        let row_mask = (1usize << self.row_hash_log) - 1;
        let row = ((combined >> ROW_TAG_BITS) as usize) & row_mask;
        Some((row, combined as u8))
    }

    #[inline(always)]
    pub(crate) fn hash_and_row(&self, abs_pos: usize) -> Option<(usize, u8)> {
        self.row_hash_at(self.scan_ctx(), abs_pos)
    }

    fn backfill_start(&self, current_abs_start: usize) -> usize {
        current_abs_start
            .saturating_sub(ROW_HASH_KEY_LEN - 1)
            .max(self.history_abs_start)
    }

    /// Used only by the dead-code [`Self::start_matching`] (lazy-style
    /// row parse). Kept paired with that method so reviving the lazy
    /// path doesn't have to re-derive the rep+row best-of-two pick.
    #[inline(always)]
    pub(crate) fn best_match_rl<K: RowTags, const ROW_LOG: usize>(
        &self,
        abs_pos: usize,
        lit_len: usize,
    ) -> Option<MatchCandidate> {
        let rep = self.repcode_candidate(abs_pos, lit_len);
        // SAFETY: `K` selected by `dispatch_tag_kernel!` after `detect` confirmed
        // its ISA; `K::probe` upholds the per-tier feature contract.
        let row = unsafe { K::probe::<ROW_LOG>(self, abs_pos, lit_len, None) };
        best_len_offset_candidate(rep, row)
    }

    #[inline(always)]
    pub(crate) fn pick_lazy_match_rl<K: RowTags, const ROW_LOG: usize>(
        &self,
        abs_pos: usize,
        lit_len: usize,
        best: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        let best = best?;
        // Same C-faithful length decision as the production monolith — route
        // through the shared `lazy_decide!` macro (test-only path; its finder is
        // out-of-line here, so the macro adds no register cost). `None` = commit.
        // `target_len = usize::MAX` matches the production row lazy body: upstream
        // lazy has no sufficient-length early-out (that belongs to the OPT parser).
        match crate::encoding::lazy_parse::lazy_decide!(
            best_len = best.match_len,
            best_off = best.offset,
            target_len = usize::MAX,
            lazy_depth = self.lazy_depth,
            abs_pos = abs_pos,
            lit_len = lit_len,
            history_end = self.history_abs_end(),
            min_match = self.mls,
            search = |p, l| self.best_match_rl::<K, ROW_LOG>(p, l),
        ) {
            ::core::option::Option::None => Some(best),
            ::core::option::Option::Some(_) => None,
        }
    }

    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn repcode_candidate(
        &self,
        abs_pos: usize,
        lit_len: usize,
    ) -> Option<MatchCandidate> {
        repcode_candidate_shared(
            self.cpl_kernel,
            self.live_history(),
            self.history_abs_start,
            self.offset_hist,
            abs_pos,
            lit_len,
            self.mls,
        )
    }

    // Two-level bounded dispatch: resolve the tag kernel (`K: RowTags`)
    // then the `row_log` const, both cold (once per block / call), into the
    // fully monomorphised `_rl::<K, ROW_LOG>` hot loop. The per-position
    // loop carries no runtime kernel enum and no `row_log` reload. Callers
    // (driver + tests) use these bare names; the hot loops call the `_rl`
    // siblings directly with the type and const already bound. `skip` does
    // no tag compare, so it dispatches on `row_log` only.
    pub(crate) fn start_matching(&mut self, handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        // SAFETY: same per-tier umbrella contract as `start_matching_greedy`.
        #[cfg(all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        ))]
        {
            // SAFETY: simd128 is a compile-time feature here; no runtime gate.
            unsafe { dispatch_lazy!(self.lazy_simd128::<Simd128Tags>(handle_sequence)) }
        }
        #[cfg(not(all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        )))]
        {
            match self.tag_kernel {
                #[cfg(all(
                    any(target_arch = "x86", target_arch = "x86_64"),
                    feature = "kernel-avx2"
                ))]
                FastpathKernel::Avx2Bmi2 => unsafe {
                    dispatch_lazy!(self.lazy_avx2bmi2::<Avx2Bmi2Tags>(handle_sequence))
                },
                #[cfg(all(
                    any(target_arch = "x86", target_arch = "x86_64"),
                    feature = "kernel-sse"
                ))]
                FastpathKernel::Sse2 | FastpathKernel::Sse42 => unsafe {
                    dispatch_lazy!(self.lazy_sse42::<Sse42Tags>(handle_sequence))
                },
                #[cfg(all(
                    target_arch = "aarch64",
                    target_endian = "little",
                    feature = "kernel-neon"
                ))]
                FastpathKernel::Neon => unsafe {
                    dispatch_lazy!(self.lazy_neon::<NeonTags>(handle_sequence))
                },
                // SAFETY: the scalar kernel has no `#[target_feature]`; the
                // fn is `unsafe` only for macro uniformity.
                FastpathKernel::Scalar => unsafe {
                    dispatch_lazy!(self.lazy_scalar::<ScalarTags>(handle_sequence))
                },
            }
        }
    }

    gen_lazy_monolith!(
        lazy_scalar,
        false,
        row_tag_mask_scalar,
        crate::encoding::fastpath::scalar::common_prefix_len_ptr
    );
    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-sse"
    ))]
    gen_lazy_monolith!(
        lazy_sse42,
        true,
        row_tag_mask_sse2,
        crate::encoding::fastpath::sse42::common_prefix_len_ptr,
        "sse2"
    );
    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-avx2"
    ))]
    gen_lazy_monolith!(
        lazy_avx2bmi2,
        true,
        row_tag_mask_avx2,
        crate::encoding::fastpath::avx2_bmi2::common_prefix_len_ptr,
        "avx2,bmi2"
    );
    #[cfg(all(
        target_arch = "aarch64",
        target_endian = "little",
        feature = "kernel-neon"
    ))]
    gen_lazy_monolith!(
        lazy_neon,
        true,
        row_tag_mask_neon,
        crate::encoding::fastpath::neon::common_prefix_len_ptr,
        "neon"
    );
    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ))]
    gen_lazy_monolith!(
        lazy_simd128,
        true,
        row_tag_mask_simd128,
        crate::encoding::fastpath::scalar::common_prefix_len_ptr
    );

    pub(crate) fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        match self.row_log {
            4 => self.skip_matching_with_hint_rl::<4>(incompressible_hint),
            5 => self.skip_matching_with_hint_rl::<5>(incompressible_hint),
            6 => self.skip_matching_with_hint_rl::<6>(incompressible_hint),
            _ => unreachable!("row_log is clamped to 4..=6 in configure()"),
        }
    }

    /// Borrowed (no-copy) one-shot equivalent of [`Self::start_matching`]:
    /// stage `[block_start, block_end)` of the registered borrowed window,
    /// then run the SAME parse dispatch. The parse body reads its block range
    /// via `current_block_range()` and its bytes via `live_history()`, both
    /// borrowed-aware, so the staged block is scanned in place (no
    /// `add_data` copy into the owned mirror). `history_abs_start` was forced
    /// to 0 in `set_borrowed_window`, so positions stay absolute input
    /// offsets and the window-low candidate cap bounds offsets to the window.
    pub(crate) fn start_matching_borrowed(
        &mut self,
        block_start: usize,
        block_end: usize,
        greedy: bool,
        handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        self.stage_borrowed_block(block_start, block_end);
        // Greedy runs the same lazy monolith at depth 0.
        let _ = greedy;
        self.start_matching(handle_sequence);
    }

    /// Borrowed equivalent of [`Self::skip_matching_with_hint`]: stage the
    /// block (so the RLE/Raw emit's `get_last_space` reserve reports it) and
    /// seed the row tables without a copy, mirroring the owned skip.
    pub(crate) fn skip_matching_borrowed(
        &mut self,
        block_start: usize,
        block_end: usize,
        incompressible_hint: Option<bool>,
    ) {
        self.stage_borrowed_block(block_start, block_end);
        self.skip_matching_with_hint(incompressible_hint);
    }

    #[allow(dead_code)]
    pub(crate) fn best_match(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        dispatch_tag_kernel!(self.best_match_k(abs_pos, lit_len))
    }
    fn best_match_k<K: RowTags>(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        dispatch_row_log!(self.best_match_rl::<K>(abs_pos, lit_len))
    }

    #[allow(dead_code)]
    pub(crate) fn pick_lazy_match(
        &self,
        abs_pos: usize,
        lit_len: usize,
        best: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        dispatch_tag_kernel!(self.pick_lazy_match_k(abs_pos, lit_len, best))
    }
    fn pick_lazy_match_k<K: RowTags>(
        &self,
        abs_pos: usize,
        lit_len: usize,
        best: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        dispatch_row_log!(self.pick_lazy_match_rl::<K>(abs_pos, lit_len, best))
    }

    // Per-kernel row match probe. Runtime kernel selection happens ONCE via
    // `dispatch_tag_kernel!`; the selected tier's `row_probe_*` method is the
    // monomorphised per-position hot loop with the SIMD tag-match inlined under
    // its `#[target_feature]` umbrella (no dispatcher branch inside the loop).
    #[allow(dead_code)]
    pub(crate) fn row_candidate(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        dispatch_tag_kernel!(self.row_candidate_k(abs_pos, lit_len))
    }
    fn row_candidate_k<K: RowTags>(
        &self,
        abs_pos: usize,
        lit_len: usize,
    ) -> Option<MatchCandidate> {
        // SAFETY: `dispatch_tag_kernel!` only selects a `K` whose ISA `detect`
        // confirmed present, upholding `K::probe`'s per-tier feature contract.
        match self.row_log {
            4 => unsafe { K::probe::<4>(self, abs_pos, lit_len, None) },
            5 => unsafe { K::probe::<5>(self, abs_pos, lit_len, None) },
            6 => unsafe { K::probe::<6>(self, abs_pos, lit_len, None) },
            _ => unreachable!("row_log is clamped to 4..=6 in configure()"),
        }
    }

    // Each tier pairs its tag-match mask macro with the matching
    // `fastpath::<tier>::common_prefix_len_ptr` so BOTH inline under the tier's
    // `#[target_feature]` umbrella (the cpl features must be a subset of the
    // probe's: the SSE2 umbrella covers the 128-bit mask intrinsics,
    // AVX2+BMI2 ⊇ the AVX2 mask).
    // Scalar uses the on-the-fly per-slot byte compare (`use_mask = false`).
    gen_row_probe!(
        row_probe_scalar,
        false,
        row_tag_mask_scalar,
        crate::encoding::fastpath::scalar::common_prefix_len_ptr
    );
    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-sse"
    ))]
    gen_row_probe!(
        row_probe_sse42,
        true,
        row_tag_mask_sse2,
        crate::encoding::fastpath::sse42::common_prefix_len_ptr,
        "sse2"
    );
    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-avx2"
    ))]
    gen_row_probe!(
        row_probe_avx2bmi2,
        true,
        row_tag_mask_avx2,
        crate::encoding::fastpath::avx2_bmi2::common_prefix_len_ptr,
        "avx2,bmi2"
    );
    #[cfg(all(
        target_arch = "aarch64",
        target_endian = "little",
        feature = "kernel-neon"
    ))]
    gen_row_probe!(
        row_probe_neon,
        true,
        row_tag_mask_neon,
        crate::encoding::fastpath::neon::common_prefix_len_ptr,
        "neon"
    );
    // wasm simd128 tag-match mask + scalar (portable) cpl. wasm simd128 is
    // compile-time, so no `#[target_feature]` umbrella is passed; mirrors the
    // wasm tier's behavior of vectorising only the tag scan, with the portable
    // prefix-length kernel.
    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ))]
    gen_row_probe!(
        row_probe_simd128,
        true,
        row_tag_mask_simd128,
        crate::encoding::fastpath::scalar::common_prefix_len_ptr
    );

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

    fn insert_positions<const ROW_LOG: usize>(&mut self, start: usize, end: usize) {
        for pos in start..end {
            self.insert_position::<ROW_LOG>(pos);
        }
    }

    fn insert_positions_with_step<const ROW_LOG: usize>(
        &mut self,
        start: usize,
        end: usize,
        step: usize,
    ) {
        if step <= 1 {
            self.insert_positions::<ROW_LOG>(start, end);
            return;
        }
        let mut pos = start;
        while pos < end {
            self.insert_position::<ROW_LOG>(pos);
            let next = pos.saturating_add(step);
            if next <= pos {
                break;
            }
            pos = next;
        }
    }

    #[inline(always)]
    fn insert_position<const ROW_LOG: usize>(&mut self, abs_pos: usize) {
        let Some((row, tag)) = self.hash_and_row(abs_pos) else {
            return;
        };
        self.insert_at::<ROW_LOG>(abs_pos, row, tag);
    }

    /// Prefetch a row's tag bytes and position words into L1 ahead of the
    /// next iteration's probe (no-op on targets without a prefetch hint).
    #[inline]
    fn prefetch_row<const ROW_LOG: usize>(&self, row: usize) {
        let row_base = row << ROW_LOG;
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            #[cfg(target_arch = "x86")]
            use core::arch::x86::{_MM_HINT_T0, _mm_prefetch};
            #[cfg(target_arch = "x86_64")]
            use core::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
            // SAFETY: prefetch is a hint and never faults; the indexes are in
            // bounds by the same `ensure_tables` sizing as `insert_at`.
            unsafe {
                _mm_prefetch(self.row_heads.as_ptr().add(row).cast(), _MM_HINT_T0);
                _mm_prefetch(self.row_tags.as_ptr().add(row_base).cast(), _MM_HINT_T0);
                _mm_prefetch(
                    self.row_positions.as_ptr().add(row_base).cast(),
                    _MM_HINT_T0,
                );
                // Upstream zstd `ZSTD_row_prefetch` (zstd_lazy.c:816): rows of
                // >= 32 entries span several 64-byte lines of positions; the
                // second line is fetched too.
                if ROW_LOG >= 5 {
                    _mm_prefetch(
                        self.row_positions.as_ptr().add(row_base + 16).cast(),
                        _MM_HINT_T0,
                    );
                }
            }
        }
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        {
            let _ = row_base;
        }
    }

    /// [`Self::insert_position`] with the (row, tag) pair already computed —
    /// the greedy miss path reuses the probe's hash instead of re-hashing
    /// the same position.
    #[inline(always)]
    fn insert_at<const ROW_LOG: usize>(&mut self, abs_pos: usize, row: usize, tag: u8) {
        // `ROW_LOG` is the compile-time row width for this monomorphisation;
        // the dispatcher guarantees `ROW_LOG == self.row_log` so the table
        // bounds (`ensure_tables` sized by `self.row_log`) hold.
        debug_assert_eq!(ROW_LOG, self.row_log);
        let row_entries = 1usize << ROW_LOG;
        let row_mask = row_entries - 1;
        let row_base = row << ROW_LOG;
        // SAFETY: `hash_and_row` masks `row` to `row_hash_log` bits and
        // `row_heads.len() == 1 << row_hash_log` by `ensure_tables`.
        // `row_base = row << row_log = row * row_entries` and
        // `next < row_entries`, so `row_base + next < row_count *
        // row_entries == row_positions.len() == row_tags.len()`. Both
        // index pairs are provably in bounds; per-byte hot path on
        // fast/dfast/row levels saves ~6 instructions and 3 branches.
        debug_assert!(row < self.row_heads.len());
        debug_assert!(row_base + row_entries <= self.row_positions.len());
        unsafe {
            let head = *self.row_heads.get_unchecked(row) as usize;
            // Upstream `ZSTD_row_nextIndex`: slot 0 is never written (it
            // holds the head byte in upstream's tag row), so the cursor
            // cycles over `1..=row_mask`.
            let next = match head.wrapping_sub(1) & row_mask {
                0 => row_mask,
                n => n,
            };
            *self.row_heads.get_unchecked_mut(row) = next as u8;
            *self.row_tags.get_unchecked_mut(row_base + next) = tag;
            // `abs_pos < u32::MAX` holds: `add_data` caps a Row frame's
            // absolute cursor below `u32::MAX`, so the cast is lossless and
            // never collides with the `ROW_EMPTY_SLOT == u32::MAX` sentinel.
            *self.row_positions.get_unchecked_mut(row_base + next) = abs_pos as u32;
        }
    }

    /// The whole dictionary is committed: finish its index and mark it built
    /// (CDict cache). The binary tree is built here rather than per slice
    /// because upstream sorts it in ONE pass over the whole dictionary
    /// (`ZSTD_updateTree`), each node compared against bytes up to the
    /// dictionary END; a per-slice build would compare against the slice
    /// end and link the tree differently. A copied dictionary also leaves
    /// `nextToUpdate` at the dictionary end, as `ZSTD_loadDictionaryContent`
    /// does: its last 8 positions are never indexed (their keys would read
    /// into the source, which upstream's non-contiguous window cannot).
    pub(crate) fn mark_dict_primed(&mut self) {
        if let Some(plan) = self.dict_plan {
            let concat_len = self.history.len() - self.history_start;
            if self.finder == LazyFinder::Tree {
                if plan.attach {
                    self.prime_dict_tree(plan, concat_len);
                } else {
                    self.build_live_dict_tree(concat_len);
                }
            }
            if !plan.attach {
                self.lazy_next_to_update = self.history_abs_start + concat_len;
            }
        }
        self.dict.mark_primed();
    }

    /// Upstream zstd `ZSTD_insertBt1`: insert the position at `content[pos]`
    /// (tree index `pos + index_base`) into the sorted tree, comparing
    /// against at most `nb_compares` nodes down to `window_low` (a tree
    /// index). Returns how many positions the tree update may skip
    /// (`forward`, at least 1): past the longest match seen minus 8, more
    /// after a very long match.
    ///
    /// # Safety
    /// `content` must point to `content_len` readable bytes with `pos + 8 <=
    /// content_len`; every tree index in `hash` / `bt` above `window_low`
    /// must map to a position below `pos`; `hash.len() == 1 << hash_log`,
    /// `bt.len() == 1 << chain_log`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn insert_bt1_sorted(
        hash: &mut [u32],
        bt: &mut [u32],
        content: *const u8,
        content_len: usize,
        pos: usize,
        index_base: usize,
        window_low: usize,
        hash_log: usize,
        chain_log: usize,
        nb_compares: usize,
        mls: u32,
    ) -> usize {
        use crate::encoding::fastpath::scalar::common_prefix_len_ptr;
        let bt_mask = (1usize << (chain_log - 1)) - 1;
        // SAFETY: `pos + 8 <= content_len` (caller contract).
        let h = unsafe { Self::key_hash_raw(content, content_len, pos, mls, hash_log, 0) } as usize;
        let curr = pos + index_base;
        let mut match_index = hash[h] as usize;
        // SAFETY: `pos < content_len`.
        let ip = unsafe { content.add(pos) };
        let iend_rem = content_len - pos;
        let bt_low = curr.saturating_sub(bt_mask);
        let mut smaller_ptr = 2 * (curr & bt_mask);
        let mut larger_ptr = smaller_ptr + 1;
        let mut match_end_idx = curr + 8 + 1;
        let mut best_len = 8usize;
        let mut nb = nb_compares;
        let mut common_smaller = 0usize;
        let mut common_larger = 0usize;
        hash[h] = curr as u32;
        while nb > 0 && match_index >= window_low {
            let next_ptr = 2 * (match_index & bt_mask);
            let mut ml = common_smaller.min(common_larger);
            let m_off = match_index - index_base;
            // SAFETY: `m_off < pos` (caller contract); the reads stop at
            // `iend_rem`.
            let (smaller, at_end) = unsafe {
                let mptr = content.add(m_off);
                ml += common_prefix_len_ptr(mptr.add(ml), ip.add(ml), iend_rem - ml);
                if ml == iend_rem {
                    (false, true)
                } else {
                    (*mptr.add(ml) < *ip.add(ml), false)
                }
            };
            if ml > best_len {
                best_len = ml;
                if ml > match_end_idx - match_index {
                    match_end_idx = match_index + ml;
                }
            }
            if at_end {
                break;
            }
            if smaller {
                bt[smaller_ptr] = match_index as u32;
                common_smaller = ml;
                if match_index <= bt_low {
                    smaller_ptr = BT_DISCARD;
                    break;
                }
                smaller_ptr = next_ptr + 1;
                match_index = bt[next_ptr + 1] as usize;
            } else {
                bt[larger_ptr] = match_index as u32;
                common_larger = ml;
                if match_index <= bt_low {
                    larger_ptr = BT_DISCARD;
                    break;
                }
                larger_ptr = next_ptr;
                match_index = bt[next_ptr] as usize;
            }
            nb -= 1;
        }
        if smaller_ptr != BT_DISCARD {
            bt[smaller_ptr] = 0;
        }
        if larger_ptr != BT_DISCARD {
            bt[larger_ptr] = 0;
        }
        let positions = if best_len > 384 {
            192.min(best_len - 384)
        } else {
            0
        };
        positions.max(match_end_idx - (curr + 8))
    }

    /// Upstream zstd `ZSTD_updateTree` over a dictionary of `concat_len`
    /// bytes at the front of the live history: every position up to
    /// `concat_len - 8` is inserted sorted (skipping past long matches as
    /// `ZSTD_insertBt1` directs), the tree indexed `position + index_base`
    /// with the tree's first index as the window floor.
    ///
    /// # Safety
    /// `hash.len() == 1 << hash_log`, `bt.len() == 1 << chain_log`, both
    /// holding only indices below `concat_len + index_base` (or none).
    #[allow(clippy::too_many_arguments)]
    unsafe fn update_dict_tree(
        hash: &mut [u32],
        bt: &mut [u32],
        content: *const u8,
        concat_len: usize,
        index_base: usize,
        hash_log: usize,
        chain_log: usize,
        nb_compares: usize,
        mls: u32,
    ) {
        let target = concat_len.saturating_sub(8);
        let mut idx = 0usize;
        while idx < target {
            // SAFETY: `idx + 8 <= concat_len`; the tree holds only earlier
            // dictionary positions (this loop's own inserts).
            let forward = unsafe {
                Self::insert_bt1_sorted(
                    hash,
                    bt,
                    content,
                    concat_len,
                    idx,
                    index_base,
                    index_base,
                    hash_log,
                    chain_log,
                    nb_compares,
                    mls,
                )
            };
            idx += forward.max(1);
        }
    }

    /// COPY-mode tree: the dictionary's sorted tree built straight into the
    /// live tables (upstream `ZSTD_resetCCtx_byCopyingCDict` copies the
    /// CDict's tree, built with the same cParams the frame now runs).
    fn build_live_dict_tree(&mut self, concat_len: usize) {
        self.ensure_tables();
        let hash_log = self.row_hash_log + self.row_log;
        let chain_log = self.hc_chain_log;
        let nb_compares = self.search_depth;
        let mls = self.row_hash_mls;
        let index_base = self.history_abs_start + BT_IDX_BASE;
        let base = self.history.as_ptr();
        let history_start = self.history_start;
        // SAFETY: `concat_len` bytes of dictionary follow `history_start`;
        // the live tables were just sized to the logs and hold nothing of
        // this frame yet (the tree convention's 0 = none).
        unsafe {
            Self::update_dict_tree(
                &mut self.hc_hash,
                &mut self.hc_chain,
                base.add(history_start),
                concat_len,
                index_base,
                hash_log,
                chain_log,
                nb_compares,
                mls,
            );
        }
    }

    /// ATTACH-mode tree: the CDict's sorted tree (its `hashLog` /
    /// `chainLog` / `searchLog` / `minMatch`) over the dictionary, probed by
    /// the search as `dictMatchState`. Skipped when the cached tree for this
    /// dictionary is already primed.
    fn prime_dict_tree(&mut self, plan: RowDictPlan, concat_len: usize) {
        let cd = plan.cdict;
        let hash_log = cd.hash_log as usize;
        let chain_log = cd.chain_log as usize;
        let row_log = (cd.search_log as usize).clamp(4, 6);
        let nb_compares = 1usize << cd.search_log;
        let mls = cd.min_match.clamp(4, 6);
        if self.dict.table().is_some_and(|d| {
            d.hash_log != hash_log || d.chain_log != chain_log || d.row_log != row_log || !d.use_bt
        }) {
            self.dict.invalidate();
        }
        self.dict.set_region_len(concat_len);
        if self.dict.is_primed() {
            return;
        }
        let base = self.history.as_ptr();
        let history_start = self.history_start;
        let dict = self.dict.table_mut_or_init(|| RowDictTables {
            heads: Vec::new(),
            positions: Vec::new(),
            tags: Vec::new(),
            hc_hash: alloc::vec![0u32; 1usize << hash_log],
            hc_chain: alloc::vec![0u32; 1usize << chain_log],
            hash_log,
            chain_log,
            row_log,
            use_row: false,
            use_bt: true,
        });
        // A cached tree of the right shape that was not marked primed was
        // built for another dictionary of the same length: rebuild it.
        dict.hc_hash.fill(0);
        dict.hc_chain.fill(0);
        // SAFETY: `concat_len` bytes of dictionary follow `history_start`;
        // the tables are freshly cleared.
        unsafe {
            Self::update_dict_tree(
                &mut dict.hc_hash,
                &mut dict.hc_chain,
                base.add(history_start),
                concat_len,
                BT_IDX_BASE,
                hash_log,
                chain_log,
                nb_compares,
                mls,
            );
        }
    }

    /// Drop the cached dict row index (next frame carries no dict, or eviction /
    /// resize staled the concat positions).
    pub(crate) fn invalidate_dict_cache(&mut self) {
        self.dict.invalidate();
    }

    /// Index the just-committed dictionary block (current `chunk_lens` tail)
    /// per the frame's [`RowDictPlan`]: upstream `ZSTD_loadDictionaryContent`
    /// for the lazy strategies indexes every dictionary position up to
    /// `dictSize - 8` (`ZSTD_row_update` / `ZSTD_insertAndFindFirstIndex`)
    /// with the CDict's cParams and no salt. ATTACH keeps that index in the
    /// separate dictionary tables the search probes as `dictMatchState`;
    /// COPY writes it into the live tables (whose geometry and salt are the
    /// CDict's for such a frame) and leaves `nextToUpdate` at `dictSize - 8`,
    /// as `ZSTD_resetCCtx_byCopyingCDict` inherits it. The dictionary arrives
    /// in slices; the index cursor carries across them so a slice boundary is
    /// no boundary for the 8-byte key reads.
    pub(crate) fn prime_dictionary_current_block(&mut self) {
        self.ensure_tables();
        let Some(plan) = self.dict_plan else {
            // A dictionary on a frame the plan does not cover stays plain
            // history.
            self.skip_matching_with_hint(Some(false));
            return;
        };
        let concat_len = self.history.len() - self.history_start;
        let indexable_end = concat_len.saturating_sub(8);
        let hist_start = self.history_abs_start;
        // Upstream window after the dictionary load: `loadedDictEnd` and the
        // prefix start (`dictLimit`) at the dictionary end; `lowLimit` at the
        // dictionary start when it is copied (the `extDict` segment below the
        // prefix) and at the prefix start when it is attached (the dictionary
        // is outside the window, reached through its own tables).
        self.loaded_dict_end = hist_start + concat_len;
        self.prefix_low = hist_start + concat_len;
        self.low_limit = if plan.attach {
            self.prefix_low
        } else {
            hist_start
        };
        if self.finder == LazyFinder::Tree {
            // The tree is sorted in one pass over the whole dictionary in
            // `mark_dict_primed`.
            self.dict.set_region_len(concat_len);
            return;
        }
        if plan.attach {
            self.prime_dict_tables(plan, concat_len, indexable_end);
        } else {
            // COPY: the dictionary is the frame's `extDict` segment
            // (`prefixStart` = its end) even though it lives in the live
            // tables. The index cursor carries across the dictionary's
            // slices; `mark_dict_primed` moves it to the dictionary end.
            self.dict.set_region_len(concat_len);
            let from = self.lazy_next_to_update.max(hist_start) - hist_start;
            if from < indexable_end {
                let scan = self.scan_ctx();
                if self.finder == LazyFinder::Chain {
                    // Absolute positions, like every other chain insert: a
                    // reused matcher's floor sits past its previous frames.
                    let chain_mask = (1usize << self.hc_chain_log) - 1;
                    for idx in from..indexable_end {
                        let abs = hist_start + idx;
                        let h = self.hc_hash_at(scan, abs);
                        self.hc_chain[abs & chain_mask] = self.hc_hash[h];
                        self.hc_hash[h] = abs as u32;
                    }
                } else {
                    match self.row_log {
                        4 => self.copy_dict_rows::<4>(scan, from, indexable_end),
                        5 => self.copy_dict_rows::<5>(scan, from, indexable_end),
                        _ => self.copy_dict_rows::<6>(scan, from, indexable_end),
                    }
                }
            }
            self.lazy_next_to_update = hist_start + indexable_end.max(from);
        }
    }

    /// COPY-mode row indexing of dictionary positions `[from, to)` (concat
    /// indices) into the live rows.
    fn copy_dict_rows<const ROW_LOG: usize>(&mut self, scan: RowScan, from: usize, to: usize) {
        for idx in from..to {
            let abs = scan.hist_start + idx;
            if let Some((row, tag)) = self.row_hash_at(scan, abs) {
                self.insert_at::<ROW_LOG>(abs, row, tag);
            }
        }
    }

    /// ATTACH-mode index: build the separate dictionary tables over the
    /// concat range `[cursor, indexable_end)` with the CDict's geometry
    /// (`hash_log`, `chain_log`, `row_log = clamp(searchLog, 4, 6)`), its key
    /// width, and salt 0 (`ZSTD_reset_matchState` for a CDict). Row form when
    /// the CDict resolved to rows, hash-chain form otherwise. Positions are
    /// CONCAT indices (stable across rebases). Skipped when the cached index
    /// for this dictionary is already primed.
    fn prime_dict_tables(&mut self, plan: RowDictPlan, concat_len: usize, indexable_end: usize) {
        let cd = plan.cdict;
        let hash_log = cd.hash_log as usize;
        let chain_log = cd.chain_log as usize;
        let row_log = (cd.search_log as usize).clamp(4, 6);
        let use_row = plan.use_row;
        let mls = cd.min_match.clamp(4, 6);
        if self.dict.table().is_some_and(|d| {
            d.hash_log != hash_log
                || d.chain_log != chain_log
                || d.row_log != row_log
                || d.use_row != use_row
                || d.use_bt
        }) {
            self.dict.invalidate();
        }
        self.dict.set_region_len(concat_len);
        if self.dict.is_primed() {
            return;
        }
        let from = self.dict.next_to_update();
        if from >= indexable_end {
            return;
        }
        self.dict.set_next_to_update(indexable_end);
        let history_start = self.history_start;
        // Raw history base taken before the mutable dict borrow (disjoint
        // fields; the raw ptr holds no borrow).
        let base = self.history.as_ptr();
        let dict = self.dict.table_mut_or_init(|| {
            let (row_count, total) = if use_row {
                (1usize << (hash_log - row_log), 1usize << hash_log)
            } else {
                (0, 0)
            };
            RowDictTables {
                heads: alloc::vec![0u8; row_count],
                positions: alloc::vec![ROW_EMPTY_SLOT; total],
                tags: alloc::vec![0u8; total],
                hc_hash: if use_row {
                    Vec::new()
                } else {
                    alloc::vec![ROW_EMPTY_SLOT; 1usize << hash_log]
                },
                hc_chain: if use_row {
                    Vec::new()
                } else {
                    alloc::vec![ROW_EMPTY_SLOT; 1usize << chain_log]
                },
                hash_log,
                chain_log,
                row_log,
                use_row,
                use_bt: false,
            }
        });
        // SAFETY: `concat + 8 <= concat_len` for every indexed position
        // (`indexable_end = concat_len - 8`), so the key reads stay inside the
        // history; every table index is masked to its table's width.
        unsafe {
            let content = base.add(history_start);
            if use_row {
                let row_hash_log = hash_log - row_log;
                let row_mask = (1usize << row_log) - 1;
                let row_count_mask = (1usize << row_hash_log) - 1;
                let heads = dict.heads.as_mut_ptr();
                let positions = dict.positions.as_mut_ptr();
                let tags = dict.tags.as_mut_ptr();
                for concat in from..indexable_end {
                    let combined = Self::key_hash_raw(
                        content,
                        concat_len,
                        concat,
                        mls,
                        row_hash_log + ROW_TAG_BITS,
                        0,
                    );
                    let row = ((combined >> ROW_TAG_BITS) as usize) & row_count_mask;
                    let tag = combined as u8;
                    let row_base = row << row_log;
                    let head = *heads.add(row) as usize;
                    // Upstream `ZSTD_row_nextIndex`: slot 0 is never written.
                    let next = match head.wrapping_sub(1) & row_mask {
                        0 => row_mask,
                        n => n,
                    };
                    *heads.add(row) = next as u8;
                    *tags.add(row_base + next) = tag;
                    *positions.add(row_base + next) = concat as u32;
                }
            } else {
                let chain_mask = (1usize << chain_log) - 1;
                let hash = dict.hc_hash.as_mut_ptr();
                let chain = dict.hc_chain.as_mut_ptr();
                for concat in from..indexable_end {
                    let h =
                        Self::key_hash_raw(content, concat_len, concat, mls, hash_log, 0) as usize;
                    *chain.add(concat & chain_mask) = *hash.add(h);
                    *hash.add(h) = concat as u32;
                }
            }
        }
    }
}

// Gated on `feature = "std"` because the runtime feature probe
// (`std::arch::is_x86_feature_detected!`) used to skip kernels the host CPU
// lacks is std-only, matching how `RowTagKernel::detect` gates the same probe.
#[cfg(all(
    test,
    feature = "std",
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
mod tag_mask_tests;

#[cfg(all(
    test,
    target_arch = "aarch64",
    target_endian = "little",
    feature = "kernel-neon"
))]
mod neon_tag_mask_tests;
